# Benchmarking Gardenbed against Git

Gardenbed's claim is narrower than "faster than Git". It is: **for many
agents working in one project, the per-worker cost of getting the files
you need, staying current, and landing work should not scale with the
size of the repository or with the number of other agents.** Git's costs
mostly do.

This document does three things:

1. maps the dimensions where the claim can be *measured* (and the ones
   where it cannot, or where Git legitimately wins);
2. specifies the paired harness (`gardenbed-ab`) that measures them on
   both sides from one history;
3. records what has been measured so far, including the methodology
   traps that will otherwise produce flattering nonsense.

The harness lives at `crates/bench/src/bin/ab.rs`. Everything below is
about running it honestly; the numeric results in
[§5](#5-what-has-been-measured) are from the recorded run described there.

## 1. The dimensions

Each row is a *mechanism* — why the two systems should differ — not just
a command. A dimension without a mechanism is not worth a benchmark.

| # | Dimension | Mechanism | Git cost scales with | Gardenbed cost scales with |
|---|---|---|---|---|
| 1 | Cold worker provisioning | Git must have the object graph before it can answer; Gardenbed answers from a server that already has it | repo size (commits + trees, or trees only when filtered) | files actually selected |
| 2 | Disk per worker | A Git clone stores history; a Gardenbed working copy stores a ref, an index, and the blobs it materialized | full history, once per clone — unless worktrees share it | selected blobs only |
| 3 | Network per worker | `--filter=blob:none` postdates most CI setups, and lazy fetches amplify per query | packfile for history | want-list of exact objects |
| 4 | Refresh cycle | Both fetch, but only Gardenbed's tree derivation is server-side | change size | change size |
| 5 | Commit / changeset cost | Gardenbed writes changes against the first parent; Git writes a whole tree object set | changed paths (both) | changed paths |
| 6 | Landing contention | Git push is a ref compare-and-swap with no server-side rebase; Gardenbed's rebase and merge execute server-side | retry rounds × rebase cost | retry rounds × server-side replay |
| 7 | Historical queries without a checkout | Gardenbed's log/diff/file/blame are RPCs; Git needs history locally and `blame` is always local | history size | one answered query |
| 8 | Multi-repo fan-out | One server serves many repositories with LRU handles; Git needs one clone per repository | repos × repo size | repos × ref lookup |
| 9 | Failure/retry cost | Git leaves partial clones to clean up; Gardenbed's want-list is idempotent by construction | interrupted transfer | nothing |
| 10 | Offline work | `bed commit` queues and `bed sync` replays; Git needs the remote for push | — | — |

Dimensions **not** in the table, deliberately:

* **Fetch throughput on a LAN.** Git's packfile generation, delta
  compression, and `--filter` are extremely good at bulk transfer. A
  benchmark that measures megabytes/second on a fast link measures Git's
  strength, not Gardenbed's. The interesting number is *bytes per
  worker*, not bytes per second.
* **Working-copy mutation.** `status`, `diff` on dirty files, and
  checkout are local filesystem work on both sides; expect parity.
  `bed status` at 10k files is already covered by the criterion suite.
* **Packfile size on disk for a single clone.** Git will usually win on
  a single long-lived clone, because packfiles are delta-compressed
  across history and Gardenbed stores objects individually (with
  chunking, but no cross-object deltas). The claim is about N workers,
  not one.

## 2. The harness

```sh
cargo build --release --bin gardenbed-ab
```

Every scenario takes the same fixture flags and runs each side against
the *same history*:

```sh
gardenbed-ab provision \
  --bed-url http://127.0.0.1:9461/demo \
  --git-url git://127.0.0.1:9462/demo.git \
  --branch main --sparse big --workers 8
```

### Sides

| Side | What it does |
|---|---|
| `bed` | `bed clone` (a ref lookup; no files) then `bed sparse add <prefix>` |
| `git-partial` | `git clone --filter=blob:none --no-checkout --sparse`, `sparse-checkout set`, `checkout` — Git's best cold-worker configuration |
| `git-full` | full clone, then a sparse checkout — what most projects actually do |
| `git-worktree` | one shared clone, then `git worktree add` per worker — the realistic "many agents on one machine" Git setup |

`git-worktree` matters: it is the fair Git answer to "N workers, one
object store", and on a single host it removes most of Git's provisioning
disadvantage while keeping its checkout cost. Any claim that ignores it is
inflated.

### What is recorded

* **wall**, **user + system CPU**, and **max RSS** per spawned process,
  via `wait4(2)` — not `Instant::now()` around a `Child::wait`, which
  misses the child's CPU entirely;
* **network bytes** per step, in both directions, through a transparent
  TCP relay in front of each server (works for `git://` and h2c gRPC);
* **disk** for the worker tree: apparent size and allocated blocks;
* the **outcome** of every step, so a failure is never silently timed.

`--json <path>` writes the rows for charting or regression tracking.

### Scenarios

| Scenario | Measures |
|---|---|
| `provision` | N cold workers, each materializing one sparse prefix |
| `query` | a fresh worker per query kind (`log`, `file`, `blame`), with an equivalence check against the reference |
| `land` | N lanes commit, then land **serially** — the release queue |
| `contend` | N lanes publish at once; first-wave outcomes and retry rounds |
| `refresh` | N warm workers pick up a base that moved |
| `history` | history queries with **no working copy** (API vs. checkout) |

## 3. Methodology traps

These are not hypothetical; each one produced a wrong answer during
development of the harness. The harness now enforces the fix.

1. **`git daemon` ignores `--filter` unless the repository sets
   `uploadpack.allowFilter=true`.** Git prints a warning and then
   performs a *full* clone. The first kernel measurement downloaded
   6.9 GB where a blobless clone needed 2.1 GB. The harness **fails**
   the run if `remote.origin.partialclonefilter` is not `blob:none` or if
   the warning appears.
2. **Local-path clones ignore `--filter` too** (`file://` is required).
   The harness rewrites a local `--git-url` to a canonical `file://` URL
   for this reason.
3. **A relay without `TCP_NODELAY` fabricates latency.** Gardenbed's
   fetch is many small round-trips; Git's is a few bulk transfers. With
   Nagle/delayed-ACK interaction, `bed sparse add` measured 430 ms
   instead of 11 ms. The relay sets `TCP_NODELAY` on both sockets.
4. **Sparse checkouts reject writes outside the selected prefix**, so
   lane files must live inside `--sparse` or Git silently commits
   nothing. Lane paths also carry the side name and a per-run token: a
   shared fixture would otherwise let a rerun find its own previous
   commits already at HEAD and "land" nothing.
5. **`git daemon` needs `--enable=receive-pack`** for pushes, and Git
   needs `user.name`/`user.email` in the environment or `rebase`
   re-writes fail.
6. **A full tmpfs makes Git fail fast.** `git checkout` dies with
   "unable to write file" and the harness would otherwise record a
   12 ms "provisioning" time. The harness refuses to start with less
   than 2 GiB free and names failed workers in the aggregate line.
7. **Do not warm a cache before timing.** A single captured run per
   query; no warm-up iteration. Warm-up runs would hide exactly what
   `query` exists to measure.
8. **Fixtures on tmpfs, fsync on.** The same rule as the criterion
   benches: measure the design, not the host disk. `GARDENBED_FSYNC` is
   never set by the harness, and `GARDENBED_DAEMON=""` disables the
   shared cache — a daemon would silently flatter per-worker numbers.

## 4. Running it

One command builds a fresh fixture and runs everything:

```sh
cargo build --release
target/release/gardenbed-ab suite \
  --source /path/to/repo \
  --commits 200 \
  --out /tmp/ab-run \
  --workers 1,4,16
```

Each run:

1. slices the newest `--commits` commits and **grafts a real root** at the
   cut point, so the slice is a complete history both systems accept
   (`--limit` re-roots it on the Gardenbed side; a shallow clone cannot
   be imported);
2. exports that branch into a fresh bare repository and imports it into a
   Gardenbed root, so both sides hold byte-identical history;
3. starts a git daemon and a `gardenbed-server` on free ports, writing
   `fixture.json` with the URLs;
4. runs every scenario against that fixture and writes one JSON file per
   scenario.

`--out` must be empty. That is deliberate: a run can never inherit a
previous run's pushes, lane commits, or moving refs. Kill the fixture
servers afterwards with `kill $(cat /tmp/ab-run/fixture/*.pid)`.

For one scenario at a time, build the fixture first and pass its URLs:

```sh
target/release/gardenbed-ab fixture --source /path/to/repo --out /tmp/fx
target/release/gardenbed-ab refresh \
  --bed-url http://127.0.0.1:PORT/demo \
  --git-url git://127.0.0.1:PORT/slice.git \
  --git-reference /tmp/fx/slice.git \
  --sparse big --workers 4
```

`--concurrency 1` is the defensible default for `provision`: parallel
workers contend for one server, and a concurrency sweep is a separate
question. Report the sweep separately rather than burying it in an
aggregate.

### The harness verifies its own inputs

The run is only meaningful if it measured the intended binaries and the
intended behaviour, so the harness checks both and fails loudly:

* **Which binaries.** `bed` and `gardenbed-server` resolve to `$BED_BIN`
  / `$BED_SERVER_BIN`, else **siblings of `gardenbed-ab`**, else PATH.
  Each run prints the resolved paths. (A bare `bed` from PATH is
  frequently a stale installed build — during development, reading it
  instead of the rebuilt one produced a 141 ms number where the truth
  was 10 ms.)
* **Which history.** Both sides must hold the same commits; the fixture
  builder builds them from one source and asserts the slice's root has
  no parents.
* **Whether the answers match.** `query` compares `file` byte-for-byte
  against a local reference, and checks `log`/`blame` line counts.
* **Whether git was configured badly.** A `git-partial` run fails if the
  server ignored `--filter` or the clone did not record the promisor
  filter.

## 5. What has been measured

Recorded Oct 19, 2026 on a 16-core host, tmpfs fixtures, fsync on, no
daemon, one worker at a time (`--concurrency 1`). The fixture is the
synthetic `gen-demo-corpus` history — 20,040 commits, 3,222 files at
head, 43 MiB bare — imported into both sides, so the two systems hold
byte-identical history. Query results were verified equivalent against a
local reference copy (`--git-reference`): no divergence notes.

Provisioning, 8 cold workers, `--sparse big` (3,222 files):

| | bed | `git-partial` | `git-full` | `git-worktree` |
|---|---:|---:|---:|---:|
| wall | **1.95 s** | 24.91 s | 69.38 s | 8.91 s |
| CPU total | **4.17 s** | 45.64 s | 59.08 s | 7.77 s |
| server→client | **4.0 MiB** | 265.0 MiB | 316.8 MiB | 39.6 MiB |
| disk for 8 workers | 204.9 MiB | 386.1 MiB | 446.1 MiB | **145.6 MiB** |

One query on a freshly provisioned worker (`big/f00001.txt`), including
that worker's provisioning:

| | bed | `git-partial` | `git-full` |
|---|---:|---:|---:|
| `log` (50 changesets) | 4.4 ms | 1.6 ms | 2.2 ms |
| `file` | 4.8 ms | 2.5 ms | 2.5 ms |
| `blame` (all lines) | **19.6 ms** | 1001.7 ms | 548.2 ms |

History with no working copy at all:

| | bed | git (bare mirror) |
|---|---:|---:|
| connect | **4.9 ms** | 2747.9 ms |
| `log` (50) | 3.7 ms | 1.7 ms |
| `file` | 7.2 ms | 52.8 ms |
| `blame` (all lines) | **8.7 ms** | 1006.5 ms |

Landing 6 lanes serially, `--sparse data` (10 files) — the release queue:

| | bed | `git-partial` | `git-worktree` |
|---|---:|---:|---:|
| total | **76.9 ms** | 1169.6 ms | 709.5 ms |
| rebase (mean/lane) | **8.9 ms** | 53.3 ms | 11.9 ms |
| fetch (mean/lane) | in rebase | 85.0 ms | 50.7 ms |
| publish/push (mean/lane) | **3.9 ms** | 56.4 ms | 55.5 ms |

Refresh, 5 workers picking up a moved base:

| | bed `switch` | `git-partial` fetch + checkout | `git-worktree` fetch + reset |
|---|---:|---:|---:|
| `--sparse data` (10 files) | **7.0 ms** | 150.1 ms | 66.0 ms |
| `--sparse big` (3,222 files) | 139.7 ms | 174.0 ms | — |

Contention, 6 lanes publishing at once: both sides accept exactly one
first-wave publish and resolve the rest in one serialized retry round.
The conflict contract is equivalent; the cost difference is the same
server-side-rebase advantage seen under `land`.

### Kernel history (1,485,163 commits, 96,080 files)

Recorded Oct 19, 2026, same host. The Gardenbed side is the real kernel
store (`seedbed.sqlite3` + `loam/objects`, opened in place by the layout
fix); the Git side is the same repository served by `git daemon` with
`uploadpack.allowFilter=true`. One worker, `--sparse drivers/net`
(6,900 files):

| | bed | `git-partial` | `git-full` |
|---|---:|---:|---:|
| wall | **32.6 s** | 177.2 s | 381.3 s |
| CPU | **5.2 s** | 139.7 s | 657.3 s |
| server→client | **146.2 MiB** | 1,818.2 MiB | 7,948.8 MiB |
| disk | **214.8 MiB** | 2,253.8 MiB | 6,730.7 MiB |
| `clone` step | 62 ms | — | — |
| materialize step | 25.1 s | — | — |

Per-worker wall time is flat for Gardenbed and grows with repository
size for Git: the 4-worker run measured 96.2 s / 584.9 MiB for bed
against 697.4 s / 7,272.7 MiB for `git-partial`, i.e. roughly linear in
worker count on both sides with a ~7× constant.

Warm queries on the same history (`drivers/net/Kconfig`, median of 3):

| | bed | git |
|---|---:|---:|
| `blame` (whole file) | **0.10 s** | 0.85 s |
| file contents | ~0.01 s | ~0.01 s |
| `log -n 50` | ~0.01 s | ~0.01 s |

Two caveats this run makes concrete:

* **The first query pays for derived data.** An untouched kernel store
  answered the first `blame` in 18.6 s while the server computed the
  walk; every subsequent call took 0.10 s. Report warm and cold
  separately — a single cold number would misrepresent both systems.
* **Git's lazy fetch shows up as amplification.** A `git-partial` worker
  whose server is unreachable cannot even `blame` a file it has checked
  out: blame needs historical blobs, which the promisor filter omitted.
  That is a real operational property of partial clones, not a
  benchmark artifact, and it is why the harness keeps the daemon up for
  the whole run.

### What these numbers say

* **Provisioning is the strongest result and it is not close.** Eight
  workers cost 4.0 MiB of server→client traffic where Git's best
  configuration spent 265.0 MiB, and 2.23 s against 26.41 s wall. Read
  that as "the cost Gardenbed was designed to avoid is, in fact,
  avoided", not as a general speed claim: the whole difference is that a
  Git worker must hold history before it can answer.
* **`git-worktree` is the honest comparison.** One shared clone plus 8
  worktrees costs 8.91 s, 39.6 MiB of network, and 145.6 MiB of disk —
  less disk than bed's 204.9 MiB, because every worktree reads the same
  object store. So at 20k commits on one machine, Gardenbed's advantage
  over the *right* Git setup is real but modest (≈4.5× wall, ≈0.2× the
  network, and it *loses* on disk). Any disk claim must be made against
  worktrees. Scale is what separates them: at kernel size a worktree
  still needs the whole history on the machine (see the kernel section).
* **Small queries are a wash, or Git wins.** `log` and `file` are
  1.6–4.8 ms on both sides; process startup and local I/O dominate.
  There is no claim here, and a benchmark that reported one would be
  measuring noise.
* **`blame` is where the architectures actually differ.** Git's blame is
  local and walks history — 548–1002 ms on a 20k-commit file;
  Gardenbed's is a server-side answer in 8.7–19.6 ms. This is the
  largest measured ratio in the whole matrix, and it is the one that
  matters if agents run blame inside their loop.
* **No-checkout history is a provisioning claim, not a query claim.**
  Git's per-query times are fine once a mirror exists (sometimes faster:
  `log` 1.7 ms vs 3.7 ms); what it cannot do is answer without first
  paying 2.75 s and 32.9 MiB of history transfer. Gardenbed pays 4.9 ms
  and 497 bytes to connect.
