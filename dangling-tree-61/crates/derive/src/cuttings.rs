//! Copy/rename detection (D?): per-changeset exact-blob facts.
//!
//! A changeset's **cuttings** are the exact-blob copy and rename facts its
//! changes state against the first parent's tree:
//!
//! * A **rename** is an [`Op::Rename`] change whose blob equals the blob at
//!   its `src_path` in the first parent's tree (a pure move).
//! * A **copy** is an [`Op::Add`] change whose blob exists at another path
//!   in the first parent's tree (the content already existed there; the
//!   add duplicated it). The source is the lexicographically smallest such
//!   path.
//!
//! Detection is exact-blob only: blob id equality, never a similarity
//! heuristic. A rename whose blob differs from the source path's blob is a
//! move-and-edit — no fact. An add of brand-new content is no fact. Only
//! the first parent is consulted (the delta base), matching the diff and
//! blame lineage.
//!
//! Everything here is a deterministic cache over committed data: a missing
//! or unreadable entry costs a recompute, never a wrong answer.

use std::collections::HashMap;

use gardenbed_model::{Changeset, Id, Op};
use gardenbed_store::{MetadataStore, StoreError};

use crate::Engine;

/// Derived-data kind tag for the copy/rename facts.
pub const DERIVED_CUTTINGS: &str = "cuttings";

/// Version byte of the encoded entry format.
const CUTTINGS_VERSION: u8 = 0x01;

/// One copy/rename fact: the path, the source path it copied/moved from,
/// the shared blob id, and which of the two it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cutting {
    /// The new path (the rename destination or the copied-to path).
    pub path: String,
    /// The source path in the first parent's tree.
    pub src: String,
    /// The blob id shared by both paths — exact-blob equality is the whole
    /// detection rule.
    pub blob: Id,
    /// Whether this fact is a rename or a copy.
    pub kind: CuttingKind,
}

/// Which exact-blob fact a [`Cutting`] records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CuttingKind {
    /// A pure move: rename whose blob equals the source path's blob.
    Rename,
    /// An add duplicating an existing blob at another path.
    Copy,
}

impl CuttingKind {
    fn to_u8(self) -> u8 {
        match self {
            CuttingKind::Rename => 0,
            CuttingKind::Copy => 1,
        }
    }

    fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(CuttingKind::Rename),
            1 => Some(CuttingKind::Copy),
            _ => None,
        }
    }
}

/// The copy/rename facts of one changeset (always present once computed,
/// possibly empty — mirrors the path summary's always-a-row shape).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cuttings {
    /// Sorted by path bytes, strictly ascending (the changeset's canonical
    /// change order is ascending by path; facts keep that order).
    pub entries: Vec<Cutting>,
}

impl Cuttings {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 5 + self.entries.len() * 80);
        out.push(CUTTINGS_VERSION);
        put_varint(&mut out, self.entries.len() as u64);
        for e in &self.entries {
            out.push(e.kind.to_u8());
            put_str(&mut out, &e.path);
            put_str(&mut out, &e.src);
            out.extend_from_slice(e.blob.as_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut i = 0usize;
        if *bytes.first()? != CUTTINGS_VERSION {
            return None;
        }
        i += 1;
        let count = get_varint(bytes, &mut i)?;
        let mut entries = Vec::new();
        for _ in 0..count {
            let kind = CuttingKind::from_u8(*bytes.get(i)?)?;
            i += 1;
            let path = get_str(bytes, &mut i)?;
            let src = get_str(bytes, &mut i)?;
            let blob = {
                let arr: [u8; 32] = bytes.get(i..i + 32)?.try_into().ok()?;
                i += 32;
                Id::from_bytes(arr)
            };
            entries.push(Cutting {
                path,
                src,
                blob,
                kind,
            });
        }
        if i != bytes.len() {
            return None; // trailing bytes = not well-formed
        }
        Some(Cuttings { entries })
    }
}

/// The cached cuttings for `cs`, or `None` when absent/unreadable (an
/// unreadable cache entry is a miss, never an error — recompute).
fn load_entry(metadata: &dyn MetadataStore, cs: Id) -> Result<Option<Cuttings>, StoreError> {
    match metadata.derived_get(DERIVED_CUTTINGS, cs)? {
        None => Ok(None),
        Some(bytes) => Ok(Cuttings::decode(&bytes)),
    }
}

