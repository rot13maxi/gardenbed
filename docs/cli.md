# gardenbed CLI reference

Generated from the real `--help` output of the v0.1.0 binaries (`bed`,
`gardenbed-server`, `gardenbed-mcp`); every example is a verified
invocation from `demo/run-v0.1.sh`, [docs/quickstart.md](quickstart.md),
or the recorded demo run.

## Conventions

- **Exit codes.** `bed` exits `0` on success, `1` on a failed command
  (the error prints to stderr as `bed: <message>` — see the
  troubleshooting table in [docs/quickstart.md](quickstart.md)), and `2`
  on a usage error (unknown subcommand or flag, missing required
  argument). `gardenbed-server` likewise exits `0`/`1`/`2` (`--root` is
  required for the top-level binary). `gardenbed-mcp` exits `0` on clean
  EOF, `1` when the arboretum will not open (reason on stderr); its
  stdout carries the MCP protocol and nothing else.
- **`<cs-or-ref>` arguments** (wherever an id or ref is taken) accept a
  64-hex changeset id, a ref name, or an **8+-hex prefix** of a changeset
  id. An existing ref with that exact name wins over prefix
  interpretation; ambiguous prefixes are refused with the match count,
  shorter tokens with the minimum-length hint (see
  [docs/architecture.md](architecture.md) §2).
- **Ref names**: stored full (`refs/heads/main`), accepted bare (`main`)
  — the arboretum resolves both spellings.
- Every `bed` subcommand takes `--bed-dir <DIR>` (default `./.bed`) —
  where the bed's local state lives (config, treestate, object cache,
  heads). It is omitted from the per-command tables below.

## bed

```text
tend your gardenbed — a sparse, server-backed working copy

Usage: bed [OPTIONS] <COMMAND>
```

| option | meaning |
|---|---|
| `--bed-dir <DIR>` | where this bed's local state lives (default `./.bed`) |
| `-h, --help` / `-V, --version` | help / `bed 0.1.0` |

| command | one-line description |
|---|---|
| `clone` | plant a new bed: fetch an arboretum's roots, materializing nothing |
| `plot` | tend the plot: which path prefixes this bed materializes |
| `goto` | move the bed to a changeset id or ref |
| `status` | compare the working files under the plot against the treestate |
| `file` | print one file's content at a changeset (or the current one) |
| `log` | history from a changeset or ref, newest first — every parent lineage, date-ordered, like `git log`'s default walk |
| `diff` | diff two changesets (default: current vs first parent) |
| `sow` | sow the plot's changes as a new changeset |
| `graft` | replay the current changeset onto a new parent (server-side rebase) |
| `harvest` | fast-forward `main` to the current changeset |
| `blame` | first-parent lineage blame of a path (docs/v0.1.md D7) |
| `help` | print help for any subcommand |

### bed clone

```text
Usage: bed clone [OPTIONS] <SERVER>
```

`<SERVER>` — the arboretum to plant from: `http(s)://host:port` or
`unix:///path/to/socket`.

| flag | meaning |
|---|---|
| `--bed-dir <DIR>` | local state directory (default `./.bed`) |
| `--repo <REPO>` | repo to tend; the server's choice when omitted |
| `--ref <REF>` | the ref to plant from. Default `main`, falling back to the arboretum's other common names (`master`, `trunk`) when it does not grow it — pass this flag to name it exactly |
| `--plot <PATH>...` | plot prefixes to record (repeat or list several); materialize with `bed plot add` |

```sh
bed clone http://127.0.0.1:9418
# planted a bed for garden at http://127.0.0.1:9418
# the garden grows from b273e12d1c6f11ee (main)
# materialized nothing — plant files with `bed plot add <path>`

bed clone http://127.0.0.1:9418 --ref master --plot src docs
```

A ref the arboretum does not have is refused — nothing is planted:
`the arboretum has no ref "feature"`. With none of the standard names,
clone errors with `no ref main (tried main, master, trunk); pass --ref`,
naming the refs that do exist.

### bed plot

```text
Usage: bed plot [OPTIONS] <COMMAND>
```

`add` fetches and materializes; `list` and `remove` are fully offline.

```text
Usage: bed plot add [OPTIONS] <PATH>...
Usage: bed plot remove [OPTIONS] <PATH>...
Usage: bed plot list [OPTIONS]
```

| flag | command | meaning |
|---|---|---|
| `--bed-dir <DIR>` | all | local state directory (default `./.bed`) |
| `--concurrency <N>` | `add` | bound on concurrent object fetches from the arboretum (default `8`) |

