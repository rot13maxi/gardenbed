//! `metadata` — the SQLite metadata store: changesets, refs, the git-SHA
//! mapping table, and the derived-data cache.
//!
//! Opened in WAL mode with a 5 s busy timeout. All mutations are
//! transactional; in particular [`MetadataStore::cas_ref`] performs its
//! read-and-compare inside one `BEGIN IMMEDIATE` transaction, making a ref
//! swap a single atomic compare-and-swap.
//!
//! Concurrency contract: WAL journaling lets any number of connections
//! read the last committed snapshot without blocking a writer or each
//! other, but SQLite still allows one writer at a time. This store
//! therefore opens one dedicated write connection, guarded by a mutex and
//! used for every mutation, plus a pool of reader connections (4 by
//! default, [`SqliteMetadataStore::open_with_readers`]) checked out one
//! per concurrent read. Reads on a WAL snapshot see the state as of the
//! last commit when the read began — the same data the single-connection
//! design served, just without serializing readers behind a writer (or
//! each other) in-process.

/// Generates a fresh 16-byte repository incarnation (see
/// [`crate::traits::MetadataStore::repo_incarnation`]): 16 bytes from
/// `/dev/urandom` (never-blocking on Linux; the store crate deliberately
/// avoids a rand dependency for one 16-byte draw).
pub(crate) fn random_incarnation() -> Result<[u8; 16], StoreError> {
    use std::io::Read;
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| {
            StoreError::Db(format!(
                "cannot read /dev/urandom for repo incarnation: {e}"
            ))
        })?;
    Ok(buf)
}

/// Parses a stored incarnation blob, refusing lengths other than 16 (a
/// hand-edited row must not silently re-key the daemon's cache
/// namespaces onto a colliding value).
pub(crate) fn parse_incarnation(bytes: &[u8]) -> Result<[u8; 16], StoreError> {
    bytes.try_into().map_err(|_| {
        StoreError::Corrupt(format!(
            "repo_incarnation row holds {} bytes (expected 16)",
            bytes.len()
        ))
    })
}

