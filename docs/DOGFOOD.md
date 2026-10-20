# Gardenbed dogfood onboarding

We now develop gardenbed on gardenbed. This doc is the working agreement.

## Infrastructure (already running)
- Server: herdr tab `server` (workspace wG) — `gardenbed-server --root /home/user/gardenbed/arb --addr 127.0.0.1:9418`
- Client: `bed` on PATH (release build of main)
- `/home/user/code/gardenbed` stays the GitHub mirror; ping the chain operator to re-import after git-side commits
- Landing chain, deploy, retry/rewind, and gate rules: [OPERATIONS.md](OPERATIONS.md)

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
4. After publish: notify the release manager ("landed <id>: <one-line>") — that role reviews main

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
  the chain operator when protocol messages change.
- Before publishing: `cargo fmt --all && cargo clippy -p <pkg> --all-targets -- -D warnings`
- Hit a bed bug or papercut? Report it to the chain operator (see
  [OPERATIONS.md](OPERATIONS.md)) with the exact command + error. Fixing bed is
  the point of this dogfood. Don't silently work around.
- A checkout belongs to ONE agent. Never run bed commands (switch/sparse/commit/publish) in
  another agent's checkout — a surprise current_cs move looks exactly like data loss to the
  owner. Coordinators verify in throwaway clones under /tmp, never in live checkouts.

## Git interop
- All 12 git branch refs are imported; `bed switch <branch>` works
- New commits made in git (not bed) need a server re-import — ping the chain operator
- GitHub pushes: export main via `gardenbed-server export`, then push from the git mirror

