# CI verification coverage

This document is the map of what CI actually verifies, tier by tier, and
— just as important — what each tier deliberately does **not** claim. It
exists because "implemented" and "verified" are different rows in
[status.md](status.md), and a gate that looks like coverage but skips
silently is worse than no gate: it launders a claim.

Two workflows own the coverage:

| Workflow | Trigger | Jobs |
|---|---|---|
| [ci.yml](../.github/workflows/ci.yml) | push/PR + nightly schedule | fmt, clippy, test, perf, fuzz-smoke, playwright, demo (push); fuzz-campaign, release-canary, live-services, fuse-mount (nightly) |
| [kernel-corpus.yml](../.github/workflows/kernel-corpus.yml) | manual (`workflow_dispatch`) | kernel real-corpus gate (self-hosted runner) |

## Tier 1 — every push and PR (blocking)

| Job | What it proves | Notes |
|---|---|---|
| `fmt` | formatting | unchanged |
| `clippy` | no lint debt (`-D warnings`) | unchanged |
| `test` | full workspace suite, chaos included | unchanged |
| `perf` | release-mode budget tests, `--ignored`, plus the `corpus_ripgrep_*` import gates against a fresh bare clone | unchanged, including the `--skip kernel_` carve-out (see Tier 3) |
| `fuzz-smoke` | **blocking** bounded fuzz: 5 targets × 30 s / 250k execs, seeded from `fuzz/corpus/` | **was** `continue-on-error`; see below |
| `playwright` | **new** — the full `e2e/` Playwright suite against the real `gardenbed-web` binary built from the tree | see below |
| `demo` | `demo/run-v0.1.sh` | unchanged |

### fuzz-smoke: how it became blocking without pretending

The old job ran five targets for 30 s each and was
`continue-on-error: true` — "a finding here fails the release gate, not
every PR". That had a real cost: a *regression* (a parser that used to
survive a class of inputs and now panics) is exactly what a short seeded
run catches deterministically, because the committed corpus replays every
input that ever executed the code. A non-blocking job that replays the
known-covered input space and never blocks anyone is not a smoke test;
it is a notification channel.

The blocking job is bounded in **both** dimensions, so it cannot stretch
or fake depth:

* **Coverage bound** — the five targets are the ones with cheap, dense
  executions: the canonical decoders (`decode_changeset`,
  `decode_tree`) and the hostile-input surfaces (`fuzz_difftree_parse`,
  `fuzz_mcp_line`, `fuzz_pack_reader`). Every one of them is a
  bytes-in/verdict-out parser; libFuzzer covers millions of inputs in
  30 s (recorded: 82.9 M and 113.2 M execs in 120 s for the decoders,
  [release-0.1.md](release-0.1.md)). The three *property* targets
  (`fuzz_tree_apply`, `fuzz_diff`, `fuzz_merge3`) build whole changeset
  chains or merge triples per input — orders of magnitude fewer execs
  per second — so a 30 s blocking budget on them would be mostly build
  overhead with token exploration. They run in the nightly campaign at
  their release-bar duration instead.
* **Resource bounds** — `-max_total_time=30` AND `-runs=250000` per
  target: whichever fires first ends the run, so a target that wanders
  into a pathologically slow input cannot stretch the job, and a fast
  target cannot balloon its execution count into an unbounded run.
* **Seed honesty** — cargo-fuzz loads the committed corpora under
  `fuzz/corpus/<target>/` by default (its own per-target corpus dir; no
  `-dir` override needed — verified locally that seeds are read and new
  units written there) (the accumulated sweep artifacts
  [release-0.1.md](release-0.1.md) documents). Every push therefore
  replays the *entire historically covered input set* plus fresh short
  exploration. A regression on any input the corpus reaches is a hard
  red; that is the defensible claim of a blocking smoke.

What this job does **not** claim: novel fuzzing depth. The 24 h / 1
CPU-hour bars from [v0.1.md](v0.1.md) §M8 and the v0.2 merge/pack bars
belong to the release checklist and to the nightly campaign. The
blocking smoke is a regression gate over known-covered ground plus a
short fresh probe — nothing more, and the job comment says so.

One mechanical note: the smoke job now installs only the nightly
toolchain (the old job installed stable *and* nightly; cargo-fuzz builds
its targets with nightly, and no stable artifact is used anywhere in the
job). `cargo install cargo-fuzz --locked` and `Swatinem/rust-cache` with
`workspaces: fuzz` are unchanged.

### playwright: the web UI finally in the gate