* **Landing favors Gardenbed at this size** (76.9 ms vs 709.5–1169.6 ms
  for 6 lanes) because rebase runs server-side and the lane never checks
  out or updates the base. The gap should shrink as the change grows
  relative to per-command overhead; that is untested.
* **Contention behaviour is equivalent.** Both sides accept exactly one
  first-wave publish out of six and need exactly one serialized retry
  round. Gardenbed's compare-and-swap is not more permissive than Git's;
  the win, if any, is in the cost of each retry (see `land`).

### Refresh cost: fixed

`bed switch` — and therefore `refresh` — used to re-verify the **entire
sparse selection**, not just the change. Measured on the same fixture
before the fix:

| selection | bed `switch` | git fetch + checkout |
|---|---:|---:|
| 10 files (`data`) | 7.0 ms | 150.1 ms |
| 3,222 files (`big`) | 139.7 ms | 174.0 ms |

Git's cost is flat against selection size; Gardenbed's grew with it —
exactly the shape the design says should not happen. A syscall profile of
a warm `switch` over 3,222 files showed ~16k `statx` + ~32k `fcntl` and,
worse, two full read-and-hash passes over every selected file (once in
preflight, once in the write loop) plus one SQLite lookup and one upsert
per file.

The fix has three parts, all in `crates/bed`:

