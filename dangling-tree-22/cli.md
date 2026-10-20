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
  EOF, `1` when the server will not open (reason on stderr); its
  stdout carries the MCP protocol and nothing else.
- **`<cs-or-ref>` arguments** (wherever an id or ref is taken) accept a
  64-hex changeset id, a ref name, or an **8+-hex prefix** of a changeset
  id. An existing ref with that exact name wins over prefix
  interpretation; ambiguous prefixes are refused with the match count,
  shorter tokens with the minimum-length hint (see
  [docs/architecture.md](architecture.md) §2).
- **Ref names**: stored full (`refs/heads/main`), accepted bare (`main`)
  — the server resolves both spellings.
- Every `bed` subcommand takes `--state-dir <DIR>` (default `./.bed`) —
  where the working copy's local state lives (config, index, object cache,
  heads). It is omitted from the per-command tables below.

## bed

```text
Source control with sparse, server-backed working copies

Usage: bed [OPTIONS] <COMMAND>
```

| option | meaning |
|---|---|
| `--state-dir <DIR>` | where this working copy's local state lives (default `./.bed`) |
| `-h, --help` / `-V, --version` | help / `bed 0.1.0` |

| command | one-line description |
|---|---|
| `clone` | create a working copy connected to a server; materialize no files |
| `sparse` | select which repository paths to materialize |
| `switch` | move the working copy to a changeset id or ref |
| `status` | show local changes under selected paths |
| `file` | print one file's content at a changeset (or the current one) |
| `log` | show changeset history, newest first |
| `diff` | diff two changesets (default: current vs first parent) |
| `commit` | record local changes as a changeset on the server |
| `rebase` | replay the current changeset onto a new parent (server-side rebase) |
| `publish` | fast-forward `main` to the current changeset |
| `blame` | show first-parent line attribution, following renames |
| `help` | print help for any subcommand |

### bed clone

```text
Usage: bed clone [OPTIONS] <SERVER>
```

`<SERVER>` — the repository server endpoint: `http(s)://host:port[/repo]`
or `unix:///path/to/socket`. An http(s) URL's path names the repository
(the URL addresses the repository, not just the server); no path selects
`default`.

| flag | meaning |
|---|---|
| `--state-dir <DIR>` | local state directory (default `./.bed`) |
| `--repo <REPO>` | repository name (default: from the URL path, else `default`; the flag wins on conflict) |
| `--ref <REF>` | starting ref; defaults to `main`, then `master` or `trunk` |
| `--sparse <PATH>...` | sparse prefixes to record (repeat or list several); materialize with `bed sparse add` |

```sh
bed clone http://127.0.0.1:9418/myrepo
# created working copy for myrepo at http://127.0.0.1:9418
# current changeset: b273e12d1c6f11ee (main)
# no files materialized — select paths with `bed sparse add <path>`

bed clone http://127.0.0.1:9418 --repo myrepo --ref master --sparse src docs
```

A ref the server does not have is refused — no working copy is created:
`the server has no ref "feature"`. With none of the standard names,
clone errors with `no ref main (tried main, master, trunk); pass --ref`,
naming the refs that do exist.

### bed sparse

```text
Usage: bed sparse [OPTIONS] <COMMAND>
```

`add` fetches and materializes; `list` and `remove` are fully offline.

```text
Usage: bed sparse add [OPTIONS] <PATH>...
Usage: bed sparse remove [OPTIONS] <PATH>...
Usage: bed sparse list [OPTIONS]
```

| flag | command | meaning |
|---|---|---|
| `--state-dir <DIR>` | all | local state directory (default `./.bed`) |
| `--concurrency <N>` | `add` | bound on concurrent object fetches from the server (default `8`) |

`sparse add` takes repo-relative prefixes (directory or exact file; `.`
selects the whole repository) and materializes them at the current changeset.

Checkout updates (`sparse add`, `switch`, and `rebase`) preserve local edits
when the target leaves that file unchanged. They refuse to overwrite
local edits or untracked entries; commit or move that work aside before
retrying. Paths overlapping the active bed state directory and symlink
ancestors are also refused before files or checkout state change.

The selected state directory may live on another filesystem, including
through a symlink. Interrupted staging entries are reclaimed on retry
only when their recorded identity still matches. Unknown or replaced
temporary-looking files remain visible as untracked and are preserved.