## Dogfood findings (2026-10-17)
- **#11 — Export leaves a non-bare mirror's index/worktree stale**: `gardenbed-server
  export` drives `git fast-import` (`crates/git/src/export.rs`), which advances the ref but
  never touches the index or working tree. Every export into `/home/user/code/gardenbed`
  (non-bare) therefore stages the prior checkout's content as deletions against the new
  HEAD (observed: 226 files/361k lines, 7 files, 1 file across three exports today).
  Coordinator mitigation: `git reset --hard main` in the mirror after every export.
  Exports themselves are unaffected (export reads server metadata). Product fix on backlog.
  **Product fix: refusal** — non-bare targets are refused before any write, naming the
  worktree and `use a bare target` (test:
  `export_into_a_non_bare_worktree_is_refused_untouched`).
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
  **Status: fixed (test-infra only, product untouched).** `await_counters`
  (crates/bed/tests/resumability.rs) now backs off 10 ms → 250 ms, doubling only while
  the counters sit still and resetting to 10 ms whenever any counter moves — a stalled
  server is polled ~4×/s instead of adding 100 GetStats/s to a box with none to spare,
  while a live fetch is still watched at the tight cadence (cycle A kills a quarter of
  the way in, so a coarse poll would let the fetch finish first). Its cap rose from
  120 s to 10 min: instrumented healthy waits are 106.7 ms and 120.1 ms, so that is
  ~5,000× the real path — far beyond any load stall, yet under the 30-min CI job
  timeout so a stuck run fails with the diagnostic instead of being killed silently.
  A bound on a hung run, not a skip. Failure messages now distinguish causes: "no progress" means the
  counters never advanced past their start (a genuine product stall), "deadline" means
  they advanced but the target was not reached (a load/slow-path case). A silent skip
  under load is explicitly rejected: it would hide exactly the product stall the
  assertion exists to catch.
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
- **#19 — Debug-green is not evidence for release-mode timing classes**: the M13 gc
  landing (2026-10-19) passed every debug gate (bed 226/0, store+server 140/0, gc 11/0)
  but the RELEASE canary through the deployed pair caught two reds the debug gates and
  the round-3 review both missed: (a) gc_chaos `v3_sigkill_mid_gc_leaves_sound_store`
  failed deterministically in release only (8/8; debug 11/0), and (b) the CI perf job's
  `commit_p50_is_size_independent_10k_vs_1m` regressed (1M p50 2.9s vs 10k 56.1ms —
  a per-commit cost proportional to repo size, the signature of the handshake's
  closure walk inside the commit write transaction). Main was reverted to 33a7c799
  (ruling: two independent reds attributable to the lane's span). **Rule**: the release
  canary through the deployed pair is a first-class gate, not a formality — lane gates
  for timing-sensitive code MUST include the release-mode run of the affected suites;
  review signoff on debug-green alone is insufficient for timing classes.
  Evidence: /home/user/gc-red-evidence/gc-strace{2,3}.txt.
  **Correction (same day, follow-up diagnosis)**: red (a) was re-attributed — the v3
  test's bug, not gc's. The orphan replant lived inside `debug_assert_eq!`, which does
  NOT evaluate its operands in release builds, so the replant never ran after the
  prime gc correctly deleted the orphans; the subsequent `backdate` then failed on the
  (correctly) absent file. The strace "zero unlinkat" reading was a filter artifact:
  Rust `fs::remove_file` issues plain `unlink` (syscall 87), invisible to an
  `unlinkat`-filtered trace; reproduced independently by two agents with `rustc -O`
  strace experiments. gc was correct and its report accurate. Red (b) was real and
  forced the write-set redesign. The #19 rule stands and strengthens: the release
  canary caught a real release-only red (the p50 regression) AND exposed the
  debug_assert-in-release pitfall — two distinct failure classes that debug gates are
  structurally blind to. (Lanes must never gate timing- or assert-sensitive code on
  debug builds alone; also: `debug_assert_eq!` hides *side effects*, not just
  assertions — evaluate required effects before it.)
- **#20 — `multi_repo::mcp_serves_the_named_repository_only` needs a binary the
  suite does not build**: `cargo test -p gardenbed-server` fails that one test with
  `gardenbed-mcp binary not found under target — run \`cargo build --workspace\` first`
  unless `cargo build -p gardenbed-mcp` ran first (observed 2026-10-19 while gating the
  gc-mark batching lane; 13/14 passed, this one red; green immediately after the build).
  Same class as #12/#17: a test locates a sibling bin target the crate's own test build
  never produces. The guard message is actionable. Fix candidate: add `gardenbed-mcp`
  to the #12 rebuild note and confirm the CI `--workspace --bins` step covers it.

- **#21 — `gardenbed-server`: top-level `--root`/`--addr` are ignored when the
  `serve` subcommand is used**: the documented short form
  `gardenbed-server --addr 127.0.0.1:9418 --root DIR` works, but the equally
  natural `gardenbed-server --root DIR serve --addr 127.0.0.1:PORT` parses
  the flags into the *top-level* struct, whose values the `Serve` arm never
  reads (it only sees its own copies), so startup dies before serving.
  Exact commands (v0.2 demo lane, release build):
  `./target/release/gardenbed-server --root /tmp/r serve --addr 127.0.0.1:19702`
  → exit 2, `error: --root is required (the server root directory)`;
  same for `gardenbed-server --addr 127.0.0.1:19701 --root /tmp/r2 serve`.
  Workaround used by `demo/run-v0.2.sh` and the test harness: put every flag
  after `serve`. Product fix: make the top-level flags `global = true` (or
  fall back to them inside the `Serve` arm) so both spellings work. Found
  while wiring the v0.2 auth step, which needs `serve --auth`.

- **#22 — `bed rebase --stack <ancestor>` panics on an empty chain** (found during
  the docs lane's runbook verification, main 236691ca): when the current
  changeset is already an ancestor of `<new-parent>` — e.g. a fresh clone of
  `main` running `bed rebase --stack main` — the walk loop exits with an empty
  chain and `crates/bed/src/ops.rs` indexes `replayed[len - 1]`, panicking with
  `index out of bounds: the len is 0 but the index is 18446744073709551615`
  (exit 101). Reproduced on a throwaway root: `bed clone --repo r
  http://127.0.0.1:<port>` then `bed rebase --stack main`. The single form
  stores a redundant replay instead of refusing. Owner: `lane/backlog-rebase`
  (its D1 span answers `already up to date` for both forms when the current
  changeset is an ancestor of the new parent, and pins it in
  `crates/bed/tests/write_path.rs`).

