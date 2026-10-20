//! Git object ids (SHA-1) and hex helpers.
//!
//! Git object ids are 20 raw bytes; every boundary we cross (gitoxide,
//! `git diff-tree` output, the metadata's git-SHA mapping column) speaks
//! lowercase 40-character hex. This module is the single place that parses
//! and renders them, so nothing else can accept a malformed id by accident.

use std::fmt;

/// A 20-byte git object id.
///
/// Public for the fuzz target that builds plausible commit ids
/// (`fuzz_difftree_parse`); not part of the stabilized surface.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitOid([u8; 20]);

impl GitOid {
    /// Wraps raw bytes.
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        GitOid(bytes)
    }

    /// The raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Parses exactly-40 lowercase/uppercase hex characters; anything else
    /// is rejected (zero-assumption parsing of git output).
    pub fn from_hex(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() != 40 {
            return None;
        }
        let mut out = [0u8; 20];
        for (i, byte) in out.iter_mut().enumerate() {
            let hi = hex_val(b[2 * i])?;
            let lo = hex_val(b[2 * i + 1])?;
            *byte = (hi << 4) | lo;
        }
        Some(GitOid(out))
    }

    /// Lowercase hex, 40 characters.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(40);
        for b in self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

impl fmt::Debug for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl TryFrom<&[u8]> for GitOid {
    type Error = ();

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        let arr: [u8; 20] = value.try_into().map_err(|_| ())?;
        Ok(GitOid(arr))
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::GitOid;

    #[test]
    fn hex_roundtrip() {
        let hex = "0123456789abcdef0123456789abcdef01234567";
        let oid = GitOid::from_hex(hex).expect("valid hex");
        assert_eq!(oid.to_hex(), hex);
        assert_eq!(format!("{oid:?}"), hex);
    }

    #[test]
    fn rejects_bad_hex() {
        assert!(GitOid::from_hex("").is_none());
        assert!(GitOid::from_hex("00").is_none());
        assert!(GitOid::from_hex(&"0".repeat(39)).is_none());
        assert!(GitOid::from_hex(&"g".repeat(40)).is_none());
        assert!(GitOid::from_hex(&"0".repeat(41)).is_none());
    }
}
