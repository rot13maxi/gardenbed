//! A faithful port of git's xdiff Myers engine — the line diff that blame's
//! line-identity tracking is anchored to.
//!
//! Blame inherits lines through the *diff* between a changeset's blob and
//! its base blob: a line present in both versions is NOT automatically
//! "unchanged" — identity is whatever the diff pairs up. To make
//! [`crate::Engine::blame`] agree with `git blame --first-parent`
//! line-for-line, this module reimplements git's exact algorithm (the
//! xdiff engine as of git 2.43; behavior modeled on its published
//! sources):
//!
//! 1. **classification** — lines (records, including their trailing `\n`)
//!    map to equivalence classes by exact byte equality, matching git's
//!    `xdl_classify_record`. A line `foo` at EOF (no trailing newline) is
//!    a different record from `foo\n`, exactly as in git.
//! 2. **trim** (`xdl_trim_ends`) — common prefix/suffix cut.
//! 3. **record cleanup** (`xdl_cleanup_records`) — lines with no
//!    counterpart in the other file are pre-marked changed and removed
//!    from the search box; "multimatch" lines (count ≥ `bogosqrt(n)` in
//!    the other file) are discarded when they sit inside runs of
//!    discardable lines (`xdl_clean_mmatch`).
//! 4. **Myers O(ND) divide & conquer** (`xdl_split` + `xdl_recs_cmp`) —
//!    including git's snake-sampling heuristic, its cost cap
//!    (`mxcost = bogosqrt(ndiags)`, min 256) and its furthest-reaching
//!    suboptimal-split fallback, so boundary behavior matches too.
//! 5. **change compaction** (`xdl_change_compact`) — change groups slide
//!    as far as possible, preferring alignment with the other file's
//!    changes, then the **indent heuristic** scores slide positions (the
//!    git default since 2.14; blame inherits `XDF_INDENT_HEURISTIC`).
//!
//! Unchanged lines pair up 1:1 in order (git's `xdl_build_script`
//! invariant) — exactly the line identity blame needs.
//!
//! ## Bounds
//!
//! Inputs beyond [`MAX_DIFF_LINES`] records, or a search exceeding
//! [`MAX_DIFF_EFFORT`] elementary steps, return `None`; the caller falls
//! back to whole-file attribution (every new-side line attributed to the
//! changing changeset). This is a documented divergence from git for
//! pathological sizes only — the same reason git's own cost caps exist
//! (Myers is O((N+M)·D) worst case).
//!
//! Everything is deterministic: classes are content-keyed (no hash
//! collisions), no float math, no iteration-order dependence — wiped and
//! rebuilt caches reproduce byte-identical answers (M6.3).

/// Refuse to diff when either side has more records than this; the
/// caller falls back to whole-file attribution (documented divergence
/// beyond the bound).
pub const MAX_DIFF_LINES: usize = 400_000;

/// Refuse to continue a Myers search beyond this many elementary steps
/// (diagonal extensions plus snake comparisons).
const MAX_DIFF_EFFORT: u64 = 64_000_000;

/// A completed line diff: which lines of each side are "changed".
///
/// Unchanged lines on both sides pair up in order: the k-th unchanged old
/// line is the ancestor of the k-th unchanged new line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineDiff {
    /// Per old-side line: the diff considers it removed.
    pub changed_old: Vec<bool>,
    /// Per new-side line: the diff considers it added.
    pub changed_new: Vec<bool>,
}

/// Splits blob bytes into git-style records: each line includes its
/// trailing `\n`; the final line may lack one. Empty content has no
/// records.
pub fn split_records(content: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, &b) in content.iter().enumerate() {
        if b == b'\n' {
            out.push(&content[start..=i]);
            start = i + 1;
        }
    }
    if start < content.len() {
        out.push(&content[start..]);
    }
    out
}

struct Ctx<'a> {
    /// Record byte slices (git's `recs`).
    recs: Vec<&'a [u8]>,
    /// Equivalence class id per record (git's `ha`).
    cls: Vec<u32>,
    /// Changed flags; `rchg(i)` applies git's sentinel semantics (indices
    /// -1 and nrec read as `false`).
    rchg: Vec<bool>,
    dstart: isize,
    /// Inclusive end of the un-trimmed window; `-1` when there are no
    /// records (git's `dend = nrec - 1`).
    dend: isize,
}

