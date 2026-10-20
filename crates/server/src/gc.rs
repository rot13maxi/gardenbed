//! Garbage collection (v0.2.md §7, D14): mark/sweep/report over one
//! repository's stores, plus the `.tmp-*` reaper and the `repo.lock` flock
//! protocol that makes offline the default stance.
//!
//! **Mark** is a pure function of SQLite state: every ref tip, closed
//! recursively over all parents of stored changeset canonical bytes; every
//! blob referenced by a non-gitlink change of any stored changeset; every
//! certificate root tree id and every `derived(kind='tree')` id, closed
//! transitively over tree payloads. No object file participates — the mark
//! decides reachability, never the disk. It materializes into SQLite temp
//! tables (`gc_live`), so RSS stays bounded at ten-million-object scale and
//! the report is one SQL join.
//!
//! **Sweep** is one walk over the 256 shard directories: candidates are
//! files present on disk but not live, and older than `--min-age` (default
//! 24 h). Plain `unlink` — a candidate is already unreachable, and POSIX
//! unlink never disturbs an in-flight read. The same walk reaps `.tmp-*`
//! files past `min_age`. Each unlink is independent: SIGKILL at any point
//! leaves a sound store, and a follow-up GC converges (soundness lives in
//! the mark, never in the sweep's progress).
//!
//! **D14: changeset rows and git-mapping rows are never collected.** The
//! git↔changeset bijection, the mapped-frontier incremental import, and
//! export topology all assume complete rows; history pruning stays out of
//! scope. Consequently every stored changeset row is a mark root (its blob
//! references stay live even when no ref reaches it), so GC only ever
//! reclaims never-referenced or once-referenced-then-superseded objects —
//! which is exactly the orphan population the write paths can produce.
//!
//! **Locking.** `serve` holds a shared `flock` on `<root>/repo.lock`;
//! `gc` takes it exclusive unless `--force`. The default stance is
//! offline-first: a running server makes plain `gc` fail with the
//! actionable message; `--force` runs live and aborts the sweep if an
//! ingest lands mid-sweep (the mark is re-checked against an ingest
//! generation watermark before every deletion).
//!
//! **Young-orphan immunity.** `put_blob` refreshes the mtime of an existing
//! identical blob (idempotent re-put), so a slow client's
//! uploaded-but-not-yet-referenced blob — including an interrupted sync —
//! keeps its grace period from its last arrival, not from its first.
//!
//! **Mark transaction shape.** Every mark write lands in the mark
//! connection's `temp` schema, so no mark row is ever visible to another
//! connection and none survives the mark connection: the documented
//! contract "a crash before the mark commits writes nothing" holds
//! structurally, not through a single enclosing transaction. The mark
//! therefore batches its temp writes — `gc_live` ids and tree-closure
//! claims each in bounded [`WRITE_BATCH`] transactions instead of one
//! statement per id. Those transactions are temp-database only: they take
//! no main-database write lock, so they cannot contend with the ingest
//! path; what they save is a journal create/delete cycle per batch.
//! Batching is invisible to the mark set: the reachability
//! computation is unchanged, and the sweep starts only after the build
//! returns, when every buffer has been flushed and every batch committed.
//! A failure mid-build propagates before the sweep; the partially built
//! temp tables die with the mark connection and can never be observed as
//! a mark.
//!
//! **Blob vs tree on disk**: blobs carry the `GBLO1` envelope (first byte
//! `'G'`), trees are canonical payloads (first byte `0x02`). The sweep
//! types every candidate by magic so the report's blob/tree split is
//! observed, not inferred from the mark.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use gardenbed_model::{Changeset, Id, Mode, Tree, TreeKind};
use gardenbed_store::{ObjectStore, StoreError};

/// Default grace period for unreachable objects (24 h).
pub const DEFAULT_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Metadata rows per batch read. Bounded so the mark stays
/// constant-memory at any scale (the kernel-store criterion).
const BATCH: usize = 4_096;

/// Live-set ids buffered before one `gc_live` insert transaction, and
/// claims buffered before one `gc_expanded` claim transaction. Same bound
/// as [`BATCH`]: small enough that the buffers (128 KiB each) never
/// register against the kernel-store RSS budget, large enough that the
/// tens-of-millions of mark ids cost thousands of transactions instead of
/// tens of millions.
///
/// Public so the batch-boundary regression test sizes its fixture from
/// the real bound instead of a copy: a test-local copy silently stops
/// covering the flush points whenever this value changes.
pub const WRITE_BATCH: usize = BATCH;

/// Trees are canonical payloads whose first byte is the tree format byte
/// `0x02` (canonical-encoding.md §4); blob envelopes start `"GBLO1"`.
const TREE_MAGIC_FIRST_BYTE: u8 = 0x02;

// ---------------------------------------------------------------- locking

/// Error returned by [`lock_exclusive`].
#[derive(Debug)]
pub enum LockError {
    /// A running server (or another holder) holds the lock.
    Held,
    /// Opening or locking the lock file failed outright.
    Io(io::Error),
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Held => write!(
                f,
                "repository is in use: gc needs the exclusive repo.lock \
                 (stop the server, or pass --force for live gc)"
            ),
            LockError::Io(e) => write!(f, "cannot lock repo.lock: {e}"),
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockError::Held => None,
            LockError::Io(e) => Some(e),
        }
    }
}