* **Hint-guided proof.** A path whose treestate row is `HINT_VERIFIED`
  and whose size, mtime, and mode still match is settled by `stat`
  alone — no open, no read, no hash (`CheckoutFs::entry_verified`). This
  is the same trust `bed status` already extends to hints.
* **Settled paths are skipped entirely.** A path whose target blob and
  mode already equal its verified row cannot be written and cannot
  refuse anything, so it does not need the disk at all
  (`sync_sparse` / `preflight`).
* **One query, one transaction.** `Treestate::all_entries` loads the
  index once, and `upsert_all` / `remove_all` write it in one
  transaction, replacing per-path round-trips.

After:

| selection | bed `switch` | `git-partial` fetch + checkout | `git-worktree` fetch + reset |
|---|---:|---:|---:|
| 10 files (`data`) | **5.4 ms** | 150.0 ms | 75.7 ms |
| 3,222 files (`big`) | **18.9 ms** | 167.5 ms | 113.1 ms |

Gardenbed wins at both sizes now, and the remaining growth (5.4 → 18.9 ms
for a 320× larger selection) is the one stat pass that correctness still
requires. The safety contract is unchanged and covered by tests: a
verified row that no longer matches is read and refused, an unverified
row is always read, and a skipped path is never rewritten.

A caveat about these particular figures: they come from the **bounded
202-commit fixture**, where Git's own refresh is cheap because there is
little history to walk. The pre-fix asymmetry is what mattered
structurally — Gardenbed's cost grew with the selection while Git's did
not — and that is gone. Re-run on a full-sized corpus before quoting the
ratios in public.