## Dogfood finding #23 (2026-10-19) — `rebase --stack` refused legitimately re-unioned lanes
- **Symptom** (repeated real incidents; p9 hand-unioned three lanes in one day because of
  it): after a lane's tip content was landed on `main` as a NEW changeset — hand-union,
  TREE-EQUAL and SAME-MESSAGE but a different id, no ancestry link — `bed rebase --stack
  main` replays the lane stack and dies on the server's own-content check:
  `bed: bed rebase --stack: the server refused (FailedPrecondition): rebase conflicts on:
  "docs/lane.md" was changed or deleted`. Operators then hand-union again instead of
  rebasing, compounding divergence.
- **Reproduction** (red before green, kept as permanent regression tests in
  `crates/bed/tests/write_path.rs`): build a 3-deep lane editing one file, move `main`,
  union the lane's middle element's tree onto main with the same message via
  `Repo::create_changeset(parents=[main]), changes=diff(main→lane element)`, point `main`
  at it, run `bed rebase --stack main` → the exact refusal above (asserted at
  `stack_rebase_recognizes_a_tree_equal_same_message_twin`).
- **Fix**: twin recognition in `crates/bed/src/ops.rs` — a bounded walk of the new
  parent's span above the merge base records `(message, root_tree) → id`; an element
  matching BOTH exactly is already applied and is not replayed, each resolution is
  printed, the last twinned element subsumes the prefix, the suffix replays onto main's
  current tip, and a whole-stack match replays nothing and lands on the new parent.
  A twin later rewritten/reverted by `main` is NOT skipped (the element replays, the
  ordinary conflict rule decides). Partial overlap stays a hard conflict; same message
  with a different tree stays a hard conflict. The single (non-stack) `bed rebase` uses
  the same rule. Review round 2 closed four defects: the cap note is emitted the moment
  the scan truncates (so it accompanies a cap-induced conflict), the walk prunes
  frontier parents that are ancestors of the base (visited set is exactly
  `ancestors(parent) \ ancestors(base)`; a merge-carrying main was descending into
  ancient history and could exhaust the cap), a stale candidate inside a subsumed
  prefix is reported as subsumed instead of "replaying", and the single form also
  answers `already up to date` (its doc bullet had scoped it to `--stack`). Each fix
  has a test that is red when the fix is reverted. Round 3 narrowed the cap note: it is
  a statement about a possible MISS, so it is silent when the capped scan still found
  and applied the twin (or when a stack fully resolved), and prints when the run still
  has to replay — including the stale-twin case, where the found twin is rejected and
  the ordinary conflict follows (`cap_note_speaks_only_when_the_twin_was_actually_missed`,
  `cap_note_fires_when_the_found_twin_is_stale`, both mutation-checked).
- **Docs/tests**: `docs/cli.md` §`bed rebase`; regression tests cover the twin,
  whole-stack + no-op publish, prefix twin, reverted twin, same-message/different-tree
  refusal, single rebase, and already-up-to-date.

## Dogfood finding #24 (2026-10-19) — command substitution in a commit message published `main`

An unquoted backtick pair inside a `bed commit -m "..."` message was expanded by the
shell *before* `bed commit` ran, and the substituted text executed `bed switch <cs>`
followed by `bed publish` in the lane's checkout. That published a docs-lane tip to
`refs/heads/main` on the canonical 9418 server; the recovery was the deliberate rewind
route (`gardenbed-server import --ref refs/heads/main --allow-rewind`), which restored
`main` to the pre-incident tip `236691ca5487efc0` and warned that 4 changesets might no
longer be reachable from the ref (they remain on `lane/backlog-docs`).