impl Ctx<'_> {
    fn nrec(&self) -> usize {
        self.recs.len()
    }
    fn rchg(&self, i: i64) -> bool {
        if i < 0 || i as usize >= self.rchg.len() {
            return false;
        }
        self.rchg[i as usize]
    }
    fn set_rchg(&mut self, i: usize, v: bool) {
        self.rchg[i] = v;
    }
}

/// Runs the full pipeline. Returns `None` past the documented bounds.
pub fn diff_lines(a: &[&[u8]], b: &[&[u8]]) -> Option<LineDiff> {
    if a.len() > MAX_DIFF_LINES || b.len() > MAX_DIFF_LINES {
        return None;
    }
    let (mut cx1, mut cx2) = classify_pair(a, b);

    trim_ends(&mut cx1, &mut cx2);

    let (rindex1, ha1) = cleanup_records(&mut cx1, &cx2);
    let (rindex2, ha2) = cleanup_records(&mut cx2, &cx1);

    let mut env = Env {
        effort: 0,
        cx1: &mut cx1,
        cx2: &mut cx2,
    };
    // The initial `need_min` mirrors git's `(xpp->flags & XDF_NEED_MINIMAL)
    // != 0`; blame leaves that flag unset, so the snake heuristic and the
    // cost cap are active exactly as in git.
    env.recs_cmp(
        &rindex1,
        &ha1,
        &rindex2,
        &ha2,
        0,
        ha1.len() as i64,
        0,
        ha2.len() as i64,
        false,
    )?;

    // Change compaction, both directions, in git's order. The indent
    // heuristic flag mirrors git's default (blame inherits it).
    change_compact(env.cx1, env.cx2, true);
    change_compact(env.cx2, env.cx1, true);

    Some(LineDiff {
        changed_old: cx1.rchg[..cx1.nrec()].to_vec(),
        changed_new: cx2.rchg[..cx2.nrec()].to_vec(),
    })
}

// -------------------------------------------------------- classification

/// Classifies both files against ONE shared equivalence map (git's
/// classifier is shared, so class ids are comparable across sides).
fn classify_pair<'a>(a: &[&'a [u8]], b: &[&'a [u8]]) -> (Ctx<'a>, Ctx<'a>) {
    let mut map: std::collections::HashMap<&[u8], u32> = std::collections::HashMap::new();
    fn ctx_of<'a>(
        recs: &[&'a [u8]],
        map: &mut std::collections::HashMap<&'a [u8], u32>,
    ) -> Ctx<'a> {
        let mut cls = Vec::with_capacity(recs.len());
        for r in recs {
            let next = map.len() as u32;
            let c = *map.entry(r).or_insert(next);
            cls.push(c);
        }
        Ctx {
            recs: recs.to_vec(),
            cls,
            rchg: vec![false; recs.len() + 2],
            dstart: 0,
            dend: recs.len() as isize - 1,
        }
    }
    let cx1 = ctx_of(a, &mut map);
    let cx2 = ctx_of(b, &mut map);
    (cx1, cx2)
}

fn trim_ends(cx1: &mut Ctx, cx2: &mut Ctx) {
    let lim = cx1.nrec().min(cx2.nrec());
    let mut i = 0usize;
    while i < lim && cx1.cls[i] == cx2.cls[i] {
        i += 1;
    }
    cx1.dstart = i as isize;
    cx2.dstart = i as isize;
    let lim2 = lim - i;
    let mut j = 0usize;
    while j < lim2 && cx1.cls[cx1.nrec() - 1 - j] == cx2.cls[cx2.nrec() - 1 - j] {
        j += 1;
    }
    cx1.dend = cx1.nrec() as isize - j as isize - 1;
    cx2.dend = cx2.nrec() as isize - j as isize - 1;
}

// ------------------------------------------------------ record cleanup

