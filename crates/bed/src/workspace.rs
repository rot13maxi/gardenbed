//! One bed: the `.bed` directory tying together config, treestate, local
//! object cache, and heads (v0.1.md §6).
//!
//! ```text
//! .bed/config.toml    WorkingCopyConfig
//! .bed/index.sqlite3      Treestate (sqlite, WAL)
//! .bed/objects/       gardenbed_store::FsObjectStore — the local CAS cache,
//!                     same GBLO1/GBLO2 envelopes and 2-level sharding
//!                     as objects
//! .bed/heads          Heads — local stack tips
//! ```
//!
//! The working-copy root — the directory containing the working files, i.e. the `.bed`
//! directory's parent — is where working files live; [`WorkingCopy::workspace_root`]
//! resolves it for the status scan.

use std::path::Path;
use std::path::PathBuf;

use gardenbed_store::FsObjectStore;

use crate::config::WorkingCopyConfig;
use crate::error::ClientError;
use crate::heads::Heads;
use crate::treestate::Treestate;

/// The working copy's local state: config, treestate, object cache, heads.
#[derive(Clone, Debug)]
pub struct WorkingCopy {
    state_dir: PathBuf,
    config: WorkingCopyConfig,
}

impl WorkingCopy {
    /// The default bed directory: `./.bed`.
    pub fn default_dir() -> PathBuf {
        PathBuf::from("./.bed")
    }