## 6. What to build next

In rough order of value per unit of work:

1. **Selection-size sweep for `refresh`.** The suite already runs a
   small and a large selection; extend it to three or four sizes and plot
   the curve. The fix removed the two read-and-hash passes; what remains
   is one stat per selected path, and the sweep should show exactly that
   line.
2. **Concurrency sweep for `provision`.** 1, 4, 16, 64 workers at
   `--concurrency` 1, 4, 16. The interesting question is the server's
   behavior, not the client's: does `get_tree`/`get_blob` throughput
   degrade, and does RSS grow with open repositories?
3. **Release-queue depth.** `land` with 4, 16, 64 lanes where each lane
   touches a *different* file versus the *same* file (conflict path).
   The former is the intended case; the latter measures conflict
   detection and the retry contract.
4. **More real-corpus slices.** Full kernel measurements are already
   recorded in §5; broaden them across slices and selection sizes. The
   kernel corpus is directly usable: the pre-rename
   layout (`seedbed.sqlite3` + `loam/objects`) opens in place, and
   `gardenbed-ab fixture --source ...` builds a bounded slice from any
   repository. First measurement on the real 1.48M-commit store, one
   worker taking `drivers/net` (6,900 files): **3.6 s and 55 MB** for
   bed, versus **155 s and 2.1 GB** for a blobless sparse Git clone.
   Run the suite across several slices to get the full picture; files
   count matters more than commit count here.
5. **Multi-repo fan-out** (dimension 8). One Gardenbed server with 32
   repositories versus 32 clones. This is where the LRU handle cap and
   per-repository counters matter, and where Git has no server-side
   equivalent at all.
6. **Interrupted provisioning** (dimension 9). `SIGKILL` a worker mid
   `sparse add`, then resume; compare with a killed `git clone` and its
   partial packfile. Gardenbed claims idempotent want-list recovery; this
   is the test.
7. **CI footprint** as a whole, not per worker. Disk + network + peak
   RSS summed over a realistic job matrix, including the fact that
   Gardenbed needs a running server (or a daemon) while Git does not.
   Any honest CI comparison has to charge Gardenbed for the server.
8. **Offline queue** (dimension 10). `bed commit` with the server down,
   N commits, then `bed sync`; compare with Git's local commit followed by
   push. This is a workflow comparison as well as a timing one.

Do **not** build: a single-clone packfile-size comparison (Git wins and
it does not bear on the claim), or a LAN throughput benchmark (same
reason).