/// Takes the exclusive flock on `<root>/repo.lock`; the guard releases on
/// drop. The file is created when missing (a fresh root has never been
/// served). Non-blocking: `Err(LockError::Held)` when anyone else holds it
/// at all — a live server's *shared* hold included, which is exactly the
/// offline-first contract.
pub fn lock_exclusive(root: &Path) -> Result<File, LockError> {
    let path = root.join("repo.lock");
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(LockError::Io)?;
    }
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(LockError::Io)?;
    let fd = file.as_fd();
    match rustix::fs::flock(fd, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(LockError::Held),
        Err(e) => Err(LockError::Io(io::Error::from(e))),
    }
}

/// The shared flock `serve` holds for its lifetime; drop to release.
/// Blocking: contending with a live GC simply waits it out (GC is finite).
pub fn lock_shared_blocking(root: &Path) -> Result<File, LockError> {
    let path = root.join("repo.lock");
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(LockError::Io)?;
    }
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(LockError::Io)?;
    let fd = file.as_fd();
    rustix::fs::flock(fd, rustix::fs::FlockOperation::LockShared)
        .map_err(|e| LockError::Io(io::Error::from(e)))?;
    Ok(file)
}

// ----------------------------------------------------------------- report

/// What one `gc` run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Object files seen under `objects/` (excluding `.tmp-*` entries).
    pub scanned: u64,
    /// Of those, how many the mark proved reachable.
    pub live: u64,
    /// Blob files unlinked (or that would be, under `--dry-run`).
    pub blobs_deleted: u64,
    /// Tree files unlinked (or that would be, under `--dry-run`).
    pub trees_deleted: u64,
    /// `.tmp-*` files reaped (or that would be, under `--dry-run`).
    pub tmp_reaped: u64,
    /// Bytes reclaimed by unlinks and tmp reaps (file sizes as seen).
    pub bytes_reclaimed: u64,
    /// The sweep was aborted because an ingest landed mid-sweep (`--force`
    /// only): unlinks already performed stand (each was dead at mark
    /// time), the rest of the candidates were left in place.
    pub aborted: bool,

    /// Mark timing (for the budget criteria).
    pub mark_elapsed: Duration,
    /// Sweep + report timing.
    pub sweep_elapsed: Duration,
}

impl fmt::Display for GcReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "gc: {} object(s) scanned, {} live, {} blob(s) and {} tree(s) deleted, \
             {} tmp file(s) reaped, {} byte(s) reclaimed",
            self.scanned,
            self.live,
            self.blobs_deleted,
            self.trees_deleted,
            self.tmp_reaped,
            self.bytes_reclaimed
        )?;
        if self.aborted {
            write!(f, "; sweep aborted: ingest landed mid-sweep")?;
        }
        write!(
            f,
            " [mark {:.3}s, sweep {:.3}s]",
            self.mark_elapsed.as_secs_f64(),
            self.sweep_elapsed.as_secs_f64()
        )
    }
}

// ------------------------------------------------------------- the sweep

/// Path of one file the sweep decided to delete (or would, under
/// `--dry-run`). Only unreachable-and-old objects ever materialize
/// here, so the deletion set is bounded by the garbage volume, never
/// the store size.
#[derive(Clone, Debug)]
struct Deletable {
    path: PathBuf,
    /// The counted kind: `trees_deleted` when a tree, `blobs_deleted`
    /// otherwise — so a mid-sweep abort can report only the chunks that
    /// were actually unlinked, not the whole candidate set.
    kind: DeletableKind,
    /// The file's size, for `bytes_reclaimed` on an aborted report.
    size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeletableKind {
    Blob,
    Tree,
}

/// The result of one chunk's transactional unlink step.
enum ChunkOutcome {
    /// The in-transaction ingest check tripped: nothing in the chunk was
    /// unlinked; the sweep should abort.
    Aborted,
    /// The chunk's files were unlinked and the transaction committed.
    Unlinked,
}

/// One on-disk file the sweep looked at (transient, per shard).
struct ShardObject {
    id: Id,
    path: PathBuf,
    size: u64,
    mtime: SystemTime,
    /// The kind byte, read lazily (dead objects only — live objects are
    /// never classified, so the sweep costs no per-object open+read at
    /// healthy stores).
    kind_byte: Option<u8>,
}

/// The sweep-side accounting `run` accumulates while streaming shards.
struct SweepStats {
    scanned: u64,
    live: u64,
    unrecognized: u64,
    tmp_reaped: u64,
    bytes_reclaimed: u64,
    blobs_deleted: u64,
    trees_deleted: u64,
}

/// Walks `objects/` shard by shard (constant memory: one shard's entries
/// at a time) and drives `on_objects` per shard with the parsed objects
/// plus the stats accumulator. `.tmp-*` files past `cutoff` are reaped
/// inline (respecting `dry_run`); the min-age gate and the live join
/// both happen in `run`'s per-shard callback.
fn walk_shards(
    objects_dir: &Path,
    cutoff: SystemTime,
    dry_run: bool,
    stats: &mut SweepStats,
    mut on_objects: impl FnMut(&mut SweepStats, Vec<ShardObject>) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    for entry in fs::read_dir(objects_dir)
        .map_err(|e| StoreError::Io(format!("read {}: {e}", objects_dir.display())))?
    {
        let shard = entry.map_err(|e| StoreError::Io(e.to_string()))?.path();
        if !shard.is_dir() {
            continue;
        }
        let mut objects: Vec<ShardObject> = Vec::new();
        for file in fs::read_dir(&shard).map_err(|e| StoreError::Io(e.to_string()))? {
            let file = file.map_err(|e| StoreError::Io(e.to_string()))?;
            let name = file.file_name();
            let name = name.to_string_lossy();
            let path = file.path();
            if is_tmp_name(&name) {
                let meta = fs::metadata(&path).map_err(|e| StoreError::Io(e.to_string()))?;
                if !meta.is_file() {
                    continue;
                }
                if mtime_of(&meta, &path)? <= cutoff {
                    stats.tmp_reaped += 1;
                    stats.bytes_reclaimed += meta.len();
                    if !dry_run {
                        fs::remove_file(&path)
                            .map_err(|e| StoreError::Io(format!("reap {}: {e}", path.display())))?;
                    }
                }
                continue;
            }
            let Some(id) = parse_object_name(&shard, &name) else {
                // Unknown debris: never deleted, counted so reports stay
                // honest.
                stats.unrecognized += 1;
                continue;
            };
            let meta = match fs::metadata(&path) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue, // reaped meanwhile
                Err(e) => return Err(StoreError::Io(e.to_string())),
            };
            if !meta.is_file() {
                stats.unrecognized += 1;
                continue;
            }
            objects.push(ShardObject {
                id,
                size: meta.len(),
                mtime: mtime_of(&meta, &path)?,
                path,
                kind_byte: None,
            });
        }
        stats.scanned += objects.len() as u64;
        on_objects(stats, objects)?;
    }
    Ok(())
}