use crate::StoreError;
use gardenbed_model::Changeset;
use gardenbed_model::Id;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::TransactionBehavior;
use rusqlite::params;
use rusqlite::params_from_iter;
use sha2::Digest;
use sha2::Sha256;
use std::path::Path;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS changesets (
    id        BLOB PRIMARY KEY,
    canonical BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS validated_changesets_v1 (
    cs        BLOB PRIMARY KEY REFERENCES changesets(id),
    root_tree BLOB NOT NULL CHECK(length(root_tree) IN (0, 32))
);
CREATE TABLE IF NOT EXISTS refs (
    name TEXT PRIMARY KEY,
    cs   BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS git_map (
    git_sha TEXT PRIMARY KEY,
    cs      BLOB NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS derived (
    kind  TEXT NOT NULL,
    cs    BLOB NOT NULL,
    bytes BLOB NOT NULL,
    PRIMARY KEY (kind, cs)
);
CREATE TABLE IF NOT EXISTS git_raw_commit (
    cs   BLOB PRIMARY KEY REFERENCES changesets(id),
    data BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS repo_incarnation (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    incarnation BLOB NOT NULL CHECK (length(incarnation) = 16)
);
";

/// zstd level for raw-commit frames — fixed for determinism (D21).
pub(crate) const ZSTD_LEVEL: i32 = 3;

/// Sanity bound on a decompressed raw commit (64 MiB): far above any real
/// commit object, small enough to refuse a corrupt frame's huge claim.
pub(crate) const MAX_RAW_COMMIT: usize = 64 * 1024 * 1024;

/// The SQLite (metadata) metadata store.
///
/// Cloning-free sharing: wrap in an `Arc` and share across threads. One
/// dedicated write connection (guarded by a mutex) serves every mutation
/// as a transaction; reads borrow pooled read connections, which WAL
/// journaling keeps unblocked by writes and by each other.
#[derive(Debug)]
pub struct SqliteMetadataStore {
    write_conn: Mutex<Connection>,
    read_pool: Vec<Mutex<Connection>>,
}

impl SqliteMetadataStore {
    /// Opens (creating if needed) the metadata database at `path`.
    ///
    /// Configures WAL journaling, `synchronous=NORMAL` (crash-safe against
    /// process kill; power-loss durability comes from the WAL), a 5 s busy
    /// timeout, and creates the schema if absent. The read pool holds the
    /// default of 4 reader connections.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::open_with_readers(path, 4)
    }

    /// [`SqliteMetadataStore::open`] with an explicit read-pool size.
    ///
    /// `readers` = 0 restores the pre-pool single-connection shape (every
    /// read borrows the write connection's mutex, so reads serialize
    /// behind writes exactly as before).
    pub fn open_with_readers(path: &Path, readers: usize) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let conn = Self::open_conn(path)?;
        conn.execute_batch(SCHEMA).map_err(db_err)?;
        let mut read_pool = Vec::with_capacity(readers);
        for _ in 0..readers {
            // Same pragmas as the write connection: the pool is
            // indistinguishable from it except for never writing.
            read_pool.push(Mutex::new(Self::open_conn(path)?));
        }
        Ok(SqliteMetadataStore {
            write_conn: Mutex::new(conn),
            read_pool,
        })
    }

    /// Opens one connection with the full pragma set (WAL, 5 s busy
    /// timeout, synchronous=NORMAL); the caller applies the schema.
    fn open_conn(path: &Path) -> Result<Connection, StoreError> {
        let conn = Connection::open(path).map_err(db_err)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(db_err)?;
        // journal_mode returns the resulting mode as a row, hence query_row.
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(db_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db_err)?;
        Ok(conn)
    }

    fn lock_write(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.write_conn
            .lock()
            .map_err(|_| StoreError::Db("metadata connection lock poisoned".into()))
    }

    /// Flushes the WAL into the database and truncates it, so committed
    /// data stops living in `metadata.sqlite3-wal`. With
    /// `synchronous=NORMAL` a commit legally leaves the database file
    /// itself untouched and the committed rows in the WAL; anything that
    /// relocates the database by renaming files (a repository move) must
    /// first fold the WAL in or the moved database is missing committed
    /// rows.
    ///
    /// The PRAGMA answers even when it could not finish: its result row
    /// is `(busy, log, checkpointed)` and `busy != 0` means another
    /// connection is holding frames the checkpoint cannot pass (an open
    /// read transaction, a writer mid-commit). That is exactly the
    /// caller's signal to refuse the relocation, so it is surfaced as
    /// [`StoreError::Db`] rather than reported as success.
    pub fn wal_checkpoint_truncate(&self) -> Result<(), StoreError> {
        let conn = self.lock_write()?;
        let (busy, _log, _checkpointed): (i64, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(db_err)?;
        if busy != 0 {
            return Err(StoreError::Db(
                "WAL checkpoint is blocked by another connection holding the database".into(),
            ));
        }
        Ok(())
    }

    /// Runs `f` in one immediate transaction on the write connection;
    /// commits on `Ok`, rolls back on `Err`.
    fn in_tx<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut conn = self.lock_write()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_err)?;
        match f(&tx) {
            Ok(value) => {
                tx.commit().map_err(db_err)?;
                Ok(value)
            }
            Err(e) => Err(e), // dropping `tx` rolls back
        }
    }

    /// Borrows a connection for reading: a pooled reader when one is
    /// free, else the write connection's mutex (the pool never grows
    /// past its fixed size, so excess readers queue exactly as before).
    fn read_conn(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        for r in &self.read_pool {
            if let Ok(guard) = r.try_lock() {
                return Ok(guard);
            }
        }
        self.lock_write()
    }
}

impl SqliteMetadataStore {
    #[allow(clippy::too_many_arguments)]
    fn write_changeset(
        &self,
        canonical: &[u8],
        git_sha_hex: Option<&str>,
        derived: &[crate::traits::DerivedRow],
        validated_tree: Option<Option<Id>>,
        raw_commit: Option<&[u8]>,
        verify: Option<&dyn Fn() -> Result<(), StoreError>>,
    ) -> Result<Id, StoreError> {
        use sha2::Digest;
        let sha = git_sha_hex.map(normalize_git_sha).transpose()?;
        let cs = Changeset::decode(canonical)
            .map_err(|e| StoreError::Invalid(format!("invalid canonical changeset: {e}")))?;
        let id = Id::from_bytes(sha2::Sha256::digest(canonical).into());
        // One immediate transaction for changeset + git mapping + derived
        // rows: a crash lands either the whole commit or none of it, and
        // the per-commit WAL traffic (and its shm lock churn) shrinks from
        // one transaction per row to one per commit.
        self.in_tx(|tx| {
            if validated_tree.is_some() {
                for parent in &cs.parents {
                    let certified: bool = tx
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM validated_changesets_v1 WHERE cs = ?1)",
                            params![parent.as_bytes()],
                            |r| r.get(0),
                        )
                        .map_err(db_err)?;
                    if !certified {
                        return Err(StoreError::Invalid(format!(
                            "parent {parent} is not certified"
                        )));
                    }
                }
            }
            let existing: Option<Vec<u8>> = tx
                .prepare_cached("SELECT canonical FROM changesets WHERE id = ?1")
                .map_err(db_err)?
                .query_row(params![id.as_bytes()], |r| r.get(0))
                .optional()
                .map_err(db_err)?;
            match existing {
                Some(bytes) if bytes == canonical => {} // idempotent re-put
                Some(_) => return Err(StoreError::ImmutabilityViolation(id)),
                None => {
                    tx.prepare_cached("INSERT INTO changesets(id, canonical) VALUES(?1, ?2)")
                        .map_err(db_err)?
                        .execute(params![id.as_bytes(), canonical])
                        .map_err(db_err)?;
                }
            }
            if let Some(sha) = sha.as_ref() {
                // Mapping bijection, mirroring record_git_mapping's checks.
                let by_sha: Option<Vec<u8>> = tx
                    .prepare_cached("SELECT cs FROM git_map WHERE git_sha = ?1")
                    .map_err(db_err)?
                    .query_row(params![sha], |r| r.get(0))
                    .optional()
                    .map_err(db_err)?;
                let by_cs: Option<String> = tx
                    .prepare_cached("SELECT git_sha FROM git_map WHERE cs = ?1")
                    .map_err(db_err)?
                    .query_row(params![id.as_bytes()], |r| r.get(0))
                    .optional()
                    .map_err(db_err)?;
                match (by_sha, by_cs) {
                    (Some(bytes), Some(existing_sha))
                        if id_from_blob(&bytes)? == id && existing_sha == *sha => {}
                    (Some(bytes), _) => {
                        let other = id_from_blob(&bytes)?;
                        return Err(StoreError::Invalid(format!(
                            "git sha {sha} already maps to changeset {}",
                            other.to_hex()
                        )));
                    }
                    (_, Some(existing_sha)) => {
                        return Err(StoreError::Invalid(format!(
                            "changeset {} already mapped from git sha {existing_sha}",
                            id.to_hex()
                        )));
                    }
                    (None, None) => {
                        tx.prepare_cached("INSERT INTO git_map(git_sha, cs) VALUES(?1, ?2)")
                            .map_err(db_err)?
                            .execute(params![sha, id.as_bytes()])
                            .map_err(db_err)?;
                    }
                }
            }
            for row in derived {
                check_kind(&row.kind)?;
                tx.prepare_cached(
                    "INSERT OR REPLACE INTO derived(kind, cs, bytes) VALUES(?1, ?2, ?3)",
                )
                .map_err(db_err)?
                .execute(params![row.kind, row.cs.as_bytes(), row.bytes])
                .map_err(db_err)?;
            }
            if let Some(root) = validated_tree {
                let payload = root.map(|id| id.as_bytes().to_vec()).unwrap_or_default();
                let previous: Option<Vec<u8>> = tx
                    .query_row(
                        "SELECT root_tree FROM validated_changesets_v1 WHERE cs = ?1",
                        params![id.as_bytes()],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(db_err)?;
                if let Some(previous) = previous {
                    if previous != payload {
                        return Err(StoreError::ImmutabilityViolation(id));
                    }
                } else {
                    tx.execute(
                        "INSERT INTO validated_changesets_v1(cs, root_tree) VALUES(?1, ?2)",
                        params![id.as_bytes(), payload],
                    )
                    .map_err(db_err)?;
                }
            }
            if let Some(raw) = raw_commit {
                // Sparse capture (D21): keep any existing row for the same
                // bytes; a different row is an immutability violation
                // (content addressing means the same changeset id always
                // reconstructs the same original commit).
                let previous: Option<Vec<u8>> = tx
                    .query_row(
                        "SELECT data FROM git_raw_commit WHERE cs = ?1",
                        params![id.as_bytes()],
                        |r| r.get::<_, Vec<u8>>(0),
                    )
                    .optional()
                    .map_err(db_err)?;
                let previous = previous
                    .map(|frame| {
                        // Rows are zstd frames; compare the *bytes* they
                        // decode to, so a level change never fabricates an
                        // immutability violation.
                        zstd::bulk::decompress(&frame, MAX_RAW_COMMIT)
                            .map_err(|e| StoreError::Corrupt(format!("raw commit frame: {e}")))
                    })
                    .transpose()?;
                match previous {
                    Some(bytes) if bytes == raw => {}
                    Some(_) => {
                        return Err(StoreError::ImmutabilityViolation(id));
                    }
                    None => {
                        let frame = zstd::bulk::compress(raw, ZSTD_LEVEL)
                            .map_err(|e| StoreError::Corrupt(format!("raw commit frame: {e}")))?;
                        tx.execute(
                            "INSERT INTO git_raw_commit(cs, data) VALUES(?1, ?2)",
                            params![id.as_bytes(), frame],
                        )
                        .map_err(db_err)?;
                    }
                }
            }
            // The last-chance object re-check runs under the write lock,
            // inside this transaction: indivisible against a concurrent
            // gc sweep's generation-check-plus-unlink step (which takes
            // the same lock). A failure rolls the whole commit back.
            if let Some(verify) = verify {
                verify()?;
            }
            Ok(())
        })?;
        Ok(id)
    }
}

