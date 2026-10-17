//! Multi-repository serving (v0.2.md §5, D12–D13, M11).
//!
//! One server process serves N repositories out of a server root:
//! `<root>/<repo>/objects` + `<root>/<repo>/metadata.sqlite3`. The directory
//! tree is the registry (a directory listing cannot lie; a registry file can
//! drift), so serving a new repository is a directory plus a request — no
//! restart, no new code path. [`RepoSet`] opens repositories lazily on first
//! use, singleflighted through the map lock, and caps open handles with
//! `--max-repos` by evicting least-recently-used handles nobody is using.
//!
//! Every gRPC handler resolves its request's `repo` field through
//! [`RepoSet::resolve`] — the one seam where authentication slots in (v0.3).
//! Storage stays per-repository: nothing in the storage layer learns that
//! other repositories exist, so per-repository metadata keeps the
//! compare-and-swap write lock per-repository (cross-repo write parallelism
//! for free).
//!
//! Migration (D12): a v0.1 root — `metadata.sqlite3` or `objects/` at the
//! top level — serves unchanged as repository `default`, the name `bed
//! clone` and `gardenbed-mcp` already fall back to; `gardenbed-server move`
//! performs the two renames explicitly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::{Repo, ServerError};

/// The repository name a v0.1 root (or a request naming none) serves as.
pub const DEFAULT_REPO: &str = "default";

/// The default open-handle cap (`--max-repos`).
pub const DEFAULT_MAX_REPOS: usize = 256;

/// Names a repository directory can never take (v0.2.md §5 layout rules):
/// reserved for root-level files a repository must not shadow.
const RESERVED_NAMES: [&str; 5] = [
    "objects",
    "metadata.sqlite3",
    "seedbed.sqlite3",
    "git-mirror.git",
    "tmp",
];

/// Is `name` a valid repository name? 1..=64 bytes of `[A-Za-z0-9._-]`, not
/// starting with `.`, not reserved (v0.2.md §5). Case-sensitive.
pub fn valid_repo_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || name.starts_with('.') {
        return false;
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return false;
    }
    !RESERVED_NAMES.contains(&name)
}

/// Why a repo name failed to resolve: not a legal name
/// (`invalid_argument`) or legal but not served (`not_found`, listing the
/// served names).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RepoResolveError {
    /// The empty string or an otherwise illegal name (bad length, bad
    /// characters, leading `.`, or reserved).
    #[error("invalid repository name {name:?}: {reason}")]
    InvalidName {
        /// The name as sent.
        name: String,
        /// What is wrong with it.
        reason: String,
    },
    /// A legal name the server does not serve.
    #[error("repository {name} not found (this server serves: {served})")]
    NotFound {
        /// The name as sent.
        name: String,
        /// The served names, comma-separated.
        served: String,
    },
    /// Opening the repository's stores failed (I/O, corruption).
    #[error("cannot open repository {name}: {source}")]
    Open {
        /// The name as sent.
        name: String,
        /// The underlying error.
        source: ServerError,
    },
}

/// One lazily opened repository: the [`Repo`] plus the bookkeeping the
/// [`RepoSet`] eviction needs — a use counter (handles currently resolving
/// requests; only zero-use handles are evictable) and a monotonic clock
/// stamp for LRU ordering.
pub struct RepoHandle {
    repo: Arc<Repo>,
    uses: AtomicU64,
    last_used: AtomicU64,
}

impl RepoHandle {
    /// The repository service (stores + engine + counters).
    pub fn repo(&self) -> &Arc<Repo> {
        &self.repo
    }

    fn stamp(&self, clock: &AtomicU64) {
        self.last_used
            .store(clock.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    }
}

/// The serving set: `<root>/<repo>/` opened on first request, bounded by
/// `max_repos` open handles.
///
/// Cheap to share behind an `Arc`; every method takes `&self`. Open is
/// singleflighted through the map lock (one thread opens a repository;
/// concurrent requesters wait and share the result), and a use counter on
/// each handle keeps in-flight requests alive while LRU eviction runs.
pub struct RepoSet {
    root: PathBuf,
    max_repos: usize,
    /// Open handles by name. Repositories are opened (and inserted here)
    /// under the write lock; reads take the read lock only to clone an
    /// already-open `Arc`.
    open: RwLock<HashMap<String, Arc<RepoHandle>>>,
    /// Monotonic clock for LRU stamps.
    clock: AtomicU64,
    /// Bound on concurrent opens (makes the singleflight map lock a real
    /// singleflight even under a burst).
    open_gate: Mutex<()>,
}

impl std::fmt::Debug for RepoSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = self
            .open
            .read()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        f.debug_struct("RepoSet")
            .field("root", &self.root)
            .field("max_repos", &self.max_repos)
            .field("open", &names)
            .finish()
    }
}

