//! `seedbed` — the SQLite metadata store: changesets, refs, the git-SHA
//! mapping table, and the derived-data cache (compost).
//!
//! Opened in WAL mode with a 5 s busy timeout so readers never block writers
//! and contending writers queue instead of failing. All mutations are
//! transactional; in particular [`MetadataStore::cas_ref`] performs its
//! read-and-compare inside one `BEGIN IMMEDIATE` transaction, making a ref
//! swap a single atomic compare-and-swap.

use crate::StoreError;
use gardenbed_model::Changeset;
use gardenbed_model::Id;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::TransactionBehavior;
use rusqlite::params;
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
";

/// The SQLite (seedbed) metadata store.
///
/// Cloning-free sharing: wrap in an `Arc` and share across threads; all
/// access serializes on one connection guarded by a mutex, and every
/// mutation is a transaction. WAL mode keeps readers unblocked.
#[derive(Debug)]
pub struct SqliteMetadataStore {
    conn: Mutex<Connection>,
}

impl SqliteMetadataStore {
    /// Opens (creating if needed) the metadata database at `path`.
    ///
    /// Configures WAL journaling, `synchronous=NORMAL` (crash-safe against
    /// process kill; power-loss durability comes from the WAL), a 5 s busy
    /// timeout, and creates the schema if absent.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path).map_err(db_err)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(db_err)?;
        // journal_mode returns the resulting mode as a row, hence query_row.
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(db_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(db_err)?;
        conn.execute_batch(SCHEMA).map_err(db_err)?;
        Ok(SqliteMetadataStore {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.conn
            .lock()
            .map_err(|_| StoreError::Db("seedbed connection lock poisoned".into()))
    }

    /// Runs `f` in one immediate transaction; commits on `Ok`, rolls back on
    /// `Err`.
    fn in_tx<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut conn = self.lock()?;
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
}

impl crate::traits::MetadataStore for SqliteMetadataStore {
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
        let conn = self.lock()?;
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
        let conn = self.lock()?;
        let cs: Option<Vec<u8>> = conn
            .prepare_cached("SELECT cs FROM refs WHERE name = ?1")
            .map_err(db_err)?
            .query_row(params![name], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        cs.map(|b| id_from_blob(&b)).transpose()
    }

    fn list_refs(&self) -> Result<Vec<(String, Id)>, StoreError> {
        let conn = self.lock()?;
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
        let conn = self.lock()?;
        let cs: Option<Vec<u8>> = conn
            .prepare_cached("SELECT cs FROM git_map WHERE git_sha = ?1")
            .map_err(db_err)?
            .query_row(params![sha], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        cs.map(|b| id_from_blob(&b)).transpose()
    }

    fn git_for_cs(&self, cs: Id) -> Result<Option<String>, StoreError> {
        let conn = self.lock()?;
        conn.prepare_cached("SELECT git_sha FROM git_map WHERE cs = ?1")
            .map_err(db_err)?
            .query_row(params![cs.as_bytes()], |r| r.get(0))
            .optional()
            .map_err(db_err)
    }

    fn derived_put(&self, kind: &str, cs: Id, bytes: &[u8]) -> Result<(), StoreError> {
        check_kind(kind)?;
        let conn = self.lock()?;
        conn.prepare_cached("INSERT OR REPLACE INTO derived(kind, cs, bytes) VALUES(?1, ?2, ?3)")
            .map_err(db_err)?
            .execute(params![kind, cs.as_bytes(), bytes])
            .map_err(db_err)?;
        Ok(())
    }

    fn derived_get(&self, kind: &str, cs: Id) -> Result<Option<Vec<u8>>, StoreError> {
        check_kind(kind)?;
        let conn = self.lock()?;
        let bytes: Option<Vec<u8>> = conn
            .prepare_cached(
                "SELECT bytes FROM derived WHERE kind = ?1 AND cs = ?2",
            )
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
        use sha2::Digest;
        let sha = normalize_git_sha(git_sha_hex)?;
        let id = Id::from_bytes(sha2::Sha256::digest(canonical).into());
        // One immediate transaction for changeset + git mapping + derived
        // rows: a crash lands either the whole commit or none of it, and
        // the per-commit WAL traffic (and its shm lock churn) shrinks from
        // one transaction per row to one per commit.
        self.in_tx(|tx| {
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
                    tx.prepare_cached(
                        "INSERT INTO changesets(id, canonical) VALUES(?1, ?2)",
                    )
                    .map_err(db_err)?
                    .execute(params![id.as_bytes(), canonical])
                    .map_err(db_err)?;
                }
            }
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
                    if id_from_blob(&bytes)? == id && existing_sha == sha => {}
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
                    tx.prepare_cached(
                        "INSERT INTO git_map(git_sha, cs) VALUES(?1, ?2)",
                    )
                    .map_err(db_err)?
                    .execute(params![sha, id.as_bytes()])
                    .map_err(db_err)?;
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
            Ok(())
        })?;
        Ok(id)
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
