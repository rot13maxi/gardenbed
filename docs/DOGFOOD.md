# Gardenbed dogfood onboarding

We now develop gardenbed on gardenbed. This doc is the working agreement.

## Infrastructure (already running)
- Server: herdr tab `server` (workspace wG) — `gardenbed-server --root /home/user/gardenbed/arb --addr 127.0.0.1:9418`
- Client: `bed` on PATH (release build of main)
- `/home/user/code/gardenbed` stays the GitHub mirror; ping wG:p9 to re-import after git-side commits

## New working copy
```sh
mkdir -p ~/code/gb-<you> && cd ~/code/gb-<you>
bed clone http://127.0.0.1:9418
bed sparse add crates docs fuzz tools demo .github Cargo.toml Cargo.lock README.md DESIGN.md LICENSE CHANGELOG.md .gitignore
```
NEVER `bed sparse add .` or `/` — `target/` (cargo output) must stay OUTSIDE the sparse
selection or `bed status` drowns in untracked build files.

## Identity
`GARDENBED_AUTHOR="Your Name <you@dogfood.gardenbed.dev>"` — set it in your shell;
every commit records it and reviewers filter by author.

## Daily loop
- `bed status` — local changes (works offline)
- `bed commit -m "..."` — server-side changeset; cheap; commit early and often
- `bed log --limit 20` — note: `--limit N`, not `-N`
- `bed switch main` — sync to newest main (incremental fetch; warm cache is nearly free)
- `bed diff` (no args) — dirty working copy: working-copy changes vs current changeset (untracked excluded); clean tree: current changeset vs its first parent. One-line header names the sides. `bed diff <id>` — review a changeset
- `bed file <path> --cs <ref|id>` — any file at any changeset, no neighbor fetch

## Multi-agent collaboration
1. You commit; share the changeset id in your reports
2. Reviewer: `bed diff <id>`, or `bed switch <id>` then build/test
3. Land: `bed rebase main && bed publish`
   - publish is compare-and-swap on `main`. With ~10 writers you WILL lose races;
     the error names the new tip — `bed rebase main` and publish again. Normal, not failure.
4. After publish: `herdr agent prompt wG:p9 "landed <id>: <one-line>"` — wG:p9 reviews main

## Rules of the road
- Scope test runs: `cargo test -p <your-crate>`; full-workspace runs fight 10 agents for CPU
- `cargo test -p gardenbed` does NOT rebuild the workspace `gardenbed-server`
  binary its tests spawn from `target/{debug,release}`. After a change in
  `crates/server`, rebuild first (`cargo build -p gardenbed-server`) or a stale
  binary will fail protocol-dependent tests in ways that look like code
  regressions (same class as the e2e-vs-cli binary mismatch).
