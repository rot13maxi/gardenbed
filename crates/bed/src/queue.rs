//! The offline commit queue (v0.2.md §6, decision D17): `pending` and
//! `pending_files` tables inside the working copy's own `index.sqlite3`.
//!
//! When the server is unreachable *at connect time* — and only then; a
//! server that answers, even to refuse, is online and its error must be
//! seen — `bed commit` completes everything local: the changeset is built
//! and content-addressed offline, every changed blob is staged into
//! `.bed/objects`, and one sqlite transaction records the row [`sync`]
//! later replays. The state move that follows is the exact one an online
//! commit performs, so a queued working copy behaves like a synced one —
//! except that `switch`, `rebase`, and `publish` refuse, and `log`/`diff`
//! at a pending tip fail with a queue-aware error, until `bed sync`
//! drains the queue.
//!
//! The queue stores canonical bytes and ids — nothing else — so replay is
//! a re-upload of content-addressed facts: the server's existing-id path
//! is an idempotent re-put, `PutBlob` likewise, and a crash anywhere costs
//! a re-upload, never a divergence.

use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use gardenbed_model::{Changeset, Id, Mode};
use gardenbed_store::FsObjectStore;
use gardenbed_store::ObjectStore as _;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use rusqlite::params;

use crate::commit;
use crate::error::ClientError;
use crate::rpc::Session;
use crate::workspace::WorkingCopy;

/// One treestate row a pending commit's state move applies: the
/// post-change `(path, blob, mode, size, mtime)` for every written path.
/// These are the `pending_files` rows — the patch that lets recovery
/// re-apply the move idempotently.
#[derive(Clone, Debug)]
pub struct PendingFile {
    /// Repository-relative path (the path after the change, for renames).
    pub path: String,
    /// The content address observed at commit time.
    pub blob_id: Id,
    /// §4 mode code.
    pub mode: i64,
    /// Content length in bytes.
    pub size: i64,
    /// File mtime, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
}

/// One queued commit, oldest-first by `seq`.
#[derive(Clone, Debug)]
pub struct PendingRow {
    /// The sqlite row key (insertion order).
    pub seq: i64,
    /// The locally computed changeset id.
    pub cs_id: Id,
    /// The changeset this one grows from (`None`: a root changeset).
    pub parent: Option<Id>,
    /// The changeset's full canonical bytes.
    pub canonical: Vec<u8>,
    /// The commit message.
    pub message: String,
    /// Unix seconds at enqueue time.
    pub created: i64,
    /// The treestate patch (the `pending_files` rows).
    pub files: Vec<PendingFile>,
}

/// The offline commit queue: the `pending` tables in `index.sqlite3`.
///
/// A second connection to the treestate's database (WAL allows both);
/// the tables are created `IF NOT EXISTS`, so v0.1 working copies load
/// unchanged.
#[derive(Debug)]
pub struct Queue {
    path: PathBuf,
    conn: Connection,
}

impl Queue {
    /// Opens (creating if needed) the queue tables in the database at
    /// `path`, with the treestate's pragmas.
    pub fn open(path: &Path) -> Result<Self, ClientError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| ClientError::Treestate {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
        }
        let conn = Connection::open(path).map_err(|e| q_err(path, e))?;