/// Integer sqrt approximation identical to git's `xdl_bogosqrt`.
fn bogosqrt(mut n: i64) -> i64 {
    let mut i: i64 = 1;
    while n > 0 {
        n >>= 2;
        i <<= 1;
    }
    i
}

/// git's `xdl_clean_mmatch`: decides whether a multimatch line inside a
/// run of nomatch/discardable lines is itself discardable.
fn clean_mmatch(dis: &[u8], i: usize, s: isize, e: isize) -> bool {
    const XDL_SIMSCAN_WINDOW: isize = 100;
    const XDL_KPDIS_RUN: usize = 4;

    let mut s = s;
    let mut e = e;
    if i as isize - s > XDL_SIMSCAN_WINDOW {
        s = i as isize - XDL_SIMSCAN_WINDOW;
    }
    if e - i as isize > XDL_SIMSCAN_WINDOW {
        e = i as isize + XDL_SIMSCAN_WINDOW;
    }

    // Run of nomatch/multimatch lines before `i`.
    let mut rdis0 = 0usize;
    let mut rpdis0 = 1usize;
    let mut r = 1isize;
    while i as isize - r >= s {
        match dis[i - r as usize] {
            0 => rdis0 += 1,
            2 => rpdis0 += 1,
            _ => break,
        }
        r += 1;
    }
    if rdis0 == 0 {
        return false;
    }
    // ... and after `i`.
    let mut rdis1 = 0usize;
    let mut rpdis1 = 1usize;
    let mut r = 1isize;
    while i as isize + r <= e {
        match dis[i + r as usize] {
            0 => rdis1 += 1,
            2 => rpdis1 += 1,
            _ => break,
        }
        r += 1;
    }
    if rdis1 == 0 {
        return false;
    }
    let rdis1 = rdis1 + rdis0;
    let rpdis1 = rpdis1 + rpdis0;
    rpdis1 * XDL_KPDIS_RUN < rpdis1 + rdis1
}

/// git's `xdl_cleanup_records` for one side: pre-marks lines with no
/// counterpart changed, drops discardable multimatches from the box, and
/// returns the reduced index map plus reduced class array.
fn cleanup_records(cx: &mut Ctx, other: &Ctx) -> (Vec<usize>, Vec<u32>) {
    let nrec = cx.nrec();
    if nrec == 0 {
        return (Vec::new(), Vec::new());
    }
    let mut other_count: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    for &c in &other.cls {
        *other_count.entry(c).or_insert(0) += 1;
    }
    let mlim = bogosqrt(nrec as i64).min(1024);
    let mut dis = vec![0u8; nrec];
    for i in cx.dstart..=cx.dend {
        let nm = other_count.get(&cx.cls[i as usize]).copied().unwrap_or(0);
        dis[i as usize] = if nm == 0 {
            0
        } else if i64::from(nm) >= mlim {
            2
        } else {
            1
        };
    }
    let mut rindex = Vec::new();
    let mut ha = Vec::new();
    for i in cx.dstart..=cx.dend {
        let iu = i as usize;
        let keep = dis[iu] == 1 || (dis[iu] == 2 && !clean_mmatch(&dis, iu, cx.dstart, cx.dend));
        if keep {
            rindex.push(iu);
            ha.push(cx.cls[iu]);
        } else {
            cx.set_rchg(iu, true);
        }
    }
    (rindex, ha)
}

// --------------------------------------------------- Myers divide & conquer

const XDL_LINE_MAX: i64 = i64::MAX;
const XDL_MAX_COST_MIN: i64 = 256;
const XDL_HEUR_MIN_COST: i64 = 256;
const XDL_SNAKE_CNT: i64 = 20;
const XDL_K_HEUR: i64 = 4;

struct Split {
    i1: i64,
    i2: i64,
    min_lo: bool,
    min_hi: bool,
}

/// Diagonal-indexed value arrays with git's layout (offset by
/// `nreff2 + 1` so negative diagonals index fine).
struct KV {
    v: Vec<i64>,
    base: i64,
}

impl KV {
    fn get(&self, d: i64) -> i64 {
        self.v[(d + self.base) as usize]
    }
    fn set(&mut self, d: i64, x: i64) {
        self.v[(d + self.base) as usize] = x;
    }
}

