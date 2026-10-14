//! One bed: the `.bed` directory tying together config, treestate, local
//! object cache, and heads (v0.1.md §6).
//!
//! ```text
//! .bed/config.toml    BedConfig
//! .bed/treestate      Treestate (sqlite, WAL)
//! .bed/objects/       gardenbed_store::FsObjectStore — the local CAS cache,
//!                     same GBLO1 envelope and 2-level sharding as loam
//! .bed/heads          Heads — local stack tips
//! ```
//!
//! The garden root — the directory the bed grows in, i.e. the `.bed`
//! directory's parent — is where working files live; [`Bed::garden_root`]
//! resolves it for the status scan.

use std::path::Path;
use std::path::PathBuf;

use gardenbed_store::FsObjectStore;

use crate::config::BedConfig;
use crate::error::BedError;
use crate::heads::Heads;
use crate::treestate::Treestate;

/// The bed's local state: config, treestate, object cache, heads.
#[derive(Clone, Debug)]
pub struct Bed {
    bed_dir: PathBuf,
    config: BedConfig,
}

impl Bed {
    /// The default bed directory: `./.bed`.
    pub fn default_dir() -> PathBuf {
        PathBuf::from("./.bed")
    }

    /// Creates a fresh bed at `bed_dir` with the given server and repo.
    ///
    /// The config is validated *before* anything touches the filesystem, so
    /// a bad server/repo leaves no half-planted bed. The full layout is
    /// created: `config.toml`, the treestate database, the object cache
    /// directory, and an empty heads file.
    ///
    /// Today this serves tests; `bed clone` calls it once the server half
    /// of the clone flow is wired.
    pub fn init(bed_dir: &Path, server: &str, repo: &str) -> Result<Self, BedError> {
        let config = BedConfig::new(server, repo)?;
        std::fs::create_dir_all(bed_dir)?;

        config.save(&bed_dir.join("config.toml"))?;
        // FsObjectStore::open takes the store ROOT and creates `<root>/objects`.
        FsObjectStore::open(bed_dir)?;
        Treestate::open(&bed_dir.join("treestate"))?;
        std::fs::write(bed_dir.join("heads"), b"")?;

        Ok(Self {
            bed_dir: bed_dir.to_path_buf(),
            config,
        })
    }

    /// Opens an existing bed. Fails with [`BedError::NotABed`] when there
    /// is no `config.toml` to read.
    pub fn open(bed_dir: &Path) -> Result<Self, BedError> {
        let config = BedConfig::load(&bed_dir.join("config.toml"))?;
        Ok(Self {
            bed_dir: bed_dir.to_path_buf(),
            config,
        })
    }

    /// The `.bed` directory itself.
    pub fn dir(&self) -> &Path {
        &self.bed_dir
    }

    /// The garden root — the directory the bed grows in (the `.bed`
    /// directory's parent). Working files live here; treestate paths are
    /// relative to it.
    pub fn garden_root(&self) -> PathBuf {
        match self.bed_dir.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            // ".bed" with no parent component means the current directory.
            _ => PathBuf::from("."),
        }
    }

    /// The bed's configuration.
    pub fn config(&self) -> &BedConfig {
        &self.config
    }

    /// The bed's configuration, for mutation (e.g. `bed plot add`); persist
    /// with [`Bed::save_config`].
    pub fn config_mut(&mut self) -> &mut BedConfig {
        &mut self.config
    }

    /// Persists the (possibly mutated) config back to `config.toml`.
    pub fn save_config(&self) -> Result<(), BedError> {
        self.config.save(&self.bed_dir.join("config.toml"))
    }

    /// Opens the treestate database (`.bed/treestate`).
    pub fn treestate(&self) -> Result<Treestate, BedError> {
        Treestate::open(&self.bed_dir.join("treestate"))
    }

    /// Opens the local CAS cache (`.bed/objects`) — loam's
    /// `FsObjectStore`, reused as-is. The store root is `.bed` itself
    /// (`FsObjectStore::open` appends `objects/`), so objects live at the
    /// documented `.bed/objects/<2 hex>/<62 hex>` layout.
    pub fn objects(&self) -> Result<FsObjectStore, BedError> {
        Ok(FsObjectStore::open(&self.bed_dir)?)
    }

    /// A handle on the heads file (`.bed/heads`).
    pub fn heads(&self) -> Heads {
        Heads::open(self.bed_dir.join("heads"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_the_full_layout() {
        let dir = tempfile::tempdir().unwrap();
        let bed_dir = dir.path().join(".bed");

        let bed = Bed::init(&bed_dir, "http://127.0.0.1:7400", "gardenbed").unwrap();
        assert!(bed_dir.join("config.toml").is_file());
        assert!(bed_dir.join("treestate").is_file());
        assert!(bed_dir.join("objects").is_dir());
        assert!(bed_dir.join("heads").is_file());

        assert_eq!(bed.config().server, "http://127.0.0.1:7400");
        assert_eq!(bed.config().repo, "gardenbed");
        assert_eq!(bed.garden_root(), dir.path());
    }

    #[test]
    fn init_rejects_bad_fields_before_touching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let bed_dir = dir.path().join(".bed");
        assert!(Bed::init(&bed_dir, "", "repo").is_err());
        // Nothing was created.
        assert!(!bed_dir.exists());
    }

    #[test]
    fn open_loads_config_and_fails_clearly_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Bed::open(&dir.path().join(".bed")),
            Err(BedError::NotABed(_))
        ));

        let bed_dir = dir.path().join(".bed");
        Bed::init(&bed_dir, "http://s", "r").unwrap();
        let bed = Bed::open(&bed_dir).unwrap();
        assert_eq!(bed.config().repo, "r");
        assert_eq!(bed.garden_root(), dir.path());
    }

    #[test]
    fn garden_root_survives_relative_and_bare_bed_dirs() {
        let dir = tempfile::tempdir().unwrap();
        // Absolute layout.
        Bed::init(&dir.path().join(".bed"), "s", "r").unwrap();
        assert_eq!(
            Bed::open(&dir.path().join(".bed")).unwrap().garden_root(),
            dir.path()
        );
        // A bed dir with no parent component means "here".
        let bed = Bed {
            bed_dir: std::path::PathBuf::from(".bed"),
            config: BedConfig::new("s", "r").unwrap(),
        };
        assert_eq!(bed.garden_root(), std::path::PathBuf::from("."));
    }

    #[test]
    fn accessors_open_the_same_layout() {
        let dir = tempfile::tempdir().unwrap();
        let bed_dir = dir.path().join(".bed");
        let mut bed = Bed::init(&bed_dir, "s", "r").unwrap();

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

        // plot add → save_config → reopen sees it.
        bed.config_mut().plot.push("src".to_owned());
        bed.save_config().unwrap();
        assert_eq!(Bed::open(&bed_dir).unwrap().config().plot, vec!["src"]);
    }
}
