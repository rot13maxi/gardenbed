//! `.bed/config.toml` — the bed's configuration (v0.1.md §6).
//!
//! ```toml
//! server = "http://127.0.0.1:7400"
//! repo = "gardenbed"
//! plot = ["src", "docs"]
//! current_cs = "9f2b…64 hex chars…"   # optional
//! ```
//!
//! Loading validates with clear errors; `current_cs` is normalized to
//! lowercase hex and plot prefixes are normalized (trailing slashes
//! stripped) on the way in, so the rest of bed can trust what it reads.

use std::fs;
use std::path::Path;
use std::str::FromStr;

use gardenbed_model::Id;
use serde::Deserialize;
use serde::Serialize;

use crate::error::BedError;
use crate::plot;

/// The bed's configuration, persisted at `<bed>/config.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BedConfig {
    /// The arboretum this bed grows from: `http(s)://host:port` or
    /// `unix:///path/to/socket`.
    pub server: String,
    /// The repo this bed tends on that server.
    pub repo: String,
    /// Sparse checkout path prefixes (the plot), normalized — no trailing
    /// slashes; `.` plants the whole garden.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plot: Vec<String>,
    /// The changeset the bed currently sits at, lowercase hex, once known.
    /// Written by `goto`/`sow`/`clone` in later waves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_cs: Option<String>,
}

impl BedConfig {
    /// Builds and validates a fresh config (no plot, no current changeset).
    ///
    /// This is the entry point used by [`crate::bed::Bed::init`]; it
    /// validates *before* anything touches the filesystem.
    pub fn new(server: &str, repo: &str) -> Result<Self, BedError> {
        let server = server.trim();
        let repo = repo.trim();
        if server.is_empty() {
            return Err(BedError::ConfigInvalid("server address is empty".into()));
        }
        if repo.is_empty() {
            return Err(BedError::ConfigInvalid("repo name is empty".into()));
        }
        Ok(Self {
            server: server.to_owned(),
            repo: repo.to_owned(),
            plot: Vec::new(),
            current_cs: None,
        })
    }

    /// Loads and validates `<bed>/config.toml`. A missing file is
    /// [`BedError::NotABed`] — that is the "there is no bed here" signal.
    pub fn load(path: &Path) -> Result<Self, BedError> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(BedError::NotABed(path.to_path_buf()));
            }
            Err(e) => {
                return Err(BedError::ConfigRead {
                    path: path.to_path_buf(),
                    source: e,
                });
            }
        };
        let mut cfg: Self = toml::from_str(&raw).map_err(|e| BedError::ConfigParse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        cfg.normalize_and_validate(path)?;
        Ok(cfg)
    }

    /// Serializes and writes the config, creating parent directories.
    pub fn save(&self, path: &Path) -> Result<(), BedError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).map_err(|e| BedError::ConfigWrite {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
        }
        let raw = toml::to_string(self).map_err(|e| BedError::ConfigWrite {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        fs::write(path, raw).map_err(|e| BedError::ConfigWrite {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// Checks value-level invariants serde cannot express and normalizes
    /// what should be canonical (lowercase hex id, clean plot prefixes).
    fn normalize_and_validate(&mut self, path: &Path) -> Result<(), BedError> {
        if self.server.trim().is_empty() {
            return Err(BedError::ConfigInvalid(format!(
                "{}: `server` is empty",
                path.display()
            )));
        }
        if self.repo.trim().is_empty() {
            return Err(BedError::ConfigInvalid(format!(
                "{}: `repo` is empty",
                path.display()
            )));
        }
        if let Some(hex) = &self.current_cs {
            // Validate and normalize to lowercase; `Id::from_str` accepts
            // any case and exactly 64 hex characters.
            let id = Id::from_str(hex).map_err(|_| {
                BedError::ConfigInvalid(format!(
                    "{}: `current_cs` is not a 64-character changeset id",
                    path.display()
                ))
            })?;
            self.current_cs = Some(id.to_hex());
        }
        for raw in std::mem::take(&mut self.plot) {
            self.plot.push(plot::normalize_prefix(&raw).map_err(|e| {
                BedError::ConfigInvalid(format!("{}: {e}", path.display()))
            })?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
        dir.path().join(name)
    }

    #[test]
    fn roundtrips_all_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmpfile(&dir, "config.toml");

        let mut cfg = BedConfig::new("http://127.0.0.1:7400", "gardenbed").unwrap();
        cfg.plot = vec!["src".into(), "docs/".into()];
        cfg.current_cs = Some("ABCD".repeat(16));
        cfg.save(&path).unwrap();

        let loaded = BedConfig::load(&path).unwrap();
        assert_eq!(loaded.server, "http://127.0.0.1:7400");
        assert_eq!(loaded.repo, "gardenbed");
        // Normalization happens on load: trailing slashes stripped, hex
        // lowercased.
        assert_eq!(loaded.plot, vec!["src".to_owned(), "docs".to_owned()]);
        assert_eq!(
            loaded.current_cs.as_deref(),
            Some("abcd".repeat(16).as_str())
        );
    }

    #[test]
    fn roundtrips_minimal_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmpfile(&dir, "config.toml");
        BedConfig::new("unix:///tmp/arboretum.sock", "repo").unwrap().save(&path).unwrap();
        let loaded = BedConfig::load(&path).unwrap();
        assert_eq!(loaded.server, "unix:///tmp/arboretum.sock");
        assert_eq!(loaded.repo, "repo");
        assert!(loaded.plot.is_empty());
        assert!(loaded.current_cs.is_none());
        // Saved without the optional fields.
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("plot"));
        assert!(!raw.contains("current_cs"));
    }

    #[test]
    fn rejects_garbage_with_the_path_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmpfile(&dir, "config.toml");
        fs::write(&path, "server = \n[[[oops").unwrap();
        match BedConfig::load(&path) {
            Err(BedError::ConfigParse { path: p, .. }) => assert_eq!(p, path),
            other => panic!("expected ConfigParse, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_file_as_not_a_bed() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmpfile(&dir, "missing.toml");
        assert!(matches!(
            BedConfig::load(&path),
            Err(BedError::NotABed(_))
        ));
    }

    #[test]
    fn rejects_empty_server_and_repo() {
        assert!(BedConfig::new("", "repo").is_err());
        assert!(BedConfig::new("http://x", "  ").is_err());
        assert!(BedConfig::new("  ", "repo").is_err());
        let ok = BedConfig::new(" http://x ", " repo ").unwrap();
        assert_eq!(ok.server, "http://x");
        assert_eq!(ok.repo, "repo");
    }

    #[test]
    fn rejects_bad_current_cs_and_bad_plot_on_load() {
        let dir = tempfile::tempdir().unwrap();

        let path = tmpfile(&dir, "bad-cs.toml");
        fs::write(&path, "server = \"s\"\nrepo = \"r\"\ncurrent_cs = \"nothex\"\n").unwrap();
        assert!(matches!(
            BedConfig::load(&path),
            Err(BedError::ConfigInvalid(_))
        ));

        let path = tmpfile(&dir, "bad-plot.toml");
        fs::write(&path, "server = \"s\"\nrepo = \"r\"\nplot = [\"../up\"]\n").unwrap();
        assert!(matches!(
            BedConfig::load(&path),
            Err(BedError::ConfigInvalid(_))
        ));

        // And the server-less config serde would otherwise accept.
        let path = tmpfile(&dir, "empty-server.toml");
        fs::write(&path, "server = \"\"\nrepo = \"r\"\n").unwrap();
        assert!(matches!(
            BedConfig::load(&path),
            Err(BedError::ConfigInvalid(_))
        ));
    }
}