impl crate::traits::MetadataStore for SqliteMetadataStore {
    fn repo_incarnation(&self) -> Result<[u8; 16], StoreError> {
        // Read-then-insert in ONE immediate transaction: the write lock
        // serializes concurrent first callers (including other processes
        // on the same database), so exactly one racer inserts and both
        // return the same stored bytes. The singleton CHECK (id = 1)
        // makes a second insert a constraint error even if the
        // transaction lock were somehow bypassed.
        self.in_tx(|tx| {
            if let Some(bytes) = tx
                .prepare_cached("SELECT incarnation FROM repo_incarnation WHERE id = 1")
                .map_err(db_err)?
                .query_row(params![], |r| r.get::<_, Vec<u8>>(0))
                .optional()
                .map_err(db_err)?
            {
                return parse_incarnation(&bytes);
            }
            let fresh = random_incarnation()?;
            tx.prepare_cached("INSERT INTO repo_incarnation(id, incarnation) VALUES(1, ?1)")
                .map_err(db_err)?
                .execute(params![fresh.as_slice()])
                .map_err(db_err)?;
            Ok(fresh)
        })
    }

    fn put_changeset(&self, cs: &Changeset) -> Result<Id, StoreError> {
        let canonical = cs.canonical_bytes().map_err(|e| {
            StoreError::Invalid(format!("changeset is not canonically encodable: {e}"))
        })?;
        let id = Id::from_bytes(Sha256::digest(&canonical).into());
        // Check-and-insert in one transaction so a concurrent writer of the
        // same id gets the idempotent path, not a constraint error.
        self.in_tx(|tx| {
            let existing: Option<Vec<u8>> = tx
                .prepare_cached("SELECT canonical FROM changesets WHERE id = ?1")
                .map_err(db_err)?
                .query_row(params![id.as_bytes()], |r| r.get(0))
                .optional()
                .map_err(db_err)?;
            match existing {
                Some(bytes) if bytes == canonical => Ok(id), // idempotent re-put
                Some(_) => Err(StoreError::ImmutabilityViolation(id)),
                None => {
                    tx.prepare_cached("INSERT INTO changesets(id, canonical) VALUES(?1, ?2)")
                        .map_err(db_err)?
                        .execute(params![id.as_bytes(), canonical])
                        .map_err(db_err)?;
                    Ok(id)
                }
            }
        })
    }