struct Env<'e, 'a> {
    effort: u64,
    cx1: &'e mut Ctx<'a>,
    cx2: &'e mut Ctx<'a>,
}

impl Env<'_, '_> {
    /// git's `xdl_split`, ported statement-for-statement over the reduced
    /// class arrays.
    #[allow(clippy::too_many_arguments)]
    fn split(
        &mut self,
        ha1: &[u32],
        ha2: &[u32],
        off1: i64,
        lim1: i64,
        off2: i64,
        lim2: i64,
        kvdf: &mut KV,
        kvdb: &mut KV,
        need_min: bool,
        spl: &mut Split,
    ) -> Option<i64> {
        let dmin: i64 = off1 - lim2;
        let dmax: i64 = lim1 - off2;
        let fmid: i64 = off1 - off2;
        let bmid: i64 = lim1 - lim2;
        let odd = (fmid - bmid) & 1;
        let mut fmin: i64 = fmid;
        let mut fmax: i64 = fmid;
        let mut bmin: i64 = bmid;
        let mut bmax: i64 = bmid;

        let ndiags = (ha1.len() + ha2.len() + 3) as i64;
        let mxcost = bogosqrt(ndiags).max(XDL_MAX_COST_MIN);
        let heur_min = XDL_HEUR_MIN_COST;
        let snake_cnt = XDL_SNAKE_CNT;

        kvdf.set(fmid, off1);
        kvdb.set(bmid, lim1);

        let mut ec: i64 = 1;
        loop {
            let mut got_snake = false;

            if fmin > dmin {
                fmin -= 1;
                kvdf.set(fmin - 1, -1);
            } else {
                fmin += 1;
            }
            if fmax < dmax {
                fmax += 1;
                kvdf.set(fmax + 1, -1);
            } else {
                fmax -= 1;
            }

            let mut d = fmax;
            while d >= fmin {
                let mut i1 = if kvdf.get(d - 1) >= kvdf.get(d + 1) {
                    kvdf.get(d - 1) + 1
                } else {
                    kvdf.get(d + 1)
                };
                let prev1 = i1;
                let mut i2 = i1 - d;
                while i1 < lim1 && i2 < lim2 && ha1[i1 as usize] == ha2[i2 as usize] {
                    i1 += 1;
                    i2 += 1;
                    self.effort += 1;
                }
                if i1 - prev1 > snake_cnt {
                    got_snake = true;
                }
                kvdf.set(d, i1);
                if odd != 0 && bmin <= d && d <= bmax && kvdb.get(d) <= i1 {
                    spl.i1 = i1;
                    spl.i2 = i2;
                    spl.min_lo = true;
                    spl.min_hi = true;
                    return Some(ec);
                }
                self.effort += 1;
                d -= 2;
            }

            if bmin > dmin {
                bmin -= 1;
                kvdb.set(bmin - 1, XDL_LINE_MAX);
            } else {
                bmin += 1;
            }
            if bmax < dmax {
                bmax += 1;
                kvdb.set(bmax + 1, XDL_LINE_MAX);
            } else {
                bmax -= 1;
            }

            let mut d = bmax;
            while d >= bmin {
                let mut i1 = if kvdb.get(d - 1) < kvdb.get(d + 1) {
                    kvdb.get(d - 1)
                } else {
                    kvdb.get(d + 1) - 1
                };
                let prev1 = i1;
                let mut i2 = i1 - d;
                while i1 > off1 && i2 > off2 && ha1[(i1 - 1) as usize] == ha2[(i2 - 1) as usize] {
                    i1 -= 1;
                    i2 -= 1;
                    self.effort += 1;
                }
                if prev1 - i1 > snake_cnt {
                    got_snake = true;
                }
                kvdb.set(d, i1);
                if odd == 0 && fmin <= d && d <= fmax && i1 <= kvdf.get(d) {
                    spl.i1 = i1;
                    spl.i2 = i2;
                    spl.min_lo = true;
                    spl.min_hi = true;
                    return Some(ec);
                }
                self.effort += 1;
                d -= 2;
            }

            if self.effort > MAX_DIFF_EFFORT {
                return None; // documented bound: caller falls back
            }

            if need_min {
                ec += 1;
                continue;
            }

            // Snake-sampling heuristic (git's, verbatim).
            if got_snake && ec > heur_min {
                let mut best: i64 = 0;
                let mut d = fmax;
                while d >= fmin {
                    let dd = if d > fmid { d - fmid } else { fmid - d };
                    let i1 = kvdf.get(d);
                    let i2 = i1 - d;
                    let v = (i1 - off1) + (i2 - off2) - dd;
                    if v > XDL_K_HEUR * ec
                        && v > best
                        && off1 + snake_cnt <= i1
                        && i1 < lim1
                        && off2 + snake_cnt <= i2
                        && i2 < lim2
                    {
                        let mut k: i64 = 1;
                        while ha1[(i1 - k) as usize] == ha2[(i2 - k) as usize] {
                            if k == snake_cnt {
                                best = v;
                                spl.i1 = i1;
                                spl.i2 = i2;
                                break;
                            }
                            k += 1;
                        }
                    }
                    d -= 2;
                }
                if best > 0 {
                    spl.min_lo = true;
                    spl.min_hi = false;
                    return Some(ec);
                }

                let mut best: i64 = 0;
                let mut d = bmax;
                while d >= bmin {
                    let dd = if d > bmid { d - bmid } else { bmid - d };
                    let i1 = kvdb.get(d);
                    let i2 = i1 - d;
                    let v = (lim1 - i1) + (lim2 - i2) - dd;
                    if v > XDL_K_HEUR * ec
                        && v > best
                        && off1 < i1
                        && i1 <= lim1 - snake_cnt
                        && off2 < i2
                        && i2 <= lim2 - snake_cnt
                    {
                        let mut k: i64 = 0;
                        while ha1[(i1 + k) as usize] == ha2[(i2 + k) as usize] {
                            if k == snake_cnt - 1 {
                                best = v;
                                spl.i1 = i1;
                                spl.i2 = i2;
                                break;
                            }
                            k += 1;
                        }
                    }
                    d -= 2;
                }
                if best > 0 {
                    spl.min_lo = false;
                    spl.min_hi = true;
                    return Some(ec);
                }
            }

            // Cost cap: collect the furthest-reaching path (git's fallback).
            if ec >= mxcost {
                let mut fbest: i64 = -1;
                let mut fbest1: i64 = -1;
                let mut d = fmax;
                while d >= fmin {
                    let mut i1 = kvdf.get(d).min(lim1);
                    let mut i2 = i1 - d;
                    if lim2 < i2 {
                        i1 = lim2 + d;
                        i2 = lim2;
                    }
                    if fbest < i1 + i2 {
                        fbest = i1 + i2;
                        fbest1 = i1;
                    }
                    d -= 2;
                }
                let mut bbest: i64 = i64::MAX;
                let mut bbest1: i64 = i64::MAX;
                let mut d = bmax;
                while d >= bmin {
                    let mut i1 = kvdb.get(d).max(off1);
                    let mut i2 = i1 - d;
                    if i2 < off2 {
                        i1 = off2 + d;
                        i2 = off2;
                    }
                    if i1 + i2 < bbest {
                        bbest = i1 + i2;
                        bbest1 = i1;
                    }
                    d -= 2;
                }
                if (lim1 + lim2) - bbest < fbest - (off1 + off2) {
                    spl.i1 = fbest1;
                    spl.i2 = fbest - fbest1;
                    spl.min_lo = true;
                    spl.min_hi = false;
                } else {
                    spl.i1 = bbest1;
                    spl.i2 = bbest - bbest1;
                    spl.min_lo = false;
                    spl.min_hi = true;
                }
                return Some(ec);
            }
            ec += 1;
        }
    }