impl RepoSet {
    /// The serving set over `root`. Legacy detection runs eagerly (one
    /// `metadata_dir` probe) so `serve` can log it, but no repository is
    /// opened until a request names it.
    pub fn new(root: &Path, max_repos: usize) -> Self {
        RepoSet {
            root: root.to_path_buf(),
            max_repos: max_repos.max(1),
            open: RwLock::new(HashMap::new()),
            clock: AtomicU64::new(1),
            open_gate: Mutex::new(()),
        }
    }

    /// The server root this set serves.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The open-handle cap.
    pub fn max_repos(&self) -> usize {
        self.max_repos
    }

    /// True if `root` is a v0.1 layout (`metadata.sqlite3` or `objects/` at
    /// the top level) rather than a multi-repo root. Such a root serves
    /// unchanged as repository [`DEFAULT_REPO`].
    pub fn is_legacy_root(root: &Path) -> bool {
        root.join("metadata.sqlite3").is_file() || root.join("objects").is_dir()
    }

    /// The directory of repository `name` (`root/name`; the legacy root
    /// itself for [`DEFAULT_REPO`]).
    pub fn repo_dir(&self, name: &str) -> PathBuf {
        if name == DEFAULT_REPO && Self::is_legacy_root(&self.root) {
            self.root.clone()
        } else {
            self.root.join(name)
        }
    }

