//! Read-only virtualized checkout (FUSE): every file of the current
//! changeset's tree is visible and readable at the mountpoint, and each
//! read lazily materializes exactly the blob it needs through the
//! existing cache-first fetch path — no sparse selection, no second
//! checkout mechanism.
//!
//! Reuse is the whole design: the mount is a *view* over the same
//! [`crate::rpc::Session`]/[`crate::rpc::Fetcher`] stack every other
//! command uses. `getattr` and `readdir` walk the changeset's derived
//! trees via the want-list fetch path (trees land in `.bed/objects` the
//! first time they are visited); `stat` and `read` pull the blob
//! through `Fetcher::blob`, which is hash-verified and cached before
//! the kernel ever sees the bytes. The local object cache therefore
//! stays bounded by what was actually touched (a file's blob lands on
//! first `stat` — buffered reads are clamped to the reported size, so
//! it must be real), while the *whole* repository tree is listable —
//! virtualization is lazy materialization, not a checkout.
//!
//! # Snapshot semantics
//!
//! The mount reflects the working copy's current changeset at mount time
//! and does **not** track ref movement afterwards — a `publish` on the
//! server does not change a live mount. Unmount and mount again to
//! switch: the remount resolves `current_cs` fresh (the v0.3 story).
//!
//! # Deterministic metadata
//!
//! Blobs and trees carry no timestamps in the object model, so the mount
//! reports deterministic ones: every entry's `atime`/`mtime`/`ctime` is
//! the changeset's **committer time** (the only timestamp the model
//! records). Directories report size 4096 with `nlink = 2 + immediate
//! subdirectories` (the POSIX convention `find`-style tools expect).
//! Regular files report `nlink = 1` and their **true size**, fetched
//! cache-first on first `stat` — a buffered read is clamped to the
//! reported size, so the size must be real; a file's blob is
//! materialized when it is first stat'ed (a stat is a touch in the
//! lazy-materialization sense). Symlinks report the target's length
//! (the same fetch `readlink` needs anyway). Gitlinks are opaque
//! upstream pointers, not stored objects: they list as read-only empty
//! files.
//!
//! # The async/sync bridge
//!
//! fuser callbacks are synchronous (the kernel wants an answer now) but
//! the fetch path is async. [`mount`] bridges the two worlds with a
//! dedicated single-threaded tokio runtime that owns **everything
//! async**: the `Session` is created and the snapshot resolved on it,
//! so the gRPC connection and every fetch live there — the caller's
//! runtime can park for arbitrary blocking syscalls against the mount
//! without starving the connection. A callback never blocks on the
//! runtime: it hands its reply object (fuser's replies are sendable
//! one-shot values) to the runtime through an unbounded channel and
//! returns, and the runtime fills and sends the reply when the fetch
//! completes. The runtime thread runs one `block_on` over that channel
//! — while `recv` is pending, spawned tasks still get polled — so there
//! is no cross-wait anywhere: no deadlock.
//!
//! # No root required
//!
//! Mounting goes through fuser's `fusermount3` path, so **no root
//! privileges are needed** (`/dev/fuse` is world-writable on ordinary
//! desktops; [`fuse_available`] reports whether this host can mount at
//! all, and tests gate on it). All mutating operations return `EROFS` —
//! this is a read-only view by construction.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::future::Future;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use fuser::Errno;
use fuser::FileAttr;
use fuser::FileType;
use fuser::Filesystem;
use fuser::FopenFlags;
use fuser::Generation;
use fuser::INodeNo;
use fuser::MountOption;
use fuser::ReplyAttr;
use fuser::ReplyData;
use fuser::ReplyDirectory;
use fuser::ReplyEmpty;
use fuser::ReplyEntry;
use fuser::ReplyOpen;
use fuser::ReplyWrite;
use fuser::Request;
use fuser::Session;
use fuser::TimeOrNow;
use gardenbed_model::{Id, Mode, Tree, TreeEntry, TreeKind};
use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::rpc::Session as RpcSession;
use crate::workspace::WorkingCopy;

/// How long kernel caches may trust our attributes before re-asking.
/// The mount is an immutable snapshot, so this is purely an optimization
/// constant; one second keeps `stat` bursts cheap in shell loops without
/// observable staleness.
const ATTR_TTL: Duration = Duration::from_secs(1);

/// Size (and blksize) reported for directories — the 4096 convention.
const DIR_SIZE: u64 = 4096;

/// The root inode.
const ROOT_INO: u64 = 1;

