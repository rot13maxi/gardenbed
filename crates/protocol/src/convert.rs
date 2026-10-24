//! Conversions between the wire types ([`crate::proto`]) and
//! [`gardenbed_model`].
//!
//! Conventions:
//!
//! * **model → wire** is [`TryFrom`] and validates the model value against
//!   the codec's semantic rules first ([`gardenbed_model::codec`]): a value
//!   that cannot be canonically encoded has no identity (its `id()` would
//!   fail) and must never reach the wire.
//! * **wire → model** is [`TryFrom`] too. Wire data is untrusted, so
//!   malformed content — ids that are not 32 bytes, unknown op/mode
//!   discriminants (proto3 enums are open), tree entries with no id, missing
//!   required fields, semantically inconsistent changesets — maps to
//!   [`ConversionError`].
//! * Identity conversion is infallible, so it is plain [`From`] both ways.
//!
//! Canonical *order* (strictly ascending paths/names) is deliberately **not**
//! enforced here: the wire preserves the sender's order and the canonical
//! bytes (which define ids) are sorted by the codec. Everything else — the
//! op/blob/src_path/mode table, path rules, duplicate checks — is delegated
//! to the codec rather than duplicated.

use std::str::FromStr;

use gardenbed_model as model;
use gardenbed_model::codec::{CodecError, validate_changeset, validate_path, validate_tree};

use crate::pb;

