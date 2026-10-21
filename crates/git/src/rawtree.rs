//! Raw-tree capture (raw-tree fidelity): preserve the original git tree
//! object bytes for trees the canonical model cannot reproduce
//! byte-exactly, keyed by the ORIGINAL git tree SHA with the canonical
//! model tree id as an association.
//!
//! # The capture predicate
//!
//! For a git tree object, the canonical export renders a body from
//! [`crate::gtree::render_canonical`] — original entry ids (blob SHAs and
//! gitlink pointers are identities the pack preserves exactly; subtrees
//! recurse), canonical mode strings, lossy-decoded names. Byte-equality
//! between that render and the original tree bytes proves the canonical
//! path reproduces the tree exactly (no row). Any difference — a
//! pre-canonical mode (`100664`), a non-UTF-8 name, a `040000` directory
//! spelling — fires the predicate and the original bytes are captured
//! verbatim, so the export can emit them SHA-exact.
//!
//! The walk memoizes per git tree SHA (subtrees shared across commits are
//! decided once) and only descends into subtrees whose rendered bytes
//! differ — a mode fixup in one directory never re-reads the rest of the
//! tree. The model-tree association for a captured git tree is resolved
//! by walking the DERIVED tree of the changeset being imported along the
//! same path (the derived tree is the model's truth; the association is
//! what lets the export seed a canonical render with raw bytes).
//!
//! # Lossy sibling-name collisions (loud refusal)
//!
//! Non-UTF-8 names decode lossily in the model (`a\x80b` and `a\x81b`
//! both become `a\u{FFFD}b`). Two SIBLINGS in one git tree whose lossy
//! decodings collide cannot be represented in one model tree — the
//! canonical path would silently conflate two distinct files. The capture
//! walk detects the collision (two distinct raw names mapping to one
//! model name in one directory) and refuses the import loudly. The model
//! stays honest: a repository that cannot be queried unambiguously is
//! never ingested with a silent guess.
//!
//! # Additivity
//!
//! Rows are written through [`MetadataStore::put_raw_tree`] BEFORE the
//! changeset's own ingest transaction — the same shape as blob objects.
//! A crash between the put and the ingest leaves an orphan row, harmless
//! by construction: the key is the git SHA of bytes the importer read
//! from git, and a row is consumed only when a captured commit's `tree`
//! header names it. gc treats the table like `git_raw_commit` (rows are
//! retained; see the gc mark/sweep — changeset and Git-mapping rows are
//! never collected, and raw-tree rows share that class).

use std::collections::HashMap;

use gardenbed_store::MetadataStore;

use crate::error::ImportError;
use crate::gtree::{GKind, parse_tree_raw, render_canonical};
use crate::oid::GitOid;

/// Safety cap on captured raw-tree rows per commit: the walk is bounded
/// by the number of DISTINCT divergent subtrees, which for real corpora
/// (mode fixups touching a handful of paths) is tiny; the cap only ever
/// fires on adversarial input, where a clean error beats unbounded
/// writes.
const RAW_TREE_ROW_CAP: usize = 100_000;

/// Safety cap on tree-nesting depth, mirroring the importer's guard.
const MAX_RAW_TREE_DEPTH: usize = 4096;

/// One decision per git tree SHA: captured (with the model association)
/// or byte-exact (no row).
#[derive(Default)]
pub(crate) struct RawTreeCapture {
    /// git tree SHA → `Some(model tree id association)` when captured,
    /// `None` when decided byte-exact (no row). The walk memoizes per
    /// git tree SHA, so subtrees shared across commits are decided once.
    memo: HashMap<GitOid, Option<Option<gardenbed_model::Id>>>,
}

