//! Seeded synthetic history generator.
//!
//! [`generate`] produces a deterministic DAG of gardenbed changesets plus
//! the raw blob contents they reference. The first changeset is the root;
//! every later changeset's *first* parent is the immediately preceding
//! changeset, so the first-parent lineage is exactly the changeset order.
//! Merge commits add 1–2 extra parents chosen among recent changesets
//! (deliberately "redundant" merges: the extra parent is already an
//! ancestor of the first-parent chain, which git accepts happily — the
//! oracle comparisons in later milestones only ever use the first-parent
//! lineage).
//!
//! Every generated changeset satisfies the canonical-encoding semantic
//! rules (`docs/canonical-encoding.md` §3): paths are relative and
//! component-clean, changes are strictly ascending by path bytes with no
//! duplicates, the op/blob/src_path/mode table holds exactly, and each
//! path appears at most once per changeset whether as `path` or
//! `src_path` (so a changeset is never self-ambiguous).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use rand::rngs::StdRng;
use rand::{Rng, RngExt, SeedableRng};

/// Knobs for the synthetic history generator.
///
/// All ratios are probabilities and are clamped to `[0.0, 1.0]`
/// internally; they steer a random draw per operation, so they are
/// approximately (not exactly) the observed fractions.
#[derive(Clone, Debug, PartialEq)]
pub struct GenOptions {
    /// Number of changesets to generate (minimum 1). Default 50.
    pub commits: u32,
    /// Fraction of commits (after the first) that are merges. Default 0.1.
    pub merge_ratio: f64,
    /// Fraction of merges that are 3-parent octopus merges. Default 0.02.
    pub octopus_ratio: f64,
    /// Fraction of per-file operations that are renames. Default 0.15.
    pub rename_ratio: f64,
    /// Fraction of per-file operations that are deletes. Default 0.1.
    pub delete_ratio: f64,
    /// Working-set size (live files) cap. Default 40.
    pub max_files: u32,
    /// Maximum path nesting depth, and the breadth of the directory pool.
    /// Default 6.
    pub max_dirs: u32,
    /// If set, occasionally emit one large blob of exactly this many bytes.
    pub big_blob_bytes: Option<usize>,
    /// Probability that a new/changed file is executable (`100755`).
    /// Default 0.08.
    pub exec_ratio: f64,
    /// Probability that a new/changed file is a symlink (`120000`).
    /// Default 0.04.
    pub symlink_ratio: f64,
    /// Emit non-ASCII (UTF-8) path components. Default false.
    pub unicode_paths: bool,
    /// Some files use CRLF line endings. Default false.
    pub crlf_content: bool,
    /// Some files are empty (0 bytes). Default false.
    pub empty_files: bool,
}

impl Default for GenOptions {
    fn default() -> Self {
        GenOptions {
            commits: 50,
            merge_ratio: 0.1,
            octopus_ratio: 0.02,
            rename_ratio: 0.15,
            delete_ratio: 0.1,
            max_files: 40,
            max_dirs: 6,
            big_blob_bytes: None,
            exec_ratio: 0.08,
            symlink_ratio: 0.04,
            unicode_paths: false,
            crlf_content: false,
            empty_files: false,
        }
    }
}

/// A fully generated synthetic history.
#[derive(Clone, Debug)]
pub struct GeneratedHistory {
    /// Valid, canonical-order changesets. `changesets[i]`'s parents are
    /// ids of earlier entries; the first entry is the root (no parents).
    pub changesets: Vec<Changeset>,
    /// Every blob id used anywhere in the history, with its raw content,
    /// **sorted ascending by raw id bytes** (the canonical iteration
    /// order a `BTreeMap` keyed by id would give).
    ///
    /// Note this deviates from the original `BTreeMap<Id, Vec<u8>>` sketch:
    /// `gardenbed_model::Id` intentionally exposes no `Ord` (it is a
    /// read-only crate), so a byte-sorted vector carries the same
    /// information with the same determinism. Use [`GeneratedHistory::blob`]
    /// for map-style lookup.
    ///
    /// For `Mode::Gitlink` entries the "content" is the padded pointer
    /// itself (12 zero bytes + the 20-byte upstream git SHA-1), matching
    /// the canonical spec: the blob field carries the upstream commit SHA
    /// left-padded to 32 bytes.
    pub blob_contents: Vec<(Id, Vec<u8>)>,
    /// Id of the last changeset.
    pub head: Id,
}

