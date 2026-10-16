//! Ancestry queries: naive graph walks plus the segmented-ancestry skip
//! list ("compost" derived data, kind `"skiplist"`).
//!
//! ## Segmented ancestry
//!
//! The first-parent chain is partitioned into **segments**: a *segment head*
//! is a changeset that is a root (no parents) or a merge (≥ 2 parents);
//! every other changeset hangs off the segment headed by the first
//! merge-or-root found walking first parents downward. Between two heads a
//! segment is strictly linear, so a first-parent step always decrements the
//! DAG height by exactly 1 — which makes "is `a` on my segment below me?"
//! decidable by walking the run with height guidance, accelerated by
//! power-of-two jump samples stored per changeset.
//!
//! Per changeset the skip list stores:
//!
//! * `height` — the true DAG height, `h(cs) = 1 + max(h(parents))`,
//!   `h(root) = 0`. A proper ancestor always has strictly smaller height,
//!   which prunes whole subtrees from every walk.
//! * `is_head`, `head`, `head_dist` — the segment structure, so a walk
//!   crosses a linear run in one hop instead of one commit at a time.
//! * `samples` — first-parent jump targets at distances 1, 2, 4, … 2^K
//!   (binary lifting), so the within-segment walk is O(log n) jumps.
//!
//! `is_ancestor` walks from `b` downward: at each node it either answers
//! from the segment samples, prunes by height, or jumps to the segment
//! head / expands a merge's parents. `merge_base` answers comparable pairs
//! (one side an ancestor of the other — the dominant harvest/graft case)
//! through that check and otherwise uses the naive common-ancestor
//! intersection, which is the semantic definition. General criss-cross
//! merge-base acceleration is the segmented changelog's M6 work.
//!
//! Every derived entry is a cache: a missing or unreadable entry falls back
//! to the plain BFS. A cache gap costs time, never correctness.
//!
//! ## Entry layout
//!
//! ```text
//! 0x02            version
//! varint          height
//! u8              is_head (1 = root or merge)
//! [32 bytes]      head id        (absent when is_head)
//! varint          head_dist      (0 when is_head)
//! varint          n_samples
//! n_samples × { varint dist (1,2,4,...); 32 bytes target }
//! ```
//! Samples are ascending by distance. Distances cap at `MAX_JUMP`.

use std::collections::{HashMap, HashSet};

use gardenbed_model::Id;
use gardenbed_store::{MetadataStore, StoreError};

use crate::Engine;

/// Version byte of the skiplist entry payload.
const SKIPLIST_VERSION: u8 = 0x02;
/// Largest first-parent jump distance recorded (2^20 ≈ 1M commits).
const MAX_JUMP: u64 = 1 << 20;

/// A per-changeset segmented-ancestry summary (the skip-list entry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentEntry {
    /// True DAG height: `h(root) = 0`, `h(cs) = 1 + max(h(parents))`.
    pub height: u64,
    /// Root or merge: walking past this node must expand every parent.
    pub is_head: bool,
    /// The segment head this changeset hangs below (`None` iff `is_head`).
    pub head: Option<Id>,
    /// First-parent steps from this changeset down to its head (0 for heads).
    pub head_dist: u64,
    /// First-parent jump targets, ascending by distance
    /// (`(target, distance)` pairs at distances 1, 2, 4, …).
    pub samples: Vec<(u64, Id)>,
}

impl SegmentEntry {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(80 + self.samples.len() * 37);
        out.push(SKIPLIST_VERSION);
        put_varint(&mut out, self.height);
        out.push(u8::from(self.is_head));
        if let Some(h) = self.head {
            out.extend_from_slice(h.as_bytes());
        }
        put_varint(&mut out, self.head_dist);
        put_varint(&mut out, self.samples.len() as u64);
        for (dist, target) in &self.samples {
            put_varint(&mut out, *dist);
            out.extend_from_slice(target.as_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut i = 0usize;
        if bytes.first() != Some(&SKIPLIST_VERSION) {
            return None;
        }
        i += 1;
        let height = get_varint(bytes, &mut i)?;
        let is_head = *bytes.get(i)? == 1;
        i += 1;
        let head = if is_head {
            None
        } else {
            let raw = bytes.get(i..i + 32)?;
            let mut arr = [0u8; 32];
            arr.copy_from_slice(raw);
            i += 32;
            Some(Id::from_bytes(arr))
        };
        let head_dist = get_varint(bytes, &mut i)?;
        let n = get_varint(bytes, &mut i)?;
        // Refuse absurd sample counts before allocating.
        if n > 64 {
            return None;
        }
        let mut samples = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let dist = get_varint(bytes, &mut i)?;
            let raw = bytes.get(i..i + 32)?;
            let mut arr = [0u8; 32];
            arr.copy_from_slice(raw);
            i += 32;
            samples.push((dist, Id::from_bytes(arr)));
        }
        if bytes.len() != i {
            return None;
        }
        Some(SegmentEntry {
            height,
            is_head,
            head,
            head_dist,
            samples,
        })
    }