- **Dogfood #13 — protocol skew across server/client generations:** an
  OLD server binary (pre-`fd29b3d`, when `PublishReq` gained `ref`/`force`)
  plus a NEW `bed` makes `bed publish --ref <lane>` silently publish to
  `refs/heads/main`: proto3 unknown-field skipping means the old server
  ignores the new tag-3 `ref` field, treats the publish as the plain
  main form, and the CLIENT still prints the lane name (it echoes its
  own argv, never the server's ref choice). Reproduced: dc079f8-era
  server + fd29b3d-era client on a throwaway root — `refs/heads/main`
  moved to the published tip, no lane row. Wire-compat tests covered
  old-client/new-server only. **Upgrade note: restart servers before
  using `--ref`.** The test harness's server_bin() digest guard now
  refuses a binary built from different tree state, but a deliberately
  deployed old server is outside its reach — coordinate restarts with
  wG:p9 when protocol messages change.
- Before publishing: `cargo fmt --all && cargo clippy -p <pkg> --all-targets -- -D warnings`
- Hit a bed bug or papercut? Report to wG:p9 (`herdr agent prompt wG:p9 "..."`) with the
  exact command + error. Fixing bed is the point of this dogfood. Don't silently work around.
- A checkout belongs to ONE agent. Never run bed commands (switch/sparse/commit/publish) in
  another agent's checkout — a surprise current_cs move looks exactly like data loss to the
  owner. Coordinators verify in throwaway clones under /tmp, never in live checkouts.

## Git interop
- All 12 git branch refs are imported; `bed switch <branch>` works
- New commits made in git (not bed) need a server re-import — ping wG:p9
- GitHub pushes: export main via `gardenbed-server export`, then push from the git mirror

## Dogfood findings (2026-10-17)
- **#11 — Export leaves a non-bare mirror's index/worktree stale**: `gardenbed-server
  export` drives `git fast-import` (`crates/git/src/export.rs`), which advances the ref but
  never touches the index or working tree. Every export into `/home/user/code/gardenbed`
  (non-bare) therefore stages the prior checkout's content as deletions against the new
  HEAD (observed: 226 files/361k lines, 7 files, 1 file across three exports today).
  Coordinator mitigation: `git reset --hard main` in the mirror after every export.
  Exports themselves are unaffected (export reads server metadata). Product fix on backlog.
- **#12 — `cargo test -p gardenbed` spawns a stale workspace server binary**:
  `crates/bed/tests/common/mod.rs::server_bin()` starts `target/{debug,release}/gardenbed-server`
  without rebuilding it. After a `crates/server` change, protocol-dependent tests fail in
  ways that look like code regressions (real case: `publish_ref_advances` failed
  identically on a rebased tree AND pristine main until `cargo build -p gardenbed-server`
  ran). Rebuild the server package before re-running bed tests after server changes.
- **#17 — CI harness depends on binaries/fixtures the workflow never produces**:
  two halves of the same class. (a) The ci.yml test job ran plain
  `cargo test --workspace`, which builds test binaries but NOT the sibling
  `gardenbed-daemon` bin target that `crates/bed/tests/common/mod.rs::daemon_bin()`
  locates in `target/{debug,release}` — 4 bed tests
  (`daemon_cache_hit_respects_repo_scope`, `token_works_through_daemon`,
  `daemon_streams_blob_over_4mib`, `daemon_cache_hit_still_requires_token`)
  panicked with "gardenbed-daemon binary not found" on every run since ab012877
  landed the daemon crate. Same shape as the deploy-side #17 (p9's package list).
  Fix: the test job now runs `cargo build --workspace --bins` first.
  (b) The perf job's `-- --ignored` un-ignores the `corpus_ripgrep_*` git tests,
  which hard-code a dev-box-only bare mirror at `/tmp/opencode/ripgrep.git`
  (failing in CI since before the daemon crate landed). Fix: the perf job now
  clones `git clone --bare https://github.com/BurntSushi/ripgrep` into that path
  rather than skipping the tests and weakening the import gate.
  (c) `kernel_export_spot_check` (crates/git/tests/kernel_spot.rs) needs a
  multi-GB kernel corpus store at `GARDENBED_KERNEL_GB` (dev-box fixture,
  default `/home/user/.corpora/linux-gb`) — not cloneable in CI, and a
  synthetic store would test nothing. Fix: the perf job skips it with
  `--skip kernel_` (class prefix, so future kernel_* corpus tests are
  covered); budgets and the corpus_ripgrep_* import gates still run.
  (d) `-- --ignored` also un-ignores #[ignore]d *child entry points*:
  `fastcdc_table_child_entry` (crates/store/tests/gblo2.rs) is driven by
  its non-ignored parent `fastcdc_table_identical_across_processes` via
  GBLO2_CHILD_* env vars; run directly it panics on the missing env.
  Fix: the perf job adds `--skip fastcdc_table_child_entry`; the parent
  (the real coverage) runs in the regular test job.

## Dogfood findings continued (2026-10-18/19; reconstructed after /tmp loss)
- **#13 — Protocol skew across server/client generations**: an OLD server binary
  (pre-`fd29b3d`, when `PublishReq` gained `ref`/`force`) plus a NEW `bed` makes
  `bed publish --ref <lane>` silently publish to `refs/heads/main`: proto3 unknown-field
  skipping means the old server drops the new field. Upgrade note: restart servers
  before using `--ref`. (Fixed-side tests: ref-row pins landed 2026-10-17.)
- **#14 — v02-export findings (M17/M18)**: full-corpus kernel export needs ~6 GB RSS +
  40 GB disk (OOM-killed 4× on the shared box; retry with a free-memory guard);
  limit-slice boundary export refusal is by design (verified live); tmpfs import 907 c/s
  vs disk 7 c/s; import prints its report line ONLY on success — a missing line means
  failure, and piping import/export through exit-swallowing filters (tail, &&-chains)
  loses failures silently.
- **#15 — Landing gates and main-red policy**: p5's multi-repo landing shipped 6 red
  test sites (write_path:1354; read_path:772/919/999; switch_prune ×2 — all the
  'default vs garden' repository-name signature). Rules: (1) landing gates MUST include
  `cargo test -p gardenbed` whenever the span touches `crates/bed` sources;
  (1b) full-suite gates run `--no-fail-fast` (fail-fast masked 3 of 4 reds);
  (2) main-level reds are fixed ON MAIN by the lane that finds them (or the coordinator
  as fix-secretary) — never carried by the next lander.
- **#16 — Test-infra: await_counters poll cap saturates under heavy I/O**: bed
  resumability suite flaked once under a ~500 GB kernel-corpus read storm (box in
  D-state); two reruns green under same load. Raise poll caps or serialize heavy
  fixtures if CI runs concurrent heavy suites. (Companion: the 9418 outage in the same
  window shared the machine-load root; watchdog restarted, no data affected.)
- **#17 (deploy half) — Deploy package lists must be workspace-wide**: the scion-gaps
  landing added `gardenbed-daemon`, which the enumerated deploy list
  (`-p gardenbed -p gardenbed-server -p gardenbed-web`) missed — first canary RED
  (14 bed failures, binary-not-found). Redeploy now builds `--workspace --bins`.
- **#18 — Near-miss: landing infrastructure under /tmp is cleanup-vulnerable**:
  `/tmp/bed-land` was deleted by /tmp cleanup MID-CHAIN (CI-repair landing); only the
  completed server-side import saved it. Landing checkout now lives at
  `/home/user/bed-land` (stable path). Rule: chain infrastructure lives outside /tmp.
  (This registry's /tmp working copy was lost in the same cleanup — durable home is
  this file; entries #13–#18 reconstructed 2026-10-19.)

## Backlog — product fixes from dogfood findings
- **#11 product fix**: `export_to_path` should reset/checkout the target ref when the
  destination is a non-bare repo (or refuse/warn on non-bare targets). Owner: unassigned.
  Will bite any future non-bare mirror consumer.
