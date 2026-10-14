//! Path summaries — the bounded touched-path set per changeset, plus the
//! segment bloom backing blame's segment skips (derived kind
//! `"pathsummary"`).
//!
//! ## Semantics
//!
//! A changeset touches a path when one of its changes names that path as
//! `path` (the destination) or as `src_path` (the rename source). This is
//! exactly the set of paths whose *content or existence* the changeset can
//! alter relative to its first parent: every op that changes a path's blob
//! or existence names the path in one of those two positions. A path
//! absent from a changeset's touched set provably keeps its blob from the
//! first parent through the changeset.
//!
//! The entry therefore carries two layers:
//!
//! * **own paths** — the bounded, deduplicated set of paths this
//!   changeset touches (cap [`MAX_SUMMARY_PATHS`], ascending by bytes).
//!   Beyond the cap the list is truncated and the `truncated` flag is the
//!   **"unbounded" marker**: consumers must not use the list to prove
//!   absence ("must descend" instead).
//! * **segment bloom** — a fixed-size Bloom filter over the union of the
//!   own touched paths of every changeset in the half-open first-parent
//!   run `(head(self), self]` (empty for heads, whose own changes the
//!   blame walk reads directly). A Bloom filter is one-sided: "not
//!   present" is always true, "present" may be a false positive — so
//!   blame may skip a whole segment exactly when the bloom does not
//!   contain the path, and must otherwise descend commit by commit.
//!   False positives cost time, never correctness.
//!
//! Together with a copy of the skiplist's segment pointers (head, head
//! distance, first parent) the entry lets [`Engine::blame`] cross a
//! linear segment in one derived-data read.
//!
//! Determinism: the path list is sorted and deduplicated and the bloom
//! bits derive from fixed-seed FNV-1a hashes of the path bytes, so a
//! wiped-and-rebuilt cache reproduces byte-identical entries (M6.3).
//!
//! ## Entry layout
//!
//! ```text
//! 0x01            version
//! u8              flags: bit 0 = truncated (own paths exceeded the cap)
//! varint          n_paths
//! n_paths × str   own touched paths, deduped, ascending by raw bytes
//! u8              has_first_parent (0 for a root changeset)
//! [32 bytes]      first parent id (only when has_first_parent)
//! u8              is_head (1 = root or merge)
//! [32 bytes]      head id (absent when is_head)
//! varint          head_dist (0 for heads)
//! [BLOOM_BYTES]   segment bloom, raw bytes, bit i = byte i/8, bit i%8
//! ```

use gardenbed_model::Id;
use gardenbed_store::{MetadataStore, StoreError};

use crate::Engine;

/// Version byte of the path-summary entry payload.
const PATHSUMMARY_VERSION: u8 = 0x01;

/// Maximum number of distinct own paths kept in the bounded list. Beyond
/// this the list is truncated and the `truncated` flag is the
/// "unbounded" marker: consumers must not use the list to prove absence
/// ("must descend" instead).
pub const MAX_SUMMARY_PATHS: usize = 256;

/// Number of bits in the cumulative segment bloom filter.
pub const BLOOM_BITS: usize = 2048;
/// Number of probe functions per path.
const BLOOM_K: usize = 4;
/// Raw byte length of the bloom section.
const BLOOM_BYTES: usize = BLOOM_BITS / 8;

/// The per-changeset path summary (derived kind `"pathsummary"`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathSummary {
    /// Paths this changeset touches (dst ∪ src of its changes), deduped
    /// and ascending by raw bytes. Truncated at [`MAX_SUMMARY_PATHS`].
    pub paths: Vec<String>,
    /// True when the full touched set overflowed the cap — the
    /// "unbounded" marker. The list still holds the first
    /// [`MAX_SUMMARY_PATHS`] paths (byte-ascending); absence of a path
    /// beyond the cap cannot be proven from the list.
    pub truncated: bool,
    /// First parent id (`None` for a root changeset).
    pub first_parent: Option<Id>,
    /// Root or merge (mirrors the skiplist entry).
    pub is_head: bool,
    /// The segment head this changeset hangs below (`None` iff `is_head`).
    pub head: Option<Id>,
    /// First-parent steps from this changeset down to its head.
    pub head_dist: u64,
    /// Cumulative segment bloom over `(head, self]`; empty for heads.
    pub bloom: Vec<u8>,
}