        // WAL: the treestate's connection (and this one) never block each
        // other. Some filesystems refuse WAL and fall back — proceed
        // regardless, the row data is what matters.
        let _mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .map_err(|e| q_err(path, e))?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| q_err(path, e))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| q_err(path, e))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS pending (
                 seq       INTEGER PRIMARY KEY AUTOINCREMENT,
                 cs_id     BLOB UNIQUE NOT NULL,
                 parent    BLOB,
                 canonical BLOB NOT NULL,
                 message   TEXT,
                 created   INTEGER
             );
             CREATE TABLE IF NOT EXISTS pending_files (
                 seq      INTEGER NOT NULL,
                 path     TEXT NOT NULL,
                 blob_id  BLOB NOT NULL,
                 mode     INTEGER NOT NULL,
                 size     INTEGER NOT NULL,
                 mtime_ns INTEGER NOT NULL
             );",
        )
        .map_err(|e| q_err(path, e))?;

        Ok(Queue {
            path: path.to_path_buf(),
            conn,
        })
    }

    /// Records one pending commit in a single transaction (the row and its
    /// `pending_files` patch land together or not at all), returning how
    /// many commits now wait for `bed sync`.
    pub fn enqueue(
        &mut self,
        cs_id: Id,
        parent: Option<Id>,
        canonical: &[u8],
        message: &str,
        files: &[PendingFile],
    ) -> Result<usize, ClientError> {
        let tx = self.conn.transaction().map_err(|e| q_err(&self.path, e))?;
        tx.execute(
            "INSERT INTO pending (cs_id, parent, canonical, message, created)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                cs_id.as_bytes(),
                parent.map(|p| p.as_bytes().to_vec()),
                canonical,
                message,
                chrono::Utc::now().timestamp()
            ],
        )
        .map_err(|e| q_err(&self.path, e))?;
        let seq = tx.last_insert_rowid();
        for f in files {
            tx.execute(
                "INSERT INTO pending_files (seq, path, blob_id, mode, size, mtime_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    seq,
                    f.path,
                    f.blob_id.as_bytes(),
                    f.mode,
                    f.size,
                    f.mtime_ns
                ],
            )
            .map_err(|e| q_err(&self.path, e))?;
        }
        tx.commit().map_err(|e| q_err(&self.path, e))?;
        self.count()
    }

    /// How many commits wait for `bed sync`.
    pub fn count(&self) -> Result<usize, ClientError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM pending", [], |row| row.get(0))
            .map_err(|e| q_err(&self.path, e))?;
        Ok(n as usize)
    }

    /// Every queued commit, oldest-first.
    pub fn rows(&self) -> Result<Vec<PendingRow>, ClientError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, cs_id, parent, canonical, message, created
                 FROM pending ORDER BY seq",
            )
            .map_err(|e| q_err(&self.path, e))?;
        let raw: Vec<(i64, Vec<u8>, Option<Vec<u8>>, Vec<u8>, String, i64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .map_err(|e| q_err(&self.path, e))?
            .collect::<Result<_, _>>()
            .map_err(|e| q_err(&self.path, e))?;
        raw.into_iter()
            .map(|(seq, cs_id, parent, canonical, message, created)| {
                Ok(PendingRow {
                    seq,
                    cs_id: id_of(&self.path, &cs_id)?,
                    parent: parent
                        .as_deref()
                        .map(|bytes| id_of(&self.path, bytes))
                        .transpose()?,
                    canonical,
                    message,
                    created,
                    files: self.files_of(seq)?,
                })
            })
            .collect()
    }

    /// The newest queued commit (the tip offline commits chain onto);
    /// `None` when the queue is empty.
    pub fn newest(&self) -> Result<Option<PendingRow>, ClientError> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT canonical FROM pending ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| q_err(&self.path, e))?;
        let Some(canonical) = bytes else {
            return Ok(None);
        };
        // The newest row is the last `rows()` returns (oldest-first).
        Ok(self
            .rows()?
            .into_iter()
            .rev()
            .find(|row| row.canonical == canonical))
    }

    /// The newest queued commit's changeset id; `None` when empty. The
    /// cheap form of [`Queue::newest`] for the chaining check.
    pub fn tip(&self) -> Result<Option<Id>, ClientError> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT cs_id FROM pending ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| q_err(&self.path, e))?;
        bytes.as_deref().map(|b| id_of(&self.path, b)).transpose()
    }

    /// True when `cs_id` names a queued (not yet uploaded) commit.
    pub fn contains(&self, cs_id: Id) -> Result<bool, ClientError> {
        let found: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pending WHERE cs_id = ?1)",
                params![cs_id.as_bytes()],
                |row| row.get(0),
            )
            .map_err(|e| q_err(&self.path, e))?;
        Ok(found)
    }

    /// Drops one queued commit and its patch — only after the server
    /// acknowledged the changeset.
    pub fn delete(&self, seq: i64) -> Result<(), ClientError> {
        self.conn
            .execute("DELETE FROM pending WHERE seq = ?1", params![seq])
            .map_err(|e| q_err(&self.path, e))?;
        self.conn
            .execute("DELETE FROM pending_files WHERE seq = ?1", params![seq])
            .map_err(|e| q_err(&self.path, e))?;
        Ok(())
    }

    fn files_of(&self, seq: i64) -> Result<Vec<PendingFile>, ClientError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT path, blob_id, mode, size, mtime_ns
                 FROM pending_files WHERE seq = ?1 ORDER BY path",
            )
            .map_err(|e| q_err(&self.path, e))?;
        let raw: Vec<(String, Vec<u8>, i64, i64, i64)> = stmt
            .query_map(params![seq], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .map_err(|e| q_err(&self.path, e))?
            .collect::<Result<_, _>>()
            .map_err(|e| q_err(&self.path, e))?;
        raw.into_iter()
            .map(|(path, blob_id, mode, size, mtime_ns)| {
                Ok(PendingFile {
                    path,
                    blob_id: id_of(&self.path, &blob_id)?,
                    mode,
                    size,
                    mtime_ns,
                })
            })
            .collect()
    }
}

