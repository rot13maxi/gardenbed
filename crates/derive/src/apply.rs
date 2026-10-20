//! Incremental tree application ("only touched directories are re-created").
//!
//! A [`WorkingSet`] opens (loads from objects) only the directories a change
//! touches. Untouched subtrees stay as opaque tree ids in their parent's
//! entry and are never loaded or re-encoded, so both reads and writes scale
//! with the change, not the repo. Content addressing does the rest: an
//! untouched subtree re-encoded would hash to the id it already has, so it
//! writes no new object either way — but the lazy walk keeps commit work
//! O(change) as D3 requires.

use std::collections::{BTreeMap, HashMap, HashSet};

use gardenbed_model::{Id, Mode, Op, Tree, TreeEntry, TreeKind};
use gardenbed_store::{ObjectStore, StoreError};

use crate::Engine;

/// Guard against pathological/corrupt trees: the deepest path nesting apply
/// or diff will traverse. Far above any real path, but it keeps recursion
/// bounded so no input can exhaust the stack.
pub(crate) const MAX_TREE_DEPTH: usize = 4096;

/// One slot in an open directory's entry map.
enum Slot {
    /// A file leaf (regular/exec/symlink/gitlink).
    File { blob: Id, mode: Mode },
    /// A subdirectory. `open` marks whether the child's entries are loaded
    /// into the working set (in which case `id` is stale); otherwise `id`
    /// is the untouched child tree, kept verbatim.
    Dir { open: bool, id: Option<Id> },
}

/// A lazily-loaded hierarchical working tree over the object store.
///
/// Only directories on the path of an applied change are opened; the
/// bottom-up serialization re-encodes exactly the opened directories,
/// pushing their new ids into their (also-open) parents.
struct WorkingSet<'a> {
    objects: &'a dyn ObjectStore,
    /// Open directories by full path ("" is the root).
    open: HashMap<String, BTreeMap<String, Slot>>,
    /// File paths written during this apply (Add/Modify/rename
    /// destinations). A delete beneath one of these is a no-op: the write
    /// swallowed the whole subtree (canonical path order sorts the write
    /// of `g` before the delete of `g/h`).
    written_files: HashSet<String>,
    /// Every tree id `finish()` stored (including idempotent re-puts of
    /// already-present objects). O(touched directories) — the derivation's
    /// write set, the exact object surface a concurrent gc can race
    /// against; the commit's last-chance verify re-checks exactly this.
    written_trees: Vec<Id>,
}

impl<'a> WorkingSet<'a> {
    /// A working set rooted at `base` (`None` = the empty tree).
    fn new(objects: &'a dyn ObjectStore, base: Option<Id>) -> Result<Self, StoreError> {
        let mut open = HashMap::new();
        match base {
            None => {
                open.insert(String::new(), BTreeMap::new());
            }
            Some(id) => {
                let tree = Engine::load_tree(objects, id)?;
                open.insert(String::new(), slots_from_tree(&tree)?);
            }
        }
        Ok(WorkingSet {
            objects,
            open,
            written_files: HashSet::new(),
            written_trees: Vec::new(),
        })
    }

    /// Returns the open entry map of directory `dir`, loading it (and every
    /// ancestor on the way) on first touch. Missing ancestors are created
    /// empty (see [`WorkingSet::open_dir`]).
    fn dir(&mut self, dir: &str) -> Result<&mut BTreeMap<String, Slot>, StoreError> {
        if !self.open.contains_key(dir) {
            self.open_dir(dir)?;
        }
        Ok(self.open.get_mut(dir).expect("dir was just opened"))
    }