[status.md](status.md) recorded the gap plainly: "Playwright tests exist
but are not invoked by the current CI workflow." The suite covers every
page of the web UI (overview, history, changeset, browse, file+blame,
compare, search, review, workers, the keyboard finder, live mode) plus
the API flows — 69 tests. None of it ran in CI.

The job:

1. `npm ci` in `e2e/` — the lockfile (`@playwright/test` 1.64.1) is
   authoritative, no floating resolution.
2. `npx playwright install --with-deps chromium` — the pinned browser and
   the system libraries a bare ubuntu runner needs for the headless
   shell. The runner label `ubuntu-latest` is enough; no browser
   container is used because the suite spawns its own HTTP server on
   localhost and drives it from the same box.
3. `cargo build -p gardenbed-web` — the **real** web binary from this
   tree, the dogfood #17/#20 rule: never test against a binary the
   workflow did not produce. The suite's `webServer` hook
   (`e2e/serve-e2e.sh`) builds the same target itself; building first
   keeps the hook's 300 s startup budget from being spent on a cold
   cargo cache and proves the bin target exists before a browser ever
   launches.
4. `npx playwright test` — blocking. `serve-e2e.sh` seeds a fresh
   deterministic repo and serves it on `127.0.0.1:18430`; every spec
   runs against that real server process.

Failure artifacts (`test-results/`, trace-on-failure) upload as workflow
artifacts for 7 days.

**A real bug this job found while being built:** on a fresh Postgres
volume the parity suite failed 6/7 with `Db("db error")` — every
parallel `PostgresMetadataStore::open()` runs `CREATE TABLE IF NOT
EXISTS`, and Postgres's catalog check-then-insert is not atomic under
concurrency, so two first-opens race into
`pg_type_typname_nsp_index` duplicate-key (the Postgres log shows the
exact statement and `Key (typname, typnamespace)=(changesets, 2200)
already exists`). The immediate rerun was 7/7 green — the schema existed
by then. Any fresh-database first run (CI every night, a first `docker
compose up`) hits it.

The **product fix is assigned to a separate lane**
(`todo/postgres-schema`): narrow init/migration transaction locking in
`crates/store/src/postgres.rs` plus a cold concurrent-open regression
test. This job's warm-up step (one store open via the suite's own
cheapest test before the parallel suite) is a **temporary local
mitigation that masks the race for CI only — it does not close the
production issue**, and it must be REMOVED when the fix lands: the final
integrated CI is expected to run the parity suite against a truly fresh
service with no warm-up, so the regression test guards the real path.
Tracked as a removal dependency on `todo/postgres-schema`.

## Tier 2 — nightly (schedule + workflow_dispatch)