/// `objects/<2 hex>/<62 hex>` → the 64-hex id. `None` when the name is not
/// a well-formed object path (never deleted; counted as unrecognized).
fn parse_object_name(shard: &Path, name: &str) -> Option<Id> {
    let shard = shard.file_name()?.to_str()?;
    if shard.len() != 2 || name.len() != 62 {
        return None;
    }
    let mut hex = String::with_capacity(64);
    hex.push_str(shard);
    hex.push_str(name);
    Id::from_str(&hex).ok()
}

/// The mtime, mapped to `UNIX_EPOCH - 1` when the filesystem reports
/// something before it, so the min-age comparison is total.
fn mtime_of(meta: &fs::Metadata, path: &Path) -> Result<SystemTime, StoreError> {
    meta.modified()
        .map(|t| t.max(SystemTime::UNIX_EPOCH - Duration::from_secs(1)))
        .map_err(|e| StoreError::Io(format!("mtime of {}: {e}", path.display())))
}

/// Reads one byte of a file without buffering its contents.
fn read_first_byte(path: &Path) -> Result<u8, StoreError> {
    use std::io::Read;
    let mut f = File::open(path).map_err(|e| StoreError::Io(e.to_string()))?;
    let mut buf = [0u8; 1];
    match f.read(&mut buf) {
        Ok(1) => Ok(buf[0]),
        Ok(_) => Err(StoreError::Corrupt(format!(
            "object file {} is empty",
            path.display()
        ))),
        Err(e) => Err(StoreError::Io(e.to_string())),
    }
}

/// `true` when the file name is an object-store `.tmp-*` staging entry.
fn is_tmp_name(name: &str) -> bool {
    name.starts_with(".tmp-")
}

// ------------------------------------------------------------------- gc

/// Options for [`run`].
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Report candidates without deleting.
    pub dry_run: bool,
    /// Run without the exclusive lock, guarding the sweep with ingest
    /// detection instead.
    pub force: bool,
    /// Unreachable objects younger than this survive this run.
    pub min_age: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            dry_run: false,
            force: false,
            min_age: DEFAULT_MIN_AGE,
        }
    }
}

