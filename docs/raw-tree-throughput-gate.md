# M18.5 successor: raw-tree capture throughput gate — measurement procedure

Status: **deferred, not waived.** This is the reproducible procedure for
the ±20% kernel-segment import gate that `todo/fidelity` (raw-tree
capture) must eventually run on a quiet host. The shared 16-core box is
not quiet (documented load 28–230 in docs/v0.2.md §M18.5 history:
4–8 commits/s for identical inputs, ±100% run-to-run variance — a
35–70× contention factor), so a wall-clock gate run now would measure
the co-tenants, not the code.

## What is gated

Import throughput of a kernel-segment with raw-tree capture ON must stay
within ±20% of the recorded baseline **278 commits/s** (docs/v0.2.md
M18.5, recorded on the quiet box). Raw-tree capture adds, per commit:

- one tree walk of the commit's root closure (memoized across commits by
  git tree SHA — shared subtrees are decided once per import),
- a `render_canonical` byte-compare per distinct tree,
- one zstd-3 frame + one SQLite row per DISTINCT divergent subtree (the
  kernel's pre-canonical segments fix up modes in bulk, so the divergent
  set is large in exactly those segments — the gate exists to bound this).

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

- mean(B) ≥ 0.8 × mean(A) on the 1,000-commit segment (the ±20% gate),
- AND mean(B) within ±20% of 278 commits/s when the box is otherwise
  idle-class quiet (the recorded baseline).
- Report both ratios; if the two criteria disagree, the box was not
  quiet — rerun, do not average.

## If the gate cannot run (current state)

Record precisely: "quiet-host window unavailable (shared box, load
28–230 documented); raw-tree capture is exercised and verified
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
