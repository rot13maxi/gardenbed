//! `metadata` — the PostgreSQL metadata store: changesets, refs, the
//! git-SHA mapping table, and the derived-data cache, over a network
//! connection, for horizontally-scaled `gardenbed-server` replicas
//! (see `docs/scale-out-design.md`).
//!
//! Schema is a one-to-one mirror of the SQLite schema (decision D5):
//! same five tables, same columns and constraints, BLOB→`bytea`. Every
//! multi-row write — CAS, changeset+mapping+derived ingest, certificate
//! writes — happens in ONE deadpool transaction, so a crash mid-write
//! leaves either everything or nothing.
//!
//! CAS (decision D6) is a single guarded `UPDATE ... WHERE` on the
//! existing-ref path (rowcount 1 ⇔ won) and
//! `INSERT ... ON CONFLICT DO NOTHING` on the create path, re-reading
//! the current value for the Conflict report. No advisory locks.
//!
//! Async bridging: `tokio-postgres` is async while the traits are sync,
//! so the store owns a dedicated driver thread running a private
//! single-threaded runtime (see [`crate::driver`]); every trait method
//! `block_on`s its handle. That is safe to call from any thread,
//! including tokio workers of another runtime. Each method moves its
//! owned inputs into one `async move` block and runs the whole
//! statement sequence inline (pool checkout, optional transaction,
//! body, commit) — no closure indirection.

use crate::StoreError;
use crate::driver::Driver;
use crate::traits::DerivedRow;
use crate::traits::RefCas;
use deadpool_postgres::Manager;
use deadpool_postgres::ManagerConfig;
use deadpool_postgres::Pool;
use deadpool_postgres::RecyclingMethod;
use gardenbed_model::Changeset;
use gardenbed_model::Id;
use sha2::Digest;
use sha2::Sha256;
use tokio_postgres::NoTls;