/// Errors from one gc run.
#[derive(Debug, thiserror::Error)]
pub enum GcError {
    /// The exclusive lock could not be taken.
    #[error(transparent)]
    Lock(#[from] LockError),
    /// The mark or sweep failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Runs one mark/sweep/report cycle over the repository at `root`.
///
/// `lock` is the *held* lock guard (`repo.lock`): exclusive for offline
/// mode, a shared dummy only under `--force` — callers obtain it through
/// [`lock_exclusive`] (offline) or skip locking entirely (`--force`). The
/// guard's type is `Option<File>` so the caller can pass `None` in force
/// mode; holding the shared lock under force is also fine.
pub fn run(root: &Path, opts: Options, _lock: Option<&File>) -> Result<GcReport, GcError> {
    let t_mark = std::time::Instant::now();
    let mut mark = Mark::materialize(root, !opts.force)?;
    let mark_elapsed = t_mark.elapsed();

    let t_sweep = std::time::Instant::now();
    let cutoff = SystemTime::now()
        .checked_sub(opts.min_age)
        .unwrap_or(SystemTime::UNIX_EPOCH);

    let mut stats = SweepStats {
        scanned: 0,
        live: 0,
        unrecognized: 0,
        tmp_reaped: 0,
        bytes_reclaimed: 0,
        blobs_deleted: 0,
        trees_deleted: 0,
    };
    let mut deletable: Vec<Deletable> = Vec::new();

    // Ingest detection (live mode only): an ingest since the mark may
    // have made a candidate reachable, so the sweep must not unlink
    // anything. In force mode the mark has no exclusive lock to lean on —
    // this check is the whole pre-sweep guard (offline mode's
    // `ingest_landed` is inert: the exclusive lock is held). Dry runs
    // never mutate, so they skip the guard entirely.
    let swept_clean = opts.dry_run || !mark.ingest_landed()?;

    walk_shards(
        &root.join("objects"),
        cutoff,
        opts.dry_run,
        &mut stats,
        |stats, objects| {
            // Live join per shard: mark says yes/no per on-disk id.
            let ids: Vec<Id> = objects.iter().map(|o| o.id).collect();
            let dead = mark.batch_not_live(&ids)?;
            for (obj, is_dead) in objects.into_iter().zip(dead) {
                if !is_dead {
                    stats.live += 1;
                    continue;
                }
                // Min-age gate: young orphans survive this run.
                if obj.mtime > cutoff {
                    continue;
                }
                stats.bytes_reclaimed += obj.size;
                let kind_byte = match obj.kind_byte {
                    Some(b) => b,
                    None => read_first_byte(&obj.path)?,
                };
                let kind = if kind_byte == TREE_MAGIC_FIRST_BYTE {
                    stats.trees_deleted += 1;
                    DeletableKind::Tree
                } else {
                    stats.blobs_deleted += 1;
                    DeletableKind::Blob
                };
                deletable.push(Deletable {
                    path: obj.path,
                    kind,
                    size: obj.size,
                });
            }
            Ok(())
        },
    )?;

    let mut report = GcReport {
        scanned: stats.scanned,
        live: stats.live,
        blobs_deleted: stats.blobs_deleted,
        trees_deleted: stats.trees_deleted,
        tmp_reaped: stats.tmp_reaped,
        bytes_reclaimed: stats.bytes_reclaimed,
        aborted: false,
        mark_elapsed,
        sweep_elapsed: Duration::ZERO,
    };
    if !swept_clean {
        // An ingest landed before any unlink: nothing was deleted. The
        // walk's findings are withdrawn — the report must not claim
        // deletions that did not happen.
        report.blobs_deleted = 0;
        report.trees_deleted = 0;
        report.tmp_reaped = 0;
        report.bytes_reclaimed = 0;
        report.aborted = true;
        report.sweep_elapsed = t_sweep.elapsed();
        return Ok(report);
    }

    // Dry runs never mutate: the report above already counts what would
    // be collected; return before any unlink branch.
    if opts.dry_run {
        report.sweep_elapsed = t_sweep.elapsed();
        return Ok(report);
    }

    // The sweep unlinks in chunks, and each chunk's unlinks happen inside
    // a BEGIN IMMEDIATE metadata transaction — the very write lock a
    // changeset commit must take. In force mode the ingest watermark is
    // re-checked INSIDE that transaction, before any unlink of the chunk:
    // under the write lock, the sequence check→unlink is indivisible, so
    // a row cannot land in the gap. Either the row committed first (the
    // in-tx check sees the ingest; the chunk is skipped and the sweep
    // aborts) or it waits on the lock (and this chunk's unlinks stand —
    // the row only then commits, against objects still on disk because
    // the writer-side verify re-checks presence inside its own
    // transaction; if this chunk took a referenced object, the writer's
    // commit fails and the client retries per V4b).
    let mut unlinked_blobs: u64 = 0;
    let mut unlinked_trees: u64 = 0;
    let mut unlinked_bytes: u64 = 0;
    // tmp files were reaped during the walk, before any object unlink:
    // they stand even on abort (they are not candidates).
    let unlinked_tmp = report.tmp_reaped;
    for chunk in deletable.chunks(BATCH) {
        match mark.unlink_in_write_tx(chunk, opts.force)? {
            ChunkOutcome::Aborted => {
                // An ingest landed. Nothing in this chunk was unlinked;
                // every earlier chunk stood (dead at a still-clean
                // watermark). Abort: the report counts only completed
                // chunks, and the next gc converges.
                report.blobs_deleted = unlinked_blobs;
                report.trees_deleted = unlinked_trees;
                report.bytes_reclaimed = unlinked_bytes;
                report.tmp_reaped = unlinked_tmp;
                report.aborted = true;
                report.sweep_elapsed = t_sweep.elapsed();
                return Ok(report);
            }
            ChunkOutcome::Unlinked => {
                unlinked_blobs += chunk
                    .iter()
                    .filter(|d| d.kind == DeletableKind::Blob)
                    .count() as u64;
                unlinked_trees += chunk
                    .iter()
                    .filter(|d| d.kind == DeletableKind::Tree)
                    .count() as u64;
                unlinked_bytes += chunk.iter().map(|d| d.size).sum::<u64>();
            }
        }
    }
    report.sweep_elapsed = t_sweep.elapsed();
    Ok(report)
}

// ------------------------------------------------------------------- mark

/// A materialized mark: the live-set temp table inside one dedicated
/// SQLite connection, plus the ingest watermark for `--force` mode.
///
/// The connection is separate from `SqliteMetadataStore` so the mark's
/// temp tables never share a mutex with serving. The table lives in the
/// connection's `temp` schema: invisible to other connections, dropped
/// with the connection, and spilled by SQLite (not RAM) at
/// ten-million-object scale.
pub struct Mark {
    conn: rusqlite::Connection,
    /// Offline (`--force` absent): the exclusive `repo.lock` is held, so
    /// no ingest can land and the watermark is never consulted.
    offline: bool,
    /// The ingest generation at mark time (live mode's abort detector).
    watermark: u64,
    /// Live ids awaiting the next `gc_live` batch transaction. Bounded by
    /// [`WRITE_BATCH`]; empty once [`Mark::materialize`] returns, which is
    /// what makes the sweep's `gc_live` reads observe the complete mark.
    live_buf: Vec<[u8; 32]>,
}

impl Mark {
    /// Materializes the mark for the repository at `root`.
    ///
    /// `offline = true` asserts the caller holds the exclusive
    /// `repo.lock` (GC is the only writer); `false` is live mode, where
    /// [`Mark::ingest_landed`] guards the sweep.
    pub fn materialize(root: &Path, offline: bool) -> Result<Self, StoreError> {
        let db = root.join("metadata.sqlite3");
        // Read-write: force mode's sweep unlinks inside BEGIN IMMEDIATE
        // write transactions (see `unlink_in_write_tx`) — the same lock
        // a changeset commit takes, which is what makes the check→unlink
        // step indivisible against writers.
        let conn = rusqlite::Connection::open(&db)
            .map_err(|e| StoreError::Db(format!("open {}: {e}", db.display())))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| StoreError::Db(e.to_string()))?;
        // A repository never opened by the server has no schema yet. The
        // four core tables are required; the v0.2 certificate table is
        // optional (older stores simply contribute no certificate roots
        // to the mark).
        let core = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name IN ('changesets', 'refs', 'git_map', 'derived')",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;
        if core != 4 {
            return Err(StoreError::Db(format!(
                "{} is not a gardenbed repository (schema tables missing); \
                 serve or import first",
                db.display()
            )));
        }
        let has_certs: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type = 'table' AND name = 'validated_changesets_v1')",
                [],
                |r| r.get(0),
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;
        if !has_certs {
            // v0.1 store: an empty stand-in in temp schema (never shadows
            // a real table, which would exist) so the mark's SQL is
            // uniform. Contributes no certificate roots.
            conn.execute_batch(
                "CREATE TEMP TABLE validated_changesets_v1(\
                 cs BLOB PRIMARY KEY, root_tree BLOB NOT NULL);",
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;
        }
        let mut mark = Mark {
            conn,
            offline,
            watermark: 0,
            live_buf: Vec::new(),
        };
        mark.watermark = mark.read_generation()?;
        mark.build(root)?;
        Ok(mark)
    }

    /// Ingest generation: moves on any changeset or ref mutation. Used
    /// only as a *detector* — conservative in the abort direction (an
    /// unrelated mutation aborts a safe sweep; never the reverse).
    fn read_generation(&self) -> Result<u64, StoreError> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM changesets) + (SELECT COUNT(*) FROM refs)",
                [],
                |r| r.get(0),
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(count as u64)
    }
}

