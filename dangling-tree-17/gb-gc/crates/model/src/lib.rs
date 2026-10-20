//! gardenbed-model — the core data model for gardenbed.
//!
//! The unit of work is the changeset ("seed"): it records only the files
//! changed relative to a changeset's first parent, so commit cost scales
//! with change size, never repo size. Everything is content-addressed.
//!
//! Canonical encoding lives in [`codec`]; the normative byte-level spec is
//! `docs/canonical-encoding.md`.

pub mod codec;

use codec::CodecError;
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;

/// A content address: SHA-256 over canonical bytes (changesets, trees) or
/// over raw content (blobs).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Id([u8; 32]);

impl Id {
    /// Wraps raw digest bytes.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Id(bytes)
    }

    /// The raw digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Lowercase hex encoding, 64 characters.
    ///
    /// Hot path (every object's `path_for`), so the digits come from one
    /// table lookup per byte into a stack buffer — no per-byte formatting.
    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = [0u8; 64];
        for (i, b) in self.0.iter().enumerate() {
            out[i * 2] = HEX[(b >> 4) as usize];
            out[i * 2 + 1] = HEX[(b & 0x0f) as usize];
        }
        // Every byte of `out` is an ASCII hex digit, so this is infallible.
        std::str::from_utf8(&out)
            .expect("hex digits are ASCII")
            .to_owned()
    }

    /// Short display form (first 16 hex characters).
    pub fn short(&self) -> String {
        self.to_hex()[..16].to_owned()
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({})", self.to_hex())
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.short())
    }
}

impl FromStr for Id {
    type Err = CodecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let b = s.as_bytes();
        if b.len() != 64 {
            return Err(CodecError::Decode(
                "id hex must be exactly 64 characters".into(),
            ));
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            let hi = hex_val(b[2 * i])?;
            let lo = hex_val(b[2 * i + 1])?;
            *byte = (hi << 4) | lo;
        }
        Ok(Id(out))
    }
}

fn hex_val(c: u8) -> Result<u8, CodecError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(CodecError::Decode(format!(
            "invalid hex character {:?}",
            c as char
        ))),
    }
}

/// How a single path changed relative to the first parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Path created.
    Add = 0,
    /// Path's content or mode changed.
    Modify = 1,
    /// Path removed.
    Delete = 2,
    /// Path moved from `src_path`.
    Rename = 3,
}

impl Op {
    pub fn to_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Op::Add),
            1 => Some(Op::Modify),
            2 => Some(Op::Delete),
            3 => Some(Op::Rename),
            _ => None,
        }
    }
}

/// File mode, mirroring git's model. `Gitlink` records a submodule pointer;
/// its blob carries the upstream git SHA-1 zero-padded to 32 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Regular = 0,
    Exec = 1,
    Symlink = 2,
    Gitlink = 3,
}

impl Mode {
    pub fn to_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Mode::Regular),
            1 => Some(Mode::Exec),
            2 => Some(Mode::Symlink),
            3 => Some(Mode::Gitlink),
            _ => None,
        }
    }
}

/// One changed path in a changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub op: Op,
    /// Content address of the new blob; `None` only for `Delete` (and pure
    /// renames, which keep the old content implicitly).
    pub blob: Option<Id>,
    /// Previous path — present exactly when `op == Rename`.
    pub src_path: Option<String>,
    pub mode: Mode,
}

/// Who made a change, with git-native timestamps (epoch seconds + tz offset
/// in minutes). Both fields are preserved verbatim so git round-trips can
/// reproduce identical git SHAs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
    pub time_secs: u64,
    pub tz_min: i32,
}

/// The unit of work: a seed. Records only what changed vs. the first parent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changeset {
    /// Empty ⇒ root changeset; otherwise ≥ 1. The **first** parent is the
    /// delta base and the mainline for blame.
    pub parents: Vec<Id>,
    /// Canonical order: strictly ascending by path bytes.
    pub changes: Vec<Change>,
    pub message: String,
    pub author: Identity,
    pub committer: Identity,
    pub signature: Option<Vec<u8>>,
}

impl Changeset {
    /// Canonical bytes (spec §3). Errors if the changeset is not semantically
    /// valid; note the canonical form sorts `changes` internally.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, CodecError> {
        codec::encode_changeset(self)
    }

    /// SHA-256 over the canonical bytes.
    pub fn id(&self) -> Result<Id, CodecError> {
        Ok(Id::from_bytes(
            Sha256::digest(self.canonical_bytes()?).into(),
        ))
    }

    /// Decodes canonical bytes. Accepts only canonical payloads.
    pub fn decode(bytes: &[u8]) -> Result<Changeset, CodecError> {
        codec::decode_changeset(bytes)
    }
}

/// Kind of a tree entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeKind {
    File = 0,
    Dir = 1,
}

impl TreeKind {
    pub fn to_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(TreeKind::File),
            1 => Some(TreeKind::Dir),
            _ => None,
        }
    }
}

/// One entry of a (derived, cached) tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    /// Single path component; no `/`.
    pub name: String,
    pub kind: TreeKind,
    pub mode: Mode,
    /// Present exactly when `kind == File`.
    pub blob: Option<Id>,
    /// Present exactly when `kind == Dir`.
    pub tree: Option<Id>,
}

/// The derived, cached hierarchical view. The changeset is authoritative;
/// trees are recomputable from parent tree + changes.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Tree {
    /// Canonical order: strictly ascending by name bytes.
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    /// Canonical bytes (spec §4). Sorts entries internally.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, CodecError> {
        codec::encode_tree(self)
    }

    /// SHA-256 over the canonical bytes.
    pub fn id(&self) -> Result<Id, CodecError> {
        Ok(Id::from_bytes(
            Sha256::digest(self.canonical_bytes()?).into(),
        ))
    }

    /// Decodes canonical bytes. Accepts only canonical payloads.
    pub fn decode(bytes: &[u8]) -> Result<Tree, CodecError> {
        codec::decode_tree(bytes)
    }
}

/// Identity of a blob: SHA-256 of raw, uncompressed content.
pub fn blob_id(content: &[u8]) -> Id {
    Id::from_bytes(Sha256::digest(content).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_id_is_sha256_of_content() {
        assert_eq!(
            blob_id(b"hello").to_hex(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn id_hex_roundtrip() {
        let id = blob_id(b"gardenbed");
        assert_eq!(Id::from_str(&id.to_hex()).unwrap(), id);
        assert!(Id::from_str(&id.to_hex()[..63]).is_err());
        assert!(Id::from_str("zz").is_err());
    }
}