    /// git's `xdl_recs_cmp`, iterative over an explicit stack of boxes
    /// (the C version recurses; the sub-boxes are independent).
    #[allow(clippy::too_many_arguments)]
    fn recs_cmp(
        &mut self,
        rindex1: &[usize],
        ha1: &[u32],
        rindex2: &[usize],
        ha2: &[u32],
        off1: i64,
        lim1: i64,
        off2: i64,
        lim2: i64,
        need_min: bool,
    ) -> Option<()> {
        let nreff2 = ha2.len() as i64;
        let ndiags = ha1.len() + ha2.len() + 3;
        let mut kvdf = KV {
            v: vec![0; ndiags + 2],
            base: nreff2 + 1,
        };
        let mut kvdb = KV {
            v: vec![0; ndiags + 2],
            base: nreff2 + 1,
        };

        let mut stack: Vec<(i64, i64, i64, i64, bool)> = vec![(off1, lim1, off2, lim2, need_min)];
        while let Some((mut off1, mut lim1, mut off2, mut lim2, need_min)) = stack.pop() {
            // Shrink the box by walking each diagonal snake (SW and NE).
            while off1 < lim1 && off2 < lim2 && ha1[off1 as usize] == ha2[off2 as usize] {
                off1 += 1;
                off2 += 1;
            }
            while off1 < lim1 && off2 < lim2 && ha1[(lim1 - 1) as usize] == ha2[(lim2 - 1) as usize]
            {
                lim1 -= 1;
                lim2 -= 1;
            }
            if off1 == lim1 {
                // Old side exhausted: every remaining new-side record is added.
                for o2 in off2..lim2 {
                    let orig = rindex2[o2 as usize];
                    self.cx2.set_rchg(orig, true);
                }
            } else if off2 == lim2 {
                for o1 in off1..lim1 {
                    let orig = rindex1[o1 as usize];
                    self.cx1.set_rchg(orig, true);
                }
            } else {
                let mut spl = Split {
                    i1: 0,
                    i2: 0,
                    min_lo: false,
                    min_hi: false,
                };
                self.split(
                    ha1, ha2, off1, lim1, off2, lim2, &mut kvdf, &mut kvdb, need_min, &mut spl,
                )?;
                stack.push((off1, spl.i1, off2, spl.i2, spl.min_lo));
                stack.push((spl.i1, lim1, spl.i2, lim2, spl.min_hi));
            }
        }
        Some(())
    }
}