/// [`Mark::read_generation`] on an open transaction — the sweep's
/// in-transaction ingest check.
fn read_generation_on(tx: &rusqlite::Transaction<'_>) -> Result<u64, StoreError> {
    let count: i64 = tx
        .query_row(
            "SELECT (SELECT COUNT(*) FROM changesets) + (SELECT COUNT(*) FROM refs)",
            [],
            |r| r.get(0),
        )
        .map_err(|e| StoreError::Db(e.to_string()))?;
    Ok(count as u64)
}

impl Mark {
    /// `true` when an ingest landed after the mark (live mode only).
    /// Offline mode always answers `false` (the exclusive lock is held).
    pub fn ingest_landed(&self) -> Result<bool, StoreError> {
        if self.offline {
            return Ok(false);
        }
        Ok(self.read_generation()? != self.watermark)
    }

    /// Unlinks `chunk`'s files inside one `BEGIN IMMEDIATE` transaction.
    ///
    /// The transaction holds the database write lock — the same lock a
    /// changeset commit must take — so no row can land while these
    /// unlinks happen. In force mode the ingest watermark is re-checked
    /// INSIDE the transaction, before the first unlink of the chunk: the
    /// check→unlink sequence is indivisible under the lock. (Checking
    /// before the transaction opened would leave a gap: a row landing
    /// there would commit, and this chunk would then unlink the blob the
    /// row just made live.) The transaction writes nothing; committing it
    /// is only the lock-release mechanism. An unlink failing mid-chunk
    /// rolls the transaction back — the lock is released, the files
    /// already unlinked in this chunk stay unlinked (POSIX unlink is not
    /// transactional), and the run reports the error.
    fn unlink_in_write_tx(
        &mut self,
        chunk: &[Deletable],
        check_ingest: bool,
    ) -> Result<ChunkOutcome, StoreError> {
        let watermark = self.watermark;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| StoreError::Db(e.to_string()))?;
        if check_ingest && read_generation_on(&tx)? != watermark {
            return Ok(ChunkOutcome::Aborted);
        }
        for obj in chunk {
            fs::remove_file(&obj.path)
                .map_err(|e| StoreError::Io(format!("unlink {}: {e}", obj.path.display())))?;
        }
        tx.commit().map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(ChunkOutcome::Unlinked)
    }

    /// `true` at index `i` when `ids[i]` is NOT live. Each batch loads a
    /// `gc_probe(i INTEGER, id BLOB)` temp table (one INSERT statement,
    /// typed columns) and joins once; the ordered `i` column makes the
    /// result alignment structural, not lexicographic. The per-batch
    /// round trips are 1 prepare + 1 insert + 1 join at any store size.
    fn batch_not_live(&self, ids: &[Id]) -> Result<Vec<bool>, StoreError> {
        let mut out = Vec::with_capacity(ids.len());
        let mut insert = self
            .conn
            .prepare("INSERT INTO gc_probe(i, id) VALUES(?1, ?2)")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let mut join = self
            .conn
            .prepare(
                "SELECT gc_live.id IS NULL FROM gc_probe \
                 LEFT JOIN gc_live ON gc_live.id = gc_probe.id \
                 ORDER BY gc_probe.i",
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;
        for chunk in ids.chunks(1024) {
            self.conn
                .execute_batch("DELETE FROM gc_probe;")
                .map_err(|e| StoreError::Db(e.to_string()))?;
            for (i, id) in chunk.iter().enumerate() {
                insert
                    .execute(rusqlite::params![i as i64, id.as_bytes()])
                    .map_err(|e| StoreError::Db(e.to_string()))?;
            }
            let dead: Vec<bool> = join
                .query_map([], |r| r.get(0))
                .map_err(|e| StoreError::Db(e.to_string()))?
                .collect::<Result<_, _>>()
                .map_err(|e| StoreError::Db(e.to_string()))?;
            assert_eq!(
                dead.len(),
                chunk.len(),
                "join must answer once per probe row"
            );
            out.extend(dead);
        }
        Ok(out)
    }

    /// Is `id` live? (Test/introspection surface.)
    pub fn is_live(&self, id: Id) -> Result<bool, StoreError> {
        Ok(!self.batch_not_live(std::slice::from_ref(&id))?[0])
    }

    fn build(&mut self, root: &Path) -> Result<(), StoreError> {
        self.conn
            .execute_batch(
                "CREATE TEMP TABLE gc_live(id BLOB PRIMARY KEY); \
                 CREATE TEMP TABLE gc_probe(i INTEGER PRIMARY KEY, id BLOB); \
                 CREATE TEMP TABLE gc_expanded(id BLOB PRIMARY KEY); \
                 CREATE TEMP TABLE gc_claim_batch(id BLOB PRIMARY KEY);",
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;

        // ------------------------------------------------------ seeds
        // Ref tips (roots of the recursive parent closure) ...
        self.conn
            .execute_batch("INSERT OR IGNORE INTO gc_live(id) SELECT cs FROM refs;")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        // ... and every stored changeset row: D14 keeps all rows, so every
        // row is itself a root and its blob references stay live.
        self.conn
            .execute_batch("INSERT OR IGNORE INTO gc_live(id) SELECT id FROM changesets;")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        // Certificate roots and derived tree ids: `derived(kind='tree')`
        // payloads are empty (empty tree) or a 32-byte tree id; a
        // malformed payload is a cache miss in the engine and contributes
        // no root here either.
        self.conn
            .execute_batch(
                "INSERT OR IGNORE INTO gc_live(id) \
                 SELECT root_tree FROM validated_changesets_v1 WHERE length(root_tree) = 32;
                 INSERT OR IGNORE INTO gc_live(id) \
                 SELECT bytes FROM derived WHERE kind = 'tree' AND length(bytes) = 32;",
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;

        // ------------------------------------ changeset closure (batched)
        // Parents: a parent may be a stored row (already seeded live and
        // visited below as part of the full scan) or an absent row — a
        // dangling parent cannot exist in a valid store, but the walk
        // treats it as a non-root and simply never resolves it.
        let mut after: Option<Vec<u8>> = None;
        loop {
            let batch = self.changeset_batch(after.as_deref())?;
            if batch.is_empty() {
                break;
            }
            for cs in batch.iter().map(|(_, cs)| cs) {
                // Parents join the live set (D14: rows are never collected,
                // and absent rows simply never resolve).
                self.insert_live(cs.parents.iter().map(|p| p.as_bytes()))?;
                // Blob references of non-gitlink changes: the gitlink blob
                // field is an opaque upstream pointer, never a objects id.
                self.insert_live(
                    cs.changes
                        .iter()
                        .filter(|c| c.mode != Mode::Gitlink)
                        .filter_map(|c| c.blob.as_ref().map(|b| b.as_bytes())),
                )?;
            }
            after = batch.last().map(|(id, _)| id.as_bytes().to_vec());
        }

        // ------------------------------------------ tree closure (iterative)
        // gc_live now holds: all cs ids, all referenced blob ids, all
        // certificate/derived root tree ids. Expand tree ids transitively
        // over their payloads. Only ids that are trees contribute; blob
        // ids simply never match a tree object (different bytes).
        //
        // Claims are taken at *push* time, not pop time: a dir entry is
        // queued at most once no matter how many parents reference it.
        // Pop-time dedup would let the queue grow with the edge count
        // (~134M duplicate pushes on the kernel corpus → ~4 GiB RSS);
        // push-time claims bound it by the distinct-tree count.
        //
        // Claim *statements* are batched (C2): children accumulate in a
        // [`WRITE_BATCH`]-bounded buffer, and each batch is published with
        // one `INSERT OR IGNORE INTO gc_expanded SELECT ... RETURNING` —
        // the returned rows are exactly the ids that were not claimed
        // before, i.e. the ones to enqueue. The buffer is flushed when it
        // fills (inside the entry loop, so a single wide tree cannot
        // overshoot it) and when the queue drains, so forward progress
        // never waits on a full batch. Duplicates inside one batch
        // collapse in the temp table's primary key, so the push-time
        // invariant (one queue entry per distinct tree) is preserved; the
        // extra memory is at most `WRITE_BATCH` ids (128 KiB), not the
        // edge count.
        let objects = open_objects(root)?;
        let mut queue: VecDeque<Id> = VecDeque::new();
        let mut claim_buf: Vec<[u8; 32]> = Vec::with_capacity(WRITE_BATCH);
        for chunk in self.pending_tree_ids()?.chunks(WRITE_BATCH) {
            let ids: Vec<[u8; 32]> = chunk.iter().map(|id| *id.as_bytes()).collect();
            queue.extend(self.claim_batch(&ids)?);
        }
        loop {
            let Some(id) = queue.pop_front() else {
                // Queue drained: publish buffered claims; if that enqueued
                // nothing, every reachable tree has been expanded.
                let newly = self.flush_claims(&mut claim_buf)?;
                if newly.is_empty() {
                    break;
                }
                queue.extend(newly);
                continue;
            };
            let Some(bytes) = objects.get_tree(id)? else {
                // A certificate/derived root whose object is gone: a cache
                // miss in the engine, not data loss. Nothing to expand.
                continue;
            };
            let tree = Tree::decode(&bytes)
                .map_err(|e| StoreError::Corrupt(format!("tree {id} does not decode: {e}")))?;
            for entry in &tree.entries {
                match entry.kind {
                    TreeKind::Dir => {
                        if let Some(child) = entry.tree {
                            // Claim at push: without it, two parents that
                            // reference the same child both enqueue it
                            // before either pops it, and the queue grows
                            // with the edge count (hundreds of millions
                            // of duplicate 32-byte entries on the kernel
                            // corpus) instead of the distinct-tree count.
                            claim_buf.push(*child.as_bytes());
                            // Flush inside the entry loop, not only between
                            // trees: one tree may legally carry ~2^20
                            // entries, so flushing per tree would let the
                            // buffer overshoot [`WRITE_BATCH`] by 32 MiB.
                            // Order is irrelevant (claims are a set), so
                            // splitting one tree's children across batches
                            // is free.
                            if claim_buf.len() >= WRITE_BATCH {
                                let newly = self.flush_claims(&mut claim_buf)?;
                                queue.extend(newly);
                            }
                        }
                    }
                    TreeKind::File => {
                        if let Some(blob) = entry.blob {
                            // `insert_live` only buffers; its own bound is
                            // [`WRITE_BATCH`], so a 2^20-entry tree does
                            // not grow a per-tree staging vector either.
                            self.insert_live(std::iter::once(blob.as_bytes()))?;
                        }
                    }
                }
            }
            self.insert_live(std::iter::once(id.as_bytes()))?;
        }
        // The sweep (and every `gc_live` read) starts after `materialize`
        // returns, so the mark must be complete here — never leave a
        // buffered id uncommitted.
        self.flush_live()?;
        Ok(())
    }

    /// Claims `ids`: the ids that were not already claimed, i.e. the ones
    /// to enqueue. One transaction per batch: rows land in the
    /// `gc_claim_batch` temp table (primary key collapses duplicates
    /// inside the batch), and a single `INSERT OR IGNORE ... RETURNING`
    /// publishes exactly the rows `gc_expanded` did not already hold.
    /// This replaces one statement per distinct tree id.
    ///
    /// The transaction is temp-database only — no row in this batch (or
    /// anywhere in the mark) touches `metadata.sqlite3` — so it takes no
    /// main-database write lock and cannot contend with the ingest path
    /// the sweep's `--force` watermark guards against. Its cost is the
    /// temp database's journal create/delete cycle, which is why batching
    /// (one cycle per 4096 ids instead of per id) is the whole win.
    fn claim_batch(&mut self, ids: &[[u8; 32]]) -> Result<Vec<Id>, StoreError> {
        let tx = self
            .conn
            .transaction()
            .map_err(|e| StoreError::Db(e.to_string()))?;
        tx.execute_batch("DELETE FROM gc_claim_batch;")
            .map_err(|e| StoreError::Db(e.to_string()))?;
        insert_claim_rows(&tx, ids)?;
        let claimed = claim_returning(&tx)?;
        tx.commit().map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(claimed)
    }

    /// Publishes `buf`'s buffered claims and returns the newly claimed
    /// ids (those to enqueue). `buf` is emptied and keeps its capacity.
    fn flush_claims(&mut self, buf: &mut Vec<[u8; 32]>) -> Result<Vec<Id>, StoreError> {
        if buf.is_empty() {
            return Ok(Vec::new());
        }
        let ids = std::mem::take(buf);
        let claimed = self.claim_batch(&ids)?;
        *buf = ids;
        buf.clear();
        Ok(claimed)
    }

    /// One batch of stored changesets with `id > after`, ascending.
    fn changeset_batch(&self, after: Option<&[u8]>) -> Result<Vec<(Id, Changeset)>, StoreError> {
        let sql = match after {
            Some(_) => "SELECT id, canonical FROM changesets WHERE id > ?1 ORDER BY id LIMIT ?2",
            None => "SELECT id, canonical FROM changesets ORDER BY id LIMIT ?1",
        };
        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let rows: Vec<(Vec<u8>, Vec<u8>)> = match after {
            Some(lo) => stmt
                .query_map(rusqlite::params![lo, BATCH as i64], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(|e| StoreError::Db(e.to_string()))?
                .collect::<Result<_, _>>()
                .map_err(|e| StoreError::Db(e.to_string()))?,
            None => stmt
                .query_map(rusqlite::params![BATCH as i64], |r| {
                    Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .map_err(|e| StoreError::Db(e.to_string()))?
                .collect::<Result<_, _>>()
                .map_err(|e| StoreError::Db(e.to_string()))?,
        };
        rows.into_iter()
            .map(|(id_bytes, canonical)| {
                let id = <[u8; 32]>::try_from(&id_bytes[..])
                    .map(Id::from_bytes)
                    .map_err(|_| StoreError::Corrupt("changeset id BLOB is not 32 bytes".into()))?;
                let cs = Changeset::decode(&canonical)
                    .map_err(|e| StoreError::Corrupt(format!("changeset {id}: {e}")))?;
                Ok((id, cs))
            })
            .collect()
    }

    /// Seed ids for the tree closure: every live id that came from a
    /// tree-naming source — certificate roots and `derived(kind='tree')`
    /// payloads. Discovered children join during the walk itself. Blob
    /// ids never enter this set (they are not tree-named), so the walk
    /// never asks the object store to decode a blob envelope as a tree.
    fn pending_tree_ids(&self) -> Result<Vec<Id>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT root_tree FROM validated_changesets_v1 \
                 UNION SELECT bytes FROM derived WHERE kind = 'tree'",
            )
            .map_err(|e| StoreError::Db(e.to_string()))?;
        let rows: Vec<Vec<u8>> = stmt
            .query_map([], |r| r.get(0))
            .map_err(|e| StoreError::Db(e.to_string()))?
            .collect::<Result<_, _>>()
            .map_err(|e| StoreError::Db(e.to_string()))?;
        Ok(rows
            .into_iter()
            .filter_map(|b| <[u8; 32]>::try_from(&b[..]).ok().map(Id::from_bytes))
            .collect())
    }

    /// Buffers ids for `gc_live`, flushing every [`WRITE_BATCH`] ids.
    ///
    /// The old shape opened and committed a transaction per call (one per
    /// tree and one per file entry in the walk, plus one per parent/blob
    /// chunk per changeset): tens of millions of transactions on the
    /// kernel corpus. They are temp-database transactions — nothing here
    /// touches `metadata.sqlite3` — but each still pays a journal
    /// create/delete cycle and a commit. Ordering inside `gc_live` is
    /// irrelevant — the table is a set, and the only reader
    /// (`batch_not_live`) runs after [`Mark::materialize`] has flushed.
    /// Bounded by [`WRITE_BATCH`] ids (128 KiB), so the kernel-store RSS
    /// budget is untouched.
    fn insert_live<'a>(
        &mut self,
        ids: impl Iterator<Item = &'a [u8; 32]>,
    ) -> Result<(), StoreError> {
        for id in ids {
            self.live_buf.push(*id);
            if self.live_buf.len() >= WRITE_BATCH {
                self.flush_live()?;
            }
        }
        Ok(())
    }

    /// Commits the buffered ids into `gc_live` in ONE transaction. Called
    /// whenever the buffer fills and unconditionally at the end of
    /// [`Mark::build`], so the sweep and every test/introspection read
    /// observe the complete mark. A failure leaves the buffer intact and
    /// propagates; the mark (and its temp tables) then never materializes.
    fn flush_live(&mut self) -> Result<(), StoreError> {
        if self.live_buf.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction()
            .map_err(|e| StoreError::Db(e.to_string()))?;
        {
            let mut stmt = tx
                .prepare("INSERT OR IGNORE INTO gc_live(id) VALUES(?1)")
                .map_err(|e| StoreError::Db(e.to_string()))?;
            for id in &self.live_buf {
                stmt.execute([id.as_slice()])
                    .map_err(|e| StoreError::Db(e.to_string()))?;
            }
        }
        tx.commit().map_err(|e| StoreError::Db(e.to_string()))?;
        self.live_buf.clear();
        Ok(())
    }
}

/// Opens the object store read path for the tree closure.
fn open_objects(root: &Path) -> Result<gardenbed_store::FsObjectStore, StoreError> {
    gardenbed_store::FsObjectStore::open(root)
}

/// Loads `ids` into the `gc_claim_batch` temp table inside `tx`. A loop of
/// prepared inserts (not one statement per commit): the transaction is the
/// unit that matters, and rusqlite reuses one prepared statement.
fn insert_claim_rows(tx: &rusqlite::Transaction<'_>, ids: &[[u8; 32]]) -> Result<(), StoreError> {
    let mut stmt = tx
        .prepare("INSERT OR IGNORE INTO gc_claim_batch(id) VALUES(?1)")
        .map_err(|e| StoreError::Db(e.to_string()))?;
    for id in ids {
        stmt.execute([id.as_slice()])
            .map_err(|e| StoreError::Db(e.to_string()))?;
    }
    Ok(())
}

/// Publishes `gc_claim_batch` into `gc_expanded` and returns the rows that
/// were actually inserted (unclaimed until now). `RETURNING` on
/// `INSERT OR IGNORE` yields only the accepted rows, which is exactly the
/// enqueue set; `gc_claim_batch`'s primary key already collapsed
/// duplicates within the batch.
fn claim_returning(tx: &rusqlite::Transaction<'_>) -> Result<Vec<Id>, StoreError> {
    let mut stmt = tx
        .prepare(
            "INSERT OR IGNORE INTO gc_expanded(id) \
             SELECT id FROM gc_claim_batch RETURNING id",
        )
        .map_err(|e| StoreError::Db(e.to_string()))?;
    let rows: Vec<Vec<u8>> = stmt
        .query_map([], |r| r.get(0))
        .map_err(|e| StoreError::Db(e.to_string()))?
        .collect::<Result<_, _>>()
        .map_err(|e| StoreError::Db(e.to_string()))?;
    rows.into_iter()
        .map(|b| {
            <[u8; 32]>::try_from(&b[..])
                .map(Id::from_bytes)
                .map_err(|_| StoreError::Corrupt("claimed id BLOB is not 32 bytes".into()))
        })
        .collect()
}
