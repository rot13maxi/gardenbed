# Gardenbed quickstart

Import a Git repository, create a sparse working copy, commit a change,
and publish it to `main`. Agents can query the same history without a checkout.

## Build

You need Rust 1.90 or newer and Git. From the Gardenbed source directory:

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

Use the same binaries in each terminal below. The example assumes the source
repository has a `main` branch and a `src` directory.

## Import and serve a repository

The server serves any number of repositories from one root: each lives in
its own directory under the root. Import before starting the server:

```sh
gardenbed-server import --from /path/to/git-repo --root /tmp/gardenbed --repo myrepo

gardenbed-server --root /tmp/gardenbed --addr 127.0.0.1:9418
# listening on 127.0.0.1:9418
```

Each repository directory (`/tmp/gardenbed/myrepo`) contains `objects/` and
`metadata.sqlite3`. A URL can be used instead of a local Git path; its
mirror is kept in the repository's `git-mirror.git`. Re-importing brings in
new history and updates imported refs. A root laid out the v0.1 way (stores
directly under the root) keeps serving unchanged as the repository named
`default`; `gardenbed-server move --root DIR --from default --into NAME`
renames it into the per-directory layout.

Useful import options:

| Option | Purpose |
|---|---|
| `--ref refs/heads/main` | Import only matching branches; repeatable |
| `--renames off` | Represent moved files as delete/add pairs |
| `--limit N` | Import a history suffix for testing; not a full-fidelity import |

Object writes are fsynced by default. `GARDENBED_FSYNC=off` skips fsyncs for
rebuildable fixtures; keep the default for valuable repositories.

## Create a sparse working copy

In another terminal, with the built binaries on `PATH`:

```sh
mkdir -p /tmp/gardenbed-work && cd /tmp/gardenbed-work
bed clone http://127.0.0.1:9418/myrepo
bed sparse add src
bed status
```

Clone creates local state in `.bed/` and materializes no files. `sparse add`
fetches and writes the selected paths. Directory prefixes and individual files
are supported; `.` selects the whole repository.

```sh
bed sparse add Cargo.toml tests
bed sparse list
bed sparse remove tests    # remove the selection; keep working files
```

The clone URL addresses the repository: its path names the repository
(`http://host:port/myrepo`; no path selects `default`). `--repo` names it
too and wins on conflict. Without `--ref`, clone tries `main`, `master`,
then `trunk`. Select another starting ref with
`bed clone <server-url> --ref feature`. `clone --sparse src` records an
initial selection but does not materialize files.

`--state-dir DIR` selects another local state directory. It names the state
directory itself, not the working-copy root. The default is `./.bed`.

## Commit and publish

Edit files under the selected paths, then:

```sh
GARDENBED_AUTHOR="Your Name <you@example.com>" \
  bed commit -m "Describe the change"
bed log --limit 1
bed publish
```

`commit` creates a changeset on the server immediately and updates the working
copy's current changeset. `publish` is a separate operation that fast-forwards
`main`. There is no push step between the working copy and server.

If `main` has advanced, replay your current changeset onto its new tip:

```sh
bed rebase main
bed publish
```

Rebase currently replays **one changeset**. Concurrent edits to the same content
or path are reported as conflicts; Gardenbed does not merge file content for
you. Publication uses compare-and-swap, so concurrent publishers cannot silently
overwrite each other's updates.

## Inspect files and history

```sh
bed status --full-scan       # hash every tracked file
bed file src/main.rs         # bytes at the current changeset
bed log --limit 5
bed diff                    # current changeset against its first parent
bed diff main <changeset>
bed blame src/main.rs        # first-parent history, following renames
bed switch main
```

Changeset arguments accept full 64-hex IDs, unique prefixes of at least eight
hex characters, or ref names. A matching ref takes precedence over a prefix.
Bare branch names resolve to `refs/heads/<name>`. `file`, `log`, and `blame`
also accept `--cs <changeset-or-ref>`.

Checkout updates preserve local edits to files unchanged by the target and
refuse destructive collisions before changing files or checkout state. Commit
or move local work aside before retrying. Paths overlapping the state directory
are protected. Cross-filesystem state directories are supported, including a
`.bed` symlink to another filesystem.

## Export to Git

```sh
gardenbed-server export --root /tmp/gardenbed --repo myrepo --into /tmp/export.git \
  --mapping /tmp/mapping.tsv
git -C /tmp/export.git fsck --strict
git -C /tmp/export.git log --oneline
```

The mapping file pairs changeset IDs with exported Git commit IDs. Supported
Git histories round-trip exactly; see [interoperability limits](divergences.md)
for signatures, encodings, and other exceptions.

## Query without a checkout

The MCP server opens repository storage directly and speaks newline-delimited
JSON-RPC on stdio:

```sh
gardenbed-mcp --root /tmp/gardenbed --repo myrepo < requests.jsonl
```

The named repository is the only one the session answers from.

It exposes `log`, `diff`, `file`, and `blame`. No working copy is needed.
See [MCP setup and tool schemas](mcp.md) for client configuration.

## Offline behavior and troubleshooting

`status`, `sparse list`, and `sparse remove` work offline. Commands that query
history, fetch files, or write to the repository require the server.

| Problem | Next step |
|---|---|
| No local state | Run `bed clone`, or select existing state with `--state-dir` |
| Cannot reach the server | Start `gardenbed-server` and check its address |
| Clone finds no default ref | Pass `--ref` with an existing branch name |
| Changeset prefix is ambiguous | Use more hex characters |
| Checkout would overwrite local work | Commit or move that work aside |
| Publication would not fast-forward `main` | Run `bed rebase main`, resolve any conflicts, then retry |

For every option, see the [command reference](cli.md). The
[terminology guide](terminology.md) defines the interface's terms. The
[architecture guide](architecture.md) explains storage and server behavior.