/// True only for connect-time failures ([`ClientError::Connect`], produced
/// by the connection establish inside [`Session::open`]): the server is
/// unreachable, and the commit belongs in the queue. A server that
/// answered — even to refuse, even with `Unavailable` at the RPC level —
/// is online; its error must reach the user, never the queue.
pub fn is_connect_error(err: &ClientError) -> bool {
    matches!(err, ClientError::Connect { .. })
}

/// Completes a state move interrupted between the queue insert and the
/// local state update (v0.2 §6): when the newest pending row's parent is
/// the recorded current changeset, the previous run died right after
/// inserting the row — replay the move; when the current changeset is
/// already the pending tip, the move may have died halfway — replay it
/// again (heads appends are check-then-append here, and every treestate
/// write is an idempotent upsert). Cheap when the queue is empty.
pub fn recover(bed: &mut WorkingCopy) -> Result<(), ClientError> {
    let queue = bed.queue()?;
    let Some(newest) = queue.newest()? else {
        return Ok(());
    };
    let current = bed
        .config()
        .current_cs
        .as_deref()
        .and_then(|hex| Id::from_str(hex).ok());
    let replay = match current {
        Some(cur) => cur == newest.cs_id || Some(cur) == newest.parent,
        None => newest.parent.is_none(),
    };
    if !replay {
        return Ok(());
    }
    let cs = Changeset::decode(&newest.canonical).map_err(|e| ClientError::Treestate {
        path: bed.dir().join("index.sqlite3"),
        message: format!(
            "queued changeset {} does not decode: {e}",
            newest.cs_id.short()
        ),
    })?;
    let treestate = bed.treestate()?;
    commit::move_state(
        bed,
        &treestate,
        &cs.changes,
        &newest.files,
        newest.cs_id,
        true,
    )
}

/// What one `bed sync` did.
#[derive(Debug)]
pub struct SyncReport {
    /// The changesets the server acknowledged, oldest-first.
    pub synced: Vec<Id>,
    /// Rows still queued — a failure kept them (and only them).
    pub remaining: usize,
    /// The failure that stopped the drain early, if any.
    pub failure: Option<ClientError>,
}

