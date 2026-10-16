# gardenbed 🌱

An open-source, agent-scale source control system. Native-first: a
server-side changeset log with a thin, always-online client — designed for
repos where the merge rate, not the headcount, is the bottleneck.

**The repo is the garden. Your checkout is the gardenbed. `bed` is the tool
you tend it with.** Git planted the vocabulary (`branch`, `fork`, `root`,
`tree`, `cherry-pick`); we grow it with `sow`, `graft`, `harvest`, `prune`.

Inspired by Anthropic's [Scion](https://www.anthropic.com/engineering/redesigning-source-control)
and a decade of Sapling/Mononoke prior art — but designed from day one to
interoperate with git rather than replace it wholesale.

## The mental model

The **arboretum** is the server-side repository and the only authority: a
`loam` content-addressed object store plus a `seedbed` sqlite metadata
store. Your checkout is the **bed** — sparse by construction, it
materializes nothing until you plant a **plot** (the path prefixes you
tend). The loop, end to end:

> import a git repo into the arboretum → `bed clone` a bed that fetches
> roots only → `bed plot add` the subtrees you tend → edit and `bed sow`
> changesets → `bed graft` them onto moved parents, `bed harvest` them
> into `main` (a compare-and-swap) → `gardenbed-server export` hands the
> history back to git with identical SHAs — while `gardenbed-mcp` answers
> log/diff/blame/file questions with **no working copy anywhere**.