    fn get_changeset(&self, id: Id) -> Result<Option<(Changeset, Vec<u8>)>, StoreError> {
        let conn = self.read_conn()?;
        let canonical: Option<Vec<u8>> = conn
            .prepare_cached("SELECT canonical FROM changesets WHERE id = ?1")
            .map_err(db_err)?
            .query_row(params![id.as_bytes()], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        let canonical = match canonical {
            Some(c) => c,
            None => return Ok(None),
        };
        let decoded = Changeset::decode(&canonical)
            .map_err(|e| StoreError::Corrupt(format!("changeset {id}: {e}")))?;
        Ok(Some((decoded, canonical)))
    }

    fn resolve_ref(&self, name: &str) -> Result<Option<Id>, StoreError> {
        check_ref_name(name)?;
        let conn = self.read_conn()?;
        let cs: Option<Vec<u8>> = conn
            .prepare_cached("SELECT cs FROM refs WHERE name = ?1")
            .map_err(db_err)?
            .query_row(params![name], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        cs.map(|b| id_from_blob(&b)).transpose()
    }

    fn list_refs(&self) -> Result<Vec<(String, Id)>, StoreError> {
        let conn = self.read_conn()?;
        let mut stmt = conn
            .prepare("SELECT name, cs FROM refs ORDER BY name")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (name, cs) = row.map_err(db_err)?;
            out.push((name, id_from_blob(&cs)?));
        }
        Ok(out)
    }

    fn cas_ref(
        &self,
        name: &str,
        expected: Option<Id>,
        new: Id,
    ) -> Result<crate::traits::RefCas, StoreError> {
        check_ref_name(name)?;
        // Single atomic compare-and-swap: the read of the current value and
        // the conditional write happen inside one BEGIN IMMEDIATE
        // transaction, which holds the database write lock throughout.
        self.in_tx(|tx| {
            let current: Option<Vec<u8>> = tx
                .prepare_cached("SELECT cs FROM refs WHERE name = ?1")
                .map_err(db_err)?
                .query_row(params![name], |r| r.get(0))
                .optional()
                .map_err(db_err)?;
            match (expected, current) {
                (None, Some(cur)) => Ok(crate::traits::RefCas::Conflict {
                    current: id_from_blob(&cur)?,
                }),
                (None, None) => {
                    tx.prepare_cached("INSERT INTO refs(name, cs) VALUES(?1, ?2)")
                        .map_err(db_err)?
                        .execute(params![name, new.as_bytes()])
                        .map_err(db_err)?;
                    Ok(crate::traits::RefCas::Won { current: new })
                }
                (Some(expected), Some(cur)) => {
                    if cur.as_slice() == expected.as_bytes() {
                        // The WHERE clause re-asserts the expectation; inside
                        // the immediate transaction it cannot fail, but keep
                        // the CAS a single guarded statement.
                        let changed = tx
                            .prepare_cached("UPDATE refs SET cs = ?1 WHERE name = ?2 AND cs = ?3")
                            .map_err(db_err)?
                            .execute(params![new.as_bytes(), name, expected.as_bytes()])
                            .map_err(db_err)?;
                        if changed == 1 {
                            Ok(crate::traits::RefCas::Won { current: new })
                        } else {
                            Ok(crate::traits::RefCas::Conflict {
                                current: id_from_blob(&cur)?,
                            })
                        }
                    } else {
                        Ok(crate::traits::RefCas::Conflict {
                            current: id_from_blob(&cur)?,
                        })
                    }
                }
                (Some(_), None) => Err(StoreError::NotFound),
            }
        })
    }

    fn record_git_mapping(&self, git_sha_hex: &str, cs: Id) -> Result<(), StoreError> {
        let sha = normalize_git_sha(git_sha_hex)?;
        self.in_tx(|tx| {
            let by_sha: Option<Vec<u8>> = tx
                .prepare_cached("SELECT cs FROM git_map WHERE git_sha = ?1")
                .map_err(db_err)?
                .query_row(params![sha], |r| r.get(0))
                .optional()
                .map_err(db_err)?;
            let by_cs: Option<String> = tx
                .prepare_cached("SELECT git_sha FROM git_map WHERE cs = ?1")
                .map_err(db_err)?
                .query_row(params![cs.as_bytes()], |r| r.get(0))
                .optional()
                .map_err(db_err)?;
            match (by_sha, by_cs) {
                (Some(bytes), Some(existing_sha))
                    if id_from_blob(&bytes)? == cs && existing_sha == sha =>
                {
                    Ok(()) // already recorded, exactly as requested
                }
                (Some(bytes), _) => {
                    let other = id_from_blob(&bytes)?;
                    Err(StoreError::Invalid(format!(
                        "git sha {sha} already maps to changeset {}",
                        other.to_hex()
                    )))
                }
                (_, Some(existing_sha)) => Err(StoreError::Invalid(format!(
                    "changeset {} already mapped from git sha {existing_sha}",
                    cs.to_hex()
                ))),
                (None, None) => {
                    tx.prepare_cached("INSERT INTO git_map(git_sha, cs) VALUES(?1, ?2)")
                        .map_err(db_err)?
                        .execute(params![sha, cs.as_bytes()])
                        .map_err(db_err)?;
                    Ok(())
                }
            }
        })
    }

    fn cs_for_git(&self, git_sha_hex: &str) -> Result<Option<Id>, StoreError> {
        let sha = normalize_git_sha(git_sha_hex)?;
        let conn = self.read_conn()?;
        let cs: Option<Vec<u8>> = conn
            .prepare_cached("SELECT cs FROM git_map WHERE git_sha = ?1")
            .map_err(db_err)?
            .query_row(params![sha], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        cs.map(|b| id_from_blob(&b)).transpose()
    }

    fn git_for_cs(&self, cs: Id) -> Result<Option<String>, StoreError> {
        let conn = self.read_conn()?;
        conn.prepare_cached("SELECT git_sha FROM git_map WHERE cs = ?1")
            .map_err(db_err)?
            .query_row(params![cs.as_bytes()], |r| r.get(0))
            .optional()
            .map_err(db_err)
    }

    fn recent_changesets(&self, limit: u32) -> Result<Vec<Id>, StoreError> {
        let conn = self.read_conn()?;
        // Arrival order is insertion order; `rowid` is monotonic with it.
        // Published vs. unpublished is deliberately not filtered: the
        // point of this query is surfacing ref-less rows.
        let mut stmt = conn
            .prepare_cached("SELECT id FROM changesets ORDER BY rowid DESC LIMIT ?1")
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![limit], |r| r.get::<_, Vec<u8>>(0))
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(id_from_blob(&row.map_err(db_err)?)?);
        }
        Ok(out)
    }