impl GeneratedHistory {
    /// Map-style lookup of a blob's raw content by id (binary search over
    /// the byte-sorted contents).
    pub fn blob(&self, id: &Id) -> Option<&[u8]> {
        self.blob_contents
            .binary_search_by(|(k, _)| k.as_bytes().cmp(id.as_bytes()))
            .ok()
            .map(|i| self.blob_contents[i].1.as_slice())
    }
}

/// Fixed probability of creating a submodule-pointer (`Mode::Gitlink`)
/// entry when a new file mode is chosen. Deliberately not a
/// [`GenOptions`] field so the contractual option set stays stable.
const GITLINK_RATIO: f64 = 0.01;

/// How many recent changesets merges may pick extra parents from.
const MERGE_PARENT_WINDOW: usize = 24;

const LINE_POOL: &[&str] = &[
    "the quick brown fox jumps over the lazy dog",
    "a seed is small, self-contained, and grows when planted",
    "trees are derived, cached objects, never authoritative",
    "the first parent is significant for blame",
    "objects: the good earth everything grows from",
    "want-list negotiation for every object fetch",
    "commit cost scales with change size, not repo size",
    "blobs are content-addressed by sha-256 of raw bytes",
    "sparse sparses need no materialized working copy",
    "refs move by compare-and-swap on a single row",
    "derived data is derived data: recycle history into answers",
    "git is a peer we translate losslessly, not a legacy format",
    "timestamps keep git-native seconds plus tz offset",
    "canonical encoding is a spec, not an implementation detail",
    "a cutting took root elsewhere: that is rename detection",
    "the daemon stands in the working copy and watches",
];

const ASCII_NAMES: &[&str] = &[
    "leaf", "stem", "root", "shoot", "vine", "bloom", "sprout", "cedar", "thorn", "mulch",
];
const ASCII_EXTS: &[&str] = &[".txt", ".md", ".rs", ".dat", ""];
/// Deliberately awkward-but-legal names: spaces, punctuation, a leading
/// dash, a hidden file, a case-colliding pair, and a dots-only name.
const TRICKY_NAMES: &[&str] = &[
    "space name.txt",
    "quote'and&(paren).txt",
    "hash#tag~tilde^caret!bang$dollar%percent@at+plus,comma;semi=equal.txt",
    "-leading-dash.txt",
    ".hidden",
    "UPPER.TXT",
    "upper.txt",
    "a.b.c.txt",
    "...dots",
];
const UNICODE_NAMES: &[&str] = &[
    "café.txt",
    "naïve.md",
    "日本語.txt",
    "форест.dat",
    "kōan 文字.txt",
    "emoji🌱.txt",
];
const DIR_POOL: &[&str] = &[
    "garden",
    "orchard",
    "bed",
    "sparse",
    "derived data",
    "objects",
    "trellis",
];
const UNICODE_DIRS: &[&str] = &["日本語", "grün", "форест"];

const AUTHORS: &[(&str, &str)] = &[
    ("Ada Gardener", "ada@garden.example"),
    ("Basil Root", "basil@garden.example"),
    ("Chloe Bloom", "chloe@garden.example"),
    ("Dill Sprout", "dill@garden.example"),
    ("Earl Loam", "earl@garden.example"),
];
const COMMITTERS: &[(&str, &str)] = &[
    ("Fennel Seed", "fennel@garden.example"),
    ("Ginger Root", "ginger@garden.example"),
    ("Hazel Nut", "hazel@garden.example"),
    ("Iris Bloom", "iris@garden.example"),
    ("Juniper Berry", "juniper@garden.example"),
];