    /// Creates a fresh working copy at `state_dir` with the given server and repo.
    ///
    /// The config is validated *before* anything touches the filesystem, so
    /// a bad server/repo leaves no half-initialized bed. The full layout is
    /// created: `config.toml`, the treestate database, the object cache
    /// directory, and an empty heads file.
    ///
    /// Today this serves tests; `bed clone` calls it once the server half
    /// of the clone flow is wired.
    pub fn init(state_dir: &Path, server: &str, repo: &str) -> Result<Self, ClientError> {
        let config = WorkingCopyConfig::new(server, repo)?;
        std::fs::create_dir_all(state_dir)?;

        config.save(&state_dir.join("config.toml"))?;
        // FsObjectStore::open takes the store ROOT and creates `<root>/objects`.
        FsObjectStore::open(state_dir)?;
        Treestate::open(&state_dir.join("index.sqlite3"))?;
        std::fs::write(state_dir.join("heads"), b"")?;

        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            config,
        })
    }

    /// Opens an existing working copy. Fails with [`ClientError::NotAWorkingCopy`] when there
    /// is no `config.toml` to read. The error names the *directory* so the
    /// message does not read "`config.toml` has no `config.toml`"; when the
    /// given directory is a working-copy root (a `.bed` inside it), the error
    /// says so, since that mix-up is easy to make.
    pub fn open(state_dir: &Path) -> Result<Self, ClientError> {
        if state_dir.join("treestate").exists() && !state_dir.join("index.sqlite3").exists() {
            return Err(ClientError::ConfigInvalid(
                "working-copy state uses an obsolete layout; create a new working copy".into(),
            ));
        }
        let config_path = state_dir.join("config.toml");
        let config = WorkingCopyConfig::load(&config_path).map_err(|e| match e {
            // Rewrite the "no config.toml" case to name the directory; the
            // read/parse failures already name the file precisely.
            ClientError::NotAWorkingCopy(_) => {
                let inner = state_dir.join(".bed").join("config.toml");
                if inner.is_file() {
                    ClientError::StateDirIsWorkspace(state_dir.to_path_buf())
                } else {
                    ClientError::NotAWorkingCopy(state_dir.to_path_buf())
                }
            }
            other => other,
        })?;
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            config,
        })
    }

    /// The `.bed` directory itself.
    pub fn dir(&self) -> &Path {
        &self.state_dir
    }

    /// The working-copy root — the directory containing the working files (the `.bed`
    /// directory's parent). Working files live here; treestate paths are
    /// relative to it.
    pub fn workspace_root(&self) -> PathBuf {
        match self.state_dir.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            // ".bed" with no parent component means the current directory.
            _ => PathBuf::from("."),
        }
    }

    /// The working copy's configuration.
    pub fn config(&self) -> &WorkingCopyConfig {
        &self.config
    }

    /// The working copy's configuration, for mutation (e.g. `bed sparse add`); persist
    /// with [`WorkingCopy::save_config`].
    pub fn config_mut(&mut self) -> &mut WorkingCopyConfig {
        &mut self.config
    }

    /// Persists the (possibly mutated) config back to `config.toml`.
    pub fn save_config(&self) -> Result<(), ClientError> {
        self.config.save(&self.state_dir.join("config.toml"))
    }

    /// Opens the treestate database (`.bed/index.sqlite3`).
    pub fn treestate(&self) -> Result<Treestate, ClientError> {
        Treestate::open(&self.state_dir.join("index.sqlite3"))
    }

    /// Opens the offline commit queue (the `pending` tables inside
    /// `.bed/index.sqlite3`, created on demand so v0.1 working copies load
    /// unchanged).
    pub fn queue(&self) -> Result<crate::queue::Queue, ClientError> {
        crate::queue::Queue::open(&self.state_dir.join("index.sqlite3"))
    }

    /// Opens the local CAS cache (`.bed/objects`) — the object store's
    /// `FsObjectStore`, reused as-is. The store root is `.bed` itself
    /// (`FsObjectStore::open` appends `objects/`), so objects live at the
    /// documented `.bed/objects/<2 hex>/<62 hex>` layout.
    pub fn objects(&self) -> Result<FsObjectStore, ClientError> {
        Ok(FsObjectStore::open(&self.state_dir)?)
    }

    /// A handle on the heads file (`.bed/heads`).
    pub fn heads(&self) -> Heads {
        Heads::open(self.state_dir.join("heads"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_the_full_layout() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join(".bed");

        let bed = WorkingCopy::init(&state_dir, "http://127.0.0.1:7400", "gardenbed").unwrap();
        assert!(state_dir.join("config.toml").is_file());
        assert!(state_dir.join("index.sqlite3").is_file());
        assert!(state_dir.join("objects").is_dir());
        assert!(state_dir.join("heads").is_file());

        assert_eq!(bed.config().server, "http://127.0.0.1:7400");
        assert_eq!(bed.config().repo, "gardenbed");
        assert_eq!(bed.workspace_root(), dir.path());
    }

    #[test]
    fn init_rejects_bad_fields_before_touching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join(".bed");
        assert!(WorkingCopy::init(&state_dir, "", "repo").is_err());
        // Nothing was created.
        assert!(!state_dir.exists());
    }

    #[test]
    fn open_loads_config_and_fails_clearly_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let err = WorkingCopy::open(&dir.path().join(".bed")).unwrap_err();
        assert!(matches!(err, ClientError::NotAWorkingCopy(_)));
        // The message names the directory, not "config.toml has no
        // config.toml".
        assert!(
            err.to_string().contains(".bed has no config.toml"),
            "message names the directory: {err}"
        );

        // A working-copy root passed as the working copy dir gets a pointing hint.
        let garden = dir.path().join("garden");
        std::fs::create_dir_all(&garden).unwrap();
        WorkingCopy::init(&garden.join(".bed"), "http://s", "r").unwrap();
        let err = WorkingCopy::open(&garden).unwrap_err();
        assert!(
            err.to_string().contains("pass --state-dir"),
            "garden-root mix-up gets a hint: {err}"
        );

        let state_dir = dir.path().join(".bed");
        WorkingCopy::init(&state_dir, "http://s", "r").unwrap();
        let bed = WorkingCopy::open(&state_dir).unwrap();
        assert_eq!(bed.config().repo, "r");
        assert_eq!(bed.workspace_root(), dir.path());
    }

    #[test]
    fn workspace_root_survives_relative_and_bare_state_dirs() {
        let dir = tempfile::tempdir().unwrap();
        // Absolute layout.
        WorkingCopy::init(&dir.path().join(".bed"), "s", "r").unwrap();
        assert_eq!(
            WorkingCopy::open(&dir.path().join(".bed"))
                .unwrap()
                .workspace_root(),
            dir.path()
        );
        // A bed dir with no parent component means "here".
        let bed = WorkingCopy {
            state_dir: std::path::PathBuf::from(".bed"),
            config: WorkingCopyConfig::new("s", "r").unwrap(),
        };
        assert_eq!(bed.workspace_root(), std::path::PathBuf::from("."));
    }

    #[test]
    fn accessors_open_the_same_layout() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join(".bed");
        let mut bed = WorkingCopy::init(&state_dir, "s", "r").unwrap();

        let ts = bed.treestate().unwrap();
        assert_eq!(ts.count().unwrap(), 0);
        // The cache is a real FsObjectStore: trait methods work on it.
        use gardenbed_store::ObjectStore as _;
        assert!(
            !bed.objects()
                .unwrap()
                .has(gardenbed_model::Id::from_bytes([0; 32]))
                .unwrap()
        );

        bed.heads()
            .append(gardenbed_model::Id::from_bytes([7; 32]))
            .unwrap();
        assert_eq!(bed.heads().read().unwrap().len(), 1);

        // sparse add → save_config → reopen sees it.
        bed.config_mut().sparse.push("src".to_owned());
        bed.save_config().unwrap();
        assert_eq!(
            WorkingCopy::open(&state_dir).unwrap().config().sparse,
            vec!["src"]
        );
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn obsolete_index_is_not_silently_replaced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("treestate"), b"old index").unwrap();
        let error = WorkingCopy::open(dir.path()).unwrap_err();
        assert!(error.to_string().contains("obsolete layout"));
        assert!(!dir.path().join("index.sqlite3").exists());
        assert_eq!(
            std::fs::read(dir.path().join("treestate")).unwrap(),
            b"old index"
        );
    }
}
