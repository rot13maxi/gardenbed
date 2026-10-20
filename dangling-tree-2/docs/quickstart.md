# gardenbed in ten minutes

A working tour: bring up an arboretum, plant a bed, tend a plot, sow a
changeset, and answer repo questions without a checkout. Every command
below is real — the acceptance script `demo/run-v0.1.sh` runs the tour's
spine (import → serve → clone → plot add → sow → export → MCP) end to
end and asserts each step, and the post-M8 QA pass exercised every core
operation through real binaries against git.git (82,443 commits),
byte-compared against git itself ([docs/release-0.1.md](release-0.1.md)).

gardenbed vocabulary: **the repo is the garden** (the server-side
arboretum is authoritative); **your checkout is the bed** (sparse — you
plant only the directories you tend, the *plot*); you **sow** changesets,
**graft** them onto new parents, and **harvest** them into `main`.

## Build

```sh
cargo build --release   # Rust 1.90+
```

This puts four binaries in `target/release/`:

| binary             | role                                                          |
|--------------------|---------------------------------------------------------------|
| `bed`              | the client you tend a garden with                             |
| `gardenbed-server` | the arboretum: serve, import from git, export to git          |
| `gardenbed-mcp`    | MCP stdio server answering log/diff/file/blame, no checkout   |
| `gen-demo-corpus`  | writes the deterministic demo corpus the demo script seeds from |

## 1. Bring up an arboretum

Empty, in one line (it prints its listening address on stdout's first
line — `:0` picks a free port):

```sh
gardenbed-server --addr 127.0.0.1:0 --root /tmp/arboretum
# listening on 127.0.0.1:41234
```

`--root` is created if missing; it holds the object store (`loam/`) and
the metadata database (`seedbed.sqlite3`). Durability: every object is
fsynced on write. `GARDENBED_FSYNC=off` (also `0` or `false`) relaxes
writes to atomicity-only — tmp+rename is kept, fsync calls are skipped —
which is meant for rebuildable bulk loads (corpus imports, demo seeds)
where a crash costs a re-import, not data. Real repositories must run
with durability on; the torn-write torture tests always do.

## 2. Grow it from an existing git repository (optional)

```sh
gardenbed-server import --from /path/to/git-repo --root /tmp/arboretum
# 2 commits (2 new, 0 skipped), 9 objects, 1 ref(s) moved, 0.24s (8 commits/s)
```

The import walks the git graph, stores every commit as a changeset
(blobs inline when small, delta-compressed objects otherwise), records
renames by default (`--renames off` for delete+add pairs), and moves each
branch's ref. A URL is mirror-cloned into `<root>/rootstock-mirror.git`
first, so re-imports are incremental. Every imported commit keeps its git
identity: the seedbed's `git_map` table maps each git SHA to its
changeset id, and an export reproduces unsigned SHAs byte-exactly.

The flags you will actually reach for:

| flag | meaning |
|---|---|
| `--ref refs/heads/master` | only import branches matching this glob (repeatable; empty means every branch) — handy when the garden should grow `master`, not every mirror ref |
| `--renames on` / `--renames off` | rename detection via `git diff-tree -M` (default `on`); `off` imports moved files as delete+add pairs |
| `--limit N` | ingest only the newest N commits (topological suffix; a verification convenience, not full fidelity) |

Bulk loads of rebuildable fixtures can run under the durability toggle:

```sh
GARDENBED_FSYNC=off gardenbed-server import --from /path/to/git-repo --root /tmp/arboretum
```

That keeps writes atomic but skips the fsyncs — throwaway fixtures and
benchmarks only, never a garden you keep.

