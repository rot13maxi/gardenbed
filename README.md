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

Pre-alpha. Design lives in [DESIGN.md](DESIGN.md). The Rust workspace is
scaffolded: the `bed` CLI, the `scarecrow` daemon, the `arboretum` server,
and the `rootstock` git bridge are stubs awaiting their first real code.

## Install (planned)

```sh
cargo install gardenbed   # puts `bed` on your PATH
```

Homebrew formula will be `gardenbed` (brew formula names are global, and
`brew install bed` already belongs to a hex editor — the formula installs
the `bed` binary regardless).
