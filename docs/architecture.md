# gardenbed architecture — v0.1

Gardenbed has a central repository server, a sparse working-copy client,
and an MCP service. The server stores immutable content-addressed objects
and SQLite metadata. Derived trees, blame, and ancestry data support queries.

Use the standard terms in [Terminology](terminology.md). Repository storage uses `objects/` and `metadata.sqlite3`.

```text
bed ──gRPC── gardenbed-server
               ├─ metadata: SQLite  (changesets, refs,
               │                    git-SHA map, derived)
               └─ objects: filesystem CAS     (blobs, trees; zstd)
gardenbed-mcp ──in-process Repo (same service layer, no RPC)──┘
```

- Transport: tonic gRPC over TCP (`--addr host:port`); the working copy client
  additionally accepts `unix:///path/to/socket` endpoints. The server
  listens on TCP only in v0.1 (`gardenbed-server --addr`).
- Every tier answers the same RPCs (v0.1.md §2): all object access is
  **want-list** — a client resolves `RootTree(cs)` and requests exactly
  the trees/blobs it is missing. Every request names a content-addressed
  object, so any interrupted session resumes exactly.
- No daemon in v0.1: `bed` embeds the client; a local daemon (filesystem watching, shared
  node cache) is v0.2 (v0.1.md §1). The RPC surface is identical either
  way.

## 1. Wire protocol

`crates/protocol/proto/gardenbed.proto` — with
[docs/canonical-encoding.md](canonical-encoding.md), it *is* the published
protocol (D4). The tonic service `Gardenbed` exposes 17 RPCs; every
request carries `repo` (single-repo in v0.1, the field is wire format
from day one, v0.1.md §1).

| group | RPCs | notes |
|---|---|---|
| objects | `RootTree`, `GetTree`, `GetBlob` (stream) | ids are 32 raw SHA-256 bytes, never hex |
| changesets | `GetChangeset`, `PutBlob` (client-stream), `CreateChangeset` | blob bytes move **only** through `PutBlob` (> 1 MiB, header frame first) or `CreateChangeset.inline_blobs` (≤ 1 MiB) |
| graph + queries | `MergeBase`, `IsAncestor`, `Diff` (stream), `Log` (stream), `Blame` (stream) | blob content never rides on Diff/Log |
| refs | `ListRefs`, `GetRef`, `CasRef` | `GetRef` doubles as the shared revspec resolver (see §2) |
| write ops | `Rebase`, `Publish` | server-side replay and fast-forward CAS |
| debug | `GetStats` | per-repo request counters (e.g. the zero-duplicate-fetch proofs) |

Wire conventions mirrored from the canonical encoding: `Op`/`Mode`
enum values are the spec's op/mode bytes; timestamps stay git-native
(epoch seconds + tz offset in minutes) — that is what makes export
reproduce identical git SHAs.

## 2. Server (`gardenbed-server`, lib `gardenbed_server::Repo`)

`Repo` wires one repository's stores (`<root>/objects` + `<root>/metadata.sqlite3`)
and holds the semantics every surface shares — the `bed` binary's gRPC
service and `gardenbed-mcp` call the same methods, which is why MCP and
CLI answers agree by construction (v0.1.md §M7.1).

**Subcommands** (binary): `serve` (default — gRPC on `--addr`,
default `127.0.0.1:9418`), `import`, `export` (see
[docs/cli.md](cli.md)). The bound address is printed on stdout's first
line (`listening on …`) — the contract scripts rely on.

**Revspec resolution (`Repo::get_ref`, also served by the `GetRef`
RPC).** One resolver funnels every id-taking surface:

1. an existing ref with that exact name wins over prefix interpretation —
   the canonical `refs/heads/<name>` spelling first, then the name as
   given (imported histories store full names; native writes land there
   too, so exports stay git-valid);
2. a hex token of 8..=63 chars is a changeset id **prefix**: decoded
   case-insensitively, answered by one indexed range seek over the
   metadata store's 32-byte ids. Exactly one match resolves; several fail with
   `prefix <p> is ambiguous (N changesets)`; none with
   `no ref or changeset matching <p>`;