/// git-native tz offsets in minutes; deliberately includes half-hour
/// (330, 90), quarter-hour (345) and large/negative offsets so raw
/// `"<secs> ±hhmm"` date handling is exercised end to end.
const TZ_POOL: &[i32] = &[0, 60, -300, 330, 345, 90, -210, 480, -480, 540, 780, -840];

/// Epoch base for generated timestamps; commits advance monotonically.
const BASE_TIME: u64 = 1_700_000_000;

#[derive(Clone, Debug)]
struct FileState {
    blob: Id,
    mode: Mode,
    /// This file's content uses CRLF line endings.
    crlf: bool,
}

/// True if `path` would collide with an existing entry: same path, or one
/// is a directory-prefix of the other (a git tree cannot represent a file
/// and a directory sharing a path).
fn conflicts_with<'a, I: IntoIterator<Item = &'a String>>(existing: I, path: &str) -> bool {
    let dir_prefix = format!("{path}/");
    existing.into_iter().any(|q| {
        q.as_str() == path || q.starts_with(&dir_prefix) || path.starts_with(&(q.clone() + "/"))
    })
}

/// Formats `tz_min` as git's raw `±hhmm` form (handles negative and
/// non-hour offsets, e.g. -330 -> "-0530", 90 -> "+0130", 345 -> "+0545").
pub fn format_tz(tz_min: i32) -> String {
    let sign = if tz_min < 0 { '-' } else { '+' };
    let a = tz_min.unsigned_abs();
    format!("{}{:02}{:02}", sign, a / 60, a % 60)
}

struct Generator {
    rng: StdRng,
    opts: GenOptions,
    /// Live working set: path -> state.
    work: BTreeMap<String, FileState>,
    /// Paths already used as `path` or `src_path` in the pending changeset.
    touched: BTreeSet<String>,
    /// Candidate paths available for adds/rename destinations.
    candidates: Vec<String>,
    /// Raw content for every blob id used so far.
    blobs: HashMap<Id, Vec<u8>>,
    /// Monotonic line counter: every text line is globally unique, which
    /// keeps generated blame ranges meaningful.
    line_no: u64,
    /// Monotonic counter for synthesized fallback paths and symlink targets.
    serial: u64,
}

impl Generator {
    fn new(seed: u64, opts: &GenOptions) -> Self {
        // Candidate pool: fixed awkward names first (so the weird shapes
        // always occur), then seeded random nested paths.
        let mut candidates: Vec<String> = TRICKY_NAMES.iter().map(|s| (*s).to_owned()).collect();
        if opts.unicode_paths {
            candidates.extend(UNICODE_NAMES.iter().map(|s| (*s).to_owned()));
        }
        let dirs: Vec<&str> = if opts.unicode_paths {
            DIR_POOL
                .iter()
                .copied()
                .chain(UNICODE_DIRS.iter().copied())
                .collect()
        } else {
            DIR_POOL.to_vec()
        };
        let max_dirs = opts.max_dirs.max(1) as usize;
        let breadth = dirs.len().min(max_dirs.max(2));
        let target = opts.max_files.max(1) as usize * 4 + 64;
        let mut path_rng = StdRng::seed_from_u64(seed ^ 0x6a09_e667_0000_0000);
        let mut attempts = 0usize;
        while candidates.len() < target && attempts < 20_000 {
            attempts += 1;
            let depth = 1 + path_rng.random_range(0..max_dirs);
            let mut comps: Vec<String> = Vec::with_capacity(depth + 1);
            for _ in 0..depth {
                comps.push(dirs[path_rng.random_range(0..breadth)].to_owned());
            }
            let name = if opts.unicode_paths && path_rng.random_bool(0.25) {
                UNICODE_NAMES[path_rng.random_range(0..UNICODE_NAMES.len())].to_owned()
            } else if path_rng.random_bool(0.12) {
                TRICKY_NAMES[path_rng.random_range(0..TRICKY_NAMES.len())].to_owned()
            } else {
                format!(
                    "{}{}",
                    ASCII_NAMES[path_rng.random_range(0..ASCII_NAMES.len())],
                    ASCII_EXTS[path_rng.random_range(0..ASCII_EXTS.len())]
                )
            };
            comps.push(name);
            candidates.push(comps.join("/"));
        }
        // Drop intra-pool duplicates and prefix conflicts ("a" vs "a/b"),
        // keeping pool order stable for determinism.
        let mut dedup: Vec<String> = Vec::new();
        for p in candidates {
            if conflicts_with(dedup.iter(), &p) {
                continue;
            }
            dedup.push(p);
        }
        Generator {
            rng: StdRng::seed_from_u64(seed),
            opts: opts.clone(),
            work: BTreeMap::new(),
            touched: BTreeSet::new(),
            candidates: dedup,
            blobs: HashMap::new(),
            line_no: 0,
            serial: 0,
        }
    }