`plot add` takes repo-relative prefixes (directory or exact file; `.`
plants the whole garden) and materializes them at the current changeset.

Checkout updates (`plot add`, `goto`, and `graft`) preserve local edits
when the target leaves that file unchanged. They refuse to overwrite
local edits or untracked entries; sow or move that work aside before
retrying. Paths overlapping the active bed state directory and symlink
ancestors are also refused before files or checkout state change.

```sh
bed plot add src tests        # two prefixes at once
bed plot add big              # the demo: 1,021 files, byte-verified vs git show
bed plot list                 # which prefixes this bed materializes
bed plot remove tests/
```

### bed goto

```text
Usage: bed goto [OPTIONS] <CS_OR_REF>
```

Moves the bed to a changeset id or ref; `<CS_OR_REF>` is a `<cs-or-ref>`.

```sh
bed goto main
# the bed now grows from b273e12d1c6f11ee (main) — fetched 0 tree(s), 0 blob(s)
bed goto 9c42adff129515b7        # 8+-hex prefixes work
```

### bed status

```text
Usage: bed status [OPTIONS]
```

Works fully offline: mtime/size hints guide the scan; only suspects are
hashed.

| flag | meaning |
|---|---|
| `--bed-dir <DIR>` | local state directory (default `./.bed`) |
| `--full-scan` | hash every tracked file, ignoring the hints |

```sh
bed status                   # the plot is clean — nothing modified, deleted, or untracked.
bed status --full-scan
```

### bed file

```text
Usage: bed file [OPTIONS] <PATH>
```

Prints one file's content at a changeset (default: the current one).
`<PATH>` is repo-relative.

| flag | meaning |
|---|---|
| `--bed-dir <DIR>` | local state directory (default `./.bed`) |
| `--cs <ID-OR-REF>` | changeset id or ref to read from (default: the current one) |

```sh
bed file src/main.rs
bed file --cs 984213bc src/main.rs     # short-id prefix
```

### bed log

```text
Usage: bed log [OPTIONS]
```

History from a changeset or ref, newest first — every parent lineage,
date-ordered, exactly like `git log`'s default walk. Ids render as the
16-hex short form.

| flag | meaning |
|---|---|
| `--bed-dir <DIR>` | local state directory (default `./.bed`) |
| `--limit <N>` | show at most N changesets (default: all). Like `git log`, 0 means "no limit", not "show none" |
| `--cs <ID-OR-REF>` | where to start (default: the current changeset) |

```sh
bed log --limit 5
bed log --cs master --limit 1
```

### bed diff

```text
Usage: bed diff [OPTIONS] [A] [B]
```

Diffs two changesets (default: the current changeset vs its first
parent). `A` is the older side; with `B` omitted, diffs `A` against its
first parent. Both take a `<cs-or-ref>`.

```sh
bed diff                     # current changeset vs its first parent
bed diff main master
```

### bed sow

```text
Usage: bed sow [OPTIONS] --message <MESSAGE>
```

Sows the plot's changes as a new changeset (scans the working files
under the plot, uploads blobs — > 1 MiB via `PutBlob`, ≤ 1 MiB inline —
and stores the changeset).