3. fewer than 8 hex chars is rejected with the minimum-length hint —
   only once no ref of that name exists;
4. a full 64-hex token is never prefix-interpreted here (clients parse
   those as ids before any lookup).

`Repo::cas_ref` canonicalizes the same way and targets the existing
`refs/heads/<name>` row when one is there — an imported ref moves; a
second ref is never forked.

**Write checks (`CreateChangeset`).** Every blob id a changeset
references must exist in object store or ride in `inline_blobs` (≤ 1 MiB, content
verified against its claimed id); gitlink entries are opaque pointers —
never dereferenced, never object store objects. The stored id is
`SHA-256(canonical bytes)`: a re-put of the same id with different bytes
is an immutability violation, never a silent overwrite.

**Rebase (server-side linear replay).** Replays `cs`'s changes onto
`new_parent`'s tree with hard conflict detection; the replayed changeset
keeps message/identities/signature and gains `parents = [new_parent]`.
Semantics (deliberate, tested):

| situation | outcome |
|---|---|
| write onto a path the new parent has with the **same** blob and mode | no-op |
| write onto a path whose parent copy is the **untouched pre-image** (the changeset's parent's copy, unmodified since) | admitted — the three-way rule that makes rebasing work |
| write onto a path with **different** content or mode | conflict |
| write whose path sits under an existing **file** path | conflict |
| write that swallows existing paths under it | replaces them (delta-apply's file↔directory semantics) |
| `Delete` / rename source that does not exist | conflict |

All conflicting paths are collected and reported together
(`rebase conflicts on: …`); nothing partial is ever stored.

**Publish** asserts `IsAncestor(current main, cs)` then CAS-moves `main`
(v0.1.md D5) — violating the precondition fails with
`publish refused: "main" is not an ancestor of the given changeset`; a
lost CAS race is a clean conflict result, not an error.

## 3. Data model

- **Changesets** are the unit of work: parents (first parent = delta base
  and mainline for blame), a change list vs that first parent
  (`add`/`modify`/`delete`/`rename`, each with path, blob id, `src_path`,
  mode), message, author and committer as git-native identities
  (seconds + tz offset), optional signature. The canonical byte encoding
  ([docs/canonical-encoding.md](canonical-encoding.md) §3, mirrored in
  v0.1.md §4) is the identity: `Changeset::id` = SHA-256 over canonical
  bytes, minimal varints, fields in spec order, decoders reject
  non-minimal/trailing/unknown data.
- **Trees are derived, never stored as truth**: `tree(cs) =
  apply(changes(cs), tree(first parent))` — only directories a change
  touches are re-created (derivation locality, M2.4: commit cost scales
  with change size, not repo size). Trees are cached in object store and
  identified by their canonical bytes, so identical trees reached via
  different histories share one object.
- **Three derived caches** (the metadata store `derived` table, keyed
  `(kind, cs)`):

| kind | what it is | what it buys |
|---|---|---|
| `tree` | the changeset's derived root tree id (the tree object itself lives in the object store) | clone/sparse/export resolve trees immediately (D3 eager materialization); recompute is a cache miss, never a wrong answer |
| `skiplist` | segmented ancestry changelog over mainline (roots and merges are segment heads) | fast `MergeBase`/`IsAncestor` and blame segment skips |
| `pathsummary` | per-changeset bounded touched-path set + cumulative per-segment Bloom filter | blame skips a whole linear segment with **one** derived read when the summary proves the path untouched (a false positive costs a step, never a wrong answer) |

Everything derived is a **cache**: wipe + rebuild must converge to
identical state (verified in M6 and re-verified after every chaos kill).

## 4. Object storage

Under the server root, the object store lives at `<root>/objects`, and
`FsObjectStore` shards it two levels deep:

```text
<root>/objects/<2 hex>/<62 hex>
```

- **Blobs** are stored under `SHA-256(raw content)` in the §6 envelope:
  `"GBLO1"` magic (5 bytes) · `raw_len: u64` little-endian · one zstd
  frame. Compression is a storage detail, never identity; on read, the
  decompressed length and the SHA-256 are verified against the addressed
  id — a torn or tampered object can never be handed back as wrong bytes.
- **Trees and changesets** are stored as their canonical payload bytes
  (their own format bytes type them; no envelope).
- **Immutability** is load-bearing: putting an existing id with different
  bytes is an error (`ImmutabilityViolation`), and every write is
  tmp-file + fsync + atomic rename, so a crash leaves an object either
  absent or byte-complete (`crates/store/src/fs.rs`).

## 5. Metadata tables

`crates/store/src/sqlite.rs` — opened WAL, `synchronous=NORMAL`
(crash-safe against process kill; power-loss durability comes from the
WAL), 5 s busy timeout:

| table | schema | role |
|---|---|---|
| `changesets` | `id BLOB PK, canonical BLOB NOT NULL` | the changeset log; `canonical` is the identity bytes |
| `refs` | `name TEXT PK, cs BLOB NOT NULL` | full ref names (`refs/heads/main`); moved only by CAS |
| `git_map` | `git_sha TEXT PK, cs BLOB NOT NULL UNIQUE` | the git↔changeset bijection; import skips mapped history, export reads it back |
| `derived` | `kind, cs, bytes, PK(kind, cs)` | the three caches above |

`cas_ref` performs its read-and-compare inside one `BEGIN IMMEDIATE`
transaction — a ref swap is a single atomic compare-and-swap. The import
path's `ingest_changeset` writes the changeset row + git mapping + derived
rows in **one** transaction per commit, so a crash lands either the whole
commit or none of it.

## 6. The bed client

All local state lives under `--state-dir` (default `./.bed`):

```text
.bed/config.toml    server (http(s)://host:port or unix:///path), repo,
                    sparse prefixes, current changeset
.bed/index.sqlite3      sqlite: files(path PK, blob_id, mode,
                    size, mtime_ns, hint) — the hint marks rows whose
                    size+mtime were hash-verified at write time
.bed/objects/       local CAS cache — the same GBLO1 envelope and
                    2-level sharding as object store
.bed/heads          local stack tips
```

- **Fetch path**: want-list + cache-first — every command asks content-addressed
  questions of `.bed/objects` before the wire, with bounded parallelism
  (`--concurrency`, default 8) and singleflight on concurrent requests.
  Resumability and idempotence fall out of content addressing, not retry
  logic (v0.1.md §M4 right-thing signal).
- **Two-phase commit**: scan the selected paths, read changed files, then upload —
  blobs ≤ 1 MiB ride inline on `CreateChangeset` within a 2 MiB aggregate
  content budget; heavier blobs and overflow stream via `PutBlob` first.
  A SIGKILL between the two leaves orphaned blobs that
  are unreferenced and harmless; the retry just re-uploads (M5.5).
- **Status** uses mtime/size as hints and hashes only suspects;
  `--full-scan` forces the correctness path.
- **Atomic checkout** stages complete files and symlinks before replacing
  working entries. When the selected state directory and destination are
  on different mounts, staging happens beside the destination so the final
  rename stays on one mount. Linux regular files prefer `O_TMPFILE`; the
  fallback creates a named entry exclusively. A state journal records its
  exact path, device, inode and kind before file bytes are written.
  Status/commit hold a shared staging lock throughout the scan; destination
  writers hold the exclusive lock through publication. Recovery also
  checks the per-stage writer lock and identity before deleting debris.
  Registered temporary entries never become repository content, and
  substituted or unregistered entries are neither ignored nor deleted.
  A crash during symlink creation or named-file bootstrap can leave an
  unregistered temporary-looking entry; it remains ordinary untracked
  work rather than being assumed disposable. Checkout directory traversal
  uses open directory descriptors and refuses symlink ancestors.

## 7. Import pipeline (git)

1. A URL is mirror-cloned to `<root>/git-mirror.git` and refreshed
   on re-use (local paths import in place).
2. Object reads go through gitoxide (D6). The commit graph is walked
   topologically (Kahn, compact `u32` arena): parents always ingest
   before children, octopus merges included; changes are computed against
   the **first parent**, with merge parents recorded as additional
   parents.
3. Status classification + rename detection run batched
   `git diff-tree --stdin --root -r -z --name-status
   --diff-merges=first-parent [-M|--no-renames]` — 256 commits per spawn
   (D8). Dir→file replacements are repaired into an applier-consumable
   shape; commits git's diff machinery cannot see (pre-canonical `100664`
   mode fixups) are classified by a raw mode-aware tree comparison
   ([docs/divergences.md](divergences.md) §4) — never invented renames.