    fn changeset_ids_in_id_range(
        &self,
        lo: &[u8],
        hi_exclusive: Option<&[u8]>,
    ) -> Result<Vec<Id>, StoreError> {
        let conn = self.read_conn()?;
        // `id` is the PRIMARY KEY BLOB, so a unique index backs it and both
        // bounds seek; the one-bound shape keeps that index seek when the
        // upper bound overflowed away (an all-F prefix).
        let mut stmt = conn
            .prepare_cached(match hi_exclusive {
                Some(_) => "SELECT id FROM changesets WHERE id >= ?1 AND id < ?2 ORDER BY id",
                None => "SELECT id FROM changesets WHERE id >= ?1 ORDER BY id",
            })
            .map_err(db_err)?;
        let bound: Vec<&[u8]> = match hi_exclusive {
            Some(hi) => vec![lo, hi],
            None => vec![lo],
        };
        let rows = stmt
            .query_map(params_from_iter(bound), |r| r.get::<_, Vec<u8>>(0))
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(id_from_blob(&row.map_err(db_err)?)?);
        }
        Ok(out)
    }

    fn derived_put(&self, kind: &str, cs: Id, bytes: &[u8]) -> Result<(), StoreError> {
        check_kind(kind)?;
        let conn = self.lock_write()?;
        conn.prepare_cached("INSERT OR REPLACE INTO derived(kind, cs, bytes) VALUES(?1, ?2, ?3)")
            .map_err(db_err)?
            .execute(params![kind, cs.as_bytes(), bytes])
            .map_err(db_err)?;
        Ok(())
    }

    fn derived_get(&self, kind: &str, cs: Id) -> Result<Option<Vec<u8>>, StoreError> {
        check_kind(kind)?;
        let conn = self.read_conn()?;
        let bytes: Option<Vec<u8>> = conn
            .prepare_cached("SELECT bytes FROM derived WHERE kind = ?1 AND cs = ?2")
            .map_err(db_err)?
            .query_row(params![kind, cs.as_bytes()], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        Ok(bytes)
    }

    fn ingest_changeset(
        &self,
        canonical: &[u8],
        git_sha_hex: &str,
        derived: &[crate::traits::DerivedRow],
    ) -> Result<Id, StoreError> {
        self.write_changeset(canonical, Some(git_sha_hex), derived, None, None, None)
    }

    fn validation_tree(&self, cs: Id) -> Result<Option<Option<Id>>, StoreError> {
        let conn = self.read_conn()?;
        let bytes: Option<Vec<u8>> = conn
            .query_row(
                "SELECT root_tree FROM validated_changesets_v1 WHERE cs = ?1",
                params![cs.as_bytes()],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        bytes
            .map(|bytes| {
                if bytes.is_empty() {
                    Ok(None)
                } else {
                    id_from_blob(&bytes).map(Some)
                }
            })
            .transpose()
    }

    fn raw_commit(&self, cs: Id) -> Result<Option<Vec<u8>>, StoreError> {
        let conn = self.read_conn()?;
        let frame: Option<Vec<u8>> = conn
            .query_row(
                "SELECT data FROM git_raw_commit WHERE cs = ?1",
                params![cs.as_bytes()],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        // One zstd frame per row (D21: `data BLOB zstd`), level fixed for
        // determinism — matching the blob-envelope convention.
        frame
            .map(|f| {
                zstd::bulk::decompress(&f, MAX_RAW_COMMIT)
                    .map_err(|e| StoreError::Corrupt(format!("raw commit frame: {e}")))
            })
            .transpose()
    }

    fn put_validated_changeset(
        &self,
        cs: &Changeset,
        root_tree: Option<Id>,
        derived: &[crate::traits::DerivedRow],
        git_sha_hex: Option<&str>,
        raw_commit: Option<&[u8]>,
        verify: Option<&dyn Fn() -> Result<(), StoreError>>,
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
            verify,
        )
    }
}

/// Validates a ref name. Empty names are rejected; everything else is
/// accepted in v0.1 (validation of ref namespaces is a later concern).
fn check_ref_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty() {
        return Err(StoreError::Invalid("ref name must not be empty".into()));
    }
    Ok(())
}