impl PathSummary {
    /// The "unbounded" marker: the own-path list was truncated, so it
    /// cannot prove absence (consumers must descend).
    pub fn is_unbounded(&self) -> bool {
        self.truncated
    }

    /// One-sided Bloom membership: `false` proves the path is untouched
    /// anywhere in `(head, self]`; `true` allows a false positive.
    pub fn bloom_maybe_contains(&self, path: &str) -> bool {
        if self.bloom.len() != BLOOM_BYTES {
            // Malformed payload: be conservative.
            return true;
        }
        bloom_maybe(&self.bloom, path)
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(BLOOM_BYTES + 80 + self.paths.len() * 12);
        out.push(PATHSUMMARY_VERSION);
        out.push(u8::from(self.truncated));
        put_varint(&mut out, self.paths.len() as u64);
        for p in &self.paths {
            put_varint(&mut out, p.len() as u64);
            out.extend_from_slice(p.as_bytes());
        }
        match self.first_parent {
            None => out.push(0),
            Some(p) => {
                out.push(1);
                out.extend_from_slice(p.as_bytes());
            }
        }
        out.push(u8::from(self.is_head));
        if let Some(h) = self.head {
            out.extend_from_slice(h.as_bytes());
        }
        put_varint(&mut out, self.head_dist);
        debug_assert_eq!(self.bloom.len(), BLOOM_BYTES);
        out.extend_from_slice(&self.bloom);
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut i = 0usize;
        if bytes.first() != Some(&PATHSUMMARY_VERSION) {
            return None;
        }
        i += 1;
        let truncated = *bytes.get(i)? & 0x01 != 0;
        i += 1;
        let n = get_varint(bytes, &mut i)?;
        // Refuse hostile counts before allocating (spec §2 bounds).
        if n > MAX_SUMMARY_PATHS as u64 {
            return None;
        }
        let mut paths = Vec::with_capacity(n as usize);
        for _ in 0..n {
            paths.push(get_str(bytes, &mut i)?);
        }
        let first_parent = match bytes.get(i)? {
            0 => {
                i += 1;
                None
            }
            1 => {
                i += 1;
                let raw = bytes.get(i..i + 32)?;
                let mut arr = [0u8; 32];
                arr.copy_from_slice(raw);
                i += 32;
                Some(Id::from_bytes(arr))
            }
            _ => return None,
        };
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
        let bloom = bytes.get(i..i + BLOOM_BYTES)?.to_vec();
        i += BLOOM_BYTES;
        if bytes.len() != i {
            return None;
        }
        Some(PathSummary {
            paths,
            truncated,
            first_parent,
            is_head,
            head,
            head_dist,
            bloom,
        })
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
    let mut shift = 0u32;
    while shift < 64 {
        let b = *bytes.get(*i)?;
        *i += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
    }
    None
}

fn get_str(bytes: &[u8], i: &mut usize) -> Option<String> {
    let n = get_varint(bytes, i)?;
    if n > (1 << 26) {
        return None;
    }
    let raw = bytes.get(*i..*i + n as usize)?;
    *i += n as usize;
    String::from_utf8(raw.to_vec()).ok()
}

// ----------------------------------------------------------- bloom filter

/// Probe seed base; probe `j` uses `SEED_BASE * (j + 1)`.
const SEED_BASE: u64 = 0x9E37_79B9_7F4A_7C15;

/// FNV-1a 64-bit over `path` with a fixed per-probe seed. Deterministic
/// and dependency-free; collision quality only affects the Bloom
/// false-positive rate.
fn fnv1a64(seed: u64, bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ seed.rotate_left(17);
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn bloom_set(bits: &mut [u8], path: &str) {
    for j in 0..BLOOM_K {
        let h = fnv1a64(SEED_BASE.wrapping_mul(j as u64 + 1), path.as_bytes());
        let idx = (h as usize) % BLOOM_BITS;
        bits[idx / 8] |= 1 << (idx % 8);
    }
}

fn bloom_maybe(bits: &[u8], path: &str) -> bool {
    (0..BLOOM_K).all(|j| {
        let h = fnv1a64(SEED_BASE.wrapping_mul(j as u64 + 1), path.as_bytes());
        let idx = (h as usize) % BLOOM_BITS;
        bits[idx / 8] & (1 << (idx % 8)) != 0
    })
}

// ------------------------------------------------------------- computing

/// The touched paths of a changeset: every change's `path` plus every
/// rename's `src_path`, deduplicated, ascending by raw bytes.
fn touched_paths(cs: &gardenbed_model::Changeset) -> Vec<String> {
    let mut set: Vec<String> = Vec::with_capacity(cs.changes.len() * 2);
    for c in &cs.changes {
        if !set.iter().any(|p| p == &c.path) {
            set.push(c.path.clone());
        }
        if let Some(src) = &c.src_path {
            if !set.iter().any(|p| p == src) {
                set.push(src.clone());
            }
        }
    }
    set.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    set
}

/// The cached path summary for `cs`, or `None` when absent/unreadable
/// (an unreadable cache entry is a miss, never an error — recompute).
fn load_entry(metadata: &dyn MetadataStore, cs: Id) -> Result<Option<PathSummary>, StoreError> {
    match metadata.derived_get(crate::DERIVED_PATHSUMMARY, cs)? {
        None => Ok(None),
        Some(bytes) => Ok(PathSummary::decode(&bytes)),
    }
}

/// Computes the path summary for `cs` and persists it. Segment pointers
/// come from the skiplist (computed on demand through
/// [`Engine::skiplist_entry`]), which is itself deterministic.
fn compute_entry(engine: &Engine, cs: Id) -> Result<PathSummary, StoreError> {
    let (c, _) = engine
        .metadata()
        .get_changeset(cs)?
        .ok_or(StoreError::NotFound)?;
    let seg = engine.skiplist_entry(cs)?;
    let touched = touched_paths(&c);
    let truncated = touched.len() > MAX_SUMMARY_PATHS;
    // The bounded list carries the capped prefix; the bloom covers ALL
    // touched paths (a missing path would make blame's skip unsound).
    let paths: Vec<String> = touched.iter().take(MAX_SUMMARY_PATHS).cloned().collect();

    // Cumulative segment bloom over (head, cs]. For a member whose parent
    // hangs below the same head this is the parent's bloom OR-ed with
    // this changeset's own touched paths; otherwise (parent is the head)
    // it starts empty. Heads carry an empty bloom: the blame walk reads
    // their own changes directly.
    let mut bloom = vec![0u8; BLOOM_BYTES];
    if !seg.is_head {
        if let Some(&p) = c.parents.first() {
            let same_segment = engine.skiplist_entry(p)?.head == seg.head;
            if same_segment {
                let pe = match load_entry(engine.metadata().as_ref(), p)? {
                    Some(pe) => pe,
                    None => pathsummary_entry(engine, p)?,
                };
                bloom.copy_from_slice(&pe.bloom);
            }
        }
        for path in &touched {
            bloom_set(&mut bloom, path);
        }
    }

    let entry = PathSummary {
        paths,
        truncated,
        first_parent: c.parents.first().copied(),
        is_head: seg.is_head,
        head: seg.head,
        head_dist: seg.head_dist,
        bloom,
    };
    engine
        .metadata()
        .derived_put(crate::DERIVED_PATHSUMMARY, cs, &entry.encode())?;
    Ok(entry)
}

fn pathsummary_entry(engine: &Engine, cs: Id) -> Result<PathSummary, StoreError> {
    if let Some(e) = load_entry(&*engine.metadata, cs)? {
        return Ok(e);
    }
    engine.get_changeset(cs)?; // NotFound for unknown ids, before any walk
    compute_entry(engine, cs)
}

impl Engine {
    /// The path summary for `cs`, computing and caching it on demand.
    ///
    /// Errors with [`StoreError::NotFound`] when the changeset is unknown.
    pub fn pathsummary_entry(&self, cs: Id) -> Result<PathSummary, StoreError> {
        pathsummary_entry(self, cs)
    }
}