// ------------------------------------------------------ change compaction

/// git's `xdlgroup`: a contiguous run of changed lines (empty groups sit
/// between unchanged lines).
struct Group {
    start: i64,
    end: i64,
}

/// Exact byte equality of two records (git's `recs_match`).
fn recs_match(a: &[u8], b: &[u8]) -> bool {
    a == b
}

fn group_next(cx: &Ctx, g: &mut Group) -> bool {
    if g.end as usize == cx.nrec() {
        return false;
    }
    g.start = g.end + 1;
    g.end = g.start;
    while cx.rchg(g.end) {
        g.end += 1;
    }
    true
}

fn group_previous(cx: &Ctx, g: &mut Group) -> bool {
    if g.start == 0 {
        return false;
    }
    g.end = g.start - 1;
    g.start = g.end;
    while cx.rchg(g.start - 1) {
        g.start -= 1;
    }
    true
}

fn group_slide_down(cx: &mut Ctx, g: &mut Group) -> bool {
    if (g.end as usize) < cx.nrec()
        && recs_match(cx.recs[g.start as usize], cx.recs[g.end as usize])
    {
        cx.rchg[g.start as usize] = false;
        g.start += 1;
        cx.rchg[g.end as usize] = true;
        g.end += 1;
        while cx.rchg(g.end) {
            g.end += 1;
        }
        true
    } else {
        false
    }
}

fn group_slide_up(cx: &mut Ctx, g: &mut Group) -> bool {
    if g.start > 0
        && recs_match(
            cx.recs[(g.start - 1) as usize],
            cx.recs[(g.end - 1) as usize],
        )
    {
        g.start -= 1;
        cx.rchg[g.start as usize] = true;
        g.end -= 1;
        cx.rchg[g.end as usize] = false;
        while cx.rchg(g.start - 1) {
            g.start -= 1;
        }
        true
    } else {
        false
    }
}

