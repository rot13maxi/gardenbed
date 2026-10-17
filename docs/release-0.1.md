# v0.1.0 — release notes and verification record

Historical record: command names, package names, and storage paths in this
document describe the recorded version. See [current terminology](terminology.md)
and the [CLI reference](cli.md) for the current interface.

This document is the M8 close-out: what shipped, what was verified, and
what a reviewer should re-run before tagging. The plan of record is
[docs/v0.1.md](v0.1.md); §10 defines done.

## Scope shipped

| area | what exists |
|---|---|
| model | canonical `Changeset`/`Tree` codecs, golden fixtures, cross-language spec ([canonical-encoding.md](canonical-encoding.md)) |
| storage | `loam` content-addressed object store (fsynced, chunked) + `seedbed` sqlite metadata (WAL, CAS refs, derived data) |
| engine | tree derivation, diff, first-parent blame with skiplist + bloom segment cache |
| bridge | `rootstock`: git import (fast-import walk, renames, refs) and export (one `git fast-import` stream, TSV mapping) |
| server | tonic gRPC (17 RPCs), CAS refs, server-side graft, harvest, eager derivation |
| client | `bed` CLI: clone/plot/goto/status/file/log/diff/blame + sow/graft/harvest |
| agents | `gardenbed-mcp`: MCP stdio tools `log`, `diff`, `file`, `blame` — no working copy |
| hardening | scripted §0 demo, SIGKILL chaos suite, 6 fuzz targets, criterion benches, CI ([ci.yml](../.github/workflows/ci.yml)) |

## Definition of done — §10 checklist

1. **§0 demo passes, scripted, from a clean container.**
   `demo/run-v0.1.sh` asserts all seven steps end to end and writes
   `demo/report.md`. It is re-runnable (`--keep-going`, `--keep-work`,
   `--corpus`, `--commits`, `--workdir`) and pins nothing to this
   machine. Recorded run (2026-10-14, 16-core Linux, tmpfs workdir):

   | phase | wall time | §0 bar |
   |---|---|---|
   | import (50,100 commits, bijection proven) | 46.9 s | — |
   | `bed clone` (materializes nothing) | 87 ms | < 5 s |
   | `bed plot add big` (1,021 files, byte-verified) | 287 ms | < 2k files swept |
   | `bed sow` (3 changes) | 69 ms | — |
   | `bed log --limit 1` (sown changeset queryable) | 46 ms | < 1 s |
   | export → `git fsck --strict` + `push --dry-run` | 14.4 s | SHA roundtrip 100% |

   The SHA roundtrip check matched **all 50,100** exported commits to
   the corpus's own git SHAs (unsigned history ⇒ exact). MCP log/diff/
   file/blame answers were byte-compared against `git log`, `git
   diff-tree --name-status`, `git show`, and `git blame
   --first-parent --porcelain`. **Known deviation:** §0 names a ≥ 1 GiB
   real history; the default demo corpus is a deterministic synthetic
   repo (`gen-demo-corpus`, 50k commits, merges/renames/symlinks/exec
   bits/mixed timezones) generated locally. `--corpus DIR` runs the same
   script against a real repository (the ≥ 1 GiB bar is then asserted).

2. **Zero silent divergences from git.**
   [divergences.md](divergences.md) lists every deviation with a repro
   (signed commits, non-UTF-8 bytes, unmodeled headers, pre-canonical
   `100664` trees); everything else round-trips byte-exactly and is
   differential-tested against git in `crates/rootstock/tests/` and the
   `harness` comparators.

3. **Every milestone's verification runs green in CI.**
   `.github/workflows/ci.yml` runs six parallel jobs on every push/PR:
   `fmt`, `clippy -D warnings`, `cargo test --workspace` (chaos suite
   included), release `--ignored` perf budgets, a 30 s-per-target fuzz
   smoke, and the demo.

4. **A newcomer can sow a change from the quickstart alone.**
   [quickstart.md](quickstart.md) covers build → arboretum → clone →
   plot → sow → MCP, and every command in it is exercised verbatim by
   the demo script.

## Hardening gates — results

### Chaos (SIGKILL under load)