    /// Feasible fresh paths: not live, not touched by the pending
    /// changeset, and free of prefix conflicts with the live set.
    fn fresh_paths(&self) -> Vec<String> {
        self.candidates
            .iter()
            .filter(|p| !self.touched.contains(p.as_str()) && !self.work.contains_key(p.as_str()))
            .filter(|p| !conflicts_with(self.work.keys(), p))
            .cloned()
            .collect()
    }

    /// Untouched live paths that another pending op may still target.
    fn available_existing(&self) -> Vec<String> {
        self.work
            .keys()
            .filter(|p| !self.touched.contains(p.as_str()))
            .cloned()
            .collect()
    }

    /// Falls back to a synthesized, conflict-free path when the pool is
    /// exhausted. Each serial gets a fresh top-level directory, so it can
    /// never collide with anything seen before.
    fn synthesized_path(&mut self) -> String {
        loop {
            self.serial += 1;
            let n = self.serial;
            let p = format!("evolved_{n}/renamed_{n}.txt");
            if !self.touched.contains(&p) && !conflicts_with(self.work.keys(), &p) {
                return p;
            }
        }
    }

    /// Generates one blob: `(id, raw content, crlf)`. Content is drawn
    /// from a numbered line pool so every line is globally unique.
    fn make_blob(&mut self, crlf: bool) -> (Id, Vec<u8>, bool) {
        let content: Vec<u8> = if self.opts.empty_files && self.rng.random_bool(0.12) {
            Vec::new()
        } else if self.opts.big_blob_bytes.is_some() && self.rng.random_bool(0.04) {
            let n = self.opts.big_blob_bytes.unwrap_or(0);
            self.big_content(n)
        } else {
            let mut n_lines = self.rng.random_range(1..=40usize);
            if self.rng.random_bool(0.05) {
                n_lines += 120;
            }
            let mut lines: Vec<String> = Vec::with_capacity(n_lines);
            for _ in 0..n_lines {
                self.line_no += 1;
                let pick = LINE_POOL[self.rng.random_range(0..LINE_POOL.len())];
                lines.push(format!("{:06}: {}", self.line_no, pick));
            }
            let mut body = lines.join(if crlf { "\r\n" } else { "\n" });
            if self.rng.random_bool(0.9) {
                body.push_str(if crlf { "\r\n" } else { "\n" });
            }
            body.into_bytes()
        };
        let id = blob_id(&content);
        (id, content, crlf)
    }