4. **Mapped-frontier pruning**: commits whose git SHA is already mapped
   are skipped, and mapped history is not descended — sound because git
   ids are content-addressed. Re-importing an unchanged repository writes
   zero objects; force pushes just move refs (CAS).
5. Each commit lands through `ingest_changeset`: changeset canonical
   bytes, `git_map` row, and derived-data rows in one sqlite transaction
   (byte-identical values to the eager write path — kill+resume is
   byte-identical).

## 8. Blame (M6)

Blame is **first-parent lineage blame with rename following**, anchored to
`git blame --first-parent` (D7). The walk descends the first-parent chain
from the queried changeset: at each step it line-diffs the path's blob
against the parent side with the store's own xdiff port; added lines
attribute to the touching changeset, matched lines inherit and continue
downward; a rename is followed through `src_path` when the path did not
exist in the parent's tree. Merge commits blame through first parent
only. Three caches keep the walk off the hot path: the `pathsummary`
bloom proves whole segments untouched in one read; a per-walk blob cache
exploits the consecutive-toucher invariant (a path touched by consecutive
chain steps re-reads one blob); and results are memoized per engine
keyed by `(changeset, path)`. Attribution is anchored per-line to
`git blame --first-parent` on all corpora — the only documented
divergence is blank hunk-edge placement ([docs/divergences.md](divergences.md)
§5).

