# gardenbed in ten minutes

A working tour: bring up an arboretum, plant a bed, tend a plot, sow a
changeset, and answer repo questions without a checkout. Every command
below is real — the acceptance script `demo/run-v0.1.sh` runs this exact
loop end to end and asserts each step.

gardenbed vocabulary: **the repo is the garden** (the server-side
arboretum is authoritative); **your checkout is the bed** (sparse — you
plant only the directories you tend, the *plot*); you **sow** changesets,
**graft** them onto new parents, and **harvest** them into `main`.

## Build

```sh
cargo build --release   # Rust 1.90+
```

This puts three binaries in `target/release/`:

| binary            | role                                                        |
|-------------------|-------------------------------------------------------------|
| `bed`             | the client you tend a garden with                           |
| `gardenbed-server`| the arboretum: serve, import from git, export to git        |
| `gardenbed-mcp`   | MCP stdio server answering log/diff/file/blame, no checkout |

## 1. Bring up an arboretum

Empty, in one line (it prints its listening address on stdout's first
line — `:0` picks a free port):

```sh
gardenbed-server --addr 127.0.0.1:0 --root /tmp/arboretum
# listening on 127.0.0.1:41234
```

`--root` is created if missing; it holds the object store (`loam/`) and
the metadata database (`seedbed.sqlite3`). Durability: every object is
fsynced on write; set `GARDENBED_FSYNC=off` only for throwaway fixtures
and benchmarks.

## 2. Grow it from an existing git repository (optional)

```sh
gardenbed-server import --from /path/to/git-repo --root /tmp/arboretum
```

The import walks the git graph, stores every commit as a changeset
(blobs inline when small, delta-compressed objects otherwise), records
renames by default (`--renames off` for delete+add pairs), and moves each
branch's ref (`--ref 'refs/heads/*'` to filter; empty means every
branch). A URL is mirror-cloned into `<root>/rootstock-mirror.git` first,
so re-imports are incremental. Every imported commit keeps its git
identity: the seedbed's `git_map` table maps each git SHA to its
changeset id, and an export reproduces unsigned SHAs byte-exactly.

To hand the history back to git:

```sh
gardenbed-server export --root /tmp/arboretum --into /tmp/export.git \
    --mapping /tmp/mapping.tsv
git -C /tmp/export.git log --oneline          # a real git repository
```

## 3. Plant a bed

```sh
mkdir -p ~/garden && cd ~/garden
bed clone http://127.0.0.1:41234
# planted a bed for garden at http://127.0.0.1:41234
# the garden grows from 1a2b3c… (main)
# materialized nothing — plant files with `bed plot add <path>`
```

A clone fetches the roots and nothing else: no working files are
materialized, ever. `--repo NAME` names the repo (the arboretum's choice
when omitted; v0.1 arboreta are single-repo). `--plot src/` records plot
prefixes up front. All local state lives under `./.bed/` (`config.toml`,
a treestate sqlite index, a content-addressed object cache, and a heads
stack); `--bed-dir DIR` moves it.

## 4. Tend a plot

```sh
bed plot add src            # fetch + materialize one prefix
bed plot list               # which prefixes this bed materializes
bed plot remove tests/
```

Materialized files carry git's modes (exec bits are real, symlinks are
real symlinks). Reads work against the arboretum; status works offline:

```sh
bed status                  # mtime/size hints; only suspects get hashed
bed status --full-scan      # hash every tracked file
bed file src/main.rs        # one file's bytes at the current changeset
bed log --limit 5           # first-parent history, newest first
bed diff                    # current changeset vs its first parent
bed diff main 4f5e6d…       # any two changesets or refs
bed blame src/main.rs       # first-parent lineage blame
bed goto main               # move the bed to a changeset or ref
```

`--cs` accepts a 64-hex changeset id or a ref name on `file`, `log`,
`diff`, and `blame`; a bare branch name (`main`) resolves to its
`refs/heads/main` ref, which is how imported histories store it.

## 5. Sow, graft, harvest

Edit files under the plot, then:

```sh
export GARDENBED_AUTHOR="A. Gardener <gardener@example.com>"
bed sow -m "tend the roses"
# planted seed 9f8e7d…: 3 change(s) (2 added, 1 modified)
# the bed now grows from 9f8e7d… (was 1a2b3c…)
```

`--author` overrides the environment variable; heavy blobs (>1 MiB)
stream to the arboretum first, small ones ride inline on the changeset.
The new changeset is queryable immediately — `bed log --limit 1` or any
MCP tool sees it.

Two more write moves, both server-side:

```sh
bed graft main              # replay the current changeset onto a new parent
bed harvest                 # fast-forward `main` to the current changeset
```

`graft` detects hard conflicts (a write onto a differently-content path,
a delete of a missing path, …) and reports every conflicting path; the
replayed changeset gets a fresh id with a single parent. `harvest` is a
CAS: racing harvesters see exactly one winner, and the loser is told the
new tip. Harvesting publishes — the bed stays where it is.

## 6. Ask without a checkout

`gardenbed-mcp` opens the arboretum's stores in process and serves the
MCP stdio protocol (one JSON-RPC message per line). It needs no bed, no
working copy, and no network:

```sh
gardenbed-mcp --root /tmp/arboretum --repo garden < requests.jsonl
```

```jsonc
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"me","version":"0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"log","arguments":{"limit":3}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"blame","arguments":{"path":"story.md"}}}
```

Four tools, all taking the same `cs` convention as `bed` (omit for
`main`): `log {cs?, limit?}`, `diff {a, b}`, `file {cs?, path}`,
`blame {cs?, path}`.

## The demo

`demo/run-v0.1.sh` automates this whole tour (import → serve → clone →
plot add → sow → export → MCP) and byte-verifies every step against git
itself; it writes `demo/report.md` and exits non-zero on the first
failed assertion. Flags: `--keep-going` (run every step), `--keep-work`
(keep the workdir), `--workdir DIR`, `--corpus DIR` (import a real
repository instead of the generated one), `--commits N`.

## Where things live

- Arboretum (`--root`): `loam/` content-addressed objects,
  `seedbed.sqlite3` metadata (changesets, refs, derived data,
  `git_map`), optional `rootstock-mirror.git`.
- Bed (`.bed/`): `config.toml` (server, repo, plot, current changeset),
  `treestate` sqlite index, `objects/` cache, `heads` stack.
- Refs: stored as full names (`refs/heads/main`); clients may use the
  bare name (`main`) — the arboretum resolves both spellings, and
  `harvest` compare-and-swaps `refs/heads/main`.