    /// The jump target at an exact power-of-two distance, if recorded.
    fn sample(&self, dist: u64) -> Option<Id> {
        self.samples
            .iter()
            .find(|(d, _)| *d == dist)
            .map(|(_, t)| *t)
    }
}

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

/// The cached segment entry for `cs`, or `None` when absent/unreadable
/// (an unreadable cache entry is a miss, never an error — recompute).
fn load_entry(metadata: &dyn MetadataStore, cs: Id) -> Result<Option<SegmentEntry>, StoreError> {
    match metadata.derived_get(crate::DERIVED_SKIPLIST, cs)? {
        None => Ok(None),
        Some(bytes) => Ok(SegmentEntry::decode(&bytes)),
    }
}

/// Computes and persists segment entries for `cs` and (only where missing)
/// its ancestry, iteratively so deep histories cannot overflow the stack.
/// Post-order guarantees every ancestor's entry exists before a node's own
/// entry needs it (jump samples point at ancestors).
///
/// `defer_head` skips the write of the HEAD entry (returned instead): the
/// batched import stores it in the same transaction as the changeset row,
/// which must not be fetched from the store either — `head_override`
/// carries its content.
fn compute_entry_impl(
    engine: &Engine,
    cs: Id,
    head_override: Option<&gardenbed_model::Changeset>,
    defer_head: bool,
) -> Result<SegmentEntry, StoreError> {
    /// Stack cap: legitimate depth is bounded by DAG height; a cycle is
    /// unreachable under content addressing, but hostile data must not hang
    /// the process.
    const STACK_CAP: usize = 1 << 20;

    let mut memo: HashMap<Id, SegmentEntry> = HashMap::new();
    let mut stack: Vec<Id> = vec![cs];
    while let Some(&top) = stack.last() {
        if memo.contains_key(&top) {
            stack.pop();
            continue;
        }
        if let Some(e) = load_entry(&**engine.metadata(), top)? {
            memo.insert(top, e);
            stack.pop();
            continue;
        }
        // The head's row may not exist yet (batched import): its content is
        // supplied by the caller.
        let c_owned: gardenbed_model::Changeset = match (top == cs, head_override) {
            (true, Some(h)) => h.clone(),
            _ => {
                engine
                    .metadata()
                    .get_changeset(top)?
                    .ok_or(StoreError::NotFound)?
                    .0
            }
        };
        let c = &c_owned;
        // Ensure every parent has an entry available (memo or store cache).
        let mut ready = true;
        for p in &c.parents {
            if memo.contains_key(p) {
                continue;
            }
            match load_entry(&**engine.metadata(), *p)? {
                Some(e) => {
                    memo.insert(*p, e);
                }
                None => {
                    stack.push(*p);
                    ready = false;
                }
            }
        }
        if !ready {
            if stack.len() > STACK_CAP {
                return Err(StoreError::Invalid(
                    "changeset ancestry exceeds the traversal budget (corrupt graph?)".into(),
                ));
            }
            continue;
        }
        let entry = build_entry(engine, &memo, c)?;
        if !(defer_head && top == cs) {
            engine
                .metadata()
                .derived_put(crate::DERIVED_SKIPLIST, top, &entry.encode())?;
        }
        memo.insert(top, entry);
        stack.pop();
    }
    memo.get(&cs)
        .cloned()
        .ok_or_else(|| StoreError::Corrupt(format!("segment entry for {cs} missing after compute")))
}

