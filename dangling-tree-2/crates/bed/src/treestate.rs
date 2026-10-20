//! `.bed/treestate` — the sqlite index of the materialized plot
//! (v0.1.md §6).
//!
//! One row per materialized file:
//!
//! ```sql
//! files(path TEXT PRIMARY KEY, blob_id BLOB, mode INTEGER,
//!       size INTEGER, mtime_ns INTEGER, hint INTEGER)
//! ```
//!
//! - `blob_id` — the blob's SHA-256 content address (raw 32 bytes), as
//!   recorded when the file was materialized or sown.
//! - `mode` — §4 mode codes (0 regular · 1 exec · 2 symlink · 3 gitlink).
//! - `size` / `mtime_ns` — the *hints* `bed status` uses to avoid hashing:
//!   if a file's size and mtime still match the row (and `hint` marks the
//!   row verified), the file is assumed untouched.
//! - `hint` — 1 when the hints were hash-verified at write time, 0 when
//!   they are unproven and the file must always be hashed.
//!
//! The database is opened in WAL mode so a reader (status) never blocks a
//! writer (treestate updates during sow/goto in later waves).

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use gardenbed_model::Id;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::error::BedError;

/// Hint value meaning "these hints were hash-verified at write time; a
/// matching size+mtime on the next scan may be trusted without hashing".
pub const HINT_VERIFIED: i64 = 1;

/// Hint value meaning "unverified — always re-hash this file". Rows written
/// by anything other than a hash-checked materialization should use this.
pub const HINT_UNVERIFIED: i64 = 0;

/// §4 mode code for a regular file (materialized with default permissions).
pub const MODE_REGULAR: i64 = 0;
/// §4 mode code for an executable file.
pub const MODE_EXEC: i64 = 1;
/// §4 mode code for a symlink (the "content" is the link target).
pub const MODE_SYMLINK: i64 = 2;
/// §4 mode code for a submodule pointer (never materialized as a file).
pub const MODE_GITLINK: i64 = 3;

/// One row of the treestate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreestateEntry {
    /// Repo-relative, forward-slash path — the changesets' flat path space.
    pub path: String,
    /// SHA-256 content address of the file's bytes.
    pub blob_id: Id,
    /// §4 mode code (0 regular · 1 exec · 2 symlink · 3 gitlink).
    pub mode: i64,
    /// Content length in bytes at the time of recording.
    pub size: i64,
    /// File mtime, nanoseconds since the Unix epoch, at the time of
    /// recording.
    pub mtime_ns: i64,
    /// [`HINT_VERIFIED`] or [`HINT_UNVERIFIED`] — see the module docs.
    pub hint: i64,
}

/// The treestate database: `.bed/treestate`.
#[derive(Debug)]
pub struct Treestate {
    path: PathBuf,
    conn: Connection,
}

impl Treestate {
    /// Opens (creating if needed) the treestate database at `path`, with
    /// WAL journaling. Parent directories are created as needed.
    pub fn open(path: &Path) -> Result<Self, BedError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| BedError::Treestate {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
        }
        let conn = Connection::open(path).map_err(|e| bed_err(path, e))?;

        // WAL: readers never block writers. journal_mode returns the
        // (possibly negotiated) mode; some filesystems refuse WAL and fall
        // back — proceed regardless, the row data is what matters.
        let _mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|e| bed_err(path, e))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| bed_err(path, e))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| bed_err(path, e))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                 path     TEXT PRIMARY KEY,
                 blob_id  BLOB NOT NULL,
                 mode     INTEGER NOT NULL,
                 size     INTEGER NOT NULL,
                 mtime_ns INTEGER NOT NULL,
                 hint     INTEGER NOT NULL
             );",
        )
        .map_err(|e| bed_err(path, e))?;

        Ok(Treestate {
            path: path.to_path_buf(),
            conn,
        })
    }

    /// The database file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Inserts or replaces the row for `entry.path`.
    pub fn upsert(&self, entry: &TreestateEntry) -> Result<(), BedError> {
        self.conn
            .execute(
                "INSERT INTO files (path, blob_id, mode, size, mtime_ns, hint)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(path) DO UPDATE SET
                   blob_id = excluded.blob_id,
                   mode = excluded.mode,
                   size = excluded.size,
                   mtime_ns = excluded.mtime_ns,
                   hint = excluded.hint",
                params![
                    entry.path,
                    entry.blob_id.as_bytes().as_slice(),
                    entry.mode,
                    entry.size,
                    entry.mtime_ns,
                    entry.hint,
                ],
            )
            .map_err(|e| bed_err(&self.path, e))?;
        Ok(())
    }

    /// The row for `path`, if the treestate knows it.
    pub fn lookup(&self, path: &str) -> Result<Option<TreestateEntry>, BedError> {
        let row = self
            .conn
            .query_row(
                "SELECT blob_id, mode, size, mtime_ns, hint FROM files WHERE path = ?1",
                params![path],
                |row| {
                    let blob: Vec<u8> = row.get(0)?;
                    Ok((
                        blob,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| bed_err(&self.path, e))?;
        match row {
            None => Ok(None),
            Some((blob, mode, size, mtime_ns, hint)) => {
                let blob_id = Id::from_bytes(blob.try_into().map_err(|_| BedError::Treestate {
                    path: self.path.clone(),
                    message: format!("blob_id for {path:?} is not 32 bytes"),
                })?);
                Ok(Some(TreestateEntry {
                    path: path.to_owned(),
                    blob_id,
                    mode,
                    size,
                    mtime_ns,
                    hint,
                }))
            }
        }
    }

    /// Every known path, sorted.
    pub fn all_paths(&self) -> Result<Vec<String>, BedError> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM files ORDER BY path")
            .map_err(|e| bed_err(&self.path, e))?;
        let paths = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| bed_err(&self.path, e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| bed_err(&self.path, e))?;
        Ok(paths)
    }

    /// Forgets `path` entirely (no-op when unknown).
    pub fn remove(&self, path: &str) -> Result<(), BedError> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", params![path])
            .map_err(|e| bed_err(&self.path, e))?;
        Ok(())
    }

    /// Forgets every row (the treestate is always rebuildable).
    pub fn clear(&self) -> Result<(), BedError> {
        self.conn
            .execute("DELETE FROM files", [])
            .map_err(|e| bed_err(&self.path, e))?;
        Ok(())
    }

    /// How many rows the treestate holds.
    pub fn count(&self) -> Result<i64, BedError> {
        self.conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .map_err(|e| bed_err(&self.path, e))
    }

    /// Refreshes the hint metadata for a file whose hash just came back
    /// clean: records the observed size/mtime and marks the row verified.
    ///
    /// Never touches `blob_id` — content addresses change only through
    /// `sow`/`goto` (later waves), never through a scan.
    pub fn refresh_hint(&self, path: &str, size: i64, mtime_ns: i64) -> Result<(), BedError> {
        self.conn
            .execute(
                "UPDATE files SET size = ?2, mtime_ns = ?3, hint = ?4 WHERE path = ?1",
                params![path, size, mtime_ns, HINT_VERIFIED],
            )
            .map_err(|e| bed_err(&self.path, e))?;
        Ok(())
    }
}