    /// Loads directory `dir`'s entries from the store, opening its parent
    /// chain first and marking the parent's slot as open. A directory that
    /// does not exist in the base tree is created implicitly (`mkdir -p`
    /// semantics: an Add of `a/b` onto a tree without `a` materializes
    /// directory `a`, exactly as git's snapshot semantics would); only a
    /// *file* occupying the path is a collision.
    fn open_dir(&mut self, dir: &str) -> Result<(), StoreError> {
        if self.open.contains_key(dir) {
            return Ok(());
        }
        let (parent, name) = split_dir(dir);
        // Open the parent chain first (creates the slot for `dir`).
        self.dir(parent)?;
        /// What to do with `dir`'s slot in its (open) parent.
        enum Found {
            /// Existing subtree: load its entries.
            Load(Id),
            /// No entry: create an empty directory implicitly.
            Create,
            /// A file occupies the directory path: invalid changeset.
            Collision,
        }
        let found = match self.open.get(parent).expect("parent is open").get(name) {
            Some(Slot::Dir { id: Some(id), .. }) => Found::Load(*id),
            Some(Slot::Dir { id: None, .. }) => {
                return Err(StoreError::Corrupt(format!(
                    "directory {dir:?} is marked open but was never loaded"
                )));
            }
            Some(Slot::File { .. }) => Found::Collision,
            None => Found::Create,
        };
        match found {
            Found::Create => {
                self.open.insert(dir.to_owned(), BTreeMap::new());
                self.open.get_mut(parent).expect("parent is open").insert(
                    name.to_owned(),
                    Slot::Dir {
                        open: true,
                        id: None,
                    },
                );
                Ok(())
            }
            Found::Collision => Err(StoreError::Invalid(format!(
                "path {dir:?} collides with a file entry"
            ))),
            Found::Load(id) => {
                let tree = Engine::load_tree(self.objects, id)?;
                self.open.insert(dir.to_owned(), slots_from_tree(&tree)?);
                let parent_entries = self.open.get_mut(parent).expect("parent is open");
                parent_entries.insert(
                    name.to_owned(),
                    Slot::Dir {
                        open: true,
                        id: None,
                    },
                );
                Ok(())
            }
        }
    }

    /// Upserts a file entry at `path` (used for Add/Modify and rename
    /// destinations). The upsert replaces whatever occupies the path —
    /// including a whole directory subtree, git snapshot semantics.
    fn set_file(&mut self, path: &str, blob: Id, mode: Mode) -> Result<(), StoreError> {
        let (dir, name) = split_path(path)?;
        let entries = self.dir(dir)?;
        entries.insert(name.to_owned(), Slot::File { blob, mode });
        self.written_files.insert(path.to_owned());
        Ok(())
    }

    /// True when a proper ancestor directory of `path` currently holds a
    /// file that **this apply** wrote: the write replaced the directory and
    /// everything beneath it, so a delete inside that subtree refers to a
    /// path that no longer exists — a no-op, not a collision. (The base
    /// tree having a file there is *not* enough: deleting under a pre
    /// existing file is an inconsistent changeset and stays an error.)
    fn swallowed_by_write(&self, path: &str) -> bool {
        let mut dir = match split_path(path) {
            Ok((dir, _)) => dir,
            Err(_) => return false,
        };
        loop {
            if dir.is_empty() {
                return false;
            }
            if self.written_files.contains(dir) {
                let (parent, name) = split_dir(dir);
                if let Some(entries) = self.open.get(parent)
                    && matches!(entries.get(name), Some(Slot::File { .. }))
                {
                    return true;
                }
            }
            let (parent, _) = split_dir(dir);
            if parent.is_empty() {
                return false;
            }
            dir = parent;
        }
    }

    /// Removes the entry at `path`. `require` controls whether a missing
    /// path is an error (Delete/rename-source: an inconsistency) or a no-op.
    fn remove(&mut self, path: &str, require: bool) -> Result<(), StoreError> {
        if self.swallowed_by_write(path) {
            return Ok(());
        }
        let (dir, name) = split_path(path)?;
        let entries = self.dir(dir)?;
        match entries.remove(name) {
            Some(_) => Ok(()),
            None if require => Err(StoreError::Invalid(format!(
                "delete of missing path {path:?}: the changeset is inconsistent with its \
                 first parent's tree"
            ))),
            None => Ok(()),
        }
    }