| flag | meaning |
|---|---|
| `-m, --message <MESSAGE>` | the changeset's message (required) |
| `--author <NAME <EMAIL>>` | author as one `Name <email>` identity (default: the `GARDENBED_AUTHOR` environment variable, else the bed's own) |

```sh
export GARDENBED_AUTHOR="A. Gardener <gardener@example.com>"
bed sow -m "tend the roses"
# planted seed b273e12d1c6f11ee: 1 change(s) (1 modified)
# the bed now grows from b273e12d1c6f11ee (was 984213bc33e9f924)
```

### bed graft

```text
Usage: bed graft [OPTIONS] <NEW_PARENT>
```

Replays the current changeset onto a new parent (server-side rebase).
`<NEW_PARENT>` is a `<cs-or-ref>`. Conflicts are hard errors listing
every conflicting path.

```sh
bed graft main
# grafted <cs> onto <new-parent> as <replayed-cs>
# fetched <N> tree(s), <M> blob(s)
```

### bed harvest

```text
Usage: bed harvest [OPTIONS]
```

Fast-forwards `main` to the current changeset (a compare-and-swap: one
winner per race; a loser is told the new tip).

```sh
bed harvest
# harvested b273e12d1c6f11ee — `main` now grows from it
```

### bed blame

```text
Usage: bed blame [OPTIONS] <PATH>
```

First-parent lineage blame of one path (v0.1.md D7), rename-following;
each line renders `<16-hex id> (<author> <date> <lineno>) <content>`.

| flag | meaning |
|---|---|
| `--bed-dir <DIR>` | local state directory (default `./.bed`) |
| `--cs <ID-OR-REF>` | changeset id or ref to blame at (default: the current one) |

```sh
bed blame src/main.rs
bed blame --cs master Makefile
```

## gardenbed-server

```text
The gardenbed arboretum (authoritative server)

Usage: gardenbed-server [OPTIONS] [COMMAND]
```

| option | meaning |
|---|---|
| `--addr <ADDR>` | listen address (`host:port`; `:0` picks an ephemeral port), default `127.0.0.1:9418` |
| `--root <ROOT>` | repository directory; must be a non-empty path (created if missing) — holds `<root>/loam` and `<root>/seedbed.sqlite3` |

Without a subcommand, the top-level `--addr`/`--root` flags serve the
gRPC API — the printed address is the contract for scripts:

```sh
gardenbed-server --addr 127.0.0.1:9418 --root /tmp/arboretum
# listening on 127.0.0.1:9418
```

### gardenbed-server serve

```text
Usage: gardenbed-server serve [OPTIONS]
```

Serves the gRPC API. Takes the same `--addr`/`--root` flags; this is the
default when no subcommand is given.

### gardenbed-server import

```text
Usage: gardenbed-server import [OPTIONS] --from <FROM> --root <ROOT>
```

Imports a git repository's history into the gardenbed stores.

| flag | meaning |
|---|---|
| `--from <FROM>` | git repository: a local path (bare or working tree) or a URL. URLs are cloned (mirror) into `<root>/rootstock-mirror.git` and refreshed on re-use |
| `--root <ROOT>` | gardenbed repository directory (created if missing) |
| `--limit <LIMIT>` | ingest only the newest N commits (topological suffix; slice boundaries are re-rooted as full-tree root changesets) |
| `--renames <RENAMES>` | detect renames with `git diff-tree -M` (`on`, default) or import them as delete+add pairs (`off`) |
| `--ref <REFS>` | only import branches matching this glob (e.g. `refs/heads/*`); repeatable, empty means every branch |

```sh
gardenbed-server import --from /path/to/git-repo --root /tmp/arboretum \
    --ref refs/heads/master --renames on
# 2 commits (2 new, 0 skipped), 9 objects, 1 ref(s) moved, 0.24s (8 commits/s)
```

`demo/run-v0.1.sh` runs the same import against the deterministic demo
corpus (`$CORPUS` → `$ARBORETUM`).

### gardenbed-server export

```text
Usage: gardenbed-server export [OPTIONS] --root <ROOT> --into <INTO>
```

Exports the changeset graph into a real git repository (one
`git fast-import` stream). The changeset→git mapping rides in the
report; `--mapping FILE` writes it as TSV for verification.

| flag | meaning |
|---|---|
| `--root <ROOT>` | gardenbed repository directory (holds `loam/` and `seedbed.sqlite3`) |
| `--into <INTO>` | destination git repository: created as a bare sha1 repository when missing, accepted as-is when present |
| `--ref <REFS>` | only export refs matching this glob (e.g. `refs/heads/*`); repeatable, empty means every ref in the seedbed |
| `--mapping <MAPPING>` | write one `<64-hex changeset id>\t<git sha>` line per exported commit, sorted by changeset id, to this file |

```sh
gardenbed-server export --root /tmp/arboretum --into /tmp/export.git \
    --mapping /tmp/mapping.tsv
# 3 commits (0 signed dropped), 5 blobs, 2 ref(s), 0.01s
git -C /tmp/export.git fsck --strict   # clean
```

## gardenbed-mcp

```text
answer repo questions (log, diff, blame, file) over MCP — no working copy needed

Usage: gardenbed-mcp [OPTIONS] --root <DIR>
```

| flag | meaning |
|---|---|
| `--root <DIR>` (required) | the arboretum's data directory (its `loam/` object store and `seedbed.sqlite3` metadata database) |
| `--repo <NAME>` | repo name; accepted and validated for forward compatibility (v0.1 arboreta are single-repo; the API carries a repo field from day one, docs/v0.1.md §1) |

Newline-delimited JSON-RPC 2.0 on stdin/stdout, one response line per
request; diagnostics go to stderr only. Serves tools `log`, `diff`,
`blame`, `file` — the wire details are [docs/mcp.md](mcp.md).

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  | gardenbed-mcp --root /tmp/arboretum

gardenbed-mcp --root /tmp/arboretum --repo garden < requests.jsonl
```
