# M18.5 successor: raw-tree capture throughput gate — measurement procedure

Status: **measured on 2026-10-22.** The final release candidate passed the
matched ±20% regression threshold (97.25% of control). See the
[measurement report](raw-tree-measurement-2026-10-22.md) and its JSON evidence.
The older absolute 278 commits/s figure describes a resumed workload;
the fresh total is lower. Separate phase diagnostics identify graph scanning
and root snapshots as the difference. The procedure below remains for
reproduction; it does not assert that old contention still exists.

## What is gated

The original M18.5 plan recorded **278 commits/s** for a resumed import.
The matched fresh-workload regression threshold is mean(B) ≥ 0.8 × mean(A);
the historical absolute comparison must also be disclosed and investigated
(see Pass criteria and the measurement report). Raw-tree capture adds:

- one tree walk of the commit's root closure (memoized across commits by
  git tree SHA — shared subtrees are decided once per import),
- a `render_canonical` byte-compare per distinct tree,
- one zstd-3 frame + one row per DISTINCT divergent subtree.

**What a real quiet pair actually measures (observed 2026-10-22, pair
r2):** the 1,000-commit master segment captured **0 raw-tree rows**
(245 raw commits; `git_raw_tree: 0`) — the segment's trees are all
canonical, so a quiet pair measures the capture-decision WALK
(read + byte-compare per distinct tree), not divergent-row write cost.
The early-history mode-fixup claim (kernel pre-canonical segments
fixing modes in bulk) is NOT supported by current evidence: the
v2.6.12 raw-closure scan (1076 trees) found **0 non-canonical modes**
(`legacy-kernel-mode-scan.json`). Do not assume kernel raw-tree growth
without a captured-segment proof; a SEPARATE generated non-canonical
stress fixture may be added to bound the write path, but it is distinct
from the pinned real-kernel gate.

## Corpus (READ-ONLY pinned mirror; never import into it)

- Pinned mirror: `/home/user/.corpora/linux.git` (read-only).
- The v0.1 store `/home/user/.corpora/linux-gb` is also read-only.
- **Scratch clone required** for every run:

  ```bash
  git clone --no-hardlinks /home/user/.corpora/linux.git \
      /tmp/gardenbed-todo/m18-scratch/linux.git   # fresh each campaign
  ```

- Segments: the same 1,000-commit and 5,000-commit segments the M18.5
  campaign used (`--limit 1000` / `--limit 5000` from `refs/heads/master`,
  fixed segment tips recorded in the campaign log).

## Runs (release builds, cargo-gate, no concurrent cargo)

1. **A (control)**: bed-main binary (no raw-tree capture), release,
   import the 1,000-commit segment into a fresh store.
2. **B (treatment)**: this lane's binary (raw-tree capture), release,
   same segment, fresh store.
3. Repeat A/B three times, alternating A,B,A,B,A,B (thermal + cache
   fairness). Record wall-clock and `raw_trees_captured` per run.
4. Repeat the pair for the 5,000-commit segment (one pair is enough if
   the 1,000-commit pair is stable).
5. Resource envelope for the 5,000-commit run (from M18.5): ~5–6 GB RSS,
   ~40 GB disk — verify the scratch volume has headroom.

## Quiet-box criteria (all must hold at run time)

- 1-minute load average below 4 on the 16-core box, sampled immediately
  before AND after each run; discard a run whose bracketing samples
  differ by more than 2×.
- No other cargo/build workers active (coordinate: the coordinator
  schedules the window; single cargo-gate slot).
- Recorded evidence: `uptime` output, run start/end timestamps, and the
  per-run commits/s (`ImportReport.commits_per_second`) — attach to the
  campaign log.

## Pass criteria

- mean(B) ≥ 0.8 × mean(A) on the 1,000-commit segment (the ±20% gate).
- ALSO report mean(B) against the recorded 278 commits/s baseline — as
  a DISCLOSED comparison, not a pass/fail arm: the baseline was
  measured under different matching semantics (a full resume over an
  existing corpus slice — 619,487 commits / 2,230 s — versus this
  gate's fresh limited-snapshot workload), so the two numbers are not
  directly comparable without stating those semantics.
- If mean(B) ≥ 0.8 × mean(A) but the historical comparison is outside
  ±20%, that is NOT proof the box was noisy: the baseline and gate
  workloads differ (resume vs fresh snapshot, warm caches, object
  counts). Disclose the exact matching semantics of both sides,
  measure both ratios, and INVESTIGATE the actual difference (profile
  the capture walk, count captured rows, check io) — do not rerun
  indefinitely and do not wave the discrepancy off as noise.
- Final campaign: 3 quiet pairs on the FINAL code (A/B alternating) plus
  the 5,000-commit segment; the historical 278 baseline is a
  full-resume (619,487 commits / 2,230 s) versus this gate's fresh
  limited-snapshot workload — the matching semantics of both sides must
  be stated in the report, honestly, before any comparison is drawn.

## If a future campaign cannot run

Record precisely: "quiet-host window unavailable (load criteria above
not met at sampling time); raw-tree capture is exercised and verified
functionally (see the fidelity test suite: 100664 mode fixups, non-UTF-8
paths, nested dirs, shared-root reuse — all SHA-exact at export), and
the capture cost per commit is bounded by the memoized walk plus one
zstd-3 frame per distinct divergent subtree; the ±20% wall-clock gate
remains deferred, not waived." Do not run the gate on a noisy box and
report a number.

## Retention note (for the eventual campaign report)

gc never row-deletes metadata tables (`crates/server/src/gc.rs` sweeps
object files only); `git_raw_tree` rows share `git_raw_commit`'s
retention class — keyed by original git SHA, changeset-independent,
retained forever. Campaign storage growth per captured row is the
zstd-3 frame of one tree object (the kernel's largest trees are a few
hundred KiB raw).