Git interoperability is bidirectional: rootstock imports git history (the
seedbed's `git_map` is a bijection — every git SHA ↔ one changeset id) and
exports it back through one `git fast-import` stream; unsigned histories
round-trip byte-exactly.

## Quickstart (ten lines)

```sh
cargo build --release
BIN="$PWD/target/release"
$BIN/gardenbed-server import --from /path/to/git-repo --root /tmp/arboretum
$BIN/gardenbed-server --addr 127.0.0.1:9418 --root /tmp/arboretum &   # default addr
mkdir -p ~/garden && cd ~/garden
$BIN/bed clone http://127.0.0.1:9418         # sparse: materializes nothing
$BIN/bed plot add src                        # plant what you tend
# edit files under src/, then:
GARDENBED_AUTHOR="A. Gardener <gardener@example.com>" $BIN/bed sow -m "tend the roses"
$BIN/bed harvest                             # fast-forward main to the sown changeset
$BIN/gardenbed-server export --root /tmp/arboretum --into /tmp/export.git
```

The ten-minute tour with every flag explained is
[docs/quickstart.md](docs/quickstart.md); the full command reference is
[docs/cli.md](docs/cli.md).

## Crates

The workspace (`Cargo.toml`, `crates/*`) — every crate is real code, not
scaffolding:

| crate | binary | what it is |
|---|---|---|
| `gardenbed-model` | — | canonical objects: `Id`, `Changeset`, `Change`, `Tree` codecs over the byte spec ([docs/canonical-encoding.md](docs/canonical-encoding.md)) |
| `gardenbed-store` | — | `loam` (content-addressed fs object store) + `seedbed` (sqlite metadata: changesets, refs, git mapping, derived cache) |
| `gardenbed-derive` | — | the compost engine: tree derivation (touched dirs only), tree diff, first-parent blame, ancestry skip list |
| `gardenbed-protocol` | — | `gardenbed.proto` (17 RPCs), tonic/prost codegen, and the thin client wrapper |
| `gardenbed-server` | `gardenbed-server` | the arboretum: `Repo` service layer, the gRPC server, `import`/`export` git subcommands |
| `gardenbed` | `bed` | the client: sparse bed, treestate, want-list fetcher, sow/graft/harvest — the `bed` CLI |
| `gardenbed-mcp` | `gardenbed-mcp` | MCP stdio tools (`log`, `diff`, `blame`, `file`) with no working copy |
| `gardenbed-rootstock` | — | the git bridge: import (gitoxide pack reads, topological ingest, renames) and export (one `git fast-import` stream) |
| `gardenbed-bench` | `gen-demo-corpus` | criterion suites behind the M4/M5/M6 perf budgets, plus the deterministic demo corpus |
| `gardenbed-harness` | — | seeded history generator, git materializer, and differential comparators against git itself |

(`crates/scarecrow` is a stub — the daemon is v0.2 scope,
[docs/v0.1.md](docs/v0.1.md) §1.)

## Status

**v0.1 — complete: milestones M0–M8 all landed and verified, plus two
post-M8 hardening rounds.** The v0.1 plan — milestones with verification
criteria — is [docs/v0.1.md](docs/v0.1.md); the close-out record with every
gate result is [docs/release-0.1.md](docs/release-0.1.md).

| milestone | scope | state |
|---|---|---|
| M0 | canonical model + cross-language encoding spec | ✅ |
| M1 | loam object store + seedbed metadata (sqlite) | ✅ |
| M2 | tree derivation, diff, object API | ✅ |
| M3 | rootstock git import + differential harness | ✅ |
| M4 | gRPC server + bed read path | ✅ |
| M5 | write path: sow, graft, harvest, export | ✅ |
| M6 | blame + ancestry skip list | ✅ |
| M7 | MCP surface (log/diff/file/blame) | ✅ |
| M8 | demo, chaos suite, fuzz sweep, benches, CI, docs | ✅ |

Verification evidence, all recorded in
[docs/release-0.1.md](docs/release-0.1.md) and
[docs/corpora.md](docs/corpora.md):

- **North-star demo** (`demo/run-v0.1.sh`) ends **PASS**: import of a
  50,100-commit history with the `git_map` bijection proven
  (50,100 ↔ 50,100), sparse `bed clone` in 46 ms, a 1,021-file plot
  byte-verified against `git show` (all 1,011 materialized files, 10
  symlinks, 10 exec bits), `bed log --limit 1` answered in 46 ms,
  export → `git fsck --strict` clean with **all 50,100** exported SHAs
  matching git's (unsigned history), and MCP answers byte-matched
  against git oracles with zero working copy.
- **SIGKILL chaos suite**: every RPC class killed under load and
  re-verified offline (loam checksum sweep, sqlite `integrity_check`,
  derived-data rebuild); default 10 × 10 cycles green in `cargo test
  --workspace`, extended run of 10 × 100 cycles (1,000 kill/restart
  rounds) green in 188 s.
- **Fuzz**: six cargo-fuzz targets, sweeps clean (~120–180 s each; a
  post-M8 re-run added ~330 M execs with zero new crashes); three
  findings found during the sweeps were fixed and pinned.
- **Benchmarks**: every M4/M5/M6 budget met — sow latency is
  size-independent (p50 ratio 1.002 between 10k- and 1M-file repos),
  1k-file cold `plot add` 1.07 s against the 5 s budget.
- **Kernel-scale verification**: torvalds/linux `master` (1,485,163
  commits) imported with an exact bijection, a 1,000-sample content sweep
  vs `git show` at 100% match, and an incremental re-import proof of
  **0 new objects in 0.03 s**.
- **CI** (`.github/workflows/ci.yml`) runs fmt, clippy `-D warnings`,
  `cargo test --workspace` (chaos suite included), release perf budgets,
  a fuzz smoke, and the demo on every push/PR.

Out of scope for v0.1 (nursery cache tier, scarecrow daemon, server-side
merge, GC, offline sow queue, …) is contractual — see the table in
[docs/v0.1.md](docs/v0.1.md) §1.

## Docs

| doc | contents |
|---|---|
| [docs/quickstart.md](docs/quickstart.md) | the ten-minute tour: build → arboretum → clone → plot → sow → harvest → MCP |
| [docs/architecture.md](docs/architecture.md) | the system in layers: protocol, server, data model, stores, import pipeline, blame, durability |
| [docs/cli.md](docs/cli.md) | every `bed`, `gardenbed-server`, and `gardenbed-mcp` command, from `--help` |
| [docs/canonical-encoding.md](docs/canonical-encoding.md) | the byte-level object spec (half of the published protocol, with the `.proto`) |
| [docs/divergences.md](docs/divergences.md) | every documented divergence from git, with repros |
| [docs/corpora.md](docs/corpora.md) | pinned verification corpora and the kernel-scale import record |
| [docs/mcp.md](docs/mcp.md) | the MCP surface: protocol, tools, zero-checkout contract |
| [docs/release-0.1.md](docs/release-0.1.md) | the M8 close-out: gates, benches, deviations, release checklist |
| [docs/v0.1.md](docs/v0.1.md) | the v0.1 plan: milestones, decisions (D1–D8), verification criteria |
| [DESIGN.md](DESIGN.md) | the original design notes |

## Install (planned)

```sh
cargo install gardenbed   # puts `bed` on your PATH
```

Homebrew formula will be `gardenbed` (brew formula names are global, and
`brew install bed` already belongs to a hex editor — the formula installs
the `bed` binary regardless).