This is a product finding, not only shell hygiene: `bed publish` performed a ref move
the operator never asked for, and the only guard was the rewind route existing
afterwards. `bed commit` takes `--message` as ordinary argv, so the shell substitutes
before bed sees the text; a bed-side check cannot distinguish intended from
substituted content. Mitigations until a product fix lands: single-quote commit
messages (or use a quoted heredoc), and treat any command that takes a message
(`bed commit -m`, and any `-m` form) as shell-dangerous. A possible product-side
guard is a lint that refuses messages containing `$(`, `${`, or backticks together
with a "use single quotes / --message-file" hint — heuristic, but it would have caught
this one. Recorded in [OPERATIONS.md](OPERATIONS.md) §3.

## Dogfood finding #25 (2026-10-19) — `commit_killed_between_upload_and_commit_retries_uploading_once` is racy

`crates/bed/tests/write_path.rs::commit_killed_between_upload_and_commit_retries_uploading_once`
intermittently fails at the assertion `config untouched by the crash` (the working copy's
`current_cs` already moved, so the SIGKILL landed after the commit completed rather than
between `PutBlob` and `CreateChangeset`). The test polls `GetStats` for the `put_blob`
bump and then kills the child; the 1 ms cadence leaves a window in which a fast commit
finishes first.

Evidence (2026-10-19, this box, `GARDENBED_FSYNC=off`, `env -u GARDENBED_AUTHOR`,
serial `--exact` runs): pristine main 2 failures in 16; the sweep's union tree
3 in 16; the test function's body is byte-identical between the two trees
(verified by comparing the function text; the files differ only outside it),
so this is pre-existing, not sweep-introduced. Under 8 concurrent runs each
tree also failed at the same assertion, so load amplifies it.