    /// The names this server serves: every subdirectory of the root holding
    /// `metadata.sqlite3` or `objects/`, plus `default` for a legacy root.
    /// Sorted; a repository exists iff it appears here (D12: the directory
    /// is the registry).
    pub fn served_names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.root)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .filter(|e| {
                        let p = e.path();
                        valid_repo_name(e.file_name().to_string_lossy().as_ref())
                            && Self::is_repo_dir(&p)
                    })
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        if Self::is_legacy_root(&self.root) && !names.contains(&DEFAULT_REPO.to_owned()) {
            names.push(DEFAULT_REPO.to_owned());
        }
        names
    }

    /// Does `dir` hold a repository (either store present)?
    fn is_repo_dir(dir: &Path) -> bool {
        dir.join("metadata.sqlite3").is_file() || dir.join("objects").is_dir()
    }

    /// The names currently held open (for logs and tests).
    pub fn open_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .open
            .read()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    /// The number of open handles.
    pub fn open_count(&self) -> usize {
        self.open.read().map(|m| m.len()).unwrap_or(0)
    }

    /// Resolves the request's `repo` field to an open handle, opening the
    /// repository if this is its first request. The returned guard marks
    /// the handle in use until dropped; eviction never closes a handle a
    /// request still holds.
    ///
    /// This is the one seam every RPC passes through — where authentication
    /// lands in v0.3 (v0.2.md §5).
    pub fn resolve(&self, name: &str) -> Result<RepoGuard<'_>, RepoResolveError> {
        if name.is_empty() {
            return Err(RepoResolveError::InvalidName {
                name: name.to_owned(),
                reason: "the `repo` field is mandatory (empty name)".into(),
            });
        }
        if !valid_repo_name(name) {
            let reason = if name.len() > 64 {
                format!(
                    "longer than 64 bytes ({}), reserved names are {:?}",
                    name.len(),
                    RESERVED_NAMES
                )
            } else {
                format!(
                    "must be 1..=64 bytes of [A-Za-z0-9._-], not starting with `.`; reserved: {:?}",
                    RESERVED_NAMES
                )
            };
            return Err(RepoResolveError::InvalidName {
                name: name.to_owned(),
                reason,
            });
        }
        if !self.exists(name) {
            return Err(RepoResolveError::NotFound {
                name: name.to_owned(),
                served: self.served_list(),
            });
        }

        let handle = self.open_handle(name)?;
        let guard = RepoGuard {
            handle: Some(handle),
            set: self,
        };
        Ok(guard)
    }

    /// True if repository `name` is served (legacy `default` included).
    pub fn exists(&self, name: &str) -> bool {
        if name == DEFAULT_REPO && Self::is_legacy_root(&self.root) {
            return true;
        }
        valid_repo_name(name) && Self::is_repo_dir(&self.repo_dir(name))
    }

    /// The comma-separated served-name list for error messages.
    fn served_list(&self) -> String {
        let names = self.served_names();
        if names.is_empty() {
            "(no repositories under this root)".to_owned()
        } else {
            names.join(", ")
        }
    }

    /// Opens (or joins) repository `name`, evicting LRU handles over the
    /// cap. Singleflighted: the open itself happens under the write lock,
    /// so concurrent first requests for one repository open it once.
    fn open_handle(&self, name: &str) -> Result<Arc<RepoHandle>, RepoResolveError> {
        // Fast path: already open.
        if let Some(h) = self
            .open
            .read()
            .map(|m| m.get(name).cloned())
            .unwrap_or(None)
        {
            h.stamp(&self.clock);
            h.uses.fetch_add(1, Ordering::Relaxed);
            return Ok(h);
        }
        let _gate = self.open_gate.lock().map_err(|_| RepoResolveError::Open {
            name: name.to_owned(),
            source: ServerError::Invalid("repo open gate poisoned".into()),
        })?;
        // Re-check after taking the gate (another opener may have won).
        if let Some(h) = self
            .open
            .read()
            .map(|m| m.get(name).cloned())
            .unwrap_or(None)
        {
            h.stamp(&self.clock);
            h.uses.fetch_add(1, Ordering::Relaxed);
            return Ok(h);
        }
        let dir = self.repo_dir(name);
        let repo = Repo::open(&dir).map_err(|source| RepoResolveError::Open {
            name: name.to_owned(),
            source,
        })?;
        let handle = Arc::new(RepoHandle {
            repo: Arc::new(repo),
            uses: AtomicU64::new(1),
            last_used: AtomicU64::new(0),
        });
        handle.stamp(&self.clock);
        {
            let mut map = self.open.write().map_err(|_| RepoResolveError::Open {
                name: name.to_owned(),
                source: ServerError::Invalid("repo map lock poisoned".into()),
            })?;
            map.insert(name.to_owned(), handle.clone());
            self.evict_locked(&mut map);
        }
        log::debug!("repository {name} opened from {}", dir.display());
        Ok(handle)
    }

    /// Evicts LRU handles over the cap, closing only zero-use handles.
    /// Caller holds the write lock.
    fn evict_locked(&self, map: &mut HashMap<String, Arc<RepoHandle>>) {
        while map.len() > self.max_repos {
            // Least-recently-used evictable handle, if any.
            let victim = map
                .iter()
                .filter(|(_, h)| h.uses.load(Ordering::Relaxed) == 0)
                .min_by_key(|(_, h)| h.last_used.load(Ordering::Relaxed))
                .map(|(k, _)| k.clone());
            let Some(victim) = victim else { break };
            if let Some(h) = map.remove(&victim) {
                // Dropping the last `Arc` closes the sqlite connection and
                // releases the engine's caches.
                drop(h);
                log::debug!("repository {victim} evicted (max-repos {})", self.max_repos);
            }
        }
    }

    /// Force-opens a repository by name (import/export/mcp: administrative
    /// callers that have a name, not a request). Same errors as
    /// [`RepoSet::resolve`], minus the use guard.
    pub fn open_repo(&self, name: &str) -> Result<Arc<Repo>, RepoResolveError> {
        Ok(self.resolve(name)?.handle_arc())
    }

    /// Opens a repository by directory path relative to the root,
    /// validating the name. For `import`/`export --repo`.
    pub fn repo_dir_for(&self, name: &str) -> Result<PathBuf, RepoResolveError> {
        if name.is_empty() || !valid_repo_name(name) {
            return Err(RepoResolveError::InvalidName {
                name: name.to_owned(),
                reason: "must be 1..=64 bytes of [A-Za-z0-9._-], not starting with `.`; \
                         reserved: objects, metadata.sqlite3, seedbed.sqlite3, git-mirror.git, tmp"
                    .into(),
            });
        }
        Ok(self.repo_dir(name))
    }

    /// Moves repository `from` to name `to` (`gardenbed-server move`):
    /// the two renames — `<root>/<from>` → `<root>/<to>` — plus the mirror.
    /// Both stores are path-free and self-contained, so history is
    /// byte-identical after the move (M11.4). Refuses unless `from` exists,
    /// `to` does not, both names are valid, and the server's legacy-layout
    /// rules hold (`default` may only move away from a legacy root).
    pub fn move_repo(&self, from: &str, to: &str) -> Result<PathBuf, RepoResolveError> {
        if !valid_repo_name(from) || !valid_repo_name(to) {
            return Err(RepoResolveError::InvalidName {
                name: format!("{from} -> {to}"),
                reason: "repository names must be 1..=64 bytes of [A-Za-z0-9._-], not \
                         starting with `.`, not reserved"
                    .into(),
            });
        }
        let legacy = Self::is_legacy_root(&self.root);
        let from_dir = self.repo_dir(from);
        let to_dir = self.repo_dir(to);
        if legacy && from == DEFAULT_REPO {
            // Moving the legacy root itself: the root stays (the server
            // needs it), the repository moves into it under the new name.
            if !Self::is_legacy_root(&from_dir) {
                return Err(RepoResolveError::NotFound {
                    name: from.to_owned(),
                    served: self.served_list(),
                });
            }
        } else if !Self::is_repo_dir(&from_dir) {
            return Err(RepoResolveError::NotFound {
                name: from.to_owned(),
                served: self.served_list(),
            });
        }
        if self.exists(to) {
            return Err(RepoResolveError::InvalidName {
                name: to.to_owned(),
                reason: "a repository with this name already exists under the root".into(),
            });
        }
        if to_dir.exists() {
            return Err(RepoResolveError::InvalidName {
                name: to.to_owned(),
                reason: "the destination directory already exists".into(),
            });
        }
        // The source must not be open (its sqlite connection holds the
        // file); the set never holds the legacy `default` open across a
        // move started before any request, so require it closed.
        if self.open_names().contains(&from.to_owned()) {
            // Close it explicitly: drop our handle so the sqlite connection
            // releases, then proceed (in-process callers only; the binary
            // calls this before serving).
            let mut map = self.open.write().map_err(|_| RepoResolveError::Open {
                name: from.to_owned(),
                source: ServerError::Invalid("repo map lock poisoned".into()),
            })?;
            map.remove(from);
        }
        let do_rename = |from: &Path, to: &Path| -> Result<(), RepoResolveError> {
            std::fs::rename(from, to).map_err(|e| RepoResolveError::Open {
                name: from.display().to_string(),
                source: ServerError::Store(gardenbed_store::StoreError::Io(e.to_string())),
            })
        };
        if legacy && from == DEFAULT_REPO {
            // Legacy move: create `<root>/<to>`, move `objects/` and
            // `metadata.sqlite3` into it, then the mirror if present.
            std::fs::create_dir_all(&to_dir).map_err(|e| RepoResolveError::Open {
                name: to.to_owned(),
                source: ServerError::Store(gardenbed_store::StoreError::Io(e.to_string())),
            })?;
            for entry in ["objects", "metadata.sqlite3", "git-mirror.git"] {
                let src = self.root.join(entry);
                if src.exists() {
                    do_rename(&src, &to_dir.join(entry))?;
                }
            }
            Ok(to_dir)
        } else {
            do_rename(&from_dir, &to_dir)?;
            Ok(to_dir)
        }
    }
}