To hand the history back to git (the counts below are from after the
tour's writes — §5):

```sh
gardenbed-server export --root /tmp/arboretum --into /tmp/export.git \
    --mapping /tmp/mapping.tsv
# 3 commits (0 signed dropped), 5 blobs, 2 ref(s), 0.01s
git -C /tmp/export.git log --oneline          # a real git repository
git -C /tmp/export.git fsck --strict          # clean
```

`--mapping FILE` writes one `<changeset id>\t<git sha>` line per exported
commit (the verification artifact the demo SHA-roundtrips against).
Export defaults to every ref in the seedbed (`--ref` filters by glob);
imported and native refs all live under `refs/heads/`, so exports are
git-valid. Signed commits export unsigned (a documented divergence —
[docs/divergences.md](divergences.md) §1), and `ExportReport` counts them
as `signed_dropped`.

## 3. Plant a bed

An arboretum starts empty — plant it with §2's import before cloning.

```sh
mkdir -p ~/garden && cd ~/garden
bed clone http://127.0.0.1:41234
# planted a bed for garden at http://127.0.0.1:41234
# the garden grows from 984213bc33e9f924 (master)
# materialized nothing — plant files with `bed plot add <path>`
```

A clone fetches the roots and nothing else: no working files are
materialized, ever. Without `--ref`, clone probes `main` first, then the
other common primary-branch names (`master`, `trunk`) — the tour's
arboretum grows `master`, so the bare clone plants at it. If none of
them resolve (an empty arboretum, say), the error names what was probed:

```text
no ref main (tried main, master, trunk); pass --ref
```

`--ref` names the starting ref exactly. On an arboretum that grows
several refs — after §5's harvest, `main` and `master` both exist — a
bare clone probes its way to `main`; `--ref master` pins the other:

```sh
bed clone http://127.0.0.1:41234 --ref master
# the garden grows from 984213bc33e9f924 (master)
```

`--repo NAME` names the repo (the arboretum's choice when omitted; v0.1
arboreta are single-repo). `--plot src/` records plot prefixes up front.
All local state lives under `./.bed/` (`config.toml`, a treestate sqlite
index, a content-addressed object cache, and a heads stack); `--bed-dir
DIR` moves it.

## 4. Tend a plot

```sh
bed plot add src            # fetch + materialize one prefix
bed plot add .              # `.` plants the whole garden
bed plot list               # which prefixes this bed materializes
bed plot remove tests/
```

A prefix that exists in the config but has nothing under it at the
current changeset is still plotted — it fills as history lands:

```text
plotted docs/README.md — note: nothing exists under it at the current changeset; it fills as history lands (check the spelling if that surprises you)
```

Materialized files carry git's modes (exec bits are real, symlinks are
real symlinks). Reads work against the arboretum; status works offline:

```sh
bed status                  # mtime/size hints; only suspects get hashed
bed status --full-scan      # hash every tracked file
bed file src/main.rs        # one file's bytes at the current changeset
bed log --limit 5           # history, newest first (all lineages, like git log)
bed diff                    # current changeset vs its first parent
bed diff main 984213bc      # any two changesets or refs
bed blame src/main.rs       # first-parent lineage blame
bed goto main               # move the bed to a changeset or ref
```

`--cs` accepts a 64-hex changeset id, a ref name, or an **8+-hex
prefix** of one — `bed file --cs 984213bc src/main.rs` reads at the
changeset whose id starts `984213bc`. A bare branch name (`main`)
resolves to its `refs/heads/main` ref, which is how imported histories
store it. Precedence is deliberate: a ref with that exact name wins over
prefix interpretation; an ambiguous prefix is refused with the match
count (`prefix <p> is ambiguous (N changesets)`) so you can lengthen
it; a shorter token is rejected with the minimum-length hint.

## 5. Sow, graft, harvest

Edit files under the plot, then:

```sh
export GARDENBED_AUTHOR="A. Gardener <gardener@example.com>"
bed sow -m "tend the roses"
# planted seed b273e12d1c6f11ee: 1 change(s) (1 modified)
# the bed now grows from b273e12d1c6f11ee (was 984213bc33e9f924)
```

`--author` overrides the environment variable; with neither, the bed's
own identity (`Bed User <bed@gardenbed.local>`) signs. Heavy blobs
(> 1 MiB) stream to the arboretum first (a separate `PutBlob` upload),
small ones ride inline on the changeset — the two-phase upload is why a
crash mid-sow is retryable. The new changeset is queryable immediately —
`bed log --limit 1` or any MCP tool sees it.

Two more write moves, both server-side:

```sh
bed graft main              # replay the current changeset onto a new parent
bed harvest                 # fast-forward `main` to the current changeset
```

`graft` replays against the new parent's tree with the three-way rule: a
write onto a path the parent already has is admitted when that copy is
the untouched pre-image, and refused when a concurrent change got there
first — the refusal lists every conflicting path:

```text
bed: bed graft: the arboretum refused (FailedPrecondition): graft conflicts on: "src/main.rs" already exists with different content
```

The replayed changeset gets a fresh id with a single parent. `harvest` is
a CAS: racing harvesters see exactly one winner, and a loser is told the
new tip — graft onto it and harvest again. Harvesting publishes; the bed
stays where it is.

## 6. Ask without a checkout

`gardenbed-mcp` opens the arboretum's stores in process and serves the
MCP stdio protocol (one JSON-RPC message per line). It needs no bed, no
working copy, and no network — the zero-checkout contract (v0.1.md
§M7.2): the tool suite runs in a sandbox with no `.bed` anywhere and
still answers correctly.

```sh
gardenbed-mcp --root /tmp/arboretum --repo garden < requests.jsonl
```

```jsonc
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"me","version":"0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"log","arguments":{"limit":3}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"blame","arguments":{"path":"story.md"}}}
```

Four tools, all taking the same `cs` convention as `bed` (64-hex id, ref
name, or 8+-hex prefix; omit for `main`): `log {cs?, limit?}`,
`diff {a, b}`, `file {cs?, path}`, `blame {cs?, path}`. Details and the
protocol table live in [docs/mcp.md](mcp.md).

