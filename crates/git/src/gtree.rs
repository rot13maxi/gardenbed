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

/// One entry of a git tree, preserving the original bytes: the exact mode
/// string git wrote (e.g. `100664`, never canonicalized) and the raw name
/// bytes (which may not be UTF-8). Raw fidelity (raw-tree capture) works
/// on this view; the model view stays [`GEntry`].
#[derive(Clone, Debug)]
pub(crate) struct RawEntry {
    /// The mode string exactly as git wrote it (e.g. `100664`).
    pub mode: String,
    /// The name bytes exactly as git wrote them (any non-NUL bytes).
    pub name: Vec<u8>,
    /// The entry id (blob, tree, or commit for gitlinks).
    pub oid: GitOid,
    /// Directory or file leaf (derived from the mode's type bits).
    pub kind: GKind,
}

/// Parses a git tree into [`RawEntry`]s — the byte-preserving view.
///
/// The mode string is kept verbatim (`100664` stays `100664`) and name
/// bytes are never decoded, so a tree rendered back from these entries
/// reproduces the original object bytes exactly — the capture predicate
/// (`reconstruct_raw`) and the verbatim export path both work on this
/// view. Malformed input errors identically to [`parse_tree`].
pub(crate) fn parse_tree_raw(data: &[u8]) -> Result<Vec<RawEntry>, String> {
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
        let name = data[sp + 1..nul].to_vec();
        let mut raw = [0u8; 20];
        raw.copy_from_slice(&data[nul + 1..nul + 1 + 20]);
        let oid = GitOid::from_bytes(raw);
        // Same classification as `map_mode` (type bits decide the kind);
        // unsupported modes are the loud parse errors they always were.
        let (kind, _) =
            map_mode(&mode).ok_or_else(|| format!("unsupported tree entry mode {mode:?}"))?;
        out.push(RawEntry {
            mode,
            name,
            oid,
            kind,
        });
        pos = nul + 21;
    }
    Ok(out)
}

/// Renders the tree body the **canonical export** would produce from
/// these entries: canonical mode strings (per `git_mode`) and lossy-decoded
/// names — the model's view. Byte-equality with the original tree bytes
/// proves the model reproduces the tree exactly; any difference (mode
/// bytes, non-UTF-8 names) is the raw-tree capture predicate firing.
///
/// `blob_shas` maps each file entry's git oid to the SHA the exported
/// pack carries for it (blobs and gitlinks alike — gitlink pointers are
/// the upstream SHA-1 unchanged). `tree_shas` does the same for
/// directory entries; both come from the caller's memo. Owned `[u8; 20]`
/// values keep the render free of borrowed-lifetime plumbing.
pub(crate) fn render_canonical(
    entries: &[RawEntry],
    blob_shas: impl Fn(&RawEntry) -> Option<[u8; 20]>,
    tree_shas: impl Fn(&RawEntry) -> Option<[u8; 20]>,
) -> Option<Vec<u8>> {
    let mut sorted: Vec<&RawEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        let mut ka = a.name.clone();
        let mut kb = b.name.clone();
        if a.kind == GKind::Dir {
            ka.push(b'/');
        }
        if b.kind == GKind::Dir {
            kb.push(b'/');
        }
        ka.cmp(&kb)
    });
    let mut out = Vec::new();
    for e in sorted {
        let mode = crate::export::canonical_git_mode(e.kind, e.mode.as_str())?;
        // A name the model cannot hold (non-UTF-8) makes the canonical
        // render impossible: the caller treats `None` as a capture.
        let name = std::str::from_utf8(&e.name).ok()?;
        out.extend_from_slice(mode.as_bytes());
        out.push(b' ');
        out.extend_from_slice(name.as_bytes());
        out.push(b'\0');
        let sha: [u8; 20] = match e.kind {
            GKind::File => blob_shas(e)?,
            GKind::Dir => tree_shas(e)?,
        };
        out.extend_from_slice(&sha);
    }
    Some(out)
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
            // git): any 100xxx mode is a blob, and the OWNER exec bit
            // alone decides — git itself normalizes 100654/100645 to
            // 100644 and only 100744 to 100755 (verified against git
            // 2.43: `git add` of such trees reports 100644). Group and
            // other exec bits are dropped, exactly like git.
            let m = i32::from_str_radix(mode, 8).ok()?;
            if m & 0o170_000 == 0o100_000 {
                let mode = if m & 0o100 != 0 {
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
        // Ancient non-canonical modes: the OWNER exec bit decides
        // (git normalizes 100654/100645 → 100644, 100744 → 100755;
        // group/other bits alone never make a file executable).
        assert_eq!(map_mode("100664"), Some((GKind::File, Some(Mode::Regular))));
        assert_eq!(map_mode("100654"), Some((GKind::File, Some(Mode::Regular))));
        assert_eq!(map_mode("100645"), Some((GKind::File, Some(Mode::Regular))));
        assert_eq!(map_mode("100744"), Some((GKind::File, Some(Mode::Exec))));
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
