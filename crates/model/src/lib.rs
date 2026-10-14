//! Core data model: changesets, blobs, trees, refs.
//!
//! Everything is content-addressed. The canonical byte serialization is a
//! spec-level decision (must be byte-exact across implementations) — see
//! DESIGN.md.

/// A content address: SHA-256 over canonical bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id(pub [u8; 32]);

/// How a single path changed relative to the first parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Add,
    Modify,
    Delete,
    Rename,
}

/// One changed path in a changeset.
#[derive(Clone, Debug)]
pub struct Change {
    pub path: String,
    pub op: Op,
    /// Content address of the new blob; `None` for `Delete`.
    pub blob: Option<Id>,
    /// Previous path for `Rename` (and for cuttings detection).
    pub src_path: Option<String>,
}

/// The unit of work: a seed. Records only what changed vs. the first parent,
/// so commit cost scales with change size, never repo size.
#[derive(Clone, Debug)]
pub struct Changeset {
    pub parents: Vec<Id>,
    pub changes: Vec<Change>,
    pub message: String,
    // TODO: author, committer, timestamps, signature, file mode.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_changeset_has_no_parents() {
        let seed = Changeset {
            parents: vec![],
            changes: vec![],
            message: "initial".into(),
        };
        assert!(seed.parents.is_empty());
    }
}