/// Computes the cuttings for `cs` and persists them.
fn compute_entry(engine: &Engine, cs: Id) -> Result<Cuttings, StoreError> {
    let c = engine.get_changeset(cs)?;
    let entry = build_cuttings(engine, &c)?;
    engine
        .metadata()
        .derived_put(DERIVED_CUTTINGS, cs, &entry.encode())?;
    Ok(entry)
}

/// Builds the cuttings from an in-hand changeset — no fetch for `cs` itself
/// and no write, so the batched import can persist the row in the
/// changeset's own transaction. Parent lookups hit committed changesets
/// only.
pub(crate) fn build_cuttings(engine: &Engine, c: &Changeset) -> Result<Cuttings, StoreError> {
    let Some(parent) = c.parents.first().copied() else {
        return Ok(Cuttings::default()); // root: no first parent to copy from
    };
    let parent_tree = engine.derive_tree(parent)?;
    let flat = engine.flatten_tree(parent_tree)?;

    // Rename facts: op == Rename && blob == blob at src_path in the parent.
    // Copy facts: op == Add && blob exists at another path in the parent;
    // the source is the lexicographically smallest such path (flatten_tree
    // sorts by path bytes, so the first hit wins).
    //
    // The parent tree is looked up per path (renames) and per blob (adds);
    // one pass over the flattened parent builds both indexes, keeping the
    // walk O(changes + parent size) instead of O(changes × parent size).
    let mut by_path = HashMap::new();
    let mut by_blob: HashMap<Id, Vec<&str>> = HashMap::new();
    for (p, _, b) in &flat {
        by_path.insert(p.as_str(), *b);
        by_blob.entry(*b).or_default().push(p.as_str());
    }

    let mut entries = Vec::new();
    for change in &c.changes {
        let Some(blob) = change.blob else { continue };
        match change.op {
            Op::Rename => {
                let Some(src) = change.src_path.as_deref() else {
                    continue; // unreachable: the codec requires src_path
                };
                if by_path.get(src) == Some(&blob) {
                    entries.push(Cutting {
                        path: change.path.clone(),
                        src: src.to_owned(),
                        blob,
                        kind: CuttingKind::Rename,
                    });
                }
            }
            Op::Add => {
                let Some(paths) = by_blob.get(&blob) else {
                    continue;
                };
                // Sorted by path bytes; the first path other than the
                // added path is the lexicographically smallest source.
                if let Some(src) = paths.iter().find(|p| **p != change.path).copied() {
                    entries.push(Cutting {
                        path: change.path.clone(),
                        src: src.to_owned(),
                        blob,
                        kind: CuttingKind::Copy,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(Cuttings { entries })
}

impl Engine {
    /// The copy/rename facts for `cs`, computing and caching them on demand.
    ///
    /// Errors with [`StoreError::NotFound`] when the changeset is unknown.
    pub fn cuttings_entry(&self, cs: Id) -> Result<Cuttings, StoreError> {
        if let Some(e) = load_entry(&*self.metadata, cs)? {
            return Ok(e);
        }
        self.get_changeset(cs)?; // NotFound for unknown ids, before any walk
        compute_entry(self, cs)
    }
}

// -------------------------------------------------------------- encoding

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let group = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(group);
            return;
        }
        out.push(group | 0x80);
    }
}

fn get_varint(bytes: &[u8], i: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut count = 0usize;
    loop {
        let b = *bytes.get(*i)?;
        *i += 1;
        count += 1;
        if count > 10 {
            return None;
        }
        let group = u64::from(b & 0x7f);
        let shift = 7 * (count - 1);
        // Bits beyond 64 cannot be represented (they would change the
        // value on re-encode); a zero final group is non-minimal. Either
        // way the entry is not well-formed.
        if shift >= 64 || (group << shift) >> shift != group {
            return None;
        }
        v |= group << shift;
        if b & 0x80 == 0 {
            if count > 1 && b == 0 {
                return None;
            }
            return Some(v);
        }
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn get_str(bytes: &[u8], i: &mut usize) -> Option<String> {
    let len = get_varint(bytes, i)? as usize;
    let s = bytes.get(*i..*i + len)?;
    *i += len;
    std::str::from_utf8(s).ok().map(|s| s.to_owned())
}