## 7. Offline behavior

With the arboretum down, the bed keeps its local half and refuses
anything that needs the wire — a failed command leaves the bed exactly as
it stood (no half-states):

| works offline | needs the arboretum |
|---|---|
| `bed status` (hint scan and `--full-scan`) | `bed clone`, `bed plot add`, `bed goto` |
| `bed plot list` | `bed file`, `bed log`, `bed diff`, `bed blame` |
| `bed plot remove` | `bed sow`, `bed graft`, `bed harvest` |

Refusals are one line and actionable:

```text
bed: cannot reach the arboretum at http://127.0.0.1:41234: transport error
  is it running? plant the arboretum with `gardenbed-server --addr <ip:port> --root <dir>`
```

## The demo

`demo/run-v0.1.sh` automates the tour's spine (import → serve → clone →
plot add → sow → export → MCP) and asserts each step: the import
bijection, the plot materialization, and the export (`fsck` +
`git push --dry-run` + the SHA roundtrip via the `--mapping` TSV) are
byte-verified against git, and the MCP answers are checked against git
porcelain oracles. It writes `demo/report.md` and exits non-zero on the
first failed assertion. Flags: `--keep-going` (run every step),
`--keep-work` (keep the workdir), `--workdir DIR`, `--corpus DIR`
(import a real repository instead of the generated one), `--commits N`.

## Where things live

- Arboretum (`--root`): `loam/` content-addressed objects,
  `seedbed.sqlite3` metadata (changesets, refs, derived data,
  `git_map`), optional `rootstock-mirror.git`.
- Bed (`.bed/`): `config.toml` (server, repo, plot, current changeset),
  `treestate` sqlite index, `objects/` cache, `heads` stack.
- Refs: stored as full names (`refs/heads/main`); clients may use the
  bare name (`main`) — the arboretum resolves both spellings, and
  `harvest` compare-and-swaps `refs/heads/main`.

## Troubleshooting

Messages below are verbatim from `crates/bed/src/error.rs` (server-side
refusals arrive wrapped as `bed <command>: the arboretum refused (CODE):
…`); `<angle brackets>` stand for the values bed fills in.

| you ran | you'll see | fix |
|---|---|---|
| any command outside a bed | ``not a bed: <dir> has no config.toml — plant one with `bed clone <server>` `` | `bed clone` first, or `cd` into the garden |
| a command in the garden root, not the bed | ``no bed at <dir> — but <dir>/.bed exists: --bed-dir names the .bed directory itself, so pass --bed-dir <dir>/.bed`` | pass `--bed-dir <dir>/.bed` |
| `clone` where a bed already grows | `there is already a bed at <dir> — tend it where it grows, or choose another --bed-dir` | reuse the bed or pick another `--bed-dir` |
| anything that needs the arboretum, server down | `cannot reach the arboretum at <server>: <message>` + `is it running? plant the arboretum with `gardenbed-server --addr <ip:port> --root <dir>` ` | start the server (see §1) |
| `clone` with no standard ref on the arboretum | `no ref main (tried main, master, trunk); pass --ref` | pass `--ref <one of the listed refs>` |
| `goto`/`--cs` with a name the arboretum lacks | `the arboretum has no ref "<name>"` | check the spelling; refs are `refs/heads/<name>` |
| `goto`/`--cs` with a 64-hex id the arboretum lacks | `changeset <short-id> is not on the arboretum` | that id never grew here — check which arboretum you cloned |
| `goto`/`--cs` with fewer than 8 hex chars | `changeset id prefix must be at least 8 hex chars: "<token>"` | send at least 8 hex characters |
| an ambiguous id prefix | `prefix <p> is ambiguous (N changesets)` | lengthen the prefix |
| a prefix matching nothing | `no ref or changeset matching <p>` | it is neither a ref nor an id prefix here |
| `file` on a path that is not a file there | `there is no file at "<path>" in <short-id>` | `bed log`/`bed diff` to find where it exists |
| `file` on a submodule pointer | `"<path>" is a submodule pointer in <short-id> — `bed file` prints only file and symlink content` | the pointer has no content of its own |
| `sow` on a clean plot | `the plot is clean — nothing modified, deleted, or untracked to sow` | edit something first |
| `sow -m ""` | `the message is empty — sow with -m <message>` | give the changeset a message |
| `sow --author nonsense` | `bad author "<raw>": <reason>` + `sow with --author "Name <email>"` | use `Name <email>` |
| `harvest` after `main` moved | `` `main` moved while harvesting — <short-id> is not its tip anymore `` + ``graft this changeset onto the new main (`bed graft main`) and harvest again`` | `bed graft main`, then harvest again |
| `graft` onto a moved parent | `graft conflicts on: "<path>", …` | the conflicting paths are listed; resolve and re-sow |