/// Whether a mount can run on this host: `/dev/fuse` exists and
/// `fusermount3` answers. Integration tests gate on this so CI runners
/// without FUSE skip the mount suite with a clear message instead of
/// failing; where FUSE *is* available the tests run and must pass.
pub fn fuse_available() -> bool {
    Path::new("/dev/fuse").exists()
        && std::process::Command::new("fusermount3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

/// A live virtualized checkout.
///
/// The tree is served on a background thread. Dropping the handle
/// unmounts and reclaims everything; [`MountHandle::unmount`] is the
/// explicit path and reports unmount errors. An external
/// `fusermount -u <mountpoint>` also ends the session cleanly — the
/// kernel delivers its destroy message and the serving thread exits.
pub struct MountHandle {
    mountpoint: PathBuf,
    bridge: Option<Bridge>,
    serve: Option<JoinHandle<io::Result<()>>>,
    /// Set by [`MountHandle::unmount`]; the drop path must not unmount
    /// twice.
    unmounted: bool,
}

impl MountHandle {
    /// Where the changeset's tree is visible.
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Unmounts and reclaims the serving thread. The kernel delivers its
    /// destroy message, the serving loop returns, the async bridge shuts
    /// down. Safe to call exactly once; afterwards the mountpoint is an
    /// ordinary directory again.
    pub fn unmount(mut self) -> io::Result<()> {
        self.unmounted = true;
        let (unmounted, served) = self.teardown();
        unmounted?;
        served
    }

    /// Unmount, stop the bridge, join the serve thread — in that order
    /// (the bridge must be down before joining: the serve thread may
    /// still be answering a kernel request through the runtime).
    fn teardown(&mut self) -> (io::Result<()>, io::Result<()>) {
        let unmounted = fusermount_unmount(&self.mountpoint);
        if let Some(bridge) = self.bridge.take() {
            bridge.shutdown();
        }
        let served = match self.serve.take() {
            Some(serve) => match serve.join() {
                Ok(result) => result,
                Err(_) => panic!("mount serving thread panicked"),
            },
            None => Ok(()),
        };
        (unmounted, served)
    }
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        if self.unmounted {
            return;
        }
        let _ = self.teardown();
    }
}

/// Unmounts `mountpoint` via `fusermount3 -u` — the same path an
/// external unmount takes, usable without root. "Not found in mtab"
/// (exit 1 with that message) means the mount is already gone — most
/// commonly an external `fusermount -u` — and is a clean end, not an
/// error; the serving thread observes the kernel's destroy message
/// either way.
fn fusermount_unmount(mountpoint: &Path) -> io::Result<()> {
    let output = std::process::Command::new("fusermount3")
        .arg("-u")
        .arg(mountpoint)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("not found in") || stderr.contains("not mounted") {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "fusermount3 -u {} failed with {}: {stderr}",
        mountpoint.display(),
        output.status
    )))
}

/// One unit of bridged work, executed as a task on the dedicated
/// runtime.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// The dedicated runtime + its driving thread (see module docs: "The
/// async/sync bridge"). Dropping the sender stops the loop; the thread
/// exits once every queued job has run.
struct Bridge {
    tx: UnboundedSender<Job>,
    thread: Option<JoinHandle<()>>,
}