impl RawTreeCapture {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Captures divergent raw trees for the commit's root tree, writing
    /// rows through `metadata` (additive, pre-transaction).
    ///
    /// `derived_root` is the changeset's derived tree (the model truth
    /// the association resolves against). Returns the number of raw-tree
    /// rows captured for this commit's tree closure — nonzero exactly
    /// when the root diverges, in which case the caller forces
    /// raw-commit capture so the export reads the original root SHA from
    /// the commit's `tree` header (a commit whose root tree diverges must
    /// carry its original bytes, even when every header reconstructs).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn capture_commit_tree(
        &mut self,
        repo: &gix::Repository,
        engine: &gardenbed_derive::Engine,
        root: GitOid,
        derived_root: Option<gardenbed_model::Id>,
        metadata: &dyn MetadataStore,
        sha_hex: &str,
    ) -> Result<usize, ImportError> {
        let mut rows = 0usize;
        self.capture_subtree(repo, engine, root, derived_root, metadata, sha_hex, 0, &mut rows)?;
        Ok(rows)
    }

    /// The recursive worker: decides one subtree, capturing when divergent.
    ///
    /// Returns whether the subtree diverged (any captured row inside it).
    #[allow(clippy::too_many_arguments)]
    fn capture_subtree(
        &mut self,
        repo: &gix::Repository,
        engine: &gardenbed_derive::Engine,
        tree: GitOid,
        model_tree: Option<gardenbed_model::Id>,
        metadata: &dyn MetadataStore,
        sha_hex: &str,
        depth: usize,
        rows: &mut usize,
    ) -> Result<bool, ImportError> {
        if depth > MAX_RAW_TREE_DEPTH {
            return Err(ImportError::Object {
                oid: sha_hex.to_owned(),
                detail: "tree nesting exceeds the maximum supported depth".into(),
            });
        }
        if let Some(decided) = self.memo.get(&tree) {
            // Already decided for this git tree SHA. A previously captured
            // association is reusable only when the model tree matches;
            // distinct model trees sharing one git tree SHA cannot happen
            // (same bytes → same normalized model tree), so the memo is
            // sound.
            return Ok(decided.is_some());
        }
        if *rows >= RAW_TREE_ROW_CAP {
            return Err(ImportError::Other(format!(
                "commit {sha_hex}: raw-tree capture exceeded the {RAW_TREE_ROW_CAP}-row cap"
            )));
        }

        // Read the original tree object bytes verbatim.
        let obj = repo
            .find_object(crate::graph::gix_id(tree))
            .map_err(|e| ImportError::Object {
                oid: sha_hex.to_owned(),
                detail: format!("cannot read tree {}: {e}", tree.to_hex()),
            })?;
        if obj.kind != gix::object::Kind::Tree {
            return Err(ImportError::Object {
                oid: sha_hex.to_owned(),
                detail: format!(
                    "expected a tree object for {}, found {:?}",
                    tree.to_hex(),
                    obj.kind
                ),
            });
        }
        let entries = parse_tree_raw(&obj.data).map_err(|detail| ImportError::Object {
            oid: tree.to_hex(),
            detail,
        })?;

        // Lossy sibling-name collision check (loud refusal): two distinct
        // raw names in one directory whose lossy decodings collide cannot
        // be one model tree.
        let mut lossy_names: HashMap<String, usize> = HashMap::new();
        for e in &entries {
            let lossy = String::from_utf8_lossy(&e.name).into_owned();
            *lossy_names.entry(lossy).or_default() += 1;
        }
        if lossy_names.values().any(|n| *n > 1) {
            let collisions: Vec<String> = lossy_names
                .into_iter()
                .filter(|(_, n)| *n > 1)
                .map(|(name, n)| format!("{n}× {name:?}"))
                .collect();
            return Err(ImportError::Object {
                oid: sha_hex.to_owned(),
                detail: format!(
                    "tree {} contains sibling names that collide after lossy UTF-8 \
                     decoding ({}); the model cannot represent this directory \
                     unambiguously — import refuses rather than conflate files",
                    tree.to_hex(),
                    collisions.join(", ")
                ),
            });
        }

        // Resolve the model subtree entries for associations + recursion.
        // The derived tree's entries pair with the git tree's entries by
        // name (the collision check above guarantees the pairing is
        // injective when names are representable). Trees are
        // content-addressed objects in the object store (served through
        // the engine's decoded-tree cache), not derived-table rows — the
        // import's derivation wrote them just before this walk.
        let model_entries: Vec<gardenbed_model::TreeEntry> = match model_tree {
            Some(id) => {
                let tree = engine
                    .cached_tree_arc(id)
                    .map_err(ImportError::from)?
                    .ok_or_else(|| ImportError::Object {
                        oid: sha_hex.to_owned(),
                        detail: format!(
                            "derived tree {id} is missing from the object store — \
                             cannot resolve the raw-tree model association"
                        ),
                    })?;
                tree.entries.clone()
            }
            None => Vec::new(),
        };

        // Recurse into subdirectories first (their decisions feed this
        // tree's own decision: an ancestor's bytes embed the child's
        // ORIGINAL SHA, while the canonical export renders the child's
        // canonical SHA — so a divergent child makes every ancestor
        // genuinely divergent too, which is exactly what capture
        // preserves).
        let mut any_diverged = false;
        for e in entries.iter().filter(|e| e.kind == GKind::Dir) {
            let child_model = model_entries
                .iter()
                .find(|me| {
                    me.kind == gardenbed_model::TreeKind::Dir
                        && me.name.as_bytes() == e.name.as_slice()
                })
                .and_then(|me| me.tree);
            let diverged = self.capture_subtree(
                repo,
                engine,
                e.oid,
                child_model,
                metadata,
                sha_hex,
                depth + 1,
                rows,
            )?;
            any_diverged |= diverged;
        }

        // Simplest sound predicate: if any child diverged, this tree
        // diverges; otherwise compare the full canonical render
        // byte-for-byte.
        if any_diverged {
            // Capture this tree verbatim; the model association is the
            // derived subtree id (None for an empty normalized tree).
            let model_assoc = model_tree;
            metadata
                .put_raw_tree(*tree.as_bytes(), model_assoc, &obj.data)
                .map_err(ImportError::from)?;
            *rows += 1;
            self.memo.insert(tree, Some(model_assoc));
            return Ok(true);
        }

        // No child diverged: compare the canonical render with the
        // original bytes. Blob/gitlink SHAs are identities; subtree SHAs
        // are the original ones (non-divergent children render to their
        // original bytes by induction).

        let canonical = render_canonical(
            &entries,
            // File entries: the original SHA is the identity the pack
            // carries (blobs re-hash to themselves; gitlink pointers pass
            // through unchanged).
            |e: &crate::gtree::RawEntry| Some(*e.oid.as_bytes()),
            // Directory entries: original SHA (non-divergent child).
            |e: &crate::gtree::RawEntry| Some(*e.oid.as_bytes()),
        );
        match canonical {
            Some(rendered) if rendered == obj.data => {
                // Byte-exact: no row; the memo records "decided, no row".
                self.memo.insert(tree, None);
                Ok(false)
            }
            _ => {
                // Divergent (mode bytes, non-UTF-8 name, or an
                // unrepresentable rendering): capture verbatim.
                metadata
                    .put_raw_tree(*tree.as_bytes(), model_tree, &obj.data)
                    .map_err(ImportError::from)?;
                *rows += 1;
                self.memo.insert(tree, Some(model_tree));
                Ok(true)
            }
        }
    }
}