/// git's `get_indent`: indentation of a record treating TAB as 8 columns,
/// -1 when the line is blank; clamped at 200.
fn get_indent(rec: &[u8]) -> i64 {
    const MAX_INDENT: i64 = 200;
    let mut ret: i64 = 0;
    for &c in rec {
        match c {
            b' ' => ret += 1,
            b'\t' => ret += 8 - ret % 8,
            // Other whitespace (LF, VT, FF, CR) is ignored; anything else
            // ends the indentation.
            b'\n' | 0x0b | 0x0c | b'\r' => {}
            _ => return ret,
        }
        if ret >= MAX_INDENT {
            return MAX_INDENT;
        }
    }
    -1 // whitespace-only line
}

const MAX_BLANKS: i64 = 20;

#[derive(Clone, Copy)]
struct SplitMeasurement {
    end_of_file: bool,
    indent: i64,
    pre_blank: i64,
    pre_indent: i64,
    post_blank: i64,
    post_indent: i64,
}

#[derive(Clone, Copy)]
struct SplitScore {
    effective_indent: i64,
    penalty: i64,
}

fn measure_split(cx: &Ctx, split: i64) -> SplitMeasurement {
    let mut m = SplitMeasurement {
        end_of_file: false,
        indent: -1,
        pre_blank: 0,
        pre_indent: -1,
        post_blank: 0,
        post_indent: -1,
    };
    if split >= cx.nrec() as i64 {
        m.end_of_file = true;
        m.indent = -1;
    } else {
        m.end_of_file = false;
        m.indent = get_indent(cx.recs[split as usize]);
    }
    let mut i = split - 1;
    while i >= 0 {
        m.pre_indent = get_indent(cx.recs[i as usize]);
        if m.pre_indent != -1 {
            break;
        }
        m.pre_blank += 1;
        if m.pre_blank == MAX_BLANKS {
            m.pre_indent = 0;
            break;
        }
        i -= 1;
    }
    let mut i = split + 1;
    while i < cx.nrec() as i64 {
        m.post_indent = get_indent(cx.recs[i as usize]);
        if m.post_indent != -1 {
            break;
        }
        m.post_blank += 1;
        if m.post_blank == MAX_BLANKS {
            m.post_indent = 0;
            break;
        }
        i += 1;
    }
    m
}

fn score_add_split(m: &SplitMeasurement, s: &mut SplitScore) {
    const START_OF_FILE_PENALTY: i64 = 1;
    const END_OF_FILE_PENALTY: i64 = 21;
    const TOTAL_BLANK_WEIGHT: i64 = -30;
    const POST_BLANK_WEIGHT: i64 = 6;
    const RELATIVE_INDENT_PENALTY: i64 = -4;
    const RELATIVE_INDENT_WITH_BLANK_PENALTY: i64 = 10;
    const RELATIVE_OUTDENT_PENALTY: i64 = 24;
    const RELATIVE_OUTDENT_WITH_BLANK_PENALTY: i64 = 17;
    const RELATIVE_DEDENT_PENALTY: i64 = 23;
    const RELATIVE_DEDENT_WITH_BLANK_PENALTY: i64 = 17;

    if m.pre_indent == -1 && m.pre_blank == 0 {
        s.penalty += START_OF_FILE_PENALTY;
    }
    if m.end_of_file {
        s.penalty += END_OF_FILE_PENALTY;
    }
    let post_blank = if m.indent == -1 { 1 + m.post_blank } else { 0 };
    let total_blank = m.pre_blank + post_blank;
    s.penalty += TOTAL_BLANK_WEIGHT * total_blank;
    s.penalty += POST_BLANK_WEIGHT * post_blank;
    let indent = if m.indent != -1 {
        m.indent
    } else {
        m.post_indent
    };
    let any_blanks = total_blank != 0;
    s.effective_indent += indent;
    if indent != -1 && m.pre_indent != -1 {
        if indent > m.pre_indent {
            s.penalty += if any_blanks {
                RELATIVE_INDENT_WITH_BLANK_PENALTY
            } else {
                RELATIVE_INDENT_PENALTY
            };
        } else if indent == m.pre_indent {
            // no adjustment
        } else if m.post_indent != -1 && m.post_indent > indent {
            s.penalty += if any_blanks {
                RELATIVE_OUTDENT_WITH_BLANK_PENALTY
            } else {
                RELATIVE_OUTDENT_PENALTY
            };
        } else {
            s.penalty += if any_blanks {
                RELATIVE_DEDENT_WITH_BLANK_PENALTY
            } else {
                RELATIVE_DEDENT_PENALTY
            };
        }
    }
}

