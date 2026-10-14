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

## Status

**v0.1 — complete: milestones M0–M8 all landed and verified.** Design
lives in [DESIGN.md](DESIGN.md); the v0.1 plan — milestones with
verification criteria — is in [docs/v0.1.md](docs/v0.1.md); the
two-command tour is in [docs/quickstart.md](docs/quickstart.md).

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

The Rust workspace is real code, not scaffolding: `bed` (the CLI),
`gardenbed-server` (the arboretum), `gardenbed-mcp` (checkout-free
answers), the `rootstock` git bridge, and the `loam`/`seedbed` stores —
plus a `harness` crate that differential-tests everything against git
itself. M8's hardening gates are closed and recorded in
[docs/release-0.1.md](docs/release-0.1.md): the scripted north-star demo
(`demo/run-v0.1.sh`) ends **PASS**, a SIGKILL-chaos suite covers every
RPC class, six cargo-fuzz targets ran clean (two findings found and
fixed), criterion benchmarks sit inside every budget, and CI runs all of
it.

## Install (planned)

```sh
cargo install gardenbed   # puts `bed` on your PATH
```

Homebrew formula will be `gardenbed` (brew formula names are global, and
`brew install bed` already belongs to a hex editor — the formula installs
the `bed` binary regardless).