Not a product finding as written: nothing in the failure shows state moving before a
successful commit — it shows the kill landing after success. Fix direction: make the
kill window deterministic (gate the server's `CreateChangeset` handler on a test hook,
or kill on a server-side marker that is provably before the state move) rather than
racing a counter poll. Owner: unassigned (test-infra; #16/#19 class).

## Dogfood finding #26 (2026-10-19) — the chain has no divergence detection until push

Git-native commits landed on `origin/main` while bed `main` stood still: three
commits (`c3d78765a` bed checkout cost, `0ca2e4d2d` server pre-rename layout,
`7758dfbbe` the gardenbed-ab bench harness) were pushed directly to the mirror
after the previous chain finished. Nothing imported them bed-side, so bed
`main` stayed on the old twin. The next landing chain discovered the problem
only at `git push`, after the bed-side publish and export had already run: the
push was rejected non-fast-forward, bed `main` and `origin/main` had diverged,
and four files overlapped (CHANGELOG.md, Cargo.lock, README.md,
crates/server/src/lib.rs). Recovery cost one rewind (`import --allow-rewind`),
one import, and a full hand-re-union.

The class is **not** "someone pushed to git": it is that the chain had no
divergence detection until the last possible moment. A bed-native import is
invisible to git until export, and a git-native push is invisible to bed until
import; both directions are legitimate shapes, so the chain must check rather
than assume.

Mitigation (landed with this entry): the pre-publish step MUST compare
`git ls-remote origin main` against the tip git is expected to hold and refuse
with a named error before anything moves — not discover the divergence at push
time. Two sources, in order: bed `main`'s exact `git_map` row when `main`
itself came from an import, otherwise the tip recorded at the end of the
previous chain (the export report and mapping row). With neither the check
reports "cannot verify" rather than guessing. It is scripted into the runbook
as step 1.1b ([OPERATIONS.md](OPERATIONS.md)), before the publish, and every
chain now ends by recording that tip. Structural follow-up (not done here):
`export` could write `git_map` rows, which would remove the record-keeping.
Second instance of this class on 2026-10-19 (the first was the same day's
mid-sweep rewind).

## Dogfood finding #27 (2026-10-19) — "pre-rename layout opens in place" left every one-shot path behind

The pre-rename-layout commit (`0ca2e4d2d`) taught `Repo::open`/`RepoSet` to
resolve `<root>/seedbed.sqlite3` + `<root>/loam/objects` as the same store under
historical names, and tested exactly that entry point. Every OTHER store open
kept the current-layout paths, so on a pre-rename root (the shape of the
pinned kernel corpus):

- `gardenbed-server export` printed `0 commits … 0 ref(s)` and exited 0, then
  **created** `<root>/metadata.sqlite3` + `<root>/objects` — an empty shadow
  store. Because the resolver prefers the current layout whenever that file
  exists, the shadow then hid the real history: a server on that root served
  `no ref main`.
- `gardenbed-server gc --dry-run` read the shadow database and failed with
  `… is not a gardenbed repository`, after creating it; with the shadow gone it
  scanned 0 objects (the sweep walked `<root>/objects`, the mark read
  `metadata.sqlite3`).
- `gardenbed-server import` wrote into the new shadow store, splitting one
  repository across two stores.
- `gardenbed-server move` took the legacy branch, whose rename list was
  current-layout only: nothing moved, and it still printed `moved repository
  … history byte-identical` (an empty destination beside the untouched
  store).
- A repository literally named `loam` in a multi-repo root was read as the
  pre-rename layout (`loam/objects` matched), so the root served only
  `default` and every sibling disappeared from the served list.

Found by the re-union review of this sweep. Class: a layout-resolution rule was
added to one entry point and not to the six others that open the same stores —
the tests covered the rule, not its reach. Two residuals the same review caught
after the first fix: `resolve_layout` still keyed on `loam/objects`, so a
multi-repo root containing a repository named `loam` resolved `default` to the
root itself (empty `default/` store, 0-commit export); and `export --repo
<typo>` still created the unknown repository (a fresh empty store) and exited 0.
The discriminator is now the database file only, and `export` uses
`existing_repo_dir_for` (refuses with the served list instead of creating).

Fix (landed with this entry): one public `resolve_layout`, used by
`Repo::open`, `import`, `export`, `gc` (mark, sweep walk and closure walk),
`move` (including the rename list), and `checkpoint`; the pre-rename predicate
keys on the database file, not on a `loam/objects` directory, so a repo named
`loam` stays a repo. Pinned by six tests in `crates/server/tests/integrity.rs`
— four subprocess-level (export, gc scan + live counts, move keeps history,
unknown `--repo` refused) and two in-process (`loam`-named sibling stays
served, bare `loam/objects` root refused) — each of which fails on the pre-fix
tree (mutation-verified for the two predicate tests).

## Dogfood finding #28 (2026-10-20) — guard tests were not override-safe; an aborted run poisoned the tree

Two defects surfaced while running the release canary through the deployed pair:

1. **The staleness guard tests cannot pass with `BED_SERVER_BIN`/`BED_DAEMON_BIN`
   set**, and they failed rather than saying so. The override is trusted as-is
   by design (`locate_guarded_bin` returns it after an existence check), so the
   property under test is unreachable — `catch_unwind(...).expect_err` receives
   `Ok` and panics. The canary form that points the harness at the deployed pair
   is exactly that configuration, so a correct harness setup produced a red.
2. **An aborted mutation deposited a probe and poisoned later runs.** The
   mutation phases wrote `// tree-state staleness probe` into
   `crates/{server,daemon}/src/lib.rs` and restored the file *after*
   `expect_err`. A failure before that line (missing sibling bin, wrong answer,
   kill) left the probe behind with the original mtime, so the source digest no
   longer matched the built binary and the NEXT run's oracle refused a tree it
   should have accepted. Observed growing 1 → 2 probes across attempts, and it
   blocked the first landing chain attempt.

Fix (this entry): both guard tests skip with an actionable message when their
override is set, and the mutation phases restore through the existing RAII
`Restore` guard instead of inline writes, so content and mtimes return on the
panic path too. Verified both ways: with the overrides set the guards skip
cleanly and leave zero residue; without them the oracles run and the tests pass
(3.2 s / 0.7 s). An injected panic immediately after the mutation leaves zero
residue, which the old shape did not.

Class: a test's *setup* was assumed rather than checked (`env` hadn't been
considered), and its *cleanup* was on the happy path only. Both are the same
mistake the #19 rule names in another form — green in the configuration you
tested is not green in the configuration the release actually uses.