```sh
bed sparse add src tests        # two prefixes at once
bed sparse add big              # the demo: 1,021 files, byte-verified vs git show
bed sparse list                 # which prefixes this working copy materializes
bed sparse remove tests/
```

### bed switch

```text
Usage: bed switch [OPTIONS] <CS_OR_REF>
```

Moves the working copy to a changeset id or ref; `<CS_OR_REF>` is a `<cs-or-ref>`.

```sh
bed switch main
# working copy at b273e12d1c6f11ee (main) — fetched 0 tree(s), 0 blob(s)
bed switch 9c42adff129515b7        # 8+-hex prefixes work
```

### bed status

```text
Usage: bed status [OPTIONS]
```

Works fully offline: mtime/size hints guide the scan; only suspects are
hashed.

| flag | meaning |
|---|---|
| `--state-dir <DIR>` | local state directory (default `./.bed`) |
| `--full-scan` | hash every tracked file, ignoring the hints |

```sh
bed status                   # working copy is clean — nothing modified, deleted, or untracked.
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
| `--state-dir <DIR>` | local state directory (default `./.bed`) |
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
| `--state-dir <DIR>` | local state directory (default `./.bed`) |
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

### bed commit

```text
Usage: bed commit [OPTIONS] --message <MESSAGE>
```

Commits the selected paths' changes as a new changeset (scans the working files
under the selected paths, uploads blobs — > 1 MiB via `PutBlob`, ≤ 1 MiB inline —
and stores the changeset).

| flag | meaning |
|---|---|
| `-m, --message <MESSAGE>` | the changeset's message (required) |
| `--author <NAME <EMAIL>>` | author as one `Name <email>` identity (default: the `GARDENBED_AUTHOR` environment variable, else the working copy's own) |

```sh
export GARDENBED_AUTHOR="Your Name <you@example.com>"
bed commit -m "Describe the change"
# created changeset b273e12d1c6f11ee: 1 change(s) (1 modified)
# working copy at b273e12d1c6f11ee (was 984213bc33e9f924)
```

### bed rebase

```text
Usage: bed rebase [OPTIONS] <NEW_PARENT>
```

Replays the current changeset onto a new parent (server-side rebase).
`<NEW_PARENT>` is a `<cs-or-ref>`. Conflicts are hard errors listing
every conflicting path.

```sh
bed rebase main
# rebased <cs> onto <new-parent> as <replayed-cs>
# fetched <N> tree(s), <M> blob(s)
```

### bed publish

```text
Usage: bed publish [OPTIONS]
```

Fast-forwards `main` to the current changeset (a compare-and-swap: one
winner per race; a loser is told the new tip).

```sh
bed publish
# published b273e12d1c6f11ee — `main` now points to it
```

### bed blame

```text
Usage: bed blame [OPTIONS] <PATH>
```

First-parent lineage blame of one path (v0.1.md D7), rename-following;
each line renders `<16-hex id> (<author> <date> <lineno>) <content>`.

| flag | meaning |
|---|---|
| `--state-dir <DIR>` | local state directory (default `./.bed`) |
| `--cs <ID-OR-REF>` | changeset id or ref to blame at (default: the current one) |

```sh
bed blame src/main.rs
bed blame --cs master Makefile
```

## gardenbed-server

```text
Gardenbed repository server