/// Maps a rusqlite error to [`BedError::Treestate`], naming the database.
fn bed_err(path: &Path, e: rusqlite::Error) -> BedError {
    BedError::Treestate {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(dir: &tempfile::TempDir, name: &str) -> Treestate {
        Treestate::open(&dir.path().join(name)).unwrap()
    }

    fn entry(path: &str, content: &[u8], mtime_ns: i64) -> TreestateEntry {
        TreestateEntry {
            path: path.to_owned(),
            blob_id: gardenbed_model::blob_id(content),
            mode: 0,
            size: content.len() as i64,
            mtime_ns,
            hint: HINT_VERIFIED,
        }
    }

    #[test]
    fn crud_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let t = ts(&dir, "treestate");
        assert_eq!(t.count().unwrap(), 0);

        let a = entry("src/a.rs", b"aaa", 1_000);
        let b = entry("docs/x.md", b"xxxxx", 2_000);
        t.upsert(&a).unwrap();
        t.upsert(&b).unwrap();
        assert_eq!(t.count().unwrap(), 2);

        assert_eq!(t.lookup("src/a.rs").unwrap(), Some(a.clone()));
        assert!(t.lookup("nope.rs").unwrap().is_none());

        // Upsert replaces every field.
        let a2 = entry("src/a.rs", b"zzzzzzzz", 9_999);
        t.upsert(&a2).unwrap();
        assert_eq!(t.count().unwrap(), 2);
        assert_eq!(t.lookup("src/a.rs").unwrap(), Some(a2));

        // all_paths is sorted.
        assert_eq!(t.all_paths().unwrap(), vec!["docs/x.md", "src/a.rs"]);

        t.remove("docs/x.md").unwrap();
        assert!(t.lookup("docs/x.md").unwrap().is_none());
        assert_eq!(t.count().unwrap(), 1);
        // Removing an unknown path is a no-op, not an error.
        t.remove("nope.rs").unwrap();

        t.clear().unwrap();
        assert_eq!(t.count().unwrap(), 0);
        assert!(t.all_paths().unwrap().is_empty());
    }

    #[test]
    fn blob_id_is_stored_as_raw_32_bytes_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        let t = ts(&dir, "treestate");
        let e = entry("f", b"payload", 5);
        t.upsert(&e).unwrap();
        let back = t.lookup("f").unwrap().unwrap();
        assert_eq!(back.blob_id, e.blob_id);
        assert_eq!(back.blob_id.as_bytes().len(), 32);

        // A corrupted row (blob_id of the wrong length) is an error, never
        // a silently wrong Id.
        t.clear().unwrap();
        t.conn
            .execute(
                "INSERT INTO files (path, blob_id, mode, size, mtime_ns, hint)
                 VALUES ('f', x'0102', 0, 0, 0, 0)",
                [],
            )
            .unwrap();
        assert!(matches!(t.lookup("f"), Err(BedError::Treestate { .. })));
    }

    #[test]
    fn opens_in_wal_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("treestate");
        {
            let t = Treestate::open(&path).unwrap();
            t.upsert(&entry("a", b"a", 0)).unwrap();
        }
        // WAL mode is persistent in the database file, so a fresh
        // connection sees it even after the writer closed cleanly.
        let raw = rusqlite::Connection::open(&path).unwrap();
        let mode: String = raw
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn refresh_hint_updates_metadata_but_never_the_blob() {
        let dir = tempfile::tempdir().unwrap();
        let t = ts(&dir, "treestate");
        let e = entry("f", b"payload", 1);
        t.upsert(&e).unwrap();

        t.refresh_hint("f", 7, 2_000_000).unwrap();
        let back = t.lookup("f").unwrap().unwrap();
        assert_eq!(back.blob_id, e.blob_id);
        assert_eq!(back.size, 7);
        assert_eq!(back.mtime_ns, 2_000_000);
        assert_eq!(back.hint, HINT_VERIFIED);
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c/treestate");
        Treestate::open(&deep).unwrap();
        assert!(deep.exists());
    }
}
