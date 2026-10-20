//! Parsing `<cs-or-ref>` arguments (`goto`, `file --cs`, `log --cs`,
//! `diff`, `graft`).
//!
//! The rule is deliberately simple: a token that is exactly 64 hex
//! characters is a changeset id (normalized to lowercase); anything else is
//! a ref name. Ref resolution against the server is a later wave's job —
//! for now parsing validates the argument shape so bad input fails locally,
//! before any RPC attempt.

use gardenbed_model::Id;
use std::str::FromStr;

use crate::error::BedError;

/// A changeset id (full 64-hex) or a ref name, as typed on the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CsOrRef {
    /// A full content address.
    Id(Id),
    /// A named ref (`main`, `heirloom/2026-10`, …), resolved server-side.
    Ref(String),
}

impl CsOrRef {
    /// Parses a `<cs-or-ref>` argument.
    ///
    /// * 64 hex characters (any case) → [`CsOrRef::Id`]; anything
    ///   64-characters-long that is *not* valid hex is an error, not a ref —
    ///   a typo'd id should not silently become a ref lookup.
    /// * anything else → [`CsOrRef::Ref`], after rejecting empty names,
    ///   leading dashes, and whitespace/control characters.
    pub fn parse(raw: &str) -> Result<Self, BedError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(revspec_err(raw, "it is empty"));
        }
        if trimmed.len() == 64 {
            let id = Id::from_str(trimmed).map_err(|e| {
                revspec_err(
                    raw,
                    &format!("64 characters suggests a changeset id, but it is not valid hex: {e}"),
                )
            })?;
            return Ok(CsOrRef::Id(id));
        }
        validate_ref_name(raw, trimmed)?;
        Ok(CsOrRef::Ref(trimmed.to_owned()))
    }

    /// The id, if this argument was a full content address.
    pub fn as_id(&self) -> Option<Id> {
        match self {
            CsOrRef::Id(id) => Some(*id),
            CsOrRef::Ref(_) => None,
        }
    }

    /// The ref name, if this argument was a ref.
    pub fn as_ref_name(&self) -> Option<&str> {
        match self {
            CsOrRef::Id(_) => None,
            CsOrRef::Ref(name) => Some(name),
        }
    }
}

fn revspec_err(raw: &str, reason: &str) -> BedError {
    BedError::Revspec {
        raw: raw.to_owned(),
        reason: reason.to_owned(),
    }
}

fn validate_ref_name(raw: &str, name: &str) -> Result<(), BedError> {
    if name.starts_with('-') {
        return Err(revspec_err(raw, "a ref name cannot start with '-'"));
    }
    if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(revspec_err(
            raw,
            "a ref name cannot contain whitespace or control characters",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn full_hex_parses_as_id_and_normalizes() {
        let upper = HEX.to_uppercase();
        match CsOrRef::parse(&upper).unwrap() {
            CsOrRef::Id(id) => assert_eq!(id.to_hex(), HEX),
            other => panic!("expected Id, got {other:?}"),
        }
    }

    #[test]
    fn names_parse_as_refs() {
        assert_eq!(CsOrRef::parse("main").unwrap(), CsOrRef::Ref("main".into()));
        assert_eq!(
            CsOrRef::parse("heirloom/2026-10").unwrap(),
            CsOrRef::Ref("heirloom/2026-10".into())
        );
        // Short hex-ish strings are refs, not ids.
        assert_eq!(
            CsOrRef::parse("deadbeef").unwrap(),
            CsOrRef::Ref("deadbeef".into())
        );
    }

    #[test]
    fn sixty_four_chars_must_be_valid_hex() {
        let bad: String = "z".repeat(64);
        assert!(matches!(
            CsOrRef::parse(&bad),
            Err(BedError::Revspec { .. })
        ));
    }

    #[test]
    fn rejects_empty_and_unusable_ref_names() {
        assert!(matches!(CsOrRef::parse(""), Err(BedError::Revspec { .. })));
        assert!(matches!(
            CsOrRef::parse("  "),
            Err(BedError::Revspec { .. })
        ));
        assert!(matches!(
            CsOrRef::parse("-main"),
            Err(BedError::Revspec { .. })
        ));
        assert!(matches!(
            CsOrRef::parse("two words"),
            Err(BedError::Revspec { .. })
        ));
    }

    #[test]
    fn accessors() {
        let id = CsOrRef::parse(HEX).unwrap();
        assert!(id.as_id().is_some());
        assert!(id.as_ref_name().is_none());
        let r = CsOrRef::parse("main").unwrap();
        assert_eq!(r.as_ref_name(), Some("main"));
        assert!(r.as_id().is_none());
    }
}