    /// Exactly `n` ASCII bytes of line-pool text (used for large blobs).
    fn big_content(&mut self, n: usize) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(n.min(1 << 20) + 64);
        while out.len() < n {
            self.line_no += 1;
            let pick = LINE_POOL[self.rng.random_range(0..LINE_POOL.len())];
            out.extend_from_slice(format!("{:06}: {}\n", self.line_no, pick).as_bytes());
        }
        out.truncate(n);
        out
    }

    /// A new submodule pointer: 12 zero bytes + 20 pseudo-random bytes
    /// (the upstream git SHA-1), padded per the canonical spec.
    fn make_gitlink(&mut self) -> (Id, Vec<u8>) {
        let mut padded = [0u8; 32];
        self.rng.fill_bytes(&mut padded[12..]);
        let id = Id::from_bytes(padded);
        (id, padded.to_vec())
    }

    /// Rolls a file mode for a new/changed entry.
    fn roll_mode(&mut self) -> Mode {
        let g: f64 = self.rng.random();
        let p_link = GITLINK_RATIO;
        let p_sym = clamp01(self.opts.symlink_ratio);
        let p_exec = clamp01(self.opts.exec_ratio);
        if g < p_link {
            Mode::Gitlink
        } else if g < p_link + p_sym {
            Mode::Symlink
        } else if g < p_link + p_sym + p_exec {
            Mode::Exec
        } else {
            Mode::Regular
        }
    }

    /// Produces `(id, content)` for a fresh blob of the given mode,
    /// guaranteeing `id != avoid` when `avoid` is set (a `Modify` must
    /// actually change content, otherwise git's diff would disagree with
    /// the changeset's claimed modify).
    fn fresh_blob_for_mode(&mut self, mode: Mode, crlf: bool, avoid: Option<Id>) -> (Id, Vec<u8>) {
        for _ in 0..16 {
            let (id, content) = match mode {
                Mode::Gitlink => self.make_gitlink(),
                Mode::Symlink => {
                    self.serial += 1;
                    let content = format!("sparse/target_{}.txt", self.serial).into_bytes();
                    (blob_id(&content), content)
                }
                Mode::Regular | Mode::Exec => {
                    let (id, content, _) = self.make_blob(crlf);
                    (id, content)
                }
            };
            if avoid != Some(id) {
                return (id, content);
            }
        }
        // Statistically unreachable; force a difference deterministically
        // by appending the avoided id's first bytes to the content.
        let avoid = avoid.unwrap_or_else(|| Id::from_bytes([0u8; 32]));
        let mut bytes = vec![b'f', b'o', b'r', b'c', b'e', b'd'];
        bytes.extend_from_slice(&avoid.as_bytes()[..8]);
        (blob_id(&bytes), bytes)
    }

    /// One identity pair (author, committer) for changeset index `i`.
    fn make_identities(&mut self, i: u32) -> (Identity, Identity) {
        let (an, ae) = AUTHORS[self.rng.random_range(0..AUTHORS.len())];
        let (cn, ce) = COMMITTERS[self.rng.random_range(0..COMMITTERS.len())];
        let t_a = BASE_TIME + i as u64 * 3600 + self.rng.random_range(0..600u64);
        let t_c = t_a + self.rng.random_range(0..120u64);
        let tz_a = TZ_POOL[self.rng.random_range(0..TZ_POOL.len())];
        let tz_c = TZ_POOL[self.rng.random_range(0..TZ_POOL.len())];
        (
            Identity {
                name: (*an).to_owned(),
                email: (*ae).to_owned(),
                time_secs: t_a,
                tz_min: tz_a,
            },
            Identity {
                name: (*cn).to_owned(),
                email: (*ce).to_owned(),
                time_secs: t_c,
                tz_min: tz_c,
            },
        )
    }

    /// Generates the changes for one changeset. `force_adds` seeds the
    /// root changeset with a batch of adds; merges get a smaller budget
    /// (so pure merges occur naturally).
    fn make_changes(&mut self, merge: bool) -> Vec<Change> {
        let max_files = self.opts.max_files.max(1) as usize;
        let n_ops: usize = if merge {
            self.rng.random_range(0..=4)
        } else {
            self.rng.random_range(1..=8)
        };
        let p_del = clamp01(self.opts.delete_ratio);
        let p_ren = clamp01(self.opts.rename_ratio);
        let mut changes: Vec<Change> = Vec::new();
        self.touched.clear();

        for _ in 0..n_ops {
            let existing = self.available_existing();
            let fresh = self.fresh_paths();
            // Choose an operation. The working set must never go empty and
            // adds need headroom under max_files.
            let want: Op = if existing.is_empty() && self.work.is_empty() {
                Op::Add
            } else {
                let u: f64 = self.rng.random();
                if u < p_del && !existing.is_empty() {
                    Op::Delete
                } else if u < p_del + p_ren && !existing.is_empty() && !fresh.is_empty() {
                    Op::Rename
                } else if self.work.len() < max_files && !fresh.is_empty() {
                    Op::Add
                } else if !existing.is_empty() {
                    Op::Modify
                } else {
                    Op::Add
                }
            };

            match want {
                Op::Delete => {
                    let path = self.pick(&existing);
                    if let Some(state) = self.work.remove(&path) {
                        self.touched.insert(path.clone());
                        // Canonical rule: deletes carry mode Regular and no
                        // blob/src_path, regardless of the file's old mode.
                        changes.push(Change {
                            path,
                            op: Op::Delete,
                            blob: None,
                            src_path: None,
                            mode: Mode::Regular,
                        });
                        let _ = state;
                    }
                }
                Op::Rename => {
                    if existing.is_empty() || fresh.is_empty() {
                        continue;
                    }
                    let src = self.pick(&existing);
                    let dst = self.pick(&fresh);
                    let Some(state) = self.work.get(&src).cloned() else {
                        continue;
                    };
                    self.touched.insert(src.clone());
                    self.touched.insert(dst.clone());
                    // A pure rename reuses the source blob id (canonical
                    // rule); sometimes content changes at the same time.
                    let (blob, mode) = if self.rng.random_bool(0.25) {
                        let (id, content) =
                            self.fresh_blob_for_mode(state.mode, state.crlf, Some(state.blob));
                        self.blobs.insert(id, content);
                        (id, state.mode)
                    } else {
                        (state.blob, state.mode)
                    };
                    self.work.remove(&src);
                    self.work.insert(
                        dst.clone(),
                        FileState {
                            blob,
                            mode,
                            crlf: state.crlf,
                        },
                    );
                    changes.push(Change {
                        path: dst,
                        op: Op::Rename,
                        blob: Some(blob),
                        src_path: Some(src),
                        mode,
                    });
                }
                Op::Add => {
                    let path = if fresh.is_empty() {
                        self.synthesized_path()
                    } else {
                        self.pick(&fresh)
                    };
                    let crlf = self.opts.crlf_content && self.rng.random_bool(0.35);
                    let mode = self.roll_mode();
                    let (blob, content) = self.fresh_blob_for_mode(mode, crlf, None);
                    self.blobs.insert(blob, content);
                    self.touched.insert(path.clone());
                    self.work
                        .insert(path.clone(), FileState { blob, mode, crlf });
                    changes.push(Change {
                        path,
                        op: Op::Add,
                        blob: Some(blob),
                        src_path: None,
                        mode,
                    });
                }
                Op::Modify => {
                    let Some(path) = (!existing.is_empty()).then(|| self.pick(&existing)) else {
                        continue;
                    };
                    let Some(state) = self.work.get(&path).cloned() else {
                        continue;
                    };
                    self.touched.insert(path.clone());
                    // Small chance of a mode-only or mode+content change
                    // (exec-bit flips are classic diff/blame edge cases);
                    // symlinks and gitlinks always change content instead.
                    let flip_mode = self.rng.random_bool(0.06)
                        && matches!(state.mode, Mode::Regular | Mode::Exec);
                    let mode = if flip_mode {
                        if state.mode == Mode::Regular {
                            Mode::Exec
                        } else {
                            Mode::Regular
                        }
                    } else {
                        state.mode
                    };
                    let (blob, content) = if flip_mode && self.rng.random_bool(0.5) {
                        (state.blob, None) // mode-only modify: content unchanged
                    } else {
                        let (id, content) =
                            self.fresh_blob_for_mode(mode, state.crlf, Some(state.blob));
                        (id, Some(content))
                    };
                    if let Some(content) = content {
                        self.blobs.insert(blob, content);
                    }
                    self.work.insert(
                        path.clone(),
                        FileState {
                            blob,
                            mode,
                            crlf: state.crlf,
                        },
                    );
                    changes.push(Change {
                        path,
                        op: Op::Modify,
                        blob: Some(blob),
                        src_path: None,
                        mode,
                    });
                }
            }
        }
        // Canonical form: strictly ascending by path bytes, no duplicates
        // (the generator guarantees disjointness; the sort makes the
        // in-memory struct match the encoded order exactly).
        changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        changes
    }

    /// Picks a uniformly random element (consuming rng deterministically).
    fn pick<T: Clone>(&mut self, items: &[T]) -> T {
        items[self.rng.random_range(0..items.len())].clone()
    }

    /// Seeds the root changeset: a batch of adds constituting the initial
    /// working set (about half of `max_files`, at least 1 file).
    fn seed_root(&mut self) -> Vec<Change> {
        let max_files = self.opts.max_files.max(1) as usize;
        let n = (max_files / 2 + 1).min(max_files);
        let mut changes: Vec<Change> = Vec::new();
        self.touched.clear();
        for _ in 0..n {
            let path = if self.work.len() >= max_files {
                break;
            } else {
                match self.fresh_paths().pop() {
                    Some(p) => p,
                    None => self.synthesized_path(),
                }
            };
            if self.touched.contains(&path) || conflicts_with(self.work.keys(), &path) {
                continue;
            }
            let crlf = self.opts.crlf_content && self.rng.random_bool(0.35);
            let mode = self.roll_mode();
            let (blob, content) = self.fresh_blob_for_mode(mode, crlf, None);
            self.blobs.insert(blob, content);
            self.touched.insert(path.clone());
            self.work
                .insert(path.clone(), FileState { blob, mode, crlf });
            changes.push(Change {
                path,
                op: Op::Add,
                blob: Some(blob),
                src_path: None,
                mode,
            });
        }
        changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        changes
    }
}