/// Computes segment entries for `cs`, persisting every entry except the
/// head's — the head's entry is returned for the caller to store together
/// with the changeset (batched import). `head` is the stored content of
/// `cs`, whose changeset row does not exist yet.
pub(crate) fn skiplist_entry_for_new(
    engine: &Engine,
    cs: Id,
    head: &gardenbed_model::Changeset,
) -> Result<SegmentEntry, StoreError> {
    compute_entry_impl(engine, cs, Some(head), true)
}

/// Builds the entry for `c` from its parents' (already computed) entries.
/// Sample sources are ancestors of `c`; the post-order walk guarantees
/// their entries exist in `memo`, and any that arrived via the store cache
/// instead is read back (or computed) through `engine`.
fn build_entry(
    engine: &Engine,
    memo: &HashMap<Id, SegmentEntry>,
    c: &gardenbed_model::Changeset,
) -> Result<SegmentEntry, StoreError> {
    let entry_of = |id: Id| -> Result<SegmentEntry, StoreError> {
        match memo.get(&id) {
            Some(e) => Ok(e.clone()),
            None => engine.skiplist_entry(id),
        }
    };
    Ok(match c.parents.len() {
        0 => SegmentEntry {
            height: 0,
            is_head: true,
            head: None,
            head_dist: 0,
            samples: vec![],
        },
        1 => {
            let p = c.parents[0];
            let pe = entry_of(p)?;
            let (head, head_dist) = if pe.is_head {
                (Some(p), 1)
            } else {
                (pe.head, pe.head_dist + 1)
            };
            // Binary lifting: sample(2^k) = jump(sample(2^(k-1)), 2^(k-1)).
            // The jump source is an ancestor of `top`, whose entry the
            // post-order walk has already produced (memo or store cache).
            let mut samples: Vec<(u64, Id)> = vec![(1, p)];
            let mut k = 1u32;
            while (1u64 << k) <= MAX_JUMP {
                let half = 1u64 << (k - 1);
                let x = samples.last().expect("previous sample").1;
                // The jump source is an ancestor of `top`: present in the
                // cascade's memo when freshly computed, otherwise in the
                // store cache (persisted together with its own cone).
                let xe = entry_of(x)?;
                match xe.sample(half) {
                    Some(target) => samples.push((half * 2, target)),
                    None => break,
                }
                k += 1;
            }
            SegmentEntry {
                height: pe.height + 1,
                is_head: false,
                head,
                head_dist,
                samples,
            }
        }
        _ => {
            let mut height = 0u64;
            for p in &c.parents {
                height = height.max(entry_of(*p)?.height);
            }
            SegmentEntry {
                height: height + 1,
                is_head: true,
                head: None,
                head_dist: 0,
                samples: vec![],
            }
        }
    })
}

// ---------------------------------------------------------------- naive

/// Pure breadth-first walk with no derived data — the semantic definition
/// the accelerated paths must agree with.
fn is_ancestor_naive_impl(metadata: &dyn MetadataStore, a: Id, b: Id) -> Result<bool, StoreError> {
    if a == b {
        return Ok(true);
    }
    let mut visited = HashSet::new();
    let mut queue = vec![b];
    visited.insert(b);
    while let Some(x) = queue.pop() {
        let (c, _) = metadata.get_changeset(x)?.ok_or(StoreError::NotFound)?;
        for p in &c.parents {
            if *p == a {
                return Ok(true);
            }
            if visited.insert(*p) {
                queue.push(*p);
            }
        }
    }
    Ok(false)
}

/// All ancestors of `start`, including `start` itself.
fn ancestors_naive(metadata: &dyn MetadataStore, start: Id) -> Result<HashSet<Id>, StoreError> {
    let mut visited = HashSet::new();
    let mut queue = vec![start];
    visited.insert(start);
    while let Some(x) = queue.pop() {
        let (c, _) = metadata.get_changeset(x)?.ok_or(StoreError::NotFound)?;
        for p in &c.parents {
            if visited.insert(*p) {
                queue.push(*p);
            }
        }
    }
    Ok(visited)
}