/// An in-use repository resolved from a [`RepoSet`]: keeps the handle's use
/// count elevated until dropped, so LRU eviction cannot close the sqlite
/// connection under a live request.
pub struct RepoGuard<'a> {
    handle: Option<Arc<RepoHandle>>,
    set: &'a RepoSet,
}

impl std::ops::Deref for RepoGuard<'_> {
    type Target = Repo;

    fn deref(&self) -> &Repo {
        self.handle
            .as_ref()
            .expect("guard always holds a handle")
            .repo()
    }
}

impl RepoGuard<'_> {
    /// The guarded repository as a shared handle.
    pub fn handle_arc(&self) -> Arc<Repo> {
        self.handle
            .as_ref()
            .expect("guard always holds a handle")
            .repo()
            .clone()
    }
}

impl Drop for RepoGuard<'_> {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.uses.fetch_sub(1, Ordering::Relaxed);
            // Try to shrink over-cap sets once the last user lets go.
            if let Ok(mut map) = self.set.open.write() {
                self.set.evict_locked(&mut map);
            }
        }
    }
}

/// The mirror directory for git imports of repository `name` under `root`:
/// `<root>/<repo>/git-mirror.git` (legacy: the root's own).
pub fn mirror_dir(root: &Path, repo: &str) -> PathBuf {
    if repo == DEFAULT_REPO && RepoSet::is_legacy_root(root) {
        root.join("git-mirror.git")
    } else {
        root.join(repo).join("git-mirror.git")
    }
}