fn clamp01(x: f64) -> f64 {
    x.clamp(0.0, 1.0)
}

/// Generates a deterministic synthetic history.
///
/// Pure function of `(seed, opts)`: the same inputs always produce
/// byte-identical changesets and blob contents. `opts.commits` is clamped
/// to at least 1 (a history always has a root changeset so
/// [`GeneratedHistory::head`] is always meaningful).
pub fn generate(seed: u64, opts: &GenOptions) -> GeneratedHistory {
    let mut g = Generator::new(seed, opts);
    let commits = opts.commits.max(1);
    let mut changesets: Vec<Changeset> = Vec::with_capacity(commits as usize);

    for i in 0..commits {
        let (author, committer) = g.make_identities(i);
        let is_merge = i >= 2 && g.rng.random_bool(clamp01(opts.merge_ratio));

        // Parent selection: first parent is always the previous changeset
        // (linear mainline); merges add 1-2 extra parents from a recent
        // window, excluding the immediate predecessor.
        let mut parents: Vec<Id> = Vec::new();
        let mut merge = false;
        if i > 0 {
            parents.push(id_of(&changesets[i as usize - 1]));
        }
        if is_merge {
            let lo = i.saturating_sub(MERGE_PARENT_WINDOW as u32 + 1);
            let candidates: Vec<usize> = (lo as usize..(i - 1) as usize).collect();
            if !candidates.is_empty() {
                merge = true;
                let j = g.rng.random_range(0..candidates.len());
                let octopus =
                    candidates.len() >= 2 && g.rng.random_bool(clamp01(opts.octopus_ratio));
                if octopus {
                    let k =
                        (j + 1 + g.rng.random_range(0..candidates.len() - 1)) % candidates.len();
                    // Push the two extra parents in ascending changeset
                    // order (deterministic; Id carries no Ord by design).
                    let (a, b) = if j < k {
                        (candidates[j], candidates[k])
                    } else {
                        (candidates[k], candidates[j])
                    };
                    parents.push(id_of(&changesets[a]));
                    parents.push(id_of(&changesets[b]));
                } else {
                    parents.push(id_of(&changesets[candidates[j]]));
                }
            }
        }

        let changes = if i == 0 {
            g.seed_root()
        } else {
            g.make_changes(merge)
        };

        let message = if merge {
            format!("seed {i}: merge side work into mainline")
        } else if i == 0 {
            "seed 0: plant the initial bed".to_owned()
        } else {
            format!("seed {i}: touch {} path(s)", changes.len())
        };
        let message = if i > 0 && g.rng.random_bool(0.2) {
            format!("{message}\n\nCompost body for seed {i}: generated by gardenbed-harness.")
        } else {
            message
        };

        let cs = Changeset {
            parents,
            changes,
            message,
            author,
            committer,
            signature: None,
        };
        changesets.push(cs);
    }

    // Head is the id of the last changeset; the unwrap-style helper is on
    // our own construction and covered by the determinism test suite.
    let head = changesets
        .last()
        .map(id_of)
        .expect("history always has at least one changeset");
    let blob_contents = {
        let mut v: Vec<(Id, Vec<u8>)> = g.blobs.into_iter().collect();
        v.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        v
    };
    GeneratedHistory {
        changesets,
        blob_contents,
        head,
    }
}