| Job | What it proves |
|---|---|
| `release-canary` | unchanged: release-mode gc chaos, the commit-p50 size-independence budget, `demo/run-v0.2.sh` (dogfood #19) |
| `fuzz-campaign` | **new** — all 8 targets × 120 s each, seeded, the release-bar duration [release-0.1.md](release-0.1.md) records |
| `live-services` | **new** — the PostgreSQL/S3 parity and scale suites against real deployment-compatible services |
| `fuse-mount` | **new** — the FUSE mount suite on a self-hosted runner that has FUSE |

### fuzz-campaign

Every target including the slow property targets, 120 s per target with
the committed corpora, still bounded per target (`-max_total_time=120`).
This is the sweep the release checklist records; the blocking smoke is
its bounded proxy on the push path. Nightly, because a campaign's signal
is about the tree state of `main`, not about any one PR — the same
reasoning the release canary already documents for itself.

### live-services: real services, deployment-compatible

The scale-out verification ([scale-out-design.md](scale-out-design.md) §
Verification 1–4) was opt-in by environment variable only: the suites
skip with a printed note unless `GARDENBED_TEST_PG_URL` +
`GARDENBED_TEST_S3_*` (parity) and additionally `GARDENBED_SCALE_TEST=1`
(scale) are set. That gating is correct for developers — "no developer
without Docker pays for it" — but nothing in CI ever set those variables,
so the parity and scale suites had **zero automated coverage**: the same
skip-with-note path ran on every push and every nightly canary.

The `live-services` job closes that, nightly and on PRs that opt in:

* It boots the **deployment's own stack** — `tools/scale/compose.yaml`'s
  `postgres` (postgres:17-alpine) and `minio` (the S3 endpoint) plus the
  one-shot bucket creator — not a test-double service. D4's point was
  "tests against the same services the deployment ships"; this job is
  that clause made mechanical. The compose `server` replicas are NOT
  started: the suites under test spawn `gardenbed-server` processes from
  the tree's own build (the two-process gRPC pass), so building the
  shipped image here would test Docker, not the tree.
* The bucket is unique per run (`gardenbed-ci-<run id>`): the parity
  suite requires it — S3 immutability against content-addressed keys
  makes a reused bucket collide identical-but-tampered fixtures — and it
  is created through the same `mc mb` one-shot creator the deployment
  uses, then proven readable with a signed `mc stat`.
* Then it runs the two suites exactly as documented:
  * `cargo test -p gardenbed-store --features scale --test parity
    -- --nocapture` — the full backend-independent matrix against
    Postgres metadata + S3 objects;
  * `cargo test -p gardenbed-server --test scale -- --nocapture` with
    `GARDENBED_SCALE_TEST=1` — cross-handle read-your-writes, CAS races,
    idempotent creates, cross-handle fetches, and the two-process gRPC
    pass (which is why the bins are built first — dogfood #12/#17).
* **Anti-vacuous-pass:** both suites keep their skip-with-note gating
  (that is their contract, and changing it would break every developer
  without Docker). A green cargo exit alone cannot prove the live path
  ran — a skipped suite also exits 0 — so each suite's output is teed to
  a log and a follow-up step greps it for the suites' own skip strings
  (`skipping parity tests`, `scale tests skipped`) and for the pass
  summary (`test result: ok`). If the env wiring regressed into a
  silent skip, the job fails with the skip named instead of passing
  vacuously. (A `-- --list` count would prove nothing here: skipped
  tests still list.)

Opt-in on PRs: nightly `schedule` and `workflow_dispatch` run it
unconditionally; a PR opts in with `[live-services]` in its title. A PR
without the marker runs exactly what it ran before this job existed —
the default gate is not weakened, and the nightly run is unconditional,
so `main` cannot lose live coverage quietly.

Local invocation of the same suites against the same compose stack is
documented in [tools/scale/README.md](../tools/scale/README.md)
("Environment overrides" — the env-var contract) and reproduced in the
job's `env:` block verbatim.

### fuse-mount: host gating stays explicit

The mount suite (`crates/bed/tests/mount.rs`) gates on host FUSE
availability and skips with `mount test skipped: /dev/fuse unavailable`
where the host cannot mount. That is correct and **stays exactly as is**
— the tests must never assume a runner can mount.

What was missing is that *no* CI host ever ran them: `ubuntu-latest` has
no `/dev/fuse`, so the push-path `test` job silently skipped the entire
suite, every run, forever. The FUSE path (whole-tree listing, byte
fidelity for files/symlinks/exec bits, `EROFS` on writes, concurrent
reads, explicit and external unmount, the seeded stability loop) had no
automated run at all.

The nightly `fuse-mount` job runs the suite on
`runs-on: [self-hosted, linux, fuse]`:

* The **runner label is the opt-in**. A repository admin attaches the
  `fuse` label to a self-hosted host that demonstrably has `/dev/fuse`
  and `fusermount3`. With no such runner, GitHub Actions *queues* the
  job rather than failing it — the safe outcome: nothing runs, nothing
  passes vacuously, the pending job is visible in the UI.
* The job adds its own preflight that fails loudly if the labeled host
  lacks `/dev/fuse` or a fusermount binary — a mislabeled runner is a
  configuration bug and shows up as a red run, not a green skip.
* Nothing unsafe is assumed or done: no kernel-module installation, no
  `mknod /dev/fuse`, no privileged container, no relaxation of the
  suite's own `fuse_available()` gate. The suite's skip note remains the
  tests' own decision; on this job's runner it had better not fire, and
  the follow-up step greps the run's output for the skip note (a skip on
  a *labeled* host fails the job instead of passing vacuously) and for
  the pass summary.
* Bins are built first (dogfood #12/#17): the mount tests clone and
  serve through the real `bed` / `gardenbed-server` binaries.

## Tier 3 — self-hosted, externally provisioned (manual)

### kernel-corpus.yml: the corpus that cannot be an artifact

The kernel corpus ([corpora.md](corpora.md)) is an 8.6 GiB git mirror
plus an 89 GiB / 10.36M-object bed store. No GitHub-hosted runner can
hold it, no workflow should download it per run, and a synthetic store
"would test nothing — the value is the real kernel corpus shape" (the
perf job's own comment). Until now the kernel gate was a dev-box ritual:
`GARDENBED_KERNEL_GB` pointing at `/home/user/.corpora/linux-gb`, run by
hand.

`kernel-corpus.yml` is the honest version of that gate:

* **workflow_dispatch only.** It never fires on push, so it adds nothing
  to the push gate and cannot silently weaken any default. The perf job's
  existing `--skip kernel_` is untouched.
* **Externally provisioned, explicitly declared.** The corpus paths come
  from repository variables (`vars.KERNEL_GB_ROOT`,
  `vars.KERNEL_GIT_MIRROR`) or per-dispatch inputs — CI does not and
  must not create them. The provisioning recipe is
  [corpora.md](corpora.md) (mirror clone + import; the recorded run took
  hours at 278 commits/s).
* **Runner label is the opt-in:** `runs-on: [self-hosted, linux,
  kernel-corpus]`. No such runner → the job queues, nothing runs.
* **Preflight fails loudly, not silently.** The declared paths must
  exist, be a bed store layout (`loam/objects`), the mirror must be a
  bare repo with `refs/heads/master`, and the object count must exceed a
  million files (docs record 10,360,644) — a path pointing at a partial
  or synthetic store fails the contract by name.
* **What runs:** `kernel_export_spot_check`
  (`crates/git/tests/kernel_spot.rs`) in release mode with
  `--ignored --exact` (so a test rename that makes the filter match
  nothing fails instead of passing vacuously). The test's own contract
  decides the expected outcome FROM THE CORPUS STATE, not from this
  workflow: against the currently pinned v0.1 store (no raw captures)
  the expected outcome is a loud refusal at the first pre-canonical
  `100664` tree mode (divergences.md §4); against a corpus re-imported
  with raw-tree capture the same test takes the success arm (bijection
  + sampled SHA parity). **Dependency:** the expected outcome is not a
  permanent property of this gate — it follows the corpus. When the
  M18.5 raw-tree-capture fidelity work lands and the corpus is
  re-imported, the refusal arm turns into the success arm with no
  workflow change, because the workflow asserts the test's verdict
  against whatever corpus `vars.KERNEL_GB_ROOT` names. The metadata is
  opened from a scratch copy on `/dev/shm` (`GARDENBED_KERNEL_META`)
  because opening writes WAL pragmas — the corpus store stays pristine,
  byte-for-byte.
* **Durability stays on:** the fsync toggle is never set; the scratch
  copy is the only write and is disposable by construction.

What this gate deliberately does **not** claim:

* The M18 ±20% kernel-segment import-throughput gate — still open (see
  [status.md](status.md) "M18 throughput gate"); a measurement gate
  needs a quiet host and a recorded baseline, not a workflow.
* GC at full kernel scale — the v0.2 M13.6 budget is explicit (on the
  10,360,644-object / 84 GiB kernel store: `gc --dry-run` < 60 min
  cold, RSS ≤ 1 GiB, zero live objects deleted; a post-GC re-import
  writes 0 objects), and status.md records that the mark-batching work
  has not re-measured the kernel corpus. The bar exists; the missing
  piece is fresh evidence, which the measurement workstream is closing.
  When a recorded kernel-scale run exists, adding the measured `gc
  --dry-run` invocation to this workflow is a small follow-up — the
  workflow deliberately does not run it *before* evidence exists,
  because an unmeasured assertion in CI is a fabricated number, not a
  gate.
* The `corpus_ripgrep_*` import gates — those are already bounded,
  cloneable, and running in the Tier-1 perf job.

## Anti-vacuous-pass rules (the invariants of this file)

Every job added or changed here follows the same three rules, and any
future gate should too:

1. **A skip must be loud or impossible.** Suites that legitimately skip
   (live services, FUSE) run only in jobs whose *runner label or env
   wiring* is the opt-in, and each such job carries a summary step that
   fails if the suite skipped on a host that promised not to. A job that
   can pass by skipping is not a gate.
2. **A gate produces its own fixtures.** The web binary, the workspace
   bins, the compose services, the ripgrep mirror — built or booted by
   the job, never assumed (dogfood #17/#20).
3. **Bounds are two-sided.** Time-bounded jobs also carry execution
   bounds (`-runs`), and opt-in jobs assert on the *output* of the run
   (skip-note greps, pass summaries) rather than on `-- --list` counts
   — skipped tests still list, so a list count can never distinguish a
   run from a skip.

## Verifying the workflows themselves

* YAML parses: `python3 -c "import yaml; yaml.safe_load(open(f))"` for
  both files.
* Schema and expression lint: `actionlint` with
  [`actionlint.yaml`](../actionlint.yaml) (which declares the two custom
  self-hosted labels `fuse` and `kernel-corpus`) and `shellcheck` for
  every embedded `run:` script.
* The push-path Playwright job's exact command sequence (`npm ci`,
  `npx playwright install chromium`, `cargo build -p gardenbed-web`,
  `npx playwright test`) was executed locally against this tree: 69/69
  tests pass against the real binary.
* The live-services job's suite invocations and env contract were
  executed locally against the compose stack (see the job's `env:` block
  against tools/scale/README.md).
