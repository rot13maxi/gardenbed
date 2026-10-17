//! `.bed/heads` — the working copy's local stack tips (v0.1.md §6).
//!
//! A plain text file, one lowercase-hex changeset id per line, bottom of
//! the trellis first. `commit`/`rebase` (later waves) push and re-point tips
//! here; for now the file exists so the local layout is complete and the
//! append/read contract is in place.

use std::fs;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

use gardenbed_model::Id;

use crate::error::ClientError;

/// A handle on `.bed/heads`. Cheap; the file is touched lazily.
#[derive(Clone, Debug)]
pub struct Heads {
    path: PathBuf,
}

impl Heads {
    /// Wraps the heads file at `path`. The file need not exist yet.
    pub fn open(path: PathBuf) -> Self {
        Heads { path }
    }

    /// The heads file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every recorded tip, in file order (bottom of the stack first).
    ///
    /// A missing file reads as "no tips yet" — a working copy that has never created.
    /// Blank lines are skipped; anything else must be a 64-hex id.
    pub fn read(&self) -> Result<Vec<Id>, ClientError> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(ClientError::Io(e)),
        };
        let mut tips = Vec::new();
        for (i, line) in raw.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let id = Id::from_str(line).map_err(|e| ClientError::HeadsParse {
                line: i + 1,
                reason: format!("{line:?} is not a changeset id: {e}"),
            })?;
            tips.push(id);
        }
        Ok(tips)
    }

    /// Appends one tip as a newline-terminated hex line, creating the file
    /// (and its parents) on first use. Appends are history — an id already
    /// present is written again rather than deduplicated.
    pub fn append(&self, id: Id) -> Result<(), ClientError> {
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{}", id.to_hex())?;
        Ok(())
    }

    /// Re-points the stack: the LAST occurrence of `old` becomes `new`.
    ///
    /// `rebase` replaces a created changeset with its replayed copy; the tip
    /// that named the old changeset must name the new one instead of
    /// growing a stale second tip. When `old` is not on the stack (e.g. the
    /// bed moved with `switch` after committing, so heads never recorded it),
    /// this degrades to [`Heads::append`] — the rebased changeset simply
    /// becomes a tip.
    pub fn repoint(&self, old: Id, new: Id) -> Result<(), ClientError> {
        let mut tips = self.read()?;
        match tips.iter_mut().rposition(|t| *t == old) {
            Some(i) => {
                tips[i] = new;
                if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    fs::create_dir_all(parent)?;
                }
                let mut body = String::with_capacity(tips.len() * 65);
                for tip in &tips {
                    body.push_str(&tip.to_hex());
                    body.push('\n');
                }
                fs::write(&self.path, body)?;
                Ok(())
            }
            None => self.append(new),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> Id {
        Id::from_bytes([n; 32])
    }

    #[test]
    fn append_then_read_preserves_order() {
        let dir = tempfile::tempdir().unwrap();
        let heads = Heads::open(dir.path().join("heads"));
        assert_eq!(heads.read().unwrap(), Vec::<Id>::new());

        heads.append(id(1)).unwrap();
        heads.append(id(2)).unwrap();
        heads.append(id(3)).unwrap();
        assert_eq!(heads.read().unwrap(), vec![id(1), id(2), id(3)]);
    }

    #[test]
    fn appends_preserve_earlier_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heads");
        fs::write(&path, format!("{}\n", id(9).to_hex())).unwrap();
        let heads = Heads::open(path);
        heads.append(id(10)).unwrap();
        assert_eq!(heads.read().unwrap(), vec![id(9), id(10)]);
    }

    #[test]
    fn rejects_bad_lines_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heads");
        fs::write(&path, format!("{}\n\nnot-an-id\n", id(1).to_hex())).unwrap();
        match Heads::open(path).read() {
            Err(ClientError::HeadsParse { line, .. }) => assert_eq!(line, 3),
            other => panic!("expected HeadsParse, got {other:?}"),
        }
    }

    #[test]
    fn reads_missing_file_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let heads = Heads::open(dir.path().join("never-created"));
        assert_eq!(heads.read().unwrap(), Vec::<Id>::new());
    }

    #[test]
    fn repoint_replaces_the_last_occurrence_and_keeps_order() {
        let dir = tempfile::tempdir().unwrap();
        let heads = Heads::open(dir.path().join("heads"));
        heads.append(id(1)).unwrap();
        heads.append(id(2)).unwrap();
        heads.append(id(3)).unwrap();
        heads.append(id(2)).unwrap();

        heads.repoint(id(2), id(9)).unwrap();
        assert_eq!(heads.read().unwrap(), vec![id(1), id(2), id(3), id(9)]);
        // The second occurrence (the last) was the one replaced.
    }

    #[test]
    fn repoint_of_an_unknown_tip_appends() {
        let dir = tempfile::tempdir().unwrap();
        let heads = Heads::open(dir.path().join("heads"));
        heads.append(id(1)).unwrap();
        heads.append(id(2)).unwrap();

        // `old` (7) never made it onto the stack — the rebased changeset
        // becomes a new tip instead.
        heads.repoint(id(7), id(8)).unwrap();
        assert_eq!(heads.read().unwrap(), vec![id(1), id(2), id(8)]);
    }

    #[test]
    fn repoint_is_idempotent_when_old_equals_new() {
        let dir = tempfile::tempdir().unwrap();
        let heads = Heads::open(dir.path().join("heads"));
        heads.append(id(1)).unwrap();
        heads.repoint(id(1), id(1)).unwrap();
        assert_eq!(heads.read().unwrap(), vec![id(1)]);
    }
}