Usage: gardenbed-server [OPTIONS] [COMMAND]
```

| option | meaning |
|---|---|
| `--addr <ADDR>` | listen address (`host:port`; `:0` picks an ephemeral port), default `127.0.0.1:9418` |
| `--root <ROOT>` | the server root directory (created if missing): one subdirectory per repository (`<root>/<repo>/objects` + `<root>/<repo>/metadata.sqlite3`); a v0.1 root (stores at the top level) serves unchanged as repository `default` |
| `--max-repos <N>` | cap on simultaneously open repositories (LRU eviction above it), default 256 |

Without a subcommand, the top-level `--addr`/`--root` flags serve the
gRPC API — the printed address is the contract for scripts:

```sh
gardenbed-server --addr 127.0.0.1:9418 --root /tmp/gardenbed
# listening on 127.0.0.1:9418
```

Publication uses durable validation certificates. Older databases perform a
one-time validation of previously uncertified reachable history before moving
refs; later restarts reuse that frontier. See [publication validation](validation.md)
for migration behavior and certificate guarantees.

### gardenbed-server serve

```text
Usage: gardenbed-server serve [OPTIONS]
```

Serves the gRPC API. Takes the same `--addr`/`--root` flags; this is the
default when no subcommand is given.

For its lifetime the server holds a **shared** lock on
`<root>/gardenbed.lock` (the root-lock contract): several servers may
serve the same root at once, while offline operations that relocate or
reclaim repository files — `gardenbed-server move`, offline `gc` — take
the lock exclusively and refuse while any server is live. Serve fails at
startup only when an offline operation holds the lock exclusively
(`cannot lock the server root`).

### gardenbed-server move

```text
Usage: gardenbed-server move [OPTIONS] --root <ROOT> --from <FROM> --into <INTO>
```

Renames a repository under the server root (the two renames: stores,
then the git mirror when present). Both stores are path-free and
self-contained, so history is byte-identical after the move; the vacated
name stops being served.

An **offline operation**: stop the server first. The move checkpoints
the repository's WAL into `metadata.sqlite3` before renaming (committed
rows may legally live only in the WAL sidecar), carries `-wal`/`-shm`
along, and refuses — non-zero exit, nothing changed — while a server is
serving the root or any other process holds the database.

| flag | meaning |
|---|---|
| `--root <ROOT>` | the server root directory |
| `--from <FROM>` | the repository's current name |
| `--into <INTO>` | the new name (must not name an existing repository or directory) |

```sh
gardenbed-server move --root /tmp/gardenbed --from default --into myrepo
# moved repository default to myrepo (/tmp/gardenbed/myrepo), history byte-identical
```

### gardenbed-server import

```text
Usage: gardenbed-server import [OPTIONS] --from <FROM> --root <ROOT>
```

Imports a git repository's history into the Gardenbed stores.

| flag | meaning |
|---|---|
| `--from <FROM>` | git repository: a local path (bare or working tree) or a URL. URLs are cloned (mirror) into the repository's `git-mirror.git` and refreshed on re-use |
| `--root <ROOT>` | the server root directory: one subdirectory per repository |
| `--repo <REPO>` | repository name under the root (default `default`) |
| `--limit <LIMIT>` | ingest only the newest N commits (topological suffix; slice boundaries are re-rooted as full-tree root changesets) |
| `--renames <RENAMES>` | detect renames with `git diff-tree -M` (`on`, default) or import them as delete+add pairs (`off`) |
| `--ref <REFS>` | only import branches matching this glob (e.g. `refs/heads/*`); repeatable, empty means every branch |

```sh
gardenbed-server import --from /path/to/git-repo --root /tmp/gardenbed \
    --repo myrepo --ref refs/heads/master --renames on
# 2 commits (2 new, 0 skipped), 9 objects, 1 ref(s) moved, 0.24s (8 commits/s)
```

`demo/run-v0.1.sh` runs the same import against the deterministic demo
corpus (`$CORPUS` → `$SERVER_ROOT`).

### gardenbed-server export

```text
Usage: gardenbed-server export [OPTIONS] --root <ROOT> --into <INTO>
```

Exports the changeset graph into a real git repository (one
`git fast-import` stream). The changeset→git mapping rides in the
report; `--mapping FILE` writes it as TSV for verification.

| flag | meaning |
|---|---|
| `--root <ROOT>` | the server root directory (one subdirectory per repository) |
| `--repo <REPO>` | repository name under the root (default `default`) |
| `--into <INTO>` | destination git repository: created as a bare sha1 repository when missing, accepted as-is when present |
| `--ref <REFS>` | only export refs matching this glob (e.g. `refs/heads/*`); repeatable, empty means every ref in the metadata store |
| `--mapping <MAPPING>` | write one `<64-hex changeset id>\t<git sha>` line per exported commit, sorted by changeset id, to this file |

```sh
gardenbed-server export --root /tmp/gardenbed --repo myrepo --into /tmp/export.git \
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
| `--root <DIR>` (required) | the server's root directory |
| `--repo <NAME>` | repository name under the root (default `default`); the session answers from this repository only — an unknown name exits 1 listing what the root serves |

Newline-delimited JSON-RPC 2.0 on stdin/stdout, one response line per
request; diagnostics go to stderr only. Serves tools `log`, `diff`,
`blame`, `file` — the wire details are [docs/mcp.md](mcp.md).

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  | gardenbed-mcp --root /tmp/gardenbed --repo myrepo

gardenbed-mcp --root /tmp/gardenbed --repo myrepo < requests.jsonl
```
