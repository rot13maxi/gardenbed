# Corpora — pinned snapshots

Real corpora used by differential verification (M3, M5, M6, M8) and the
v0.1 north-star demo. A snapshot is pinned by the HEAD SHA recorded at
clone time; re-clone with the commands below and record the new pin.

| Corpus | Location (outside the repo) | Pinned HEAD | Commits (all refs) | Size (packed) | Notes |
|---|---|---|---|---|---|
| linux | `/home/user/.corpora/linux.git` | `0fcb05b72344f40806cbe810de8872f1698bf053` | 1,725,150 | 8.6 GiB | master tip 1,485,163 commits. Flagship corpus (user-directed, Oct 2026): merge/octopus-dense, symlinks, exec-bit churn, ~90k files. Early history (Apr 2005, "new git world order") contains mode-normalization commits (`100664` → `100644`) that git's diff-tree cannot see; the importer's raw tree-diff fallback classifies them and their derived tree ids differ from git's raw tree shas (see `docs/divergences.md` §4). |

Fetch recipe (as executed):

```sh
git clone --mirror https://github.com/torvalds/linux.git /home/user/.corpora/linux.git
git -C /home/user/.corpora/linux.git rev-parse HEAD        # pin goes in this table
git -C /home/user/.corpora/linux.git rev-list --all --count
```

Notes for import runs (D8 cost): rename detection is a `git diff-tree -M`
subprocess per commit — at ~1.5M commits the full sweep is a multi-hour,
run-once/nightly job, not a CI gate. Verification on this corpus is
sample-based (per M3.5: ≥ 1,000 random (path, rev) pairs vs `git show`,
100% match) plus full graph/bijection checks over the whole import.

## Kernel import — COMPLETE (Oct 15, 2026)

`refs/heads/master` fully imported into `/home/user/.corpora/linux-gb`
with `--renames on`, `GARDENBED_FSYNC=off` (bulk-load durability toggle):

| Check | Result |
|---|---|
| Bijection | 1,485,163 changesets ↔ 1,485,163 `git_map` rows, all git SHAs distinct — exactly `git rev-list --count refs/heads/master` |
| Sampled content fidelity | `verify_corpus --samples 1000 --seed 42`: **1,000/1,000 byte-exact** vs `git show`, 6.1 s |
| Incremental proof (M3.3) | re-import over the completed store: **0 new, 0 objects, 0 refs moved, 0.03 s** (mapped-frontier pruning skips the whole graph) |
| Store | `loam/` 10,360,644 objects (84 GiB) + seedbed.sqlite3 ≈ 5 GiB |
| Throughput | 29/s pre-profiling → **278/s** post-fix (see the perf note in `docs/release-0.1.md`); resume run ingested 619,487 commits in 2,230 s |

Verification runs used `verify_corpus` with `--root` = the **repo root**
(`linux-gb`, holding `loam/` + `seedbed.sqlite3`). The M3 example
originally opened the object store one level up; fixed in the same
commit as the perf work (a wrong-root run silently verified a store it
had built itself — the sampler now also picks revs uniformly from
`git_map` by rowid poke and paths by random tree descent, since a full
flatten is millions of tree reads at kernel scale).