/// Id of a generated changeset. Generated changesets are canonically
/// valid by construction, so this cannot fail; a failure would be a
/// harness bug (reproducible via the deterministic seed) rather than bad
/// user input.
fn id_of(cs: &Changeset) -> Id {
    cs.id()
        .unwrap_or_else(|e| panic!("harness bug: generated changeset failed to encode: {e}"))
}

/// Replays changesets `0..=index` to answer "what files exist at
/// changeset `index`": path -> (blob id, mode), in canonical
/// (byte-ascending) order because of the `BTreeMap`.
///
/// Because every changeset's changes are defined against its first parent
/// and generated histories are first-parent-linear, sequential replay
/// reproduces each changeset's tree exactly. If `index` is out of bounds
/// an empty map is returned (documented, not silent success: callers
/// comparing against git will see a mismatch).
pub fn files_at(history: &GeneratedHistory, index: usize) -> BTreeMap<String, (Id, Mode)> {
    let mut out: BTreeMap<String, (Id, Mode)> = BTreeMap::new();
    if index >= history.changesets.len() {
        return out;
    }
    for cs in &history.changesets[..=index] {
        for c in &cs.changes {
            match c.op {
                Op::Delete => {
                    out.remove(&c.path);
                }
                Op::Rename => {
                    if let Some(src) = &c.src_path {
                        out.remove(src);
                    }
                    if let Some(b) = c.blob {
                        out.insert(c.path.clone(), (b, c.mode));
                    }
                }
                Op::Add | Op::Modify => {
                    if let Some(b) = c.blob {
                        out.insert(c.path.clone(), (b, c.mode));
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tz_formatting_covers_offset_shapes() {
        assert_eq!(format_tz(0), "+0000");
        assert_eq!(format_tz(60), "+0100");
        assert_eq!(format_tz(-330), "-0530"); // India (negative variant)
        assert_eq!(format_tz(330), "+0530"); // half-hour east
        assert_eq!(format_tz(90), "+0130"); // historical half-hour
        assert_eq!(format_tz(345), "+0545"); // quarter-hour (Nepal)
        assert_eq!(format_tz(-210), "-0330");
        assert_eq!(format_tz(-840), "-1400"); // line-islands negative edge
        assert_eq!(format_tz(780), "+1300");
    }

    #[test]
    fn small_history_is_valid_and_canonical() {
        let opts = GenOptions {
            commits: 12,
            merge_ratio: 0.3,
            octopus_ratio: 0.2,
            rename_ratio: 0.3,
            delete_ratio: 0.2,
            max_files: 8,
            max_dirs: 3,
            unicode_paths: true,
            crlf_content: true,
            empty_files: true,
            big_blob_bytes: Some(4096),
            ..Default::default()
        };
        let h = generate(1, &opts);
        assert_eq!(h.changesets.len(), 12);
        // Canonical rules hold (decode enforces sorted order + op table).
        for cs in &h.changesets {
            let bytes = cs.canonical_bytes().expect("generated changeset encodes");
            let back = Changeset::decode(&bytes).expect("generated changeset decodes");
            assert_eq!(&back, cs);
        }
        // Every blob id used is present in the contents map.
        for cs in &h.changesets {
            for c in &cs.changes {
                if let Some(b) = c.blob {
                    assert!(h.blob(&b).is_some(), "blob {b} missing from contents");
                }
            }
        }
        // Head is the last changeset's id.
        assert_eq!(h.head, h.changesets.last().unwrap().id().unwrap());
    }
}