    /// Serializes every open directory bottom-up, writing one tree object
    /// per changed directory, and returns the root tree id (`None` when the
    /// root ends up empty) plus every tree id stored along the way (the
    /// derivation's write set — idempotent re-puts included, since those
    /// are exactly the orphan-replay ids a concurrent gc wants).
    fn finish(self) -> Result<(Option<Id>, Vec<Id>), StoreError> {
        // Deepest paths first: children are re-encoded before their parents
        // read the children's new ids. A child's path is strictly longer
        // than its parent's, so a length sort is topologically safe.
        let mut dirs: Vec<String> = self.open.keys().cloned().collect();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.len()));

        let mut written_trees = self.written_trees;
        let mut done: HashMap<String, Option<Id>> = HashMap::with_capacity(dirs.len());
        for dir in dirs {
            let entries = self.open.get(&dir).expect("dir is open");
            let mut out: Vec<TreeEntry> = Vec::with_capacity(entries.len());
            for (name, slot) in entries {
                match slot {
                    Slot::File { blob, mode } => out.push(TreeEntry {
                        name: name.clone(),
                        kind: TreeKind::File,
                        mode: *mode,
                        blob: Some(*blob),
                        tree: None,
                    }),
                    Slot::Dir { open, id } => {
                        let child_id = if *open {
                            // The child serialized already (deeper first);
                            // `None` means it ended up empty and must vanish.
                            match done.get(&dir_join(&dir, name)) {
                                Some(Some(id)) => *id,
                                Some(None) => continue, // empty dir: omit entry
                                None => {
                                    return Err(StoreError::Corrupt(format!(
                                        "open directory {dir}/{name} was not serialized first"
                                    )));
                                }
                            }
                        } else {
                            // Untouched subtree: keep its stored tree id.
                            match id {
                                Some(id) => *id,
                                None => {
                                    return Err(StoreError::Corrupt(format!(
                                        "closed directory slot {dir}/{name} has no tree id"
                                    )));
                                }
                            }
                        };
                        out.push(TreeEntry {
                            name: name.clone(),
                            kind: TreeKind::Dir,
                            mode: Mode::Regular,
                            blob: None,
                            tree: Some(child_id),
                        });
                    }
                }
            }
            out.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
            let tree = Tree { entries: out };
            let result = if tree.entries.is_empty() {
                // An empty tree has no object; `None` represents it. (The
                // root cannot be dropped, only reported empty.)
                None
            } else {
                let bytes = tree.canonical_bytes().map_err(codec_err)?;
                let id = tree.id().map_err(codec_err)?;
                self.objects.put_tree(id, &bytes)?;
                written_trees.push(id);
                Some(id)
            };
            done.insert(dir, result);
        }
        Ok((
            done.get("")
                .copied()
                .expect("the root directory is always open"),
            written_trees,
        ))
    }
}

/// Applies one changeset's changes onto `base` (`None` = empty tree) and
/// returns the resulting root tree id (`None` = empty).
///
/// Changes are processed in canonical (ascending path) order. Add/Modify
/// upsert the entry; missing intermediate directories along a touched path
/// are created implicitly (git snapshot semantics); Delete must find the
/// path (otherwise the changeset is inconsistent with its first parent's
/// tree); Rename removes `src_path` (which must exist) and writes the new
/// entry. A delete beneath a path an earlier change in the same changeset
/// wrote as a *file* is a no-op: the write replaced that directory and
/// swallowed everything beneath it (this is the shape `diff` itself emits
/// for a dir→file swap — [`Op::Add`] of `g` sorts before [`Op::Delete`] of
/// `g/h`). An empty changeset inherits `base` unchanged, so it writes no
/// new tree objects.
pub(crate) fn apply_changeset(
    objects: &dyn ObjectStore,
    base: Option<Id>,
    cs: &gardenbed_model::Changeset,
) -> Result<Option<Id>, StoreError> {
    Ok(apply_changeset_with_writes(objects, base, cs)?.0)
}