/// `bed sync` — drain the offline queue oldest-first: heavy blobs via
/// `PutBlob`, then `CreateChangeset` within the same inline budgets as
/// `commit`, deleting each row only after its acknowledgment (the
/// server's existing-id path is an idempotent re-put, so a killed sync
/// replays cleanly). Upload-only: `current_cs`, heads, and treestate
/// already point at the pending tip and are left alone — after sync the
/// server simply agrees with them.
pub async fn sync(state_dir: &Path) -> Result<SyncReport, ClientError> {
    let mut bed = WorkingCopy::open(state_dir)?;
    // Open-time recovery (v0.2 §6): complete a state move a previous run
    // died in the middle of before draining. No-op when the queue is empty.
    recover(&mut bed)?;
    let queue = bed.queue()?;
    let rows = queue.rows()?;
    if rows.is_empty() {
        return Ok(SyncReport {
            synced: Vec::new(),
            remaining: 0,
            failure: None,
        });
    }
    let session = match Session::open(&bed).await {
        Ok(session) => session,
        Err(e) => {
            return Ok(SyncReport {
                synced: Vec::new(),
                remaining: rows.len(),
                failure: Some(e),
            });
        }
    };
    let repo = bed.config().repo.clone();
    let objects = bed.objects()?;

    let mut synced = Vec::new();
    let mut failure = None;
    for row in &rows {
        // Stop cleanly at the first failure; the row and everything
        // behind it stay queued for the next run.
        if let Err(e) = sync_one(&session, &repo, &objects, row).await {
            failure = Some(e);
            break;
        }
        queue.delete(row.seq)?;
        synced.push(row.cs_id);
    }
    let remaining = queue.count()?;
    Ok(SyncReport {
        synced,
        remaining,
        failure,
    })
}

/// Uploads one queued commit: heavy blobs first (content from the local
/// cache — the offline commit staged everything there), then
/// `CreateChangeset` with the inline payload. On the classic
/// "neither in objects nor inline" hole (a previous attempt died between
/// staging and uploading), the heavy blobs are pushed again and the
/// changeset retried once — the same retry-safe pattern as `commit`.
async fn sync_one(
    session: &Session,
    repo: &str,
    objects: &FsObjectStore,
    row: &PendingRow,
) -> Result<(), ClientError> {
    let cs = Changeset::decode(&row.canonical).map_err(|e| ClientError::Treestate {
        path: PathBuf::from(row.cs_id.to_hex()),
        message: format!("queued changeset does not decode: {e}"),
    })?;

    // The distinct content addresses the changeset references (gitlink
    // pointers are opaque, never objects).
    let mut blobs: Vec<Id> = Vec::new();
    for c in &cs.changes {
        if let Some(blob) = c.blob
            && c.mode != Mode::Gitlink
            && !blobs.contains(&blob)
        {
            blobs.push(blob);
        }
    }
    let mut cached: Vec<(Id, Vec<u8>)> = Vec::new();
    for id in blobs {
        if let Some(content) = objects.get_blob(id)? {
            cached.push((id, content));
        }
    }
    let (inline, heavy) = commit::split_inline(cached);
    for (id, content) in &heavy {
        commit::put_blob_heavy(session, repo, *id, content, "sync").await?;
    }

    let ack = match session
        .client
        .create_changeset(repo, &cs, inline.clone())
        .await
    {
        Ok(id) => id,
        Err(status)
            if status.code() == tonic::Code::InvalidArgument
                && status.message().contains("neither in objects nor inline") =>
        {
            for (id, content) in &heavy {
                commit::put_blob_heavy(session, repo, *id, content, "sync").await?;
            }
            match session
                .client
                .create_changeset(repo, &cs, inline.clone())
                .await
            {
                Ok(id) => id,
                Err(status) => {
                    return Err(ClientError::Rpc {
                        command: "sync",
                        code: status.code(),
                        message: status.message().to_owned(),
                    });
                }
            }
        }
        Err(status) => {
            return Err(ClientError::Rpc {
                command: "sync",
                code: status.code(),
                message: status.message().to_owned(),
            });
        }
    };
    // Content-addressed: the server's answer must be the id the queue
    // computed offline, byte-exactly.
    if ack != row.cs_id {
        return Err(ClientError::BlobHashMismatch(ack));
    }
    Ok(())
}