/// Schema (D5): same five tables as SQLite, BLOB→`bytea`. `refs.cs`
/// carries no FK to changesets — SQLite has none either (a CAS may point
/// a ref at an id before its changeset row lands). `CREATE TABLE IF NOT
/// EXISTS` on every open, exactly like SQLite; first schema cut, no
/// migrations yet.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS changesets (
    id        bytea PRIMARY KEY,
    canonical bytea NOT NULL
);
CREATE TABLE IF NOT EXISTS validated_changesets_v1 (
    cs        bytea PRIMARY KEY,
    root_tree bytea NOT NULL CHECK(length(root_tree) IN (0, 32))
);
CREATE TABLE IF NOT EXISTS refs (
    name text PRIMARY KEY,
    cs   bytea NOT NULL
);
CREATE TABLE IF NOT EXISTS git_map (
    git_sha text PRIMARY KEY,
    cs      bytea NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS derived (
    kind  text NOT NULL,
    cs    bytea NOT NULL,
    bytes bytea NOT NULL,
    PRIMARY KEY (kind, cs)
);
CREATE TABLE IF NOT EXISTS git_raw_commit (
    cs   bytea PRIMARY KEY REFERENCES changesets(id),
    data bytea NOT NULL
);
";

/// Schema amendments applied after SCHEMA on every open
/// (`IF NOT EXISTS`-guarded, so idempotent). `arrived` tracks insertion
/// order for `recent_changesets` (SQLite has rowid; Postgres needs an
/// explicit sequence column). Rows from before the column existed order
/// behind all tracked arrivals.
const MIGRATIONS: &str = "
ALTER TABLE changesets ADD COLUMN IF NOT EXISTS arrived BIGSERIAL;
";

/// The PostgreSQL (metadata) metadata store.
///
/// Connections come from a `deadpool-postgres` pool sized to the CPU
/// count. Each pooled connection runs one query at a time, so a pool of
/// N bounds concurrent trait calls at N — ample for a gRPC replica.
/// Share via `Arc`.
#[derive(Debug)]
pub struct PostgresMetadataStore {
    pool: Pool,
    driver: Driver,
}

impl PostgresMetadataStore {
    /// Connects to `url` (e.g.
    /// `postgres://user:pass@host:5432/gardenbed`), creating the schema
    /// if absent. TLS is not negotiated in v0.1 (`NoTls`) — Postgres is
    /// expected on a trusted network or a local compose network.
    pub fn open(url: &str) -> Result<Self, StoreError> {
        let driver = Driver::new("gardenbed-pg-driver");
        let url = url.to_owned();
        let pool = driver.block_on(async move {
            let config: tokio_postgres::Config = url
                .parse()
                .map_err(|e| StoreError::Invalid(format!("invalid postgres url: {e}")))?;
            let manager = Manager::from_config(
                config,
                NoTls,
                ManagerConfig {
                    recycling_method: RecyclingMethod::Fast,
                },
            );
            let pool = Pool::builder(manager)
                .max_size(std::thread::available_parallelism().map_or(8, |n| n.get()))
                .build()
                .map_err(|e| StoreError::Db(format!("postgres pool: {e}")))?;
            // One connection up front: fail fast on unreachable servers
            // (instead of at the first trait call) and create the schema.
            let conn = pool.get().await.map_err(pg_err)?;
            conn.batch_execute(SCHEMA).await.map_err(pg_err)?;
            conn.batch_execute(MIGRATIONS).await.map_err(pg_err)?;
            Ok::<Pool, StoreError>(pool)
        })?;
        Ok(PostgresMetadataStore { pool, driver })
    }

    /// The shared body of [`crate::traits::MetadataStore::put_changeset`],
    /// [`crate::traits::MetadataStore::ingest_changeset`] and
    /// [`crate::traits::MetadataStore::put_validated_changeset`]:
    /// canonical row + git mapping + derived rows (+ certificate) in ONE
    /// transaction — the Postgres twin of SQLite's `write_changeset`.
    /// An error anywhere rolls the whole transaction back.
    #[allow(clippy::too_many_arguments)]
    fn write_changeset(
        &self,
        canonical: &[u8],
        git_sha_hex: Option<&str>,
        derived: &[DerivedRow],
        validated_tree: Option<Option<Id>>,
        raw_commit: Option<&[u8]>,
    ) -> Result<Id, StoreError> {
        let sha = git_sha_hex.map(normalize_git_sha).transpose()?;
        let cs = Changeset::decode(canonical)
            .map_err(|e| StoreError::Invalid(format!("invalid canonical changeset: {e}")))?;
        let id = Id::from_bytes(Sha256::digest(canonical).into());
        // Owned captures for the async block.
        let canonical = canonical.to_vec();
        let derived: Vec<(String, Vec<u8>, Vec<u8>)> = derived
            .iter()
            .map(|row| {
                (
                    row.kind.clone(),
                    row.cs.as_bytes().to_vec(),
                    row.bytes.clone(),
                )
            })
            .collect();
        let parents: Vec<Vec<u8>> = cs.parents.iter().map(|p| p.as_bytes().to_vec()).collect();
        // One zstd frame per row (D21), level fixed for determinism —
        // matching the SQLite backend's convention.
        let raw_frame = raw_commit
            .map(|raw| {
                zstd::bulk::compress(raw, crate::sqlite::ZSTD_LEVEL)
                    .map_err(|e| StoreError::Corrupt(format!("raw commit frame: {e}")))
            })
            .transpose()?;
        // Decoded bytes for the immutability compare (a level change must
        // not fabricate a violation — SQLite twin).
        let raw_bytes = raw_commit.map(|raw| raw.to_vec());
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let mut conn = pool.get().await.map_err(pg_err)?;
            let tx = conn.transaction().await.map_err(pg_err)?;

            if validated_tree.is_some() {
                for parent in &parents {
                    let certified = tx
                        .query_one(
                            "SELECT EXISTS(SELECT 1 FROM validated_changesets_v1 \
                             WHERE cs = $1)",
                            &[parent],
                        )
                        .await
                        .map_err(pg_err)?
                        .get::<_, bool>(0);
                    if !certified {
                        return Err(StoreError::Invalid("parent is not certified".into()));
                    }
                }
            }
            // Idempotent insert: same bytes stay, different bytes are a
            // violation, absent row inserts.
            let inserted = tx
                .execute(
                    "INSERT INTO changesets(id, canonical) VALUES($1, $2) \
                     ON CONFLICT (id) DO NOTHING",
                    &[&id.as_bytes().to_vec(), &canonical],
                )
                .await
                .map_err(pg_err)?;
            if inserted == 0 {
                let existing: Vec<u8> = tx
                    .query_one(
                        "SELECT canonical FROM changesets WHERE id = $1",
                        &[&id.as_bytes().to_vec()],
                    )
                    .await
                    .map_err(pg_err)?
                    .get(0);
                if existing != canonical {
                    return Err(StoreError::ImmutabilityViolation(id));
                }
            }
            if let Some(sha) = sha.as_deref() {
                // Mapping bijection. Insert-first: under READ COMMITTED
                // two concurrent identical writers can both see the
                // mapping absent (a SELECT-then-INSERT race), so the
                // insert — not the select — must be the claim; the
                // reads then only verify. Same shape as
                // `record_git_mapping`.
                tx.execute(
                    "INSERT INTO git_map(git_sha, cs) VALUES($1, $2) \
                     ON CONFLICT DO NOTHING",
                    &[&sha, &id.as_bytes().to_vec()],
                )
                .await
                .map_err(pg_err)?;
                let by_sha = tx
                    .query_opt("SELECT cs FROM git_map WHERE git_sha = $1", &[&sha])
                    .await
                    .map_err(pg_err)?;
                let by_cs = tx
                    .query_opt(
                        "SELECT git_sha FROM git_map WHERE cs = $1",
                        &[&id.as_bytes().to_vec()],
                    )
                    .await
                    .map_err(pg_err)?;
                match (by_sha, by_cs) {
                    (Some(row), Some(existing_sha))
                        if row.get::<_, Vec<u8>>(0) == id.as_bytes()
                            && existing_sha.get::<_, String>(0) == sha => {}
                    (Some(row), _) => {
                        let other = id_from_bytea(row.get(0))?;
                        return Err(StoreError::Invalid(format!(
                            "git sha {sha} already maps to changeset {}",
                            other.to_hex()
                        )));
                    }
                    (_, Some(existing_sha)) => {
                        return Err(StoreError::Invalid(format!(
                            "changeset {} already mapped from git sha {}",
                            id.to_hex(),
                            existing_sha.get::<_, String>(0)
                        )));
                    }
                    // Unreachable: the insert above guarantees a row for
                    // at least one of the two lookups.
                    (None, None) => {}
                }
            }
            for (kind, cs_id, bytes) in &derived {
                check_kind(kind)?;
                tx.execute(
                    "INSERT INTO derived(kind, cs, bytes) VALUES($1, $2, $3) \
                     ON CONFLICT (kind, cs) DO UPDATE SET bytes = EXCLUDED.bytes",
                    &[&kind, &cs_id, &bytes],
                )
                .await
                .map_err(pg_err)?;
            }
            if let Some(frame) = &raw_frame {
                // Compare decoded bytes, never frames: a level change must
                // not fabricate an immutability violation (SQLite twin).
                let previous = tx
                    .query_opt(
                        "SELECT data FROM git_raw_commit WHERE cs = $1",
                        &[&id.as_bytes().to_vec()],
                    )
                    .await
                    .map_err(pg_err)?;
                let previous = previous
                    .map(|row| {
                        let frame: Vec<u8> = row.get(0);
                        zstd::bulk::decompress(&frame, crate::sqlite::MAX_RAW_COMMIT)
                            .map_err(|e| StoreError::Corrupt(format!("raw commit frame: {e}")))
                    })
                    .transpose()?;
                match previous {
                    Some(bytes) if raw_bytes.as_deref() == Some(bytes.as_slice()) => {}
                    Some(_) => return Err(StoreError::ImmutabilityViolation(id)),
                    None => {
                        tx.execute(
                            "INSERT INTO git_raw_commit(cs, data) VALUES($1, $2)",
                            &[&id.as_bytes().to_vec(), &frame],
                        )
                        .await
                        .map_err(pg_err)?;
                    }
                }
            }
            if let Some(root) = validated_tree {
                let payload: Vec<u8> = root.map(|rid| rid.as_bytes().to_vec()).unwrap_or_default();
                let previous = tx
                    .query_opt(
                        "SELECT root_tree FROM validated_changesets_v1 WHERE cs = $1",
                        &[&id.as_bytes().to_vec()],
                    )
                    .await
                    .map_err(pg_err)?;
                match previous {
                    Some(row) => {
                        if row.get::<_, Vec<u8>>(0) != payload {
                            return Err(StoreError::ImmutabilityViolation(id));
                        }
                    }
                    None => {
                        tx.execute(
                            "INSERT INTO validated_changesets_v1(cs, root_tree) \
                             VALUES($1, $2)",
                            &[&id.as_bytes().to_vec(), &payload],
                        )
                        .await
                        .map_err(pg_err)?;
                    }
                }
            }

            tx.commit().await.map_err(pg_err)?;
            Ok(id)
        })
    }
}

