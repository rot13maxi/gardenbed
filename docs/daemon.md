# The local daemon (`gardenbed-daemon`)

An optional local tier between `bed` and the server: a **caching gRPC
proxy** plus **per-working-copy filesystem watchers**, serving on one
unix socket (v0.2.md §6, decisions D18–D19).

```
bed ──unix socket── gardenbed-daemon ──gRPC── gardenbed-server
        (optional)                              (authoritative)
```

The daemon is invisible at both ends: identical command outputs and
identical on-disk state whether it is running or not. `bed` never
*requires* it — no daemon means the embedded client talks to the
configured server directly.

## What the daemon is not

- **Not an owner of checkout or status logic** (D18): all `.bed` writes
  stay in `bed`; the daemon never opens a working-copy index. Rejected
  by design: daemon-owned status would fork behavior between
  daemon-present and daemon-absent modes.
- **Not configured per working copy**: no `config.toml` key names it
  (v0.1 files keep loading unchanged). Discovery is environment plus one
  fixed socket path.
- **Not a second source of truth**: everything it caches is
  content-addressed and hash-verified; a corrupt cache can never poison
  anyone.

## Lifecycle

```sh
gardenbed-daemon serve --upstream http://127.0.0.1:9418
```

- `--upstream URL` (required) — the server endpoint every forwarded RPC
  goes to. The `repo` field rides untouched, so one daemon serves every
  repository on that server, exactly like a client would.
- `--socket PATH` (optional) — overrides the socket path.
- `--cache DIR` (optional) — overrides the shared object cache root.

On startup the daemon prints `listening on <path>` as its first stdout
line (the same contract the server binary offers scripts and tests). It
runs until SIGINT or SIGTERM, then removes its socket. A stale socket
from a crashed daemon is probed before bind: a live daemon there fails
startup with a clear error; a dead socket is unlinked and replaced.

## Socket and cache locations

| What | Default | Override |
|---|---|---|
| Socket | `$XDG_RUNTIME_DIR/gardenbed/daemon.sock` (else `$TMPDIR/gardenbed-<uid>/daemon.sock`, else `/tmp/...`) | `GARDENBED_DAEMON` env, or `--socket` |
| Shared object cache | `$XDG_CACHE_HOME/gardenbed` (else `$HOME/.cache/gardenbed`) | `--cache` |

The socket's directory is mode 0700 — only the owner's uid may connect.

`GARDENBED_DAEMON` rules (used identically by the daemon binary and by
`bed`'s probe):

- **set to a path** — that socket is used;
- **set to empty** — the daemon is *disabled*: `bed` never probes it and
  the embedded client is the only endpoint (the v0.1 behavior, pinned);
- **unset** — the default path is probed.

The shared cache uses the same store format and sharding as `.bed/objects`
(GBLO1/GBLO2 envelope selection by size, 2-level shard, hash-verified on
read and write). A warm second working copy in the same repository scope
fetches object contents from this cache; metadata authorization and identity
probes still reach the server. Scopes include the upstream endpoint,
repository name, and the server-published repository incarnation. A new
metadata database under a reused name gets a fresh identity and cannot
inherit the predecessor's cached objects. See [cache isolation](daemon-cache-isolation.md).
Older upstreams without `RepoInfo` are served directly, with caching disabled.

## What the daemon serves

The **full Gardenbed RPC surface** (`GetTree`, `GetBlob`, `Log`, `Diff`,
`Blame`, `CreateChangeset`, `PutBlob`, `CasRef`, `Rebase`, `Publish`, …),
forwarded verbatim to the upstream — including a client's `authorization`
metadata (an authed upstream sees the same bearer token `bed` sent) — with
two divergences:

- **`GetTree`/`GetBlob` are cache-first, but auth-checked on hits**: answered
  from the scoped object store when present. A live, read-authorized
  `RepoInfo` request selects the incarnation; cache hits also retain the
  `ListRefs` authorization check with the caller's credentials. A miss is
  fetched upstream and written through only within that proven scope. Concurrent
  requests for the same id collapse into one fetch; a denied probe denies
  the hit. Cached blobs stream back to the caller in 64 KiB chunks
  (matching the server's stream shape; tonic's 4 MiB per-message decode
  limit would fail a blob-sized single chunk).
- **Write RPCs are peer-uid-gated**: before serving `PutBlob`,
  `CreateChangeset`, `CasRef`, `Rebase`, `Publish`, or `Merge`, the daemon
  checks the unix-socket peer's credentials (`SO_PEERCRED`) and refuses
  any uid other than its owner's. Read RPCs stay open to any local peer
  (the 0700 directory is the first lock; this is the second).

A small **sidecar service** rides the same socket:

| RPC | Meaning |
|---|---|
| `Ping` | liveness; returns the daemon's pid |
| `Register(state_dir)` | start (or re-start) a recursive watch on the working-copy root — the state directory's parent — and return the current generation |
| `Generation(state_dir)` | the current generation, or absent when unregistered |

Watching is **generation-counted**: the daemon never inspects content or
the index. Every watched filesystem event (debounced 50 ms; editor
save-storms collapse into one bump) increments the working copy's
generation. Events under the state directory itself (`.bed`) and
checkout staging entries (`.bed-checkout-*`) are excluded — machinery
writes must not invalidate working-copy reports. The generation counter
is preserved across re-registration: it belongs to the working copy, not
to the watcher instance.

## How `bed` uses it

**Endpoint resolution (D19).** `bed` connects daemon-first: it probes
the socket (same `GARDENBED_DAEMON` rules) and, when a daemon answers,
uses it as the session's endpoint — keeping the embedded
`config.server` client as a fallback. An absent or dead daemon is not an
error; the embedded client is used directly.

**Fetch fallback.** When the daemon is primary and dies mid-RPC
(transport death), *fetch* RPCs (`GetTree`/`GetBlob`) retry exactly once
against the embedded endpoint — a daemon crash mid `sparse add` or `bed
file` is invisible to the command's outcome. Write RPCs are never
blindly retried.

**Status fast path.** `bed status` registers the working copy with the
sidecar and caches its report keyed on the generation. When the
generation is unchanged, the cached report is returned without scanning.
Any doubt — no daemon, dead socket, unregistered path, a different
generation, a corrupt cache, or `--full-scan` — runs today's full scan.
The mtime/size hint machinery stays the correctness fallback underneath:
a stale "clean" is forbidden; a missed cache reuse is merely slow.

## Diagnostics

The daemon merges a `daemon.cache_hits` key into the proxied `GetStats`
response — the count of `GetTree`/`GetBlob` requests answered from the
shared cache — so tests and scripts can distinguish daemon-answered
fetches from server-answered ones without a new RPC.