`crates/server/tests/chaos.rs`: for every RPC class (tree, blob, log,
diff, blame, sow, graft, harvest, ref-CAS, ref-read) the suite drives a
live client against a spawned server, kills it with SIGKILL at a random
point (`0–128 ms` delays), restarts, re-fetches **every acknowledged
result byte-exactly**, then verifies offline: loam checksum sweep,
sqlite `integrity_check`, and derived-data wipe + recompute against the
pre-kill baseline. The loam sweep skips `.tmp-*` files by design: an
atomic write (tmp + fsync + rename) killed mid-flight leaves its tmp
file behind — unpublished, unreadable by the store, and reaped by the
next rename into that shard.

- default suite (10 classes × 10 cycles): green in `cargo test --workspace`
- extended: `GARDENBED_CHAOS_ITERS=100 cargo test --release --test chaos`
  — 10 classes × 100 cycles (1,000 SIGKILL/restart rounds, every
  acknowledged answer re-fetched and every object checksummed): green,
  188 s (recorded run: 2026-10-14).

### Fuzz (cargo-fuzz)

Six targets under `fuzz/` (`--sanitizer=none`, libFuzzer, stable
toolchain; CI adds nightly + ASan for the smoke job):

| target | exercises | result |
|---|---|---|
| `decode_changeset` | canonical changeset decoder | clean — 120 s, 82.9 M execs |
| `decode_tree` | canonical tree decoder | clean — 120 s, 113.2 M execs |
| `fuzz_tree_apply` | random canonical changeset chains through `derive_tree` | clean — 120 s, 3.0 M execs |
| `fuzz_diff` | tree-pair diffing + the re-apply property | one finding (fixed, see below) — re-run clean, 180 s, 5.1 M execs |
| `fuzz_difftree_parse` | rootstock raw first-parent diff parser | clean — 120 s, 29.1 M execs |
| `fuzz_mcp_line` | MCP JSON-RPC line protocol | clean — 120 s, 13.5 M execs |

The accumulated corpora live under `fuzz/corpus/` so later sweeps start
from covered ground; the crash artifacts are kept under `fuzz/artifacts/`
as reproducing inputs (both now run clean).

**Findings fixed during the sweep:**
* `decode_changeset` accepted a 10-byte varint whose final group carried
  payload bits above bit 63 (the decoded value silently changed on
  re-encode — violating the canonical-acceptance property and spec
  §"Minimality rules"). The varint readers now reject any group that
  would lose bits (`shift ≥ 64` or overflow on shift) and any varint
  longer than 10 bytes, in the canonical codec *and* the two
  derived-data readers (ancestry skiplist, path summary — malformed
  entries there simply recompute). The crash input is committed as a
  rejection vector
  (`crates/model/tests/fixtures/invalid-changeset-varint-tenth-bit.hex`).
* `fuzz_diff` found a dir→file swap whose diff did not re-apply: the
  diff emits `Add("g")` + `Delete("g/h")`, canonical order applies the
  Add first, and the delete then hit a "file where a directory is
  expected" collision. The applier now treats a delete beneath a path
  the *same changeset* wrote as a file as a no-op (the write swallowed
  the subtree); deletes under a pre-existing file are still errors.
  Pinned by `crates/derive/tests/diff_reapply.rs` (both swap directions,
  deep nesting, sibling traffic, and the still-invalid shape).
* `file_read/cold` (bench gate) found bed's local object cache living at
  `.bed/objects/objects/…`: `Bed::objects()` handed `FsObjectStore::open`
  the `.bed/objects` *directory*, but that constructor takes a store
  *root* and appends `objects/` itself. Reads and writes agreed with each
  other (so every functional test, including the resumability proof, was
  green), but the documented `.bed/objects/<2 hex>/<62 hex>` layout was
  never what the cache actually used. Fixed by passing `.bed` as the root
  at all three call sites (`Bed::init`, `Bed::objects`, the test
  `cached_count` helper); the bench now exercises the real layout, and
  cold/warm fetch behavior is unchanged.


### Benchmarks (criterion, `crates/bench`)

`cargo bench -p gardenbed-bench` — five suites against the perf budgets
of M4/M5/M6 (recorded on the same host as the demo; `/dev/shm` fixtures,
fsync **on**):

