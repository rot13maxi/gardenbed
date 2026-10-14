# garden 🌱

An open-source, agent-scale source control system. Native-first: a
server-side changeset log with a thin, always-online client — designed for
repos where the merge rate, not the headcount, is the bottleneck.

The name continues a vocabulary git planted: `branch`, `fork`, `root`,
`tree`, `cherry-pick`. Ours adds `sow`, `graft`, `harvest`, and `prune`.

Inspired by Anthropic's [Scion](https://www.anthropic.com/engineering/redesigning-source-control)
and a decade of Sapling/Mononoke prior art — but designed from day one to
interoperate with git rather than replace it wholesale.

## Status

Pre-alpha. The design lives in [DESIGN.md](DESIGN.md); the working name is
still being brainstormed (garden-themed, client CLI to match).