Chain note: bed has no ref-delete verb; retiring lane refs required a direct
store transaction (`DELETE FROM refs WHERE name=? AND cs=<tip>`, six refs,
integrity_check ok). A first-class `bed ref delete` would make lane retirement
a supported operation.

## Landing record — 2026-10-20 backlog sweep (bed c4d27378e502c53d)

The five-lane backlog sweep (groups A-E) plus a re-union with three
git-native upstream commits landed as one union changeset.

- **Bed tip:** `c4d27378e502c53d` · **Git tip:** `6f6fc584b8f31a83`
  (the pair for the next pre-publish 1.1b check).
- **Composition:** 32 sweep paths + 12 upstream paths, four of them hand-merged
  (`crates/server/src/lib.rs`, `CHANGELOG.md`, `Cargo.lock`, `README.md`), each
  verified line-wise by a fresh reviewer.
- **Reviews:** five lane reviews (2-5 rounds each) plus a dedicated re-union
  review (5 rounds). Final verdict quoted verbatim: *"APPROVE. Rounds 1-5 are
  closed."*
- **Gates:** fmt; clippy `--workspace --all-targets -D warnings`; full workspace
  suite; release `gc_chaos` 4/4 (109.03 s deployed / 102.4 s in-tree); release
  commit-p50 ratio 0.96-1.02 (budget 2.00); `demo/run-v0.2.sh` PASS 73 checks /
  0 failures, run through the deployed pair as the capstone.