/// The naive merge base: intersect both ancestor sets, keep the maximal
/// common ancestors (those that are not proper ancestors of another common
/// one), and pick deterministically — newest committer seconds, tie-broken
/// by descending id bytes.
///
/// `git merge-base` returns "the best common ancestor"; when several are
/// equally best (criss-cross merges) git's choice is implementation-defined,
/// so this deterministic rule is gardenbed's documented answer. On v0.1
/// histories (redundant merges over a linear mainline) the maximal common
/// ancestor is unique and both agree exactly.
fn merge_base_naive_impl(
    metadata: &dyn MetadataStore,
    a: Id,
    b: Id,
) -> Result<Option<Id>, StoreError> {
    if a == b {
        return Ok(Some(a));
    }
    let anc_a = ancestors_naive(metadata, a)?;
    let anc_b = ancestors_naive(metadata, b)?;
    let common: Vec<Id> = anc_a.intersection(&anc_b).copied().collect();
    if common.is_empty() {
        return Ok(None);
    }
    let mut candidates: Vec<(u64, Id)> = Vec::with_capacity(common.len());
    for c in common {
        let (cs, _) = metadata.get_changeset(c)?.ok_or(StoreError::NotFound)?;
        candidates.push((cs.committer.time_secs, c));
    }
    candidates.sort_by(|x, y| {
        y.0.cmp(&x.0)
            .then_with(|| y.1.as_bytes().cmp(x.1.as_bytes()))
    });
    let mut kept: Vec<Id> = Vec::new();
    for (_, c) in candidates {
        // c is non-maximal iff it is a proper ancestor of an already-kept
        // candidate; ancestry is transitive, so the scan order does not
        // change the maximal set.
        let mut subsumed = false;
        for d in &kept {
            if is_ancestor_naive_impl(metadata, c, *d)? {
                subsumed = true;
                break;
            }
        }
        if !subsumed {
            kept.push(c);
        }
    }
    Ok(kept.first().copied())
}

// ------------------------------------------------------- accelerated walk

/// Decides whether `a` lies on `x`'s first-parent segment run (strictly
/// below `x`, at-or-above the head), using the segment's linearity: a
/// first-parent step inside a segment decrements the height by exactly one,
/// so the target distance is known and binary-lifting samples cover it
/// without ever stepping past `a`.
fn on_same_run(
    engine: &Engine,
    x: Id,
    xe: &SegmentEntry,
    a: Id,
    a_height: u64,
) -> Result<bool, StoreError> {
    if xe.height <= a_height {
        return Ok(false); // caller prunes equal-to-self earlier
    }
    let mut remaining = xe.height - a_height;
    let mut cur = x;
    let mut cur_entry = xe.clone();
    loop {
        if cur == a {
            return Ok(true);
        }
        if cur_entry.is_head {
            // Segment bottom reached with distance still owed: `a` is below
            // the head — not on this run (cross-segment ancestry is the
            // caller's job).
            return Ok(false);
        }
        if let Some((_, target)) = cur_entry.samples.iter().find(|(d, _)| *d == remaining) {
            // Exact jump straight to `a`'s height.
            return Ok(*target == a);
        }
        // Largest jump that stays strictly above `a`'s height.
        let jump = cur_entry
            .samples
            .iter()
            .filter(|(d, _)| *d < remaining)
            .map(|(d, _)| *d)
            .max();
        match jump {
            Some(d) => {
                let target = cur_entry.sample(d).expect("filter came from samples");
                remaining -= d;
                cur = target;
                cur_entry = engine.skiplist_entry(target)?;
            }
            None => {
                // No usable sample: step one first parent physically.
                let (c, _) = engine
                    .metadata()
                    .get_changeset(cur)?
                    .ok_or(StoreError::NotFound)?;
                let Some(p) = c.parents.first().copied() else {
                    return Ok(false); // root reached: `a` is not above it
                };
                remaining -= 1;
                cur = p;
                cur_entry = engine.skiplist_entry(p)?;
            }
        }
        if remaining == 0 {
            return Ok(cur == a);
        }
    }
}

/// The accelerated `is_ancestor` over the segmented skip list. Requires
/// derived entries (computing them on demand); no silent fallback.
fn is_ancestor_skiplist_impl(engine: &Engine, a: Id, b: Id) -> Result<bool, StoreError> {
    if a == b {
        return Ok(true);
    }
    let ea = engine.skiplist_entry(a)?;
    let mut visited = HashSet::new();
    let mut stack = vec![b];
    visited.insert(b);
    while let Some(x) = stack.pop() {
        if x == a {
            return Ok(true);
        }
        let ex = engine.skiplist_entry(x)?;
        // Height prune: proper ancestors of x all have smaller height.
        if ex.height <= ea.height {
            continue;
        }
        // `a` at the segment head, or on the linear run below x.
        if ex.head == Some(a) || on_same_run(engine, x, &ex, a, ea.height)? {
            return Ok(true);
        }
        if ex.is_head {
            let (c, _) = engine
                .metadata()
                .get_changeset(x)?
                .ok_or(StoreError::NotFound)?;
            for p in &c.parents {
                if visited.insert(*p) {
                    stack.push(*p);
                }
            }
        } else {
            let h = ex.head.expect("non-head carries a head");
            if visited.insert(h) {
                stack.push(h);
            }
        }
    }
    Ok(false)
}