/// Errors when mapping between the wire format and the model.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConversionError {
    /// A content address was not exactly 32 bytes.
    #[error("id must be exactly 32 bytes, got {0}")]
    IdLength(usize),
    /// An `inline_blobs` map key is not a 64-hex-character blob id.
    #[error("invalid blob-map key {0:?}: must be a 64-character hex blob id")]
    BlobKeyHex(String),
    /// An op discriminant outside the canonical encoding.
    #[error("invalid op discriminant {0}")]
    Op(i32),
    /// A mode discriminant outside the canonical encoding.
    #[error("invalid mode discriminant {0}")]
    Mode(i32),
    /// A tree entry that cannot be mirrored (wrong/missing id, bad name).
    #[error("invalid tree entry {name:?}: {problem}")]
    TreeEntry { name: String, problem: &'static str },
    /// A required field is absent on the wire.
    #[error("required field missing on the wire: {0}")]
    Missing(&'static str),
    /// A value violates the canonical encoding's semantic rules (paths,
    /// op/field table, duplicates, …).
    #[error(transparent)]
    Invalid(#[from] CodecError),
}

// ------------------------------------------------------------------- ids

/// A content address as exactly 32 raw wire bytes.
pub fn id_to_bytes(id: model::Id) -> Vec<u8> {
    id.as_bytes().to_vec()
}

/// Parses 32 raw wire bytes into a content address.
pub fn id_from_bytes(bytes: &[u8]) -> Result<model::Id, ConversionError> {
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| ConversionError::IdLength(bytes.len()))?;
    Ok(model::Id::from_bytes(arr))
}

/// A content address as a 64-character lowercase hex string — the canonical
/// shape of `CreateChangesetReq::inline_blobs` map keys.
pub fn id_to_blob_key(id: model::Id) -> String {
    id.to_hex()
}

/// Parses an `inline_blobs` map key (64 hex characters) into a content
/// address. Uppercase hex is accepted (parsers accept it, never emit it).
pub fn blob_key_to_id(key: &str) -> Result<model::Id, ConversionError> {
    model::Id::from_str(key).map_err(|_| ConversionError::BlobKeyHex(key.to_owned()))
}

// ------------------------------------------------------------- op / mode

/// Maps a raw wire op discriminant onto the model. Proto3 enums are open —
/// the wire may carry any `i32`, so the range check lives here (via the
/// model's `from_u8`).
pub fn op_from_wire(v: i32) -> Result<model::Op, ConversionError> {
    u8::try_from(v)
        .ok()
        .and_then(model::Op::from_u8)
        .ok_or(ConversionError::Op(v))
}

/// Maps a raw wire mode discriminant onto the model.
pub fn mode_from_wire(v: i32) -> Result<model::Mode, ConversionError> {
    u8::try_from(v)
        .ok()
        .and_then(model::Mode::from_u8)
        .ok_or(ConversionError::Mode(v))
}

impl From<model::Op> for pb::Op {
    fn from(op: model::Op) -> Self {
        match op {
            model::Op::Add => pb::Op::Add,
            model::Op::Modify => pb::Op::Modify,
            model::Op::Delete => pb::Op::Delete,
            model::Op::Rename => pb::Op::Rename,
        }
    }
}

impl TryFrom<pb::Op> for model::Op {
    type Error = ConversionError;

    fn try_from(op: pb::Op) -> Result<Self, Self::Error> {
        op_from_wire(op as i32)
    }
}

impl From<model::Mode> for pb::Mode {
    fn from(mode: model::Mode) -> Self {
        match mode {
            model::Mode::Regular => pb::Mode::Regular,
            model::Mode::Exec => pb::Mode::Exec,
            model::Mode::Symlink => pb::Mode::Symlink,
            model::Mode::Gitlink => pb::Mode::Gitlink,
        }
    }
}

impl TryFrom<pb::Mode> for model::Mode {
    type Error = ConversionError;

    fn try_from(mode: pb::Mode) -> Result<Self, Self::Error> {
        mode_from_wire(mode as i32)
    }
}

// -------------------------------------------------------------- identity

impl From<&model::Identity> for pb::IdentityMsg {
    fn from(id: &model::Identity) -> Self {
        pb::IdentityMsg {
            name: id.name.clone(),
            email: id.email.clone(),
            time_secs: id.time_secs,
            tz_min: id.tz_min,
        }
    }
}

impl From<pb::IdentityMsg> for model::Identity {
    fn from(m: pb::IdentityMsg) -> Self {
        model::Identity {
            name: m.name,
            email: m.email,
            time_secs: m.time_secs,
            tz_min: m.tz_min,
        }
    }
}

// ---------------------------------------------------------------- change

impl TryFrom<&model::Change> for pb::ChangeMsg {
    type Error = ConversionError;

    fn try_from(c: &model::Change) -> Result<Self, Self::Error> {
        validate_path(&c.path)?;
        if let Some(src) = &c.src_path {
            validate_path(src)?;
        }
        Ok(pb::ChangeMsg {
            op: pb::Op::from(c.op) as i32,
            path: c.path.clone(),
            blob_id: c.blob.map(id_to_bytes),
            src_path: c.src_path.clone(),
            mode: pb::Mode::from(c.mode) as i32,
        })
    }
}

impl TryFrom<pb::ChangeMsg> for model::Change {
    type Error = ConversionError;

    fn try_from(m: pb::ChangeMsg) -> Result<Self, Self::Error> {
        validate_path(&m.path)?;
        let src_path = match m.src_path {
            Some(src) => {
                validate_path(&src)?;
                Some(src)
            }
            None => None,
        };
        Ok(model::Change {
            path: m.path,
            op: op_from_wire(m.op)?,
            blob: m.blob_id.as_deref().map(id_from_bytes).transpose()?,
            src_path,
            mode: mode_from_wire(m.mode)?,
        })
    }
}

// -------------------------------------------------------------- changeset

impl TryFrom<&model::Changeset> for pb::ChangesetMsg {
    type Error = ConversionError;

    fn try_from(cs: &model::Changeset) -> Result<Self, Self::Error> {
        // Semantically invalid model values have no wire form (their
        // canonical encoding, and therefore their id, would not exist).
        validate_changeset(cs, false)?;
        Ok(pb::ChangesetMsg {
            parents: cs.parents.iter().map(|p| id_to_bytes(*p)).collect(),
            changes: cs
                .changes
                .iter()
                .map(pb::ChangeMsg::try_from)
                .collect::<Result<Vec<_>, ConversionError>>()?,
            message: cs.message.clone(),
            // Message-typed fields are optional on the wire (proto3): the
            // model always has both, so conversion always populates them.
            author: Some(pb::IdentityMsg::from(&cs.author)),
            committer: Some(pb::IdentityMsg::from(&cs.committer)),
            signature: cs.signature.clone(),
        })
    }
}

impl TryFrom<pb::ChangesetMsg> for model::Changeset {
    type Error = ConversionError;

    fn try_from(m: pb::ChangesetMsg) -> Result<Self, Self::Error> {
        let mut parents = Vec::with_capacity(m.parents.len());
        for p in &m.parents {
            parents.push(id_from_bytes(p)?);
        }
        let mut changes = Vec::with_capacity(m.changes.len());
        for c in m.changes {
            changes.push(model::Change::try_from(c)?);
        }
        let author = model::Identity::from(
            m.author
                .ok_or(ConversionError::Missing("ChangesetMsg.author"))?,
        );
        let committer = model::Identity::from(
            m.committer
                .ok_or(ConversionError::Missing("ChangesetMsg.committer"))?,
        );
        let cs = model::Changeset {
            parents,
            changes,
            message: m.message,
            author,
            committer,
            signature: m.signature,
        };
        // The op/blob/src_path/mode table, path rules and duplicate check are
        // the codec's semantic validation; sortedness stays the codec's job.
        validate_changeset(&cs, false)?;
        Ok(cs)
    }
}

// ------------------------------------------------------------------ tree

fn validate_entry_name(name: &str) -> Result<(), ConversionError> {
    if name.is_empty() || name.contains('/') || name.bytes().any(|b| b == 0) {
        Err(ConversionError::TreeEntry {
            name: name.to_owned(),
            problem: "names are single components: non-empty, no '/', no NUL",
        })
    } else {
        Ok(())
    }
}

impl TryFrom<&model::TreeEntry> for pb::TreeEntry {
    type Error = ConversionError;

    fn try_from(e: &model::TreeEntry) -> Result<Self, Self::Error> {
        validate_entry_name(&e.name)?;
        let id = match e.kind {
            model::TreeKind::File => {
                if e.tree.is_some() {
                    return Err(ConversionError::TreeEntry {
                        name: e.name.clone(),
                        problem: "a file entry must not carry a tree id",
                    });
                }
                let blob = e.blob.ok_or_else(|| ConversionError::TreeEntry {
                    name: e.name.clone(),
                    problem: "file entries must carry a blob id",
                })?;
                pb::tree_entry::Id::BlobId(id_to_bytes(blob))
            }
            model::TreeKind::Dir => {
                if e.blob.is_some() {
                    return Err(ConversionError::TreeEntry {
                        name: e.name.clone(),
                        problem: "a dir entry must not carry a blob id",
                    });
                }
                let tree = e.tree.ok_or_else(|| ConversionError::TreeEntry {
                    name: e.name.clone(),
                    problem: "dir entries must carry a tree id",
                })?;
                pb::tree_entry::Id::TreeId(id_to_bytes(tree))
            }
        };
        Ok(pb::TreeEntry {
            name: e.name.clone(),
            mode: pb::Mode::from(e.mode) as i32,
            id: Some(id),
        })
    }
}

impl TryFrom<pb::TreeEntry> for model::TreeEntry {
    type Error = ConversionError;

    fn try_from(m: pb::TreeEntry) -> Result<Self, Self::Error> {
        validate_entry_name(&m.name)?;
        let (kind, blob, tree) = match m.id {
            Some(pb::tree_entry::Id::BlobId(b)) => {
                (model::TreeKind::File, Some(id_from_bytes(&b)?), None)
            }
            Some(pb::tree_entry::Id::TreeId(t)) => {
                (model::TreeKind::Dir, None, Some(id_from_bytes(&t)?))
            }
            None => {
                return Err(ConversionError::TreeEntry {
                    name: m.name,
                    problem: "carries neither blob_id nor tree_id",
                });
            }
        };
        Ok(model::TreeEntry {
            name: m.name,
            kind,
            mode: mode_from_wire(m.mode)?,
            blob,
            tree,
        })
    }
}

impl TryFrom<&model::Tree> for pb::TreeResp {
    type Error = ConversionError;

    fn try_from(tree: &model::Tree) -> Result<Self, Self::Error> {
        validate_tree(tree, false)?;
        let entries = tree
            .entries
            .iter()
            .map(pb::TreeEntry::try_from)
            .collect::<Result<Vec<_>, ConversionError>>()?;
        Ok(pb::TreeResp {
            entries,
            // The proof is stamped by the serving server, not derived
            // from the model: an initializer from a model tree has no
            // incarnation to carry (the daemon re-emits cached trees
            // without one — its own cache is already scope-proven).
            incarnation: Vec::new(),
        })
    }
}

impl TryFrom<pb::TreeResp> for model::Tree {
    type Error = ConversionError;

    fn try_from(resp: pb::TreeResp) -> Result<Self, Self::Error> {
        let mut entries = Vec::with_capacity(resp.entries.len());
        for e in resp.entries {
            entries.push(model::TreeEntry::try_from(e)?);
        }
        let tree = model::Tree { entries };
        validate_tree(&tree, false)?;
        Ok(tree)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_and_mode_mirror_wire_discriminants() {
        for (m, v) in [
            (model::Op::Add, pb::Op::Add),
            (model::Op::Modify, pb::Op::Modify),
            (model::Op::Delete, pb::Op::Delete),
            (model::Op::Rename, pb::Op::Rename),
        ] {
            assert_eq!(pb::Op::from(m) as i32, m.to_u8() as i32);
            assert_eq!(model::Op::try_from(v).unwrap(), m);
        }
        assert!(matches!(op_from_wire(9), Err(ConversionError::Op(9))));
        assert!(matches!(op_from_wire(-1), Err(ConversionError::Op(-1))));

        for (m, w) in [
            (model::Mode::Regular, pb::Mode::Regular),
            (model::Mode::Exec, pb::Mode::Exec),
            (model::Mode::Symlink, pb::Mode::Symlink),
            (model::Mode::Gitlink, pb::Mode::Gitlink),
        ] {
            assert_eq!(pb::Mode::from(m) as i32, m.to_u8() as i32);
            assert_eq!(model::Mode::try_from(w).unwrap(), m);
        }
        assert!(matches!(mode_from_wire(9), Err(ConversionError::Mode(9))));
    }

    #[test]
    fn id_helpers_reject_wrong_lengths() {
        let id = model::blob_id(b"hello");
        assert_eq!(id_from_bytes(&id_to_bytes(id)).unwrap(), id);
        assert!(matches!(
            id_from_bytes(&[0u8; 31]),
            Err(ConversionError::IdLength(31))
        ));
        assert!(matches!(
            id_from_bytes(&[0u8; 33]),
            Err(ConversionError::IdLength(33))
        ));
    }
}