- **Chain:** publish → export (`fsck --strict` clean, 134 commits, mapping row
  for the bed tip == mirror main) → push → workspace-wide `--bins` redeploy
  (gardenbed-ab's first deploy) → bounce (new layout-resolution code active) →
  canary split form (guard tests without the override, the other 241 tests
  through the deployed pair — see #28) → CI run 39370920642 SUCCESS.
- **Retired:** the five lane refs and the probe ref, after the chain was green.
  `lane/backlog-union` and `main` retained.

Imported-content caveat (release manager's wording, verbatim): the three
upstream commits ride as imported content, authorship untouched. Five real
defects were found and fixed in `0ca2e4d2d` (the pre-rename layout class) with
six mutation-verified regression tests; the other two commits (`c3d78765a`
checkout cost, `7758dfbbe` bench harness) were gated by the workspace suite and
demo but not line-reviewed. Imported commits now fall under the standing rule
that all main content is reviewable — this landing establishes the precedent
that they arrive with at least gate coverage, and line review when their surface
intersects the union's.

## Backlog — product fixes from dogfood findings

The 2026-10-19 backlog sweep landed every item below in one union changeset on
`main` (lane refs `lane/backlog-*`, each with its own fresh-reviewer cycle; the
landing rules are in [OPERATIONS.md](OPERATIONS.md)). Statuses are as of that
landing; the enforcing test or artifact is named so a regression has somewhere
to point.

- **`export_to_path` non-bare target (#11 product half)** — refusal shipped on
  main; the sweep adds the operator remedy in the error (`use a bare target
  (git init --bare)`) and the regression test
  `export_into_a_non_bare_worktree_is_refused_untouched`
  (`crates/git/tests/export.rs`), which main lacked. Refusal, not
  index/worktree updating, is the sanctioned fix. Owner: `lane/backlog-guards`.
  **Status: landed.**
- **gardenbed-web `seed` writer** — was a one-shot unlocked writer; it now
  holds the shared `repo.lock` for its whole run (the lock `serve` and
  `gardenbed-server import` hold), and every write goes through `Repo`
  (`create_changeset` carries the write-set verify handshake). Test:
  `crates/web/tests/seed_lock.rs` (causal: the root holds only `repo.lock`
  while an exclusive holder is alive; an acquire-then-drop guard fails).
  Owner: `lane/backlog-guards`. **Status: landed.**
- **`daemon_bin()` staleness oracle** — `server_bin()` refuses a binary from
  different tree state (#12); `daemon_bin()` had neither oracle. Both now
  compare the located binary's own embedded fingerprint
  (`GARDENBED_<CRATE>_SRC_DIGEST:<hex>` marker, read from the file) against
  `src/**`, and the DIFFERENT refusal prescribes a remedy that actually
  rebuilds (`touch crates/<crate>/src/lib.rs && cargo build -p <crate>`).
  Tests: `crates/bed/tests/{server,daemon}_bin_guard.rs`, mutation-verified.
  Owner: `lane/backlog-guards`. **Status: landed.**
- **`demo/run-v0.2.sh`** — the v0.2 §0 acceptance test now exists: asserted
  steps for multi-repo serving, merge with two parents, offline queue + sync,
  gc orphan reclamation, >1 MiB blob identity, native export round-trip and
  source-corpus SHA fidelity, daemon shared-cache zero-object-request proof
  (with a cold-cache negative control), MCP with no working copy, and auth
  tokens. `docs/v0.2.md` §0 names it. Owner: `lane/backlog-demo-ci`.
  **Status: landed.**
- **release-bar CI job for #19** — a nightly `release canary` job (schedule +
  workflow_dispatch) runs the release-mode affected suites on `main`:
  `gc_chaos`, the commit-p50 size-independence budget, and
  `demo/run-v0.2.sh`. `.github/workflows/ci.yml` cites #19 and explains the
  nightly trigger. Owner: `lane/backlog-demo-ci`. **Status: landed.**
- **gc mark batching** — `insert_live` batched into bounded 4096-id
  transactions and tree-closure claims batched through one
  `INSERT OR IGNORE ... RETURNING` per batch (was one transaction per file
  entry and one statement per claimed tree id). The mark set is unchanged
  (V1 oracle: mark == brute-force walk); boundary coverage in
  `v1b_batch_boundaries_mark_exactly`, whose fixture sizes itself from the
  public `WRITE_BATCH` and builds state directly so every pushed id has
  multiplicity one. Measured 4.83 s → 1.08 s (~4.4x) on a 1.16M-live-id
  synthetic store; the 10.36M-object kernel corpus was not re-measured here.
  Owner: `lane/backlog-perf`. **Status: landed.**
- **`await_counters` poll-cap saturation (#16)** — adaptive, progress-sensitive
  backoff (10 ms → 250 ms, reset on any counter movement), a 10-minute
  deadline (~5,000x the measured 107–120 ms healthy wait), and failure
  messages that distinguish "no progress" (product stall) from "deadline"
  (load). No skip path. `crates/bed/tests/resumability.rs`; #16 carries the
  recorded rationale. Owner: `lane/backlog-perf`. **Status: landed.**
- **`rebase --stack` twin-changeset refusal** — tree-equal, same-message
  changesets across stacks made `--stack` refuse legitimate re-unions, forcing
  hand-unions. Both rebase forms now recognize twins, print every resolution,
  leave stale/partial cases as hard conflicts, and answer `already up to date`
  when nothing is left to replay. Full record: finding #23. Owner:
  `lane/backlog-rebase`. **Status: landed. Highest-value item of the sweep.**