## 9. Durability

- **Object writes are fully durable by default**: payload fsynced to a
  unique tmp file in the destination shard, directory fsynced, atomic
  rename, directory fsynced again — a crash leaves the object absent or
  byte-complete. Read-back verifies length + SHA-256 against the address.
- **`GARDENBED_FSYNC=off`** (`off`/`0`/`false`, read once at store open)
  keeps tmp+rename (atomicity) and skips the fsyncs — for rebuildable
  bulk loads where a crash costs a re-import, not data. The
  kernel-corpus import ran this way (docs/corpora.md); benchmarks and
  budget gates keep fsync **on** so the durability story stays under
  test.
- **Metadata**: WAL + `synchronous=NORMAL` — crash-safe against process
  kill, power-loss durability from the WAL; every mutation transactional.
- **Evidence**: the M1 torn-write torture (put blob + SIGKILL × 1000 —
  every object reads back byte-exact or absent), and the M8 SIGKILL chaos
  suite (every RPC class × 100 verified cycles: 1,000 kill/restart
  rounds, every acknowledged answer re-fetched byte-exactly, then object store
  checksum sweep + `integrity_check` + derived rebuild determinism —
  docs/release-0.1.md). The object store checksum sweep deliberately skips
  `.tmp-*` files: an atomic write killed mid-flight leaves its tmp file
  unpublished, unreadable, and reaped by the next rename into that shard.

## Where the contracts live

| contract | document |
|---|---|
| scope (what is Out) | v0.1.md §1 — the Out table is contractual |
| byte-level object identity | canonical-encoding.md (D1) |
| RPC surface | `crates/protocol/proto/gardenbed.proto` (D4) |
| rebase/publish semantics | v0.1.md D5, §M5.3 |
| blame definition | v0.1.md D7 |
| import mechanics (gitoxide, diff-tree, renames) | v0.1.md D6/D8 |
| divergences from git | divergences.md (§10.2: zero *silent* divergences) |