impl crate::traits::MetadataStore for PostgresMetadataStore {
    fn put_changeset(&self, cs: &Changeset) -> Result<Id, StoreError> {
        let canonical = cs.canonical_bytes().map_err(|e| {
            StoreError::Invalid(format!("changeset is not canonically encodable: {e}"))
        })?;
        // ON CONFLICT DO NOTHING + re-read: a concurrent writer of the
        // same id gets the idempotent path, not a constraint error.
        let id = Id::from_bytes(Sha256::digest(&canonical).into());
        let canonical = canonical.clone();
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let mut conn = pool.get().await.map_err(pg_err)?;
            let tx = conn.transaction().await.map_err(pg_err)?;
            let inserted = tx
                .execute(
                    "INSERT INTO changesets(id, canonical) VALUES($1, $2) \
                     ON CONFLICT (id) DO NOTHING",
                    &[&id.as_bytes().to_vec(), &canonical],
                )
                .await
                .map_err(pg_err)?;
            if inserted == 0 {
                let existing: Vec<u8> = tx
                    .query_one(
                        "SELECT canonical FROM changesets WHERE id = $1",
                        &[&id.as_bytes().to_vec()],
                    )
                    .await
                    .map_err(pg_err)?
                    .get(0);
                if existing != canonical {
                    return Err(StoreError::ImmutabilityViolation(id));
                }
            }
            tx.commit().await.map_err(pg_err)?;
            Ok(id)
        })
    }

    fn get_changeset(&self, id: Id) -> Result<Option<(Changeset, Vec<u8>)>, StoreError> {
        let key = id.as_bytes().to_vec();
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let conn = pool.get().await.map_err(pg_err)?;
            let row = conn
                .query_opt("SELECT canonical FROM changesets WHERE id = $1", &[&key])
                .await
                .map_err(pg_err)?;
            match row {
                None => Ok(None),
                Some(row) => {
                    let canonical: Vec<u8> = row.get(0);
                    let decoded = Changeset::decode(&canonical)
                        .map_err(|e| StoreError::Corrupt(format!("changeset {id}: {e}")))?;
                    Ok(Some((decoded, canonical)))
                }
            }
        })
    }

    /// Newest-arrival first via the `arrived` serial column (the
    /// Postgres counterpart of SQLite's rowid scan). Rows predating the
    /// column (arrival order unknown — they existed before tracking)
    /// sort behind tracked arrivals; ties broken by id for stability.
    fn recent_changesets(&self, limit: u32) -> Result<Vec<Id>, StoreError> {
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let conn = pool.get().await.map_err(pg_err)?;
            let rows = conn
                .query(
                    "SELECT id FROM changesets \
                     ORDER BY arrived DESC NULLS LAST, id DESC LIMIT $1",
                    &[&i64::from(limit)],
                )
                .await
                .map_err(pg_err)?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(id_from_bytea(row.get(0))?);
            }
            Ok(out)
        })
    }

    fn resolve_ref(&self, name: &str) -> Result<Option<Id>, StoreError> {
        check_ref_name(name)?;
        let name = name.to_owned();
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let conn = pool.get().await.map_err(pg_err)?;
            let row = conn
                .query_opt("SELECT cs FROM refs WHERE name = $1", &[&name])
                .await
                .map_err(pg_err)?;
            match row {
                None => Ok(None),
                Some(row) => id_from_bytea(row.get(0)).map(Some),
            }
        })
    }

    fn list_refs(&self) -> Result<Vec<(String, Id)>, StoreError> {
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let conn = pool.get().await.map_err(pg_err)?;
            let rows = conn
                .query("SELECT name, cs FROM refs ORDER BY name", &[])
                .await
                .map_err(pg_err)?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push((row.get(0), id_from_bytea(row.get(1))?));
            }
            Ok(out)
        })
    }

    fn cas_ref(&self, name: &str, expected: Option<Id>, new: Id) -> Result<RefCas, StoreError> {
        check_ref_name(name)?;
        let name = name.to_owned();
        let new_bytes = new.as_bytes().to_vec();
        let expected_bytes = expected.map(|e| e.as_bytes().to_vec());
        // One guarded statement per path (D6), inside one transaction so
        // the Conflict re-read cannot observe a value newer than the one
        // that blocked the write.
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let mut conn = pool.get().await.map_err(pg_err)?;
            let tx = conn.transaction().await.map_err(pg_err)?;
            let outcome = match expected_bytes {
                Some(expected_bytes) => {
                    // Existing ref: rowcount 1 ⇔ won.
                    let changed = tx
                        .execute(
                            "UPDATE refs SET cs = $1 WHERE name = $2 AND cs = $3",
                            &[&new_bytes, &name, &expected_bytes],
                        )
                        .await
                        .map_err(pg_err)?;
                    if changed == 1 {
                        Ok(RefCas::Won { current: new })
                    } else {
                        // Lost the race (or the ref vanished): distinguish.
                        let row = tx
                            .query_opt("SELECT cs FROM refs WHERE name = $1", &[&name])
                            .await
                            .map_err(pg_err)?;
                        match row {
                            None => Err(StoreError::NotFound),
                            Some(row) => Ok(RefCas::Conflict {
                                current: id_from_bytea(row.get(0))?,
                            }),
                        }
                    }
                }
                None => {
                    // Create: INSERT .. ON CONFLICT DO NOTHING is the
                    // atomic claim; rowcount 0 means someone else won.
                    let inserted = tx
                        .execute(
                            "INSERT INTO refs(name, cs) VALUES($1, $2) \
                             ON CONFLICT (name) DO NOTHING",
                            &[&name, &new_bytes],
                        )
                        .await
                        .map_err(pg_err)?;
                    if inserted == 1 {
                        Ok(RefCas::Won { current: new })
                    } else {
                        let row = tx
                            .query_one("SELECT cs FROM refs WHERE name = $1", &[&name])
                            .await
                            .map_err(pg_err)?;
                        Ok(RefCas::Conflict {
                            current: id_from_bytea(row.get(0))?,
                        })
                    }
                }
            };
            match outcome {
                Ok(won) => {
                    tx.commit().await.map_err(pg_err)?;
                    Ok(won)
                }
                // Conflict / NotFound outcomes are still committed (they
                // made no change) so the transaction closes cleanly; the
                // error drops the transaction, which is equivalent.
                Err(e) => Err(e),
            }
        })
    }

    fn record_git_mapping(&self, git_sha_hex: &str, cs: Id) -> Result<(), StoreError> {
        let sha = normalize_git_sha(git_sha_hex)?;
        let cs_bytes = cs.as_bytes().to_vec();
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let mut conn = pool.get().await.map_err(pg_err)?;
            let tx = conn.transaction().await.map_err(pg_err)?;
            // INSERT ... ON CONFLICT DO NOTHING first, then verify the
            // resulting row: under READ COMMITTED two concurrent identical
            // calls can both see the mapping absent (a SELECT-then-INSERT
            // race), so the insert — not the select — must be the claim.
            // `write_changeset`'s mapping block uses this same shape.
            tx.execute(
                "INSERT INTO git_map(git_sha, cs) VALUES($1, $2) \
                 ON CONFLICT DO NOTHING",
                &[&sha, &cs_bytes],
            )
            .await
            .map_err(pg_err)?;
            let by_sha = tx
                .query_opt("SELECT cs FROM git_map WHERE git_sha = $1", &[&sha])
                .await
                .map_err(pg_err)?;
            let by_cs = tx
                .query_opt("SELECT git_sha FROM git_map WHERE cs = $1", &[&cs_bytes])
                .await
                .map_err(pg_err)?;
            match (by_sha, by_cs) {
                (Some(row), Some(existing_sha))
                    if row.get::<_, Vec<u8>>(0) == cs_bytes
                        && existing_sha.get::<_, String>(0) == sha =>
                {
                    // exactly the mapping requested (ours or a concurrent
                    // identical call's — either way, idempotent Ok)
                }
                (Some(row), _) => {
                    let other = id_from_bytea(row.get(0))?;
                    return Err(StoreError::Invalid(format!(
                        "git sha {sha} already maps to changeset {}",
                        other.to_hex()
                    )));
                }
                (_, Some(existing_sha)) => {
                    return Err(StoreError::Invalid(format!(
                        "changeset {} already mapped from git sha {}",
                        cs.to_hex(),
                        existing_sha.get::<_, String>(0)
                    )));
                }
                // Unreachable: the insert above guarantees a row for at
                // least one of the two lookups.
                (None, None) => {}
            }
            tx.commit().await.map_err(pg_err)?;
            Ok(())
        })
    }

    fn cs_for_git(&self, git_sha_hex: &str) -> Result<Option<Id>, StoreError> {
        let sha = normalize_git_sha(git_sha_hex)?;
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let conn = pool.get().await.map_err(pg_err)?;
            let row = conn
                .query_opt("SELECT cs FROM git_map WHERE git_sha = $1", &[&sha])
                .await
                .map_err(pg_err)?;
            match row {
                None => Ok(None),
                Some(row) => id_from_bytea(row.get(0)).map(Some),
            }
        })
    }

    fn git_for_cs(&self, cs: Id) -> Result<Option<String>, StoreError> {
        let key = cs.as_bytes().to_vec();
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let conn = pool.get().await.map_err(pg_err)?;
            let row = conn
                .query_opt("SELECT git_sha FROM git_map WHERE cs = $1", &[&key])
                .await
                .map_err(pg_err)?;
            Ok(row.map(|r| r.get(0)))
        })
    }

    fn changeset_ids_in_id_range(
        &self,
        lo: &[u8],
        hi_exclusive: Option<&[u8]>,
    ) -> Result<Vec<Id>, StoreError> {
        let lo = lo.to_vec();
        let hi = hi_exclusive.map(<[u8]>::to_vec);
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let conn = pool.get().await.map_err(pg_err)?;
            // Same PK range seek as SQLite; Postgres binds optionals as
            // separate statement shapes (no `$2 IS NULL` trick with a
            // unified plan for bytea).
            let rows = match hi {
                Some(hi) => {
                    conn.query(
                        "SELECT id FROM changesets WHERE id >= $1 AND id < $2 ORDER BY id",
                        &[&lo, &hi],
                    )
                    .await
                }
                None => {
                    conn.query(
                        "SELECT id FROM changesets WHERE id >= $1 ORDER BY id",
                        &[&lo],
                    )
                    .await
                }
            }
            .map_err(pg_err)?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                out.push(id_from_bytea(row.get(0))?);
            }
            Ok(out)
        })
    }

    fn derived_put(&self, kind: &str, cs: Id, bytes: &[u8]) -> Result<(), StoreError> {
        check_kind(kind)?;
        let kind = kind.to_owned();
        let cs_bytes = cs.as_bytes().to_vec();
        let bytes = bytes.to_vec();
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let conn = pool.get().await.map_err(pg_err)?;
            conn.execute(
                "INSERT INTO derived(kind, cs, bytes) VALUES($1, $2, $3) \
                 ON CONFLICT (kind, cs) DO UPDATE SET bytes = EXCLUDED.bytes",
                &[&kind, &cs_bytes, &bytes],
            )
            .await
            .map_err(pg_err)?;
            Ok(())
        })
    }

    fn derived_get(&self, kind: &str, cs: Id) -> Result<Option<Vec<u8>>, StoreError> {
        check_kind(kind)?;
        let kind = kind.to_owned();
        let cs_bytes = cs.as_bytes().to_vec();
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let conn = pool.get().await.map_err(pg_err)?;
            let row = conn
                .query_opt(
                    "SELECT bytes FROM derived WHERE kind = $1 AND cs = $2",
                    &[&kind, &cs_bytes],
                )
                .await
                .map_err(pg_err)?;
            Ok(row.map(|r| r.get(0)))
        })
    }

    fn ingest_changeset(
        &self,
        canonical: &[u8],
        git_sha_hex: &str,
        derived: &[DerivedRow],
    ) -> Result<Id, StoreError> {
        self.write_changeset(canonical, Some(git_sha_hex), derived, None, None)
    }

    fn validation_tree(&self, cs: Id) -> Result<Option<Option<Id>>, StoreError> {
        let key = cs.as_bytes().to_vec();
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let conn = pool.get().await.map_err(pg_err)?;
            let row = conn
                .query_opt(
                    "SELECT root_tree FROM validated_changesets_v1 WHERE cs = $1",
                    &[&key],
                )
                .await
                .map_err(pg_err)?;
            match row {
                None => Ok(None),
                Some(row) => {
                    let bytes: Vec<u8> = row.get(0);
                    if bytes.is_empty() {
                        Ok(Some(None))
                    } else {
                        id_from_bytea(&bytes).map(|id| Some(Some(id)))
                    }
                }
            }
        })
    }

    fn put_validated_changeset(
        &self,
        cs: &Changeset,
        root_tree: Option<Id>,
        derived: &[DerivedRow],
        git_sha_hex: Option<&str>,
        raw_commit: Option<&[u8]>,
    ) -> Result<Id, StoreError> {
        let canonical = cs
            .canonical_bytes()
            .map_err(|e| StoreError::Invalid(e.to_string()))?;
        self.write_changeset(
            &canonical,
            git_sha_hex,
            derived,
            Some(root_tree),
            raw_commit,
        )
    }

    fn raw_commit(&self, cs: Id) -> Result<Option<Vec<u8>>, StoreError> {
        let pool = self.pool.clone();
        self.driver.block_on(async move {
            let conn = pool.get().await.map_err(pg_err)?;
            let frame: Option<Vec<u8>> = conn
                .query_opt(
                    "SELECT data FROM git_raw_commit WHERE cs = $1",
                    &[&cs.as_bytes().to_vec()],
                )
                .await
                .map_err(pg_err)?
                .map(|row| row.get(0));
            // One zstd frame per row (D21: `data bytea zstd`), level fixed
            // for determinism — matching the SQLite backend's convention.
            frame
                .map(|f| {
                    zstd::bulk::decompress(&f, crate::sqlite::MAX_RAW_COMMIT)
                        .map_err(|e| StoreError::Corrupt(format!("raw commit frame: {e}")))
                })
                .transpose()
        })
    }
}