fn score_cmp(s1: &SplitScore, s2: &SplitScore) -> i64 {
    let cmp_indents = i64::from(s1.effective_indent > s2.effective_indent)
        - i64::from(s1.effective_indent < s2.effective_indent);
    60 * cmp_indents + (s1.penalty - s2.penalty)
}

/// git's `xdl_change_compact`, including the indent heuristic when
/// `indent_heuristic` is set (git's default).
fn change_compact(cx: &mut Ctx, cxo: &Ctx, indent_heuristic: bool) {
    let mut g = Group { start: 0, end: 0 };
    while cx.rchg(g.end) {
        g.end += 1;
    }
    let mut go = Group { start: 0, end: 0 };
    while cxo.rchg(go.end) {
        go.end += 1;
    }

    loop {
        if g.end == g.start {
            // empty group: skip
            if !group_next(cx, &mut g) {
                break;
            }
            if !group_next(cxo, &mut go) {
                break; // unreachable in consistent state; mirrors C's BUG
            }
            continue;
        }

        let mut earliest_end: i64;
        let mut end_matching_other: i64;
        let mut groupsize;
        loop {
            groupsize = g.end - g.start;
            end_matching_other = -1;

            // Shift the group backward as far as possible.
            while group_slide_up(cx, &mut g) {
                if !group_previous(cxo, &mut go) {
                    break; // C would BUG(); stop defensively
                }
            }
            earliest_end = g.end;
            if go.end > go.start {
                end_matching_other = g.end;
            }
            // Now shift the group forward as far as possible.
            loop {
                if !group_slide_down(cx, &mut g) {
                    break;
                }
                if !group_next(cxo, &mut go) {
                    break;
                }
                if go.end > go.start {
                    end_matching_other = g.end;
                }
            }
            if groupsize == g.end - g.start {
                break;
            }
        }

        // Heuristics (only upwards shifts remain; the group sits as far
        // down as possible).
        if g.end == earliest_end {
            // no shifting was possible
        } else if end_matching_other != -1 {
            while go.end == go.start {
                if !group_slide_up(cx, &mut g) {
                    break;
                }
                if !group_previous(cxo, &mut go) {
                    break;
                }
            }
        } else if indent_heuristic {
            const INDENT_HEURISTIC_MAX_SLIDING: i64 = 100;
            let mut best_shift: i64 = -1;
            let mut best_score = SplitScore {
                effective_indent: 0,
                penalty: 0,
            };
            let mut shift = earliest_end;
            if g.end - groupsize - 1 > shift {
                shift = g.end - groupsize - 1;
            }
            if g.end - INDENT_HEURISTIC_MAX_SLIDING > shift {
                shift = g.end - INDENT_HEURISTIC_MAX_SLIDING;
            }
            while shift <= g.end {
                let mut score = SplitScore {
                    effective_indent: 0,
                    penalty: 0,
                };
                let m1 = measure_split(cx, shift);
                score_add_split(&m1, &mut score);
                let m2 = measure_split(cx, shift - groupsize);
                score_add_split(&m2, &mut score);
                if best_shift == -1 || score_cmp(&score, &best_score) <= 0 {
                    best_score = score;
                    best_shift = shift;
                }
                shift += 1;
            }
            while g.end > best_shift {
                if !group_slide_up(cx, &mut g) {
                    break;
                }
                if !group_previous(cxo, &mut go) {
                    break;
                }
            }
        }

        if !group_next(cx, &mut g) {
            break;
        }
        if !group_next(cxo, &mut go) {
            break;
        }
    }
}
