# gardenbed CLI reference

Reference for the current CLI, including the unreleased v0.2 feature wave.
Workspace package versions still report `0.1.0`; that does not limit the
commands to the tagged v0.1 release. Examples draw on the demo scripts,
[quickstart](quickstart.md), and recorded demo runs.

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
| `diff` | diff the working copy (when dirty by modified/deleted tracked files) or two changesets; every diff prints a header naming its sides |
| `commit` | record local changes as a changeset on the server |
| `rebase` | replay the current changeset onto a new parent (server-side rebase) |
| `merge` | merge a changeset or ref into the current changeset (server-side two-parent merge) |
| `publish` | fast-forward a ref (default `main`) to the current changeset |
| `mount` | mount the current changeset as a read-only FUSE filesystem |
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

`sparse list` stays fully offline. A selected prefix that matches nothing
at the current changeset is listed with the suffix
`(empty at this changeset)` — selected ahead of history by design, it
fills as the paths land in later changesets, but the marker keeps a dead
selection from reading like live coverage.

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

With offline commits queued (see [`bed commit`](#bed-commit)), the
pending line prints first, offline:

```sh
bed status
# 1 pending commit(s) not yet on the server — run bed sync
# working copy is clean — nothing modified, deleted, or untracked.
```

### bed sync

```text
Usage: bed sync [OPTIONS]
```

Drains the offline commit queue: every pending commit is uploaded
oldest-first as an idempotent re-upload of content-addressed facts (a
crash mid-sync costs a re-upload, never a divergence), then the state
move each offline commit already performed locally is replayed. A
failure stops the drain at the failing commit and says how many remain;
`bed switch`, `rebase`, and `publish` refuse while commits are pending.

```sh
bed sync
# synced <cs>
# synced 2 commit(s), queue empty
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

With no arguments, `bed diff` is context-aware: a working copy **dirty
by modified or deleted tracked files** diffs their local bytes against
the current changeset (untracked files are not part of it — git parity;
a deleted file, or a tracked file replaced by a directory, renders as a
removal to `/dev/null`; a tracked symlink's new side is its
link-target bytes, what `commit` records, never the target's content —
a dangling link diffs fine). A working copy clean, or dirty only by
untracked files, diffs the current changeset against its first parent.
With arguments, both are always changesets: `A` is the older side; with
`B` omitted, diffs `A` against its first parent. Both take a
`<cs-or-ref>`.

Every diff prints a one-line header naming what is being diffed, so the
modes are distinguishable at a glance — empty diffs included (identical
sides state they are equal; a root changeset says it has no parent).

Renders are git-shaped beyond the header: a pure rename (the recorded
`Rename` op, or — between two arbitrary changesets — a delete and a
write of the identical blob, the rule `bed commit` pairs by: same entry
kind, destination absent on the old side) renders git's `R100` form
(`similarity index 100%`, `rename from`, `rename to`); a moved-and-edited
file renders a delete plus a write (no similarity below 100% is ever
claimed). A submodule pointer renders `Subproject commit <sha>` hunks on
add, delete, and retarget — the real upstream SHA — and every rendered
shape reapplies with `git apply` (gitlink entries under `git apply
--cached`).

The first-parent wording appears only when the older side really is the
newer's first parent; any other pair names both sides:

```sh
bed diff                     # working copy vs current changeset (dirty),
                             # else current changeset vs its first parent
bed diff main master
```

```text
diff of working copy against 57169571952de2bf
diff of 57169571952de2bf against its first parent
diff of 3af0c1d8e29b4a6d against 57169571952de2bf
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

When the server is unreachable *at connect time* — and only then; a
server that answers, even to refuse, is online and its error must be
seen — the commit completes offline: the changeset is built and
content-addressed locally, every changed blob is staged into
`.bed/objects`, and a queue row records what `bed sync` later replays.
The working copy moves exactly as an online commit's would.

```sh
bed commit -m "offline work"
# created changeset <cs>: 1 change(s) (1 added)
# queued as <cs> — 1 pending — run bed sync when online
# working copy at <cs> (was <parent>)
```

### bed rebase

```text
Usage: bed rebase [OPTIONS] <NEW_PARENT>
```

Replays the current changeset onto a new parent (server-side rebase).
`<NEW_PARENT>` is a `<cs-or-ref>`. Conflicts are hard errors listing
every conflicting path. With `--stack`, the whole first-parent chain
from the merge base of (current, new parent) replays onto the new
parent, oldest first — each link gets the same per-changeset conflict
check, a conflict anywhere aborts the entire stack before the working
copy moves, and unrelated histories or a non-first-parent merge base
are refused up front.

**Twin recognition (already applied).** A lane is sometimes landed on
`main` by hand: the same content is committed as a NEW changeset with
the same message but no ancestry link. That changeset's post-image root
tree is *exactly* the lane element's, so replaying the element would hit
the server's own-content check (`"<path>" was changed or deleted`)
although the result is already there. Before replaying, both forms walk
the new parent's side of the merge base and match on **both** the exact
message string and the exact post-image tree. A match is a *twin*: the
element is treated as already applied and is not replayed. Every
resolution is printed, naming the skipped element and its twin:

```sh
bed rebase --stack main
# rebased 1 change(s) onto <new-parent> as <new-tip>, skipped 1 already applied
#   <cs> "layer 1" already applied — subsumed by <twin> (tree-equal, same message)
#   <twin>
```

Rules and honest limits:

- Equality is exact on both sides. Message alone proves nothing about
  content; a tree alone can coincide with an unrelated changeset.
  **Partial** file overlap is not a twin — it stays a hard conflict.
- The LAST twinned element subsumes the elements before it (its tree is
  the cumulative lane state), so the prefix is skipped and only the
  suffix after it replays, parented on the new parent's current tip.
- When **every** element is a twin, nothing is replayed: the command
  prints `stack already applied`, names the tip's twin, and moves the
  working copy to the new parent. In the incident shape the hand-union
  IS the new parent, so the working copy lands on the twin, and a
  follow-up `bed publish` is a fast-forward no-op.
- A twin whose paths `main` rewrote (or reverted) *after* the twin is
  not treated as applied: the element replays, and the ordinary
  conflict rule decides. Never a silent skip.
- When the current changeset is already an ancestor of the new parent
  there is nothing to replay at all, in both forms: `already up to date —
  <cs> is an ancestor of <parent>; nothing to rebase`.
- The ancestry walk visits exactly `ancestors(new parent) \
  ancestors(merge base)`: one `GetChangeset` per changeset main gained
  since the branch point, never the base or its history (frontier parents
  are pruned with `IsAncestor`). On top of that it costs one `RootTree`
  per message match, one `IsAncestor` per matched candidate, and one
  `Diff` per twin candidate for the rewrite guard. The walk is capped at
  4096 span changesets (`GARDENBED_TWIN_SCAN_CAP` overrides it for tests
  and diagnostics); a longer span prints the cap note on stderr
  **immediately** — including when the miss then surfaces as the ordinary
  conflict — and a truncated scan can only miss a twin, never produce a
  wrong skip. The note appears only when the run still has to replay
  something: a twin that was found and applied (or a stack whose every
  element resolved) prints nothing, because the truncation then hid
  nothing this run needed.

```sh
bed rebase main
# rebased <cs> onto <new-parent> as <replayed-cs>
# fetched <N> tree(s), <M> blob(s)

bed rebase main          # after the same change was landed by hand
# already applied — <cs> "<message>" is on <new-parent> as <twin> (tree-equal, same message)
# nothing replayed; working copy moved to <new-parent>

bed rebase --stack main
# rebased 3 change(s) onto <new-parent> as <new-tip>
#   <link1> -> <link2> -> <link3>
```

### bed merge

```text
Usage: bed merge [OPTIONS] <TARGET>...
```

Merges one or more changesets or refs into the current changeset. A
single target creates a two-parent merge with `parents = [current,
target]`. Rename detection pairs moved files across directories, so an
edit on the other branch follows the move. Multiple maximal common
ancestors are folded into a recursive virtual base before the three-way
content merge.

Multiple targets use the separate `MergeOctopus` RPC and fold in order,
creating one changeset with all remaining parents. Already-merged targets
are skipped. A fast-forward in the first round replaces the current head
in the parent list, matching Git's octopus behavior. An older server
without this RPC refuses the operation; it cannot silently ignore extra
targets. A conflict in any round names the paths and round and leaves the
working copy and published refs unchanged. If every target is already
merged, nothing is created.

See [merge behavior and limits](merge-rename-octopus.md) for rename
scoring, candidate limits, recursive bases, and conflict cases.

| flag | meaning |
|---|---|
| `-m, --message <MSG>` | merge message (default: `merge <TARGET>…`) |
| `--author <NAME-AND-EMAIL>` | author in `Name <email>` form (default: `GARDENBED_AUTHOR`, else committer identity) |
| `--state-dir <DIR>` | local state directory (default `./.bed`) |

```sh
bed merge main
# merged 9a41be12d0c6ffee into 77c0aa13b9d2210f as 3d20cc41a8e57b19
# fetched <N> tree(s), <M> blob(s)

bed merge main          # again
# already up to date — 9a41be12d0c6ffee is an ancestor of ...; nothing to merge
```

On conflict the command fails and names the operation, every affected
path, and the next step (edit the conflicting files, commit, and merge
again). The working copy is left untouched.

### bed publish

```text
Usage: bed publish [OPTIONS]
```

Fast-forwards a ref to the current changeset (a compare-and-swap: one
winner per race; a loser is told the new tip).

> **Upgrade note — restart the server before using `--ref`.** The `ref`
> field is newer than the rest of the publish message. An OLD server binary
> (pre-`fd29b3d`, before `PublishReq` gained `ref`/`force`) plus a NEW `bed`
> makes `bed publish --ref <lane>` silently publish to `refs/heads/main`:
> proto3 skips the unknown field, the old server takes the plain main form,
> and the client still prints the lane name it was given — it echoes its own
> argv, never the server's ref choice. The server never errors, so the only
> detection is reading the refs back — **both** of them: publish the lane,
> then check that `bed log --cs refs/heads/<lane> --limit 1` names the tip
> you just published **and** that `bed log --cs refs/heads/main --limit 1`
> is still the main id you recorded before publishing (a `GetRef` probe is
> equivalent). Checking only the lane ref gives a false all-clear whenever
> the lane row already existed — `import --ref` pre-creates it — while
> `main` moved silently. Coordinate the restart when protocol messages
> change. Background: dogfood finding #13 in
> [DOGFOOD.md](DOGFOOD.md); the harness's `server_bin()` digest guard
> refuses a binary built from different tree state, but a deliberately
> deployed old server is outside its reach.

| flag | meaning |
|---|---|
| `--state-dir <DIR>` | local state directory (default `./.bed`) |
| `--ref <REF>` | the ref to fast-forward (default: `main`; `main` itself must use the plain form; an old server drops the field silently — see the upgrade note) |
| `--force` | permit a non-fast-forward (rewind) move of a named ref |

A named ref is the lane-agent form: agents committing off-main publish
their branch so a fresh clone's `bed switch <branch>` resolves to the new
tip. A lane ref gets the same rewind guard as `import`: a non-descendant
move is refused (the error names the ref, both tips, and what falls off)
unless `--force` is passed. `--ref` requires a server new enough to carry
the field (see the upgrade note above); against an older server the flag is
dropped and the publish hits `main`.

```sh
bed publish
# published b273e12d1c6f11ee — `main` now points to it
bed publish --ref feat/scale-out
# published 9d1c4b7e2a5f8d3c — `feat/scale-out` now points to it
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


### bed mount

```text
Usage: bed mount [OPTIONS] <MOUNTPOINT>
```

Mounts the current changeset as a read-only FUSE filesystem — the
**virtualized checkout**: the whole repository tree is visible and
readable at `<MOUNTPOINT>` with no sparse selection, while `.bed/objects`
grows only by what is actually read (a file's blob materializes on first
`stat`, a symlink's on first `lstat`; listing alone fetches only trees).
Every mutating operation fails with `EROFS`. Snapshot semantics: the
mount reflects the current changeset at mount time and does not track
later ref movement — unmount and mount again to switch.

The command stays in the foreground serving the filesystem; `Ctrl-C`
(SIGINT or SIGTERM) unmounts cleanly and exits 0, leaving an ordinary
removable directory behind. No root privileges are needed (mounting
goes through `fusermount3`); hosts without `/dev/fuse` refuse with
`this host cannot mount a virtualized checkout`.

```sh
bed mount /tmp/view
# mounted b273e12d1c6f11ee read-only at /tmp/view — Ctrl-C to unmount
cat /tmp/view/src/main.rs      # fetched exactly this blob, cache-first
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
| `--max-repos <N>` | cap on simultaneously open repositories (LRU eviction above it), default 256; local (`--root`) mode only |
| `--storage <FILE>` | storage configuration file (TOML) for the scaled backends (Postgres metadata + S3 objects); takes precedence over `--root`. The `GARDENBED_STORAGE` environment variable (a path to the same TOML file) is the fallback when the flag is absent — resolution order is `--storage` → `GARDENBED_STORAGE` → `--root` local mode. Scaled mode serves one repository; the request's `repo` name is a label, not a directory |

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

### Scaled storage

`serve` (and `import`/`export`, below) accept `--storage <config.toml>`
to open off-box backends instead of the local `--root` layout — the
horizontal scale-out tier ([docs/scale-out-design.md](scale-out-design.md)).
The file selects one backend per storage contract:

```toml
[metadata]
backend = "postgres"            # or "sqlite" (root = "/path/to/repo")
url = "postgres://user:pass@host:5432/gardenbed"

[objects]
backend = "s3"                  # or "fs" (root = "/path/to/repo")
endpoint = "http://minio:9000"
bucket = "gardenbed"
region = "us-east-1"
access_key = "..."
secret_key = "..."
path_style = true
```

```sh
gardenbed-server --storage config.toml serve --addr 0.0.0.0:9418
GARDENBED_STORAGE=/etc/gardenbed/storage.toml gardenbed-server serve
```

The `sqlite`/`fs` branches point at the same `<root>/metadata.sqlite3`
and `<root>/objects` layout that `--root` mode opens, so a config file
can address an existing local repository. A ready-made compose stack
(Postgres + MinIO + N replicas) lives in `tools/scale/`; see
[tools/scale/README.md](../tools/scale/README.md).

### gardenbed-server serve

```text
Usage: gardenbed-server serve [OPTIONS]
```

Serves the gRPC API. Takes the same `--addr`/`--root`/`--storage` flags
(see [scaled storage](#scaled-storage)); this is the default when no
subcommand is given.

For its lifetime the server holds an exclusive lock on
`<root>/gardenbed.lock`: a second server over the same root refuses to
start (`cannot lock the server root`), and `gardenbed-server move`
refuses to run while the lock is held.

| flag | meaning |
|---|---|
| `--auth <PATH>` | optional token auth config (`auth.toml`): `[tokens.<token>]` entries, each with `repos` (repository names, or `*` for any) and `role` (`read`, `write`, or `admin`; admin implies write, write implies read). Without the flag the server is open. A malformed file refuses startup |

Clients authenticate with the gRPC `authorization: Bearer <token>`
metadata; `bed` sends it when `GARDENBED_TOKEN` is set in its
environment. Anonymous or insufficient tokens get `PERMISSION_DENIED`
naming the repository and the next step.

```toml
# auth.toml — gardenbed-server serve --auth auth.toml
[tokens."operator-token"]
repos = ["garden"]
role = "write"

[tokens."auditor-token"]
repos = ["*"]        # any repository
role = "read"
```

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
| `--allow-rewind` | permit ref moves that would lose changesets (force-push parity) |
| `--storage <FILE>` | storage configuration for the scaled backends; takes precedence over `--root` (`GARDENBED_STORAGE` is the env fallback). Note: URL `--from` values require local mode — scaled mode imports a local path |

**Backward ref moves (rewinds).** When an incremental import moves a ref
to a tip that is not a descendant of the ref's current value (e.g. the
git repository's `main` is *behind* the store's published `main`, or a
force push rewrote the tip), the import:

* **fails with a non-zero exit by default** when the move would actually
  lose changesets — the error names the ref, both tips, the number of
  changesets that would fall off, and the escape hatch. Nothing is
  moved: every selected ref is validated first (two-phase), so a
  multi-ref import whose later ref would rewind does not move its
  earlier, forward-moving refs either, and every offending ref is named
  in the error.
* with `--allow-rewind`, performs the move (force-push parity: old
  objects and mapping rows remain, merely unreachable) and prints a
  loud multi-line `warning:` to stderr naming the ref, the old and new
  tip, and how many changesets may no longer be reachable from the ref.
* counts the fell-off changesets exactly via a metadata-only BFS over
  parent edges (no git or object reads). `--limit` imports cannot
  compute the count (slice re-rooting breaks real ancestry), so they
  refuse **every** non-descendant ref move by default — reporting the
  loss as uncounted in the error — and, under `--allow-rewind`, report
  the count as uncounted in the rewind entry.

Forward moves (the common incremental case) are silent, exactly as
before. Rewinds that lose nothing (a moved-back tip that is an ancestor
of the current value is impossible by definition — the check fires on
any non-descendant move — but sideways merges onto shared history count
only what is actually unreachable) are permitted with just the warning.

```sh
gardenbed-server import --from /path/to/git-repo --root /tmp/gardenbed \
    --repo myrepo --ref refs/heads/master --renames on
# 2 commits (2 new, 0 skipped), 9 objects, 1 ref(s) moved, 0.24s (8 commits/s)
```

`demo/run-v0.1.sh` runs the same import against the deterministic demo
corpus (`$CORPUS` → `$SERVER_ROOT`).

### gardenbed-server gc

```text
Usage: gardenbed-server gc [OPTIONS] --root <ROOT>
```

Garbage-collects unreachable objects (v0.2.md §7): a mark over the
metadata (refs, every stored changeset row, certificates, derived trees —
closed over parents, blob refs, and tree payloads), then one sweep of the
object shards unlinking what is unreachable and older than `--min-age`.
Stale `.tmp-*` staging files are reaped under the same gate.

| flag | meaning |
|---|---|
| `--root <ROOT>` | gardenbed repository directory (holds `objects/` and `metadata.sqlite3`) |
| `--dry-run` | report what would be collected; delete nothing |
| `--force` | run live without the exclusive lock (the sweep unlinks under the metadata write lock and aborts if an ingest lands mid-run) |
| `--min-age <SECONDS>` | minimum age of collectible objects, default 86400 (24 h) |

```sh
gardenbed-server gc --root /tmp/gardenbed-repo
# gc: 812 object(s) scanned, 800 live, 4 blob(s) and 8 tree(s) deleted, \
# 1 tmp file(s) reaped, 262144 byte(s) reclaimed [mark 0.012s, sweep 0.004s]
```

Offline by default: the server holds a shared `flock` on
`<root>/repo.lock` (serve's root mode holds one per open repository
handle, so repositories created after startup are covered too), and gc
wants it exclusive — running gc against a live server fails with exit
code 1 and the message to stop the server or pass `--force`.

### gardenbed-server export

```text
Usage: gardenbed-server export [OPTIONS] --root <ROOT> --into <INTO>
```

Exports the changeset graph into a real git repository (a native
SHA-1 pack + `.idx`, written directly — no git subprocess). Every
computed commit SHA is asserted against the store's git-mapping row.
The changeset→git mapping rides in the report; `--mapping FILE` writes
it as TSV for verification. `--record-mappings` additionally persists accepted
pairs to metadata; [mapping semantics](export-mappings.md) cover conflicts,
concurrency and retry boundaries.

| flag | meaning |
|---|---|
| `--root <ROOT>` | the server root directory (one subdirectory per repository) |
| `--repo <REPO>` | repository name under the root (default `default`) |
| `--into <INTO>` | destination git repository: created as a bare sha1 repository when missing, accepted as-is when present |
| `--ref <REFS>` | only export refs matching this glob (e.g. `refs/heads/*`); repeatable, empty means every ref in the metadata store |
| `--storage <FILE>` | storage configuration for the scaled backends; takes precedence over `--root` (`GARDENBED_STORAGE` is the env fallback) |
| `--mapping <MAPPING>` | write one `<64-hex changeset id>\t<git sha>` line per exported commit, sorted by changeset id, to this file |
| `--record-mappings` | opt in to transactional recording of accepted Git/changeset pairs after the Git export completes; conflicts are reported without overwriting canonical bindings (default off) |

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
