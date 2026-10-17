//! Parser for raw git tree objects.
//!
//! A git tree is a sequence of `<octal mode> SP <name> NUL <20 raw bytes>`.
//! This parser is byte-exact (names may contain any non-NUL byte) and
//! strict: a truncated or malformed entry is an error, never a guess.
//!
//! Mode mapping to gardenbed [`Mode`]s (spec §3):
//!
//! | git mode   | gardenbed             |
//! |------------|-----------------------|
//! | `400`      | directory             |
//! | `100644`   | `Regular`             |
//! | `100755`   | `Exec`                |
//! | `120000`   | `Symlink`             |
//! | `160000`   | `Gitlink`             |
//! | other file | exec bit ⇒ `Exec`, else `Regular` (e.g. `100664` from
//!   ancient repos is normalized to `Regular`) |
//!
//! Non-UTF-8 entry names are decoded lossily (same documented divergence as
//! in [`crate::commit`]); a path is the `/`-join of components.

use gardenbed_model::Mode;

use crate::oid::GitOid;

/// One entry of a git tree.
#[derive(Clone, Debug)]
pub(crate) struct GEntry {
    /// Entry name (single path component, lossy UTF-8).
    pub name: String,
    /// Directory or file leaf.
    pub kind: GKind,
    /// gardenbed mode for file leaves (gitlink pointers included).
    pub mode: Mode,
    /// Blob id for files, tree id for directories.
    pub oid: GitOid,
}

/// Kind of a git tree entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GKind {
    /// A subdirectory (git mode `40000`).
    Dir,
    /// A leaf (blob or submodule pointer).
    File,
}

/// Parses the raw bytes of a git tree object.
pub(crate) fn parse_tree(data: &[u8]) -> Result<Vec<GEntry>, String> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let sp = data[pos..]
            .iter()
            .position(|b| *b == b' ')
            .ok_or("tree entry missing mode separator")?
            + pos;
        let mode = std::str::from_utf8(&data[pos..sp])
            .map_err(|_| "tree mode is not UTF-8")?
            .to_owned();
        let nul = data[sp + 1..]
            .iter()
            .position(|b| *b == 0)
            .ok_or("tree entry name not NUL-terminated")?
            + sp
            + 1;
        if nul + 21 > data.len() {
            return Err("tree entry id is truncated".into());
        }
        let name = String::from_utf8_lossy(&data[sp + 1..nul]).into_owned();
        let mut raw = [0u8; 20];
        raw.copy_from_slice(&data[nul + 1..nul + 1 + 20]);
        let oid = GitOid::from_bytes(raw);
        let (kind, file_mode) =
            map_mode(&mode).ok_or_else(|| format!("unsupported tree entry mode {mode:?}"))?;
        match kind {
            GKind::Dir => out.push(GEntry {
                name,
                kind: GKind::Dir,
                mode: Mode::Regular,
                oid,
            }),
            GKind::File => out.push(GEntry {
                name,
                kind: GKind::File,
                mode: file_mode.expect("file modes carry a mode"),
                oid,
            }),
        }
        pos = nul + 21;
    }
    Ok(out)
}

/// Maps a git mode string to `(kind, file mode)`. `file mode` is `None` for
/// directories.
pub(crate) fn map_mode(mode: &str) -> Option<(GKind, Option<Mode>)> {
    match mode {
        "400" | "40000" | "040000" => Some((GKind::Dir, None)),
        "100644" => Some((GKind::File, Some(Mode::Regular))),
        "100755" => Some((GKind::File, Some(Mode::Exec))),
        "120000" => Some((GKind::File, Some(Mode::Symlink))),
        "160000" => Some((GKind::File, Some(Mode::Gitlink))),
        _ => {
            // Non-canonical file modes (e.g. `100664` written by very old
            // git): any 100xxxx mode is a blob; the exec bit decides.
            let m = i32::from_str_radix(mode, 8).ok()?;
            if m & 0o170_000 == 0o100_000 {
                let mode = if m & 0o111 != 0 {
                    Mode::Exec
                } else {
                    Mode::Regular
                };
                Some((GKind::File, Some(mode)))
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GKind, map_mode, parse_tree};
    use gardenbed_model::Mode;

    fn oid(byte: u8) -> [u8; 20] {
        [byte; 20]
    }

    #[test]
    fn parses_entries() {
        let mut data = Vec::new();
        data.extend_from_slice(b"100644 file.txt\0");
        data.extend_from_slice(&oid(1));
        data.extend_from_slice(b"40000 src\0");
        data.extend_from_slice(&oid(2));
        data.extend_from_slice(b"100755 run.sh\0");
        data.extend_from_slice(&oid(3));

        let entries = parse_tree(&data).expect("parses");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "file.txt");
        assert_eq!(entries[0].kind, GKind::File);
        assert_eq!(entries[0].mode, Mode::Regular);
        assert_eq!(entries[0].oid.as_bytes(), &oid(1));
        assert_eq!(entries[1].name, "src");
        assert_eq!(entries[1].kind, GKind::Dir);
        assert_eq!(entries[2].mode, Mode::Exec);
    }

    #[test]
    fn modes() {
        assert_eq!(map_mode("40000"), Some((GKind::Dir, None)));
        assert_eq!(map_mode("400"), Some((GKind::Dir, None)));
        assert_eq!(map_mode("120000"), Some((GKind::File, Some(Mode::Symlink))));
        assert_eq!(map_mode("160000"), Some((GKind::File, Some(Mode::Gitlink))));
        // Ancient non-canonical mode: exec bit decides.
        assert_eq!(map_mode("100664"), Some((GKind::File, Some(Mode::Regular))));
        assert_eq!(map_mode("100777"), Some((GKind::File, Some(Mode::Exec))));
        assert_eq!(map_mode("42"), None);
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_tree(b"100644 name\0short").is_err());
        assert!(parse_tree(b"100644 nonulterminated").is_err());
        assert!(parse_tree(b"999999 x\0aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_err());
    }
}