/// [`apply_changeset`] plus the derivation's write set: every tree id the
/// application stored (idempotent re-puts included). Callers that commit
/// the result re-verify exactly this set inside their commit transaction
/// (the gc handshake's writer side) — it is O(touched directories), not
/// O(closure), so commit cost stays independent of repository size.
pub(crate) fn apply_changeset_with_writes(
    objects: &dyn ObjectStore,
    base: Option<Id>,
    cs: &gardenbed_model::Changeset,
) -> Result<(Option<Id>, Vec<Id>), StoreError> {
    if cs.changes.is_empty() {
        return Ok((base, Vec::new()));
    }
    let mut ws = WorkingSet::new(objects, base)?;
    for c in &cs.changes {
        match c.op {
            Op::Add | Op::Modify => {
                let blob = c.blob.ok_or_else(|| {
                    StoreError::Invalid(format!("{}: {} without a blob", c.path, c.op as u8))
                })?;
                ws.set_file(&c.path, blob, c.mode)?;
            }
            Op::Delete => ws.remove(&c.path, true)?,
            Op::Rename => {
                let src = c.src_path.as_deref().ok_or_else(|| {
                    StoreError::Invalid(format!("rename {} without a src_path", c.path))
                })?;
                if src == c.path {
                    return Err(StoreError::Invalid(format!(
                        "rename with src_path == path {src:?}"
                    )));
                }
                ws.remove(src, true)?;
                let blob = c.blob.ok_or_else(|| {
                    StoreError::Invalid(format!("rename {} without a blob", c.path))
                })?;
                ws.set_file(&c.path, blob, c.mode)?;
            }
        }
    }
    ws.finish()
}

// ------------------------------------------------------------- helpers

/// Extracts `(parent dir, file name)` from a full path.
fn split_path(path: &str) -> Result<(&str, &str), StoreError> {
    match path.rsplit_once('/') {
        None => Ok(("", path)),
        Some((dir, name)) => {
            if dir.is_empty() || name.is_empty() {
                return Err(StoreError::Invalid(format!("malformed path {path:?}")));
            }
            Ok((dir, name))
        }
    }
}

/// `(parent dir, last component)` of a directory path ("" is the root).
fn split_dir(dir: &str) -> (&str, &str) {
    match dir.rsplit_once('/') {
        None => ("", dir),
        Some((parent, name)) => (parent, name),
    }
}

/// Joins a directory path with a child component ("" + "a" = "a").
fn dir_join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// Converts a stored tree's entries into working-set slots.
fn slots_from_tree(tree: &Tree) -> Result<BTreeMap<String, Slot>, StoreError> {
    let mut out = BTreeMap::new();
    for e in &tree.entries {
        match e.kind {
            TreeKind::File => {
                let blob = e.blob.ok_or_else(|| {
                    StoreError::Corrupt(format!("file entry {:?} without a blob id", e.name))
                })?;
                out.insert(e.name.clone(), Slot::File { blob, mode: e.mode });
            }
            TreeKind::Dir => {
                let id = e.tree.ok_or_else(|| {
                    StoreError::Corrupt(format!("dir entry {:?} without a tree id", e.name))
                })?;
                out.insert(
                    e.name.clone(),
                    Slot::Dir {
                        open: false,
                        id: Some(id),
                    },
                );
            }
        }
    }
    Ok(out)
}

/// Maps a codec error onto a store error (corrupt-payload semantics).
pub(crate) fn codec_err(e: gardenbed_model::codec::CodecError) -> StoreError {
    StoreError::Corrupt(e.to_string())
}