// ----------------------------------------------------------- public API

impl Engine {
    /// The segmented-ancestry skip-list entry for `cs`, computing and
    /// caching it (and any missing ancestors' entries) on demand.
    ///
    /// Errors with [`StoreError::NotFound`] when the changeset is unknown.
    pub fn skiplist_entry(&self, cs: Id) -> Result<SegmentEntry, StoreError> {
        if let Some(e) = load_entry(&*self.metadata, cs)? {
            return Ok(e);
        }
        self.get_changeset(cs)?; // NotFound for unknown ids, before any walk
        compute_entry_impl(self, cs, None, false)
    }

    /// Is `a` an ancestor of `b`? (Every changeset is an ancestor of
    /// itself.)
    ///
    /// Walks the segmented skip list when its entries are available and the
    /// plain graph otherwise; the two are cross-checked for equality in the
    /// test suite.
    pub fn is_ancestor(&self, a: Id, b: Id) -> Result<bool, StoreError> {
        match is_ancestor_skiplist_impl(self, a, b) {
            Ok(v) => Ok(v),
            // Derived-data gaps degrade to the naive walk; genuine store
            // errors re-surface there.
            Err(StoreError::NotFound) => self.is_ancestor_naive(a, b),
            Err(_) => self.is_ancestor_naive(a, b),
        }
    }

    /// The accelerated walk only: no naive fallback. Derived entries are
    /// computed on demand; a missing changeset is [`StoreError::NotFound`].
    pub fn is_ancestor_skip(&self, a: Id, b: Id) -> Result<bool, StoreError> {
        is_ancestor_skiplist_impl(self, a, b)
    }

    /// Is `a` an ancestor of `b`, by plain BFS with no derived data? The
    /// semantic reference for the skip list.
    pub fn is_ancestor_naive(&self, a: Id, b: Id) -> Result<bool, StoreError> {
        is_ancestor_naive_impl(&*self.metadata, a, b)
    }

    /// The best common ancestor of `a` and `b`, or `None` when they share
    /// none. `Some(a)` when `a` is an ancestor of `b`, and vice versa.
    ///
    /// Comparable pairs go through the skip-list-accelerated ancestry check;
    /// everything else uses [`Engine::merge_base_naive`].
    pub fn merge_base(&self, a: Id, b: Id) -> Result<Option<Id>, StoreError> {
        if a == b {
            return Ok(Some(a));
        }
        if self.is_ancestor(a, b)? {
            return Ok(Some(a));
        }
        if self.is_ancestor(b, a)? {
            return Ok(Some(b));
        }
        self.merge_base_naive(a, b)
    }

    /// The naive merge base (common-ancestor intersection + maximal
    /// filter); see the module docs for the tie-breaking rule.
    pub fn merge_base_naive(&self, a: Id, b: Id) -> Result<Option<Id>, StoreError> {
        merge_base_naive_impl(&*self.metadata, a, b)
    }

    /// Eagerly derives what should exist as soon as a changeset is created
    /// (D3): its tree, its skip-list entry and its path summary.
    ///
    /// Idempotent and crash-safe: every step writes a deterministically
    /// recomputable cache value. Callers store the changeset first; a
    /// failure here leaves the changeset present with missing (recomputable)
    /// derived data.
    pub fn on_changeset_created(&self, cs_id: Id) -> Result<(), StoreError> {
        let tree = self.derive_tree(cs_id)?;
        // A durable certificate can answer derive_tree without consulting
        // compost. Explicit eager rebuilds must still restore its tree row.
        self.cache_tree(cs_id, tree)?;
        self.skiplist_entry(cs_id)?;
        self.pathsummary_entry(cs_id)?;
        Ok(())
    }
}