impl Bridge {
    /// Starts the runtime thread. Everything async in the mount — the
    /// session, the snapshot resolution, every fetch — runs there.
    fn start() -> io::Result<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<io::Result<()>>();
        let thread = std::thread::Builder::new()
            .name("gardenbed-mount-bridge".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        ready_tx.send(Err(e)).ok();
                        return;
                    }
                };
                ready_tx.send(Ok(())).ok();
                // One `block_on` drives the runtime for the mount's whole
                // life: while `recv` is pending, tasks spawned by jobs
                // below keep getting polled.
                runtime.block_on(async move {
                    while let Some(job) = rx.recv().await {
                        tokio::task::spawn(async move { job() });
                    }
                });
            })
            .map_err(io::Error::other)?;
        ready_rx
            .recv()
            .map_err(|_| io::Error::other("mount bridge thread exited"))??;
        Ok(Bridge {
            tx,
            thread: Some(thread),
        })
    }

    /// Stops the runtime thread after every queued job has run.
    fn shutdown(mut self) {
        drop(self.tx);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

/// The immutable half of the mount: what is mounted, where from, and how
/// inode numbers map back to repo-relative paths. Shared between the
/// sync callbacks (which only touch `inodes`) and the async fetches.
struct MountState {
    session: Arc<RpcSession>,
    root_tree: Option<Id>,
    /// The changeset's committer time — the mount's universal timestamp.
    commit_time: SystemTime,
    /// Inode → repo-relative path ("" is the root). The kernel pins
    /// inodes with lookup counts; we map them back through this table
    /// and never reuse a number.
    inodes: Mutex<Inodes>,
}

#[derive(Default)]
struct Inodes {
    by_ino: HashMap<u64, String>,
    by_path: HashMap<String, u64>,
    next: u64,
}

impl Inodes {
    /// The inode for `path`, assigning a stable one on first sight.
    fn inode(&mut self, path: &str) -> u64 {
        if let Some(ino) = self.by_path.get(path) {
            return *ino;
        }
        self.next = self.next.max(ROOT_INO) + 1;
        let ino = self.next;
        self.by_ino.insert(ino, path.to_owned());
        self.by_path.insert(path.to_owned(), ino);
        ino
    }

    /// The repo-relative path for `ino`, if the kernel still holds it.
    fn path(&self, ino: u64) -> Option<String> {
        if ino == ROOT_INO {
            return Some(String::new());
        }
        self.by_ino.get(&ino).cloned()
    }
}

impl MountState {
    fn path_of(&self, ino: u64) -> Option<String> {
        self.inodes.lock().path(ino)
    }

    fn inode_of(&self, path: &str) -> u64 {
        self.inodes.lock().inode(path)
    }
}

/// The result of resolving a path inside the snapshot.
enum Resolved {
    /// The path names nothing.
    Missing,
    /// A directory: its decoded tree and inode.
    Dir { tree: Tree, ino: u64 },
    /// A file: its tree entry and inode.
    File { entry: TreeEntry, ino: u64 },
}

impl MountState {
    /// Walks `path` from the root, fetching each tree along the way
    /// through the cache-first fetcher (`Missing` under a file path — a
    /// file cannot contain anything).
    async fn resolve(&self, path: &str) -> Result<Resolved, Errno> {
        let root = match self.root_tree {
            Some(root) => root,
            // The empty tree: the root exists, nothing below it does.
            None => {
                return Ok(if path.is_empty() {
                    Resolved::Dir {
                        tree: Tree::default(),
                        ino: self.inode_of(""),
                    }
                } else {
                    Resolved::Missing
                });
            }
        };
        if path.is_empty() {
            let tree = self.fetch_tree(root).await?;
            return Ok(Resolved::Dir {
                tree,
                ino: self.inode_of(""),
            });
        }
        let mut cur = root;
        let comps: Vec<&str> = path.split('/').collect();
        for (i, comp) in comps.iter().enumerate() {
            let tree = self.fetch_tree(cur).await?;
            let Some(entry) = tree.entries.iter().find(|e| e.name == *comp) else {
                return Ok(Resolved::Missing);
            };
            if i + 1 == comps.len() {
                let ino = self.inode_of(path);
                return Ok(match entry.kind {
                    TreeKind::Dir => {
                        let id = entry.tree.expect("dir entries carry a tree id");
                        Resolved::Dir {
                            tree: self.fetch_tree(id).await?,
                            ino,
                        }
                    }
                    TreeKind::File => Resolved::File {
                        entry: entry.clone(),
                        ino,
                    },
                });
            }
            match entry.kind {
                TreeKind::Dir => cur = entry.tree.expect("dir entries carry a tree id"),
                TreeKind::File => return Ok(Resolved::Missing),
            }
        }
        unreachable!("the loop returns on the final component")
    }

    async fn fetch_tree(&self, id: Id) -> Result<Tree, Errno> {
        self.session
            .fetcher()
            .tree(id)
            .await
            .map_err(|_| Errno::EIO)
    }

    /// Attributes for `path`, or `None` when it names nothing.
    async fn attr(&self, path: &str) -> Result<Option<FileAttr>, Errno> {
        Ok(match self.resolve(path).await? {
            Resolved::Missing => None,
            Resolved::File { entry, ino } => Some(file_attr(self, &entry, ino).await),
            Resolved::Dir { tree, ino } => Some(dir_attr(self, &tree, ino)),
        })
    }
}

/// Attributes of a file entry. See module docs: "Deterministic
/// metadata" for the size policy.
async fn file_attr(state: &MountState, entry: &TreeEntry, ino: u64) -> FileAttr {
    let (perm, kind) = match entry.mode {
        Mode::Regular => (0o444, FileType::RegularFile),
        Mode::Exec => (0o555, FileType::RegularFile),
        Mode::Symlink => (0o777, FileType::Symlink),
        // An opaque upstream pointer, never a stored object.
        Mode::Gitlink => (0o444, FileType::RegularFile),
    };
    // The true size, fetched cache-first: a buffered read is clamped to
    // the size reported here, so `stat` must know it — a file's blob is
    // materialized on first stat. That keeps lazy materialization intact
    // (only touched files land in `.bed/objects`) while reads return
    // every byte. Gitlinks have no stored object; they report 0.
    let size = match entry.mode {
        Mode::Gitlink => 0,
        _ => match entry.blob {
            Some(blob) => state
                .session
                .fetcher()
                .blob(blob)
                .await
                .map(|content| content.len() as u64)
                .unwrap_or(0),
            None => 0,
        },
    };
    FileAttr {
        ino: INodeNo(ino),
        size,
        blocks: size.div_ceil(512),
        atime: state.commit_time,
        mtime: state.commit_time,
        ctime: state.commit_time,
        crtime: state.commit_time,
        kind,
        perm,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn dir_attr(state: &MountState, tree: &Tree, ino: u64) -> FileAttr {
    let subdirs = tree
        .entries
        .iter()
        .filter(|e| e.kind == TreeKind::Dir)
        .count();
    FileAttr {
        ino: INodeNo(ino),
        size: DIR_SIZE,
        blocks: DIR_SIZE / 512,
        atime: state.commit_time,
        mtime: state.commit_time,
        ctime: state.commit_time,
        crtime: state.commit_time,
        kind: FileType::Directory,
        perm: 0o555,
        nlink: 2 + subdirs as u32,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// The sync face fuser calls into. Stateless: every method that needs
/// the fetcher submits the reply object to the bridge runtime and
/// returns immediately (see module docs: "The async/sync bridge").
struct SyncFs {
    tx: UnboundedSender<Job>,
    state: Arc<MountState>,
}

impl SyncFs {
    /// Ships a reply-filling future to the bridge runtime.
    fn reply_with<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.tx
            .send(Box::new(move || {
                tokio::task::spawn(fut);
            }))
            .map_err(|_| ())
            .expect("bridge thread outlives the mount");
    }
}

fn join_path(parent: &str, name: &OsStr) -> String {
    if parent.is_empty() {
        name.to_string_lossy().into_owned()
    } else {
        format!("{}/{}", parent, name.to_string_lossy())
    }
}

impl Filesystem for SyncFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_path) = self.state.path_of(parent.into()) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let child = join_path(&parent_path, name);
        let state = self.state.clone();
        self.reply_with(async move {
            match state.resolve(&child).await {
                Ok(Resolved::Missing) => reply.error(Errno::ENOENT),
                Ok(Resolved::File { entry, ino }) => {
                    let attr = file_attr(&state, &entry, ino).await;
                    reply.entry(&ATTR_TTL, &attr, Generation(0));
                }
                Ok(Resolved::Dir { tree, ino }) => {
                    let attr = dir_attr(&state, &tree, ino);
                    reply.entry(&ATTR_TTL, &attr, Generation(0));
                }
                Err(e) => reply.error(e),
            }
        });
    }

    fn getattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: Option<fuser::FileHandle>,
        reply: ReplyAttr,
    ) {
        let Some(path) = self.state.path_of(ino.into()) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let state = self.state.clone();
        self.reply_with(async move {
            match state.attr(&path).await {
                Ok(Some(attr)) => reply.attr(&ATTR_TTL, &attr),
                Ok(None) => reply.error(Errno::ENOENT),
                Err(e) => reply.error(e),
            }
        });
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(path) = self.state.path_of(ino.into()) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let state = self.state.clone();
        self.reply_with(async move {
            match state.resolve(&path).await {
                Ok(Resolved::File { entry, .. }) if entry.mode == Mode::Symlink => {
                    let blob = entry.blob.expect("file entries carry a blob id");
                    match state.session.fetcher().blob(blob).await {
                        Ok(target) => reply.data(&target),
                        Err(_) => reply.error(Errno::EIO),
                    }
                }
                _ => reply.error(Errno::EINVAL),
            }
        });
    }

    fn open(&self, _req: &Request, _ino: INodeNo, _flags: fuser::OpenFlags, reply: ReplyOpen) {
        reply.opened(fuser::FileHandle(0), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let Some(path) = self.state.path_of(ino.into()) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let state = self.state.clone();
        self.reply_with(async move {
            match state.resolve(&path).await {
                Ok(Resolved::File { entry, .. }) => {
                    if entry.mode == Mode::Symlink {
                        reply.error(Errno::EACCES);
                        return;
                    }
                    let Some(blob) = entry.blob else {
                        reply.error(Errno::EIO);
                        return;
                    };
                    match state.session.fetcher().blob(blob).await {
                        Ok(content) => {
                            let start = (offset as usize).min(content.len());
                            let end = (start + size as usize).min(content.len());
                            reply.data(&content[start..end]);
                        }
                        Err(_) => reply.error(Errno::EIO),
                    }
                }
                Ok(Resolved::Dir { .. }) => reply.error(Errno::EISDIR),
                Ok(Resolved::Missing) => reply.error(Errno::ENOENT),
                Err(e) => reply.error(e),
            }
        });
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        reply: ReplyDirectory,
    ) {
        let Some(path) = self.state.path_of(ino.into()) else {
            reply.error(Errno::ENOENT);
            return;
        };
        let state = self.state.clone();
        self.reply_with(async move {
            match state.resolve(&path).await {
                Ok(Resolved::Dir { tree, ino }) => {
                    let mut reply = reply;
                    fill_dir(&state, &tree, ino, &path, offset, &mut reply);
                    reply.ok();
                }
                Ok(Resolved::File { .. }) => reply.error(Errno::ENOTDIR),
                Ok(Resolved::Missing) => reply.error(Errno::ENOENT),
                Err(e) => reply.error(e),
            }
        });
    }

    // Everything below is the read-only contract: `EROFS`, always.

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _offset: u64,
        _data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        reply.error(Errno::EROFS);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: fuser::FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    #[allow(clippy::too_many_arguments)] // fuser's trait signature
    fn setattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<fuser::FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mkdir(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn symlink(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _link: &Path,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn create(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: fuser::ReplyCreate,
    ) {
        reply.error(Errno::EROFS);
    }
}

/// Fills a directory reply: the two dot entries, then the tree's
/// entries, honoring the kernel's resume `offset`. Offsets are 1-based
/// over `[.], [..], entry0, entry1, …` — the kernel re-asks with the
/// last offset it consumed, and only entries past it are sent.
fn fill_dir(
    state: &MountState,
    tree: &Tree,
    dir_ino: u64,
    dir_path: &str,
    offset: u64,
    reply: &mut ReplyDirectory,
) {
    let mut next = 1u64;
    if offset < next && reply.add(INodeNo(dir_ino), next, FileType::Directory, ".") {
        return;
    }
    next += 1;
    if offset < next && reply.add(INodeNo(dir_ino), next, FileType::Directory, "..") {
        return;
    }
    for entry in &tree.entries {
        next += 1;
        if next <= offset {
            continue;
        }
        let ino = state.inode_of(&join_path(dir_path, OsStr::new(&entry.name)));
        let kind = match entry.kind {
            TreeKind::Dir => FileType::Directory,
            TreeKind::File => match entry.mode {
                Mode::Symlink => FileType::Symlink,
                _ => FileType::RegularFile,
            },
        };
        if reply.add(INodeNo(ino), next, kind, &entry.name) {
            return;
        }
    }
}

/// Mounts the working copy's **current changeset** (snapshot; see module
/// docs) read-only at `mountpoint`, serving on a background thread.
///
/// The whole async half — session connect, changeset fetch, root tree,
/// committer time — runs on the bridge's dedicated runtime before
/// anything is mounted, so a server that is down or a working copy
/// without a current changeset fails here, leaving no mount behind.
/// Every fetch flows through the working copy's `.bed/objects` cache:
/// mounting and reading through the mount is cache-compatible with
/// every other command.
pub async fn mount(bed: &WorkingCopy, mountpoint: &Path) -> io::Result<MountHandle> {
    let cs_hex = bed.config().current_cs.as_deref().ok_or_else(|| {
        io::Error::other("mount: no current changeset; clone or checkout a changeset first")
    })?;
    let cs = Id::from_str(cs_hex)
        .map_err(|e| io::Error::other(format!("mount: current_cs is malformed: {e}")))?;

    let bridge = Bridge::start()?;
    let (session, root_tree, commit_time) = match run_setup(&bridge, bed.clone(), cs).await {
        Ok(setup) => setup,
        Err(e) => {
            bridge.shutdown();
            return Err(e);
        }
    };

    let state = Arc::new(MountState {
        session,
        root_tree,
        commit_time,
        inodes: Mutex::new(Inodes::default()),
    });
    let fs = SyncFs {
        tx: bridge.tx.clone(),
        state,
    };

    let mut config = fuser::Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("gardenbed".into()),
        MountOption::Subtype("gardenbed".into()),
    ];
    // One dispatch thread is plenty: callbacks hand off to the runtime
    // and return, so the loop never blocks on a fetch.
    config.n_threads = Some(1);

    let mountpoint_owned = mountpoint.to_path_buf();
    let fuse_session = match Session::new(fs, &mountpoint_owned, &config) {
        Ok(session) => session,
        Err(e) => {
            bridge.shutdown();
            return Err(e);
        }
    };
    let serve = match std::thread::Builder::new()
        .name("gardenbed-mount-serve".into())
        .spawn(move || fuse_session.run())
    {
        Ok(serve) => serve,
        Err(e) => {
            let _ = fusermount_unmount(&mountpoint_owned);
            bridge.shutdown();
            return Err(io::Error::other(e));
        }
    };

    // The returned handle implies a mountpoint that answers: fusermount
    // mounted inside `Session::new`; this probe stats *through* the
    // mount (blocking until the serving thread replies), so a mount
    // whose bridge cannot answer fails here instead of hanging the
    // caller's first read.
    wait_until_ready(mountpoint).await?;

    Ok(MountHandle {
        mountpoint: mountpoint_owned,
        bridge: Some(bridge),
        serve: Some(serve),
        unmounted: false,
    })
}

/// Runs the mount-time setup on the bridge runtime and returns the
/// resolved snapshot. Nothing is mounted yet, so failure needs no
/// cleanup beyond stopping the bridge.
async fn run_setup(
    bridge: &Bridge,
    bed: WorkingCopy,
    cs: Id,
) -> io::Result<(Arc<RpcSession>, Option<Id>, SystemTime)> {
    let (tx, rx) =
        tokio::sync::oneshot::channel::<io::Result<(Arc<RpcSession>, Option<Id>, SystemTime)>>();
    bridge
        .tx
        .send(Box::new(move || {
            tokio::task::spawn(async move {
                let setup = async {
                    let session = RpcSession::open(&bed)
                        .await
                        .map_err(|e| io::Error::other(format!("mount: connect: {e}")))?;
                    let changeset = session
                        .get_changeset(cs, "mount")
                        .await
                        .map_err(|e| io::Error::other(format!("mount: {e}")))?;
                    let root_tree = session
                        .root_tree(cs, "mount")
                        .await
                        .map_err(|e| io::Error::other(format!("mount: {e}")))?;
                    let commit_time = UNIX_EPOCH
                        .checked_add(Duration::from_secs(changeset.committer.time_secs))
                        .unwrap_or(UNIX_EPOCH);
                    Ok((Arc::new(session), root_tree, commit_time))
                };
                tx.send(setup.await).ok();
            });
        }))
        .map_err(|_| ())
        .expect("bridge thread outlives the mount");
    rx.await
        .map_err(|_| io::Error::other("mount setup task dropped"))?
}

/// Waits (bounded) for the mountpoint to answer a directory read —
/// proof the serving thread and the async bridge are both live.
async fn wait_until_ready(mountpoint: &Path) -> io::Result<()> {
    let mountpoint = mountpoint.to_path_buf();
    let probe = tokio::task::spawn_blocking(move || std::fs::read_dir(&mountpoint).map(|_| ()));
    match tokio::time::timeout(Duration::from_secs(10), probe).await {
        Ok(joined) => {
            joined.map_err(|e| io::Error::other(format!("mount readiness probe panicked: {e}")))?
        }
        Err(_) => Err(io::Error::other(
            "mountpoint did not answer within 10s — mount setup failed",
        )),
    }
}

#[cfg(test)]
mod tests {
    //! API-shape tests that always run, no FUSE device needed: the
    //! public surface is this module's contract with the CLI wiring and
    //! the gated integration tests.

    use super::*;

    #[test]
    fn mount_handle_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<MountHandle>();
    }

    #[test]
    fn availability_probe_is_stable() {
        // CI machines without /dev/fuse see `false` here — the gated
        // integration tests rely on that answer being stable.
        assert_eq!(fuse_available(), fuse_available());
    }
}