| suite | what it measures | median | budget | verdict |
|---|---|---|---|---|
| `plot_add_1k/cold_fetch` | first `plot add` of a 1k-file plot: want-list walk, wire fetch, materialize | 1.07 s | < 5 s (M4.4) | ✅ 4.7× headroom |
| `plot_add_1k/warm_retend` | re-`plot add` of an already-materialized plot (cache hits, re-hash, treestate) | 62.7 ms | — | — |
| `warm_status_10k/hint_scan` | `bed status`, 10k planted files, mtime/size hints warm | 172 ms | < 500 ms (M4.4) | ✅ |
| `warm_status_10k/full_scan` | same with `--full-scan` (every file hashed) | 225 ms | — | — |
| `file_read/warm` | `bed file` with the blob in `.bed/objects` | 42.0 ms | — | — |
| `file_read/cold` | blob uncached: one `GetBlob` round-trip + cache write on top | 83.3 ms | p95 < 50 ms (M4.4)* | * see note |
| `sow/small_repo` | 100-file sow on a 10k-file repo | 54.8 ms | p50 ratio ≤ 2× (M5.1) | ✅ ratio **1.002** |
| `sow/big_repo` | the identical 100-file delta on a 1M-file repo | 54.9 ms | (same) | ✅ |
| `blame/walk_10k` | **worst case** — the path is rewritten by *every* commit (bloom can never skip); 10k chain | 429 ms ‡ | see note † | — |
| `blame/walk_full` | the same worst case walked from the 100k head | 4.30 s ‡ | — | — |

\* The M4.4 subprocess-path budget (p95 < 50 ms localhost) was verified at
its own gate before this import started; the recorded bench run shared the
host with the 1.5M-commit linux import (CPU contention, ~29 commits/s), and
the cold path's extra ~41 ms is the library path's own fetch + hash-verify
+ cache write. Re-recorded on a quiet host (Oct 16, 2026): 84.3 ms, within
1.2 % of the recorded 83.3 ms — the gap to the subprocess budget is the
bench's own work, not host load.