/// Maps a queue storage failure onto the treestate error (the tables live
/// in the same `index.sqlite3`).
fn q_err(path: &Path, e: rusqlite::Error) -> ClientError {
    ClientError::Treestate {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// Decodes a stored 32-byte blob id.
fn id_of(path: &Path, bytes: &[u8]) -> Result<Id, ClientError> {
    let arr: [u8; 32] = bytes.try_into().map_err(|_| ClientError::Treestate {
        path: path.to_path_buf(),
        message: "a queued blob id is not 32 bytes".into(),
    })?;
    Ok(Id::from_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ClientError;

    /// Only connect-time failure queues: a server that answered (even to
    /// refuse, even with `Unavailable` at the RPC level) is online, and
    /// its error must be surfaced, never silently queued.
    #[test]
    fn only_connect_errors_queue() {
        assert!(is_connect_error(&ClientError::Connect {
            server: "http://127.0.0.1:1".into(),
            message: "connection refused".into(),
        }));
        assert!(!is_connect_error(&ClientError::Rpc {
            command: "commit",
            code: tonic::Code::Unavailable,
            message: "socket closed mid-RPC".into(),
        }));
        assert!(!is_connect_error(&ClientError::NothingToCommit));
        assert!(!is_connect_error(&ClientError::MissingCs(Id::from_bytes(
            [0u8; 32]
        ))));
    }

    fn file(path: &str) -> PendingFile {
        PendingFile {
            path: path.to_owned(),
            blob_id: Id::from_bytes([1u8; 32]),
            mode: 0,
            size: 1,
            mtime_ns: 2,
        }
    }

    /// Rows round-trip oldest-first with their patches; deletion happens
    /// per row; membership is by changeset id.
    #[test]
    fn queue_round_trips_rows_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = Queue::open(&dir.path().join("index.sqlite3")).unwrap();
        assert_eq!(queue.count().unwrap(), 0);
        assert!(queue.tip().unwrap().is_none());

        let first = Id::from_bytes([2u8; 32]);
        let second = Id::from_bytes([3u8; 32]);
        let parent = Id::from_bytes([4u8; 32]);
        assert_eq!(
            queue
                .enqueue(first, Some(parent), b"canonical-1", "one", &[file("a.txt")])
                .unwrap(),
            1
        );
        assert_eq!(
            queue
                .enqueue(second, Some(first), b"canonical-2", "two", &[file("b.txt")])
                .unwrap(),
            2
        );

        assert_eq!(queue.tip().unwrap(), Some(second));
        assert!(queue.contains(first).unwrap());
        assert!(!queue.contains(parent).unwrap());

        let rows = queue.rows().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].cs_id, first);
        assert_eq!(rows[0].parent, Some(parent));
        assert_eq!(rows[0].message, "one");
        assert_eq!(rows[0].canonical, b"canonical-1");
        assert_eq!(rows[0].files.len(), 1);
        assert_eq!(rows[0].files[0].path, "a.txt");
        assert_eq!(rows[1].cs_id, second);
        assert_eq!(rows[1].parent, Some(first));

        let newest = queue.newest().unwrap().unwrap();
        assert_eq!(newest.cs_id, second);
        assert_eq!(newest.files.len(), 1);

        queue.delete(rows[0].seq).unwrap();
        assert_eq!(queue.count().unwrap(), 1);
        assert!(!queue.contains(first).unwrap());
        let rows = queue.rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cs_id, second);
        // The patch rows went with their commit.
        assert_eq!(rows[0].files.len(), 1);
        assert_eq!(rows[0].files[0].path, "b.txt");
    }

    /// The queue tables land in the treestate's database without
    /// disturbing it: a `Treestate` opened on the same file keeps working.
    #[test]
    fn queue_shares_index_sqlite_with_treestate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.sqlite3");
        crate::treestate::Treestate::open(&path).unwrap();
        let mut queue = Queue::open(&path).unwrap();
        let id = Id::from_bytes([5u8; 32]);
        queue.enqueue(id, None, b"c", "m", &[]).unwrap();
        // The treestate (a second connection) still reads its own table.
        let treestate = crate::treestate::Treestate::open(&path).unwrap();
        assert_eq!(treestate.count().unwrap(), 0i64);
    }
}
