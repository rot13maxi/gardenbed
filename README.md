# Gardenbed

Source control for many agents working in the same repository.

Gardenbed keeps history on a central server and gives each worker a sparse
working copy. Workers fetch the files they need, create changesets, and
publish them with atomic ref updates. Agents can also query history,
files, diffs, and blame without creating a checkout.

It has its own data model and a bidirectional Git bridge. You can import
an existing Git repository, work in Gardenbed, and export back to Git.

## Why Gardenbed?

Large repositories and many concurrent workers put pressure on checkout,
commit, and integration workflows. Gardenbed is built around three ideas:

- **Fetch only what you need.** A clone creates local state without
  materializing files. Select directories or individual files afterward.
- **Record changes directly.** Changesets describe file changes against
  their first parent. Trees are derived incrementally, so creating a
  changeset touches the directories affected by the change.
- **Make repository operations available as APIs.** The CLI uses gRPC;
  an MCP server exposes read operations for agents without a working copy.

## Measured against Git

Numbers below are from `gardenbed-ab`, a paired harness that runs both
systems against **the same history** and records wall time, CPU, peak RSS,
network bytes, and disk. The first block is a synthetic 20k-commit
fixture; the other two are the real Linux kernel history (1,485,163
commits, 96,080 files), each worker taking `drivers/net` (6,900 files).
Both used a local server, tmpfs workspace where available, and fsync on.

|  | Gardenbed | `git clone --filter=blob:none` + sparse | full clone | `git worktree` ×8 |
|---|---:|---:|---:|---:|
| **20k commits, 8 workers** | | | | |
| total wall | **2.0 s** | 24.9 s | 69.4 s | 8.9 s |
| network received | **4 MiB** | 265 MiB | 317 MiB | 40 MiB |
| disk | 205 MiB | 386 MiB | 446 MiB | **146 MiB** |
| **1.48M commits, 1 worker** | | | | |
| total wall | **32.6 s** | 177 s | 381 s | — |
| network received | **146 MiB** | 1,818 MiB | 7,949 MiB | — |
| disk | **215 MiB** | 2,254 MiB | 6,731 MiB | — |
| **1.48M commits, 4 workers** | | | | |
| total wall | **96 s** | 697 s | — | 377 s |
| network received | **585 MiB** | 7,273 MiB | — | 6,131 MiB |

The gap is not a faster transfer: it is that a Git worker must hold
history before it can answer, so its cost scales with the repository. A
Gardenbed worker fetches the files it selected and a ref, so its cost
scales with the selection.

`git worktree` is the baseline that matters on one machine, and it is the
strongest competitor: at 20k commits it is within ~4.5× of Gardenbed and
uses *less* disk, because worktrees share one object store. What it
cannot avoid is having the whole history on the machine: at kernel size
four worktree workers still transferred 6.1 GiB between them (1.5 GiB
per worker) where four Gardenbed workers transferred 585 MiB total.

Queries, same kernel history (`drivers/net/Kconfig`):

|  | Gardenbed | Git (local) |
|---|---:|---:|
| `blame` (whole file) | **0.10 s** | 0.85 s |
| file contents | ~0.01 s | ~0.01 s |
| `log -n 50` | ~0.01 s | ~0.01 s |

One honest caveat: once a worker is warm, `log` and file reads are a wash
— process startup dominates on both sides. Gardenbed's claim is about the
cost of *a worker*, not about beating Git at every command.
See [docs/benchmarking.md](docs/benchmarking.md) for the dimension map,
the methodology (including the ways a benchmark like this lies), and how
to reproduce a run with `gardenbed-ab suite`.

The server owns the repository. Creating a changeset stores it there
immediately; publishing it to `main` is a separate operation. There is no
push step between client and server.

## Try it

You need Rust 1.90 or newer and Git. Build from this checkout:

```sh
cargo build --release
```

The main binaries are `bed`, `gardenbed-server`, and `gardenbed-mcp`.
The examples below start from the Gardenbed source directory and assume
an existing Git repository with a `main` branch.

In one terminal, import the repository and start the server:

```sh
./target/release/gardenbed-server import \
  --from /path/to/git-repo --root /tmp/gardenbed --repo myrepo

./target/release/gardenbed-server \
  --root /tmp/gardenbed --addr 127.0.0.1:9418
```

In another terminal, create a working copy and fetch a directory:

```sh
BIN="$PWD/target/release"
mkdir -p /tmp/gardenbed-work && cd /tmp/gardenbed-work

"$BIN/bed" clone http://127.0.0.1:9418/myrepo
"$BIN/bed" sparse add src
"$BIN/bed" status
```

Edit files under `src`, then create a changeset and publish it:

```sh
GARDENBED_AUTHOR="Your Name <you@example.com>" \
  "$BIN/bed" commit -m "Describe the change"

"$BIN/bed" publish
```