† The M6.2 latency budget (log-log slope < 0.3; 100k blame < 200 ms)
governs the **skip-enabled** case — a path untouched by recent history —
which the segment-bloom reduces to 17.3 µs at 100k commits (recorded by
M6's differential benches). This suite deliberately measures the
adversarial opposite: every commit touches the path, so every step needs
a changeset, a blob, and a line diff — O(history) by design.

‡ Re-recorded Oct 16, 2026, quiet host (clean checkout of `6cd6295`,
loadavg < 1.5): `walk_10k` 474.9 ms, `walk_full` 4.755 s — 8.5 % / 8.7 %
under the recorded numbers; re-measured again on the landed hardening
tree (`f393f8c`): **429 ms / 4.30 s** (−9.6 % / −9.5 % vs the original
records). Methodology note: the blame walk bench builds a **fresh
`Repo` per iteration** — the engine memoizes finished walks keyed
(changeset, path) (content-addressed history is immutable), so a shared
repo would measure microsecond memo hits (53 ns) instead of the walk
the M6.2 budget governs. The same run re-measured every other row
within ±1.2 % of its recorded value (sow p50 ratio 0.992 vs 1.002
recorded), with one exception: `cold_fetch` re-measured 1.20–1.24 s
(run CIs 1.02–1.40 s; intra-run samples span 0.5–2.4 s). A same-day
control run of the pre-post-M8 code (`08fe8be^`) measured 1.18 s, so
the gap to the recorded 1.07 s is this bench's run-to-run variance —
the recorded value sits at the low edge of the distribution — not a
regression; every measurement stayed under the +20 % CI-gate line and
4× inside the 5 s budget.

A full second run under deliberately hostile load — the workspace test
suite compiling on all 16 cores plus the 1.5 M-commit kernel import
(loadavg 16.5) — still landed inside every budget: `cold_fetch`
1.73 s (< 5 s), `hint_scan` 173 ms (< 500 ms), sow p50 ratio 1.004,
`file_read/cold` 83.5 ms (unchanged). The drift concentrates in CPU-bound
phases that collided with compile storms (blame walks +25–41 %,
`cold_fetch` +61 %); the I/O-shaped rows (sow, status hint scan, file
reads) moved < 1 %. The table records the quieter first run; the budgets
are robust to host load.

## Deviations and notes for reviewers

* **Ref-name normalization (server lib).** Imported histories store full
  ref names (`refs/heads/main`); the native write path and every client
  speak bare names (`main`). `Repo::get_ref`/`Repo::cas_ref` now resolve
  bare names against their `refs/heads/` spelling (read fallback; CAS
  targets the existing row instead of forking a second ref) and all
  writes land under `refs/heads/`, so exports are always git-valid. This
  is a behavior fix surfaced by the demo, not a semantic change: the
  smoke test's stored-name assertion was updated (`refs/heads/feature`),
  and unit tests cover both spellings plus the legacy bare-row case.
* **Engine edits beyond formatting.** Three engine-side functional
  changes were made during M8, each forced by a hardening gate: the
  varint strictness fix (fuzz finding), the diff-reapply no-op rule in
  the tree applier (fuzz finding), and client-facing ref-name
  normalization (demo oracle). Everything else was
  `cargo clippy --fix`-mechanical (the three functional changes are
  itemized under "Findings fixed during the sweep" above).
* **Fuzz-target API surface.** `gardenbed-rootstock` exports `difftree`
  and `oid` modules and `parse_batch`/`DiffRecord`/`DiffStatus`/`GitOid`
  publicly so the fuzz targets can drive them without new test-only
  entry points.
* **Demo corpus.** The synthetic 50k-commit corpus stands in for §0's
  ≥ 1 GiB pinned history (see above); `docs/v0.1.md` §9's real corpora
  (linux ~1.3M commits) are imported separately for verification sweeps,
  not wired into CI.
* **Perf budget method.** CI gates budgets at +20% against the recorded
  baselines; fixture repos live on tmpfs and fsync stays **on** — the
  durability story is under test, not the disk.

## Post-M8 hardening (Oct 15, 2026)

Three threads landed after M8's sweep, all committed together:

* **Importer performance (8.5×).** Profiling the live kernel import
  (`perf` + `strace` + wchan sampling) showed 86% of wall blocked in
  ext4 journal commits — an unconditional directory fsync per written
  object survived the `GARDENBED_FSYNC=off` toggle, a quadratic scan in
  `repair_dir_to_file` ate 11% of CPU, and each commit issued ~95 file
  locks across 5–6 sqlite transactions. Fixes: dir-fsync honors the
  toggle, O(C log C) repair, and a single `BEGIN IMMEDIATE` transaction
  per commit (`ingest_changeset`, values byte-identical to the eager
  path; kill+resume proven byte-identical). Measured: ripgrep 34→395/s,
  git.git 52→336/s, kernel segment 30→278/s. Bench budgets below are
  unaffected (they exercise the bed, not the importer).
* **End-to-end QA pass.** Every core operation driven through real
  binaries against git.git (82,443 commits) and compared byte-exact to
  the git oracle (clone, plot, status, file, log, diff, goto, sow,
  harvest, export, MCP, offline). One real engine bug: graft replay was
  over-strict (modify onto a file identical to the changeset's parent's
  copy was refused — unusable for the common rebase case); fixed to the
  three-way rule. Six smaller bed/server/mcp fixes; blame compared
  11,076 lines vs `git blame --first-parent` with 4 blank-line
  hunk-edge divergences documented (`docs/divergences.md` §5).
* **Kernel corpus verified (M3.5 at flagship scale).** Bijection
  1,485,163 ↔ 1,485,163; 1,000-sample sweep vs `git show` 100% match;
  incremental re-import proof 0 new / 0 objects / 0.03 s. Details in
  `docs/corpora.md`. `verify_corpus` now takes the repo root (server
  convention) and samples uniformly — the M3 example had a one-level
  root-resolution bug that made it verify a store it had built itself.

## Release procedure

1. Re-run the gates on a clean checkout:
   `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo test --workspace --release -- --ignored`
2. `demo/run-v0.1.sh` — must end with `**PASS**`.
3. Fuzz sweep: `cd fuzz && cargo fuzz run --sanitizer=none <target> -- -max_total_time=120` for each target.
4. `cargo bench -p gardenbed-bench` — compare against the table above; > 20% regression is a release blocker.
5. Tag `v0.1.0`, publish crates (`gardenbed-model`, `gardenbed-store`, `gardenbed-derive`, `gardenbed-protocol`, `gardenbed-server`, `gardenbed-mcp`, `gardenbed`).