/// Validates the derived-data `kind` tag.
fn check_kind(kind: &str) -> Result<(), StoreError> {
    if kind.is_empty() || kind.len() > 64 {
        return Err(StoreError::Invalid(
            "derived-data kind must be 1..=64 characters".into(),
        ));
    }
    Ok(())
}

/// Accepts a 40-character git SHA in either case; returns the lowercase
/// canonical form used for storage and lookups.
fn normalize_git_sha(git_sha_hex: &str) -> Result<String, StoreError> {
    if git_sha_hex.len() != 40 || !git_sha_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(StoreError::Invalid(format!(
            "git sha must be 40 hex characters, got {git_sha_hex:?}"
        )));
    }
    Ok(git_sha_hex.to_ascii_lowercase())
}

/// Rebuilds an [`Id`] from a stored BLOB, failing loudly on wrong length.
fn id_from_blob(bytes: &[u8]) -> Result<Id, StoreError> {
    <[u8; 32]>::try_from(bytes)
        .map(Id::from_bytes)
        .map_err(|_| StoreError::Corrupt("stored id BLOB is not 32 bytes".into()))
}

fn db_err(e: rusqlite::Error) -> StoreError {
    StoreError::Db(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_sha_normalization() {
        assert_eq!(
            normalize_git_sha("ABCdef0123456789ABCdef0123456789ABCdef01").unwrap(),
            "abcdef0123456789abcdef0123456789abcdef01"
        );
        assert!(normalize_git_sha("abc").is_err());
        assert!(normalize_git_sha("zzzz000000000000000000000000000000000000").is_err());
    }

    #[test]
    fn blob_ids_roundtrip_through_blobs() {
        let id = Id::from_bytes([7u8; 32]);
        assert_eq!(id_from_blob(id.as_bytes()).unwrap(), id);
        assert!(id_from_blob(&[7u8; 31]).is_err());
    }
}