/// Validates a ref name. Empty names are rejected; everything else is
/// accepted in v0.1 (identical to the SQLite backend's rule).
fn check_ref_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty() {
        return Err(StoreError::Invalid("ref name must not be empty".into()));
    }
    Ok(())
}

/// Validates the derived-data `kind` tag (identical rule to SQLite).
fn check_kind(kind: &str) -> Result<(), StoreError> {
    if kind.is_empty() {
        return Err(StoreError::Invalid("derived kind must not be empty".into()));
    }
    Ok(())
}

/// Accepts a 40-character git SHA in either case; returns the lowercase
/// canonical form used for storage and lookups.
fn normalize_git_sha(git_sha_hex: &str) -> Result<String, StoreError> {
    if git_sha_hex.len() != 40 || !git_sha_hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(StoreError::Invalid(format!(
            "git sha {git_sha_hex:?} is not 40 hex characters"
        )));
    }
    Ok(git_sha_hex.to_ascii_lowercase())
}

/// Rebuilds an [`Id`] from a stored `bytea`, failing loudly on wrong
/// length (identical to the SQLite backend's `id_from_blob`).
fn id_from_bytea(bytes: &[u8]) -> Result<Id, StoreError> {
    Ok(Id::from_bytes(bytes.try_into().map_err(|_| {
        StoreError::Corrupt(format!("stored id has {} bytes, expected 32", bytes.len()))
    })?))
}

fn pg_err(e: impl std::fmt::Display) -> StoreError {
    StoreError::Db(e.to_string())
}