If `main` has moved, `bed rebase main` replays the current changeset onto
its new tip. Conflicting changes are reported for you to resolve.

Export published history back to Git:

```sh
"$BIN/gardenbed-server" export \
  --root /tmp/gardenbed --repo myrepo --into /tmp/gardenbed-export.git

git -C /tmp/gardenbed-export.git fsck --strict
```

See the [quickstart](docs/quickstart.md) for a longer walkthrough and the
[command reference](docs/cli.md) for options.

## The commands

| Command | Purpose |
|---|---|
| `bed clone` | Connect a new working copy to a server; fetch no working files |
| `bed sparse add` | Fetch and materialize selected paths |
| `bed status` | Inspect local changes, including while offline |
| `bed switch` | Switch to a changeset or ref |
| `bed commit` | Create a changeset from local changes |
| `bed rebase` | Replay the current changeset onto a new parent |
| `bed publish` | Fast-forward `main` to the current changeset |
| `bed merge` | Merge a changeset or ref into the current changeset |
| `bed mount` | Mount the current changeset as a read-only FUSE filesystem |
| `bed log`, `diff`, `file`, `blame`, `copies` | Query repository content and history |

Checkout updates preserve local edits to files unchanged by the target
and refuse updates that would overwrite local work. Concurrent publishes
use compare-and-swap: a losing worker gets a conflict instead of silently
replacing another worker's update.

## Agent access

`gardenbed-mcp` reads the server's repository directly and exposes five
tools: `log`, `diff`, `file`, `blame`, and `copies`. It uses stdio and
requires no working copy:

```sh
"$BIN/gardenbed-mcp" --root /tmp/gardenbed --repo myrepo
```

See [MCP configuration and tool schemas](docs/mcp.md).

## Web GUI

`gardenbed-web` serves a read-only web interface over the repository —
history, refs, changeset detail, tree browsing, file view with blame, and
diff compare — with the UI embedded in the binary:

```sh
"$BIN/gardenbed-web" --root /tmp/gardenbed-repo
# listening on 127.0.0.1:8430
```

No working copy and no frontend build step required. A `seed` subcommand
writes a deterministic demo repository for trying it out; the Playwright
suite in `e2e/` verifies every page end to end. See [docs/web.md](docs/web.md).

## Current scope

Gardenbed is an early implementation at version 0.1 plus the v0.2
feature wave. It serves any number of repositories per server, with
filesystem object storage, SQLite metadata, sparse working copies,
server-side replay and merge, a read-only FUSE mount of any changeset,
an optional local caching daemon, optional token auth with
per-repository roles, and Git import/export.

Writes need a connection to the server, except `bed commit`, which
queues locally when the server is unreachable and replays with
`bed sync`. Garbage collection of unreferenced objects is not on main
yet (in review). Authentication is optional
(`gardenbed-server serve --auth auth.toml`; without it the server is
open). Blame follows first-parent history and renames; copy detection
is exact-blob only. Git roundtrips preserve commit IDs for supported
histories; signed commits, non-UTF-8 metadata, and other exceptions are
documented in [Git interoperability limits](docs/divergences.md).

For horizontal scale-out, the server can instead run as N stateless
replicas with metadata in PostgreSQL and objects in S3: pass
`--storage <config.toml>` (or set `GARDENBED_STORAGE`) to `serve`,
`import`, or `export`, and put a load balancer in front of the
replicas. The design contract is [scale-out-design.md](docs/scale-out-design.md)
and a ready-made compose stack (Postgres + MinIO + 3 replicas) lives in
[tools/scale/](tools/scale/); `demo/run-scaled.sh` proves cross-replica
visibility end to end.

Verification includes differential tests against Git (including
1,000-pair merge and copy-detection oracles), process-kill recovery
tests, fuzz targets, benchmarks, and an end-to-end demo. A Linux kernel
history of 1.485 million commits has been imported and sampled for
content fidelity. The [verification record](docs/release-0.1.md) and
[corpus results](docs/corpora.md) describe what was tested and its
limits.

## Development and documentation

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

The Rust workspace separates the object model, storage, derived data,
protocol, server, client, Git bridge, and MCP service into crates under
`crates/`. Start with the [architecture guide](docs/architecture.md) for
how they fit together.

- [Canonical object encoding](docs/canonical-encoding.md)
- [Durable publication validation](docs/validation.md)
- [Design and verification plan](docs/v0.1.md)
- [Changelog](CHANGELOG.md)

Gardenbed draws on Scion, Sapling/Mononoke, and Jujutsu. The original
[design notes](DESIGN.md) describe that background. See [Terminology](docs/terminology.md) for the
terms used in the current interface.

Licensed under [MIT](LICENSE).
