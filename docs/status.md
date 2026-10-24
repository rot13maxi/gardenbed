# Implementation status

Audited against the source, tests, CI configuration, and recorded results on
2026-10-21. Workspace packages still report `0.1.0`; the v0.2 feature wave is
implemented on main but remains under **Unreleased** in [CHANGELOG.md](../CHANGELOG.md).
Implementation status and completion of verification targets are separate.

## Implemented

| Area | Current implementation and evidence |
|---|---|
| v0.1 core (M0–M8) | Canonical model, filesystem/SQLite storage, derived trees and ancestry, sparse working copies, commit/rebase/publish, Git import/export, blame, MCP. [Release record](release-0.1.md) records results and original-target deviations. |
| Merge (M9/M10) | Rename-aware two-parent merge, recursive maximal-base resolution, and native octopus merges through `Merge`/`MergeOctopus` and `bed merge <TARGET>...`. Same-second/skewed-clock ancestry regressions, Git oracle rename/base fixtures, and server/client integration suites are covered in [merge behavior](merge-rename-octopus.md). |
| Multi-repository serving (M11/M12) | Lazy repository lookup, bounded handles, URL repository selection, import/export/move tooling; `crates/server/tests/multi_repo.rs`. |
| Garbage collection (M13) | Local offline-first mark/sweep, dry-run, min-age, force-mode write coordination; `crates/server/src/gc.rs` and `gc*` integration suites. Changeset and Git-mapping rows are retained. |
| Blob chunking (M14) | GBLO2 above 1 MiB, FastCDC boundaries, verified whole-blob identity; `crates/store/tests/gblo2.rs`. Chunks are stored inside each blob envelope, without cross-version chunk deduplication. |
| Offline commits (M15) | Local queue, crash recovery, explicit `bed sync`; `crates/bed/tests/offline.rs`. |
| Local daemon (M16) | Optional shared object cache, watchers, daemon-first connection and fallback; [daemon guide](daemon.md), `crates/bed/tests/daemon.rs`. |
| Native export and import fidelity (M17/M18) | Native pack writer, sparse raw-commit capture, verbatim export of signed/extra-header/non-UTF-8 commit metadata; `crates/git/tests/export.rs` and `fidelity.rs`. Sparse raw-tree capture covers noncanonical modes, non-UTF-8 names and empty directories; matched throughput was measured on 2026-10-22 (limits below). |
| Diff rendering (M19) | Recorded renames, exact-blob pairing, gitlink pointer hunks; `crates/bed/tests/render_patch.rs`. Similarity-based moved-and-edited pairing and combined merge diffs remain outside scope. |
| Additional client features | Read-only snapshot FUSE mount; lane refs; stack rebase and already-applied twin recognition. See [CLI reference](cli.md). |
| Authentication | Optional bearer tokens with per-repository read/write/admin roles; server/bed auth suites. Originally deferred in the plan, now implemented. |
| Scale-out | PostgreSQL metadata, S3 objects, stateless replicas, compose stack and demo; [scale-out design](scale-out-design.md). Nightly/manual CI gates cover live services; local suites are opt-in. |
| Web interface | Embedded read-only UI, browse/blame/diffs/search and worker review; [web guide](web.md), Rust API tests and Playwright suite in `e2e/`. |
| Benchmarks | Paired Gardenbed/Git harness and recorded synthetic/kernel measurements; [benchmarking](benchmarking.md). Broader sweeps remain open. |

The [v0.2 acceptance demo report](../demo/report-v0.2.md) records a passing
2026-10-19 run of multi-repo, merge, offline sync, GC, chunking, signed export,
daemon cache, zero-checkout MCP, and auth checks. This is recorded evidence,
not a fresh verification of the current checkout.

## Open implementation and verification work

- **M18 measured:** raw-tree fidelity passed code review and the matched
  kernel-segment throughput threshold on 2026-10-22 (97.25% of control).
  See the [measurement and limits](raw-tree-measurement-2026-10-22.md).
  Fresh limited imports differ from the historical resumed-import workload;
  neither the measurement nor hermetic fidelity tests establish a full-kernel
  SHA roundtrip. Full-scale GC remains separate below.
- **GC at full kernel scale:** the backlog sweep measured mark batching on a
  1.16M-live-id synthetic store; it did not re-measure the 10.36M-object kernel
  corpus. The original M13 kernel time/RSS target needs separate evidence; see
  [the backlog record](DOGFOOD.md#backlog--product-fixes-from-dogfood-findings).
- **Crash-test determinism:** dogfood #25 remains open. The commit kill
  window and a resumability kill window race counter polling; deterministic
  server-side synchronization is the recorded fix direction. See
  [DOGFOOD.md](DOGFOOD.md).
- **Git/bed divergence bookkeeping:** runbook preflight checks are implemented;
  making export persist Git mappings to remove manual tip bookkeeping remains
  a structural follow-up (dogfood #26).
- **Deferred scope:** regional cache tier, Windows, LFS, recursive submodules,
  history pruning, cross-version chunk deduplication, per-chunk blob streaming,
  combined merge diffs, similarity-based moved-and-edited diff rendering, and
  retiring the `git diff-tree` similarity subprocess. The v0.3/later labels in
  [v0.2 scope](v0.2.md#1-scope) are planning targets.
- **Benchmark coverage:** selection/concurrency/queue-depth sweeps, multi-repo
  fan-out, interrupted provisioning comparisons, aggregate CI footprint, and
  offline workflow comparisons remain in [the benchmark backlog](benchmarking.md#6-what-to-build-next).

## Verification coverage

[CI](../.github/workflows/ci.yml) defines blocking push/PR jobs for
formatting, clippy, workspace tests, release-mode ignored budgets,
bounded selected fuzz smoke, Playwright against the built web binary,
and the v0.1 demo. Workspace binaries are built before integration tests.
The extended fuzz campaign runs nightly and on manual dispatch; the
pack-reader target starts without a committed seed corpus. Kernel corpus
tests remain excluded from the ordinary push/PR budget job.

Nightly/manual jobs also cover the release canary and live PostgreSQL/S3:
cold concurrent schema initialization, backend parity, and replica
correctness. A PR can request the live-service job with `[live-services]`
in its title. The live jobs reject skipped or zero-test runs. Local live
suites still require their documented environment variables, including
`GARDENBED_SCALE_TEST=1` for replica integration.

FUSE CI requires a provisioned `[self-hosted, linux, fuse]` runner and
repository variable `ENABLE_FUSE_TESTS=true`; it is skipped until enabled.
The separate [kernel workflow](../.github/workflows/kernel-corpus.yml)
is manual and requires an externally provisioned corpus and runner. It
uses a private metadata copy and asserts that the selected test ran.
These workflow definitions do not claim a remote FUSE or kernel run has
occurred. See [CI verification](ci-verification.md) for prerequisites,
local evidence, and the exact gates.
