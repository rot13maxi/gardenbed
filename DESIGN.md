# gardenbed — design notes

Name locked (Oct 2026): project **gardenbed**, CLI **bed**, daemon
**scarecrow**, sparse checkout paths **plot**.

Ancestry: Anthropic's
[Scion](https://www.anthropic.com/engineering/redesigning-source-control)
for the architecture; Sapling/Mononoke and Jujutsu as prior art.

**Native-first**: we own the data model. Git is an import/export bridge
(the "rootstock" we graft onto), not the core.

## Principles

1. **Server-authoritative.** The server *is* the repository; the client is a
   working copy plus a cache. There is no `push`; `pull` is a cache refresh.
2. **Changesets, not commit-trees.** A changeset records only the files
   changed vs. its *first parent*, plus content-addressed blob pointers.
   Commit cost scales with change size, never repo size. Trees exist only as
   derived, cached objects.
3. **Derived data is a framework, not features.** Blame, path history,
   ancestry skip lists: `value(cs) = compute(cs, values(parents))`. Eager on
   mainline, lazy elsewhere, invisible to clients.
4. **Want-list negotiation.** Clients fetch specific content-addressed
   objects. No packfiles, no serve-time delta compression; every tier
   (scarecrow → nursery → arboretum) answers identical RPCs.
5. **Sparse, optional working copies.** Diff, blame, merge-base are APIs; a
   job that needs no files on disk needs no checkout.
6. **Lock-free writes.** Ref updates are compare-and-swap on one row.
   Nothing else is global state.

## Data model (sketch)

```
Changeset {                       // id = SHA-256(canonical bytes)
  parents: [Id]                   // first parent is significant
  changes: [ { path, op: add|modify|delete|rename, blob?, mode, src_path? } ]
  meta: { author, committer, message, time, sig? }
}
Blob: content-addressed, FastCDC-chunked, zstd
Tree: derived — tree(cs) = apply(changes(cs), tree(parent(cs))); cached only
Ref:  (repo, name) → cs_id, compare-and-swap
```

- Flat paths in changesets; trees are a hierarchical cache for checkout.
- Canonical serialization must be byte-exact (spec-level decision).
- Changesets live in the metadata DB; blobs in the object store; both behind
  traits (fs/sqlite first, s3/postgres later). Single-binary self-host mode
  is a first-class target.

## Naming system

Extends git's existing botanical vocabulary:

| Concept | Name |
|---|---|
| Changeset (WAL entry) | **seed** — small, self-contained, grows when planted |
| Create a commit | **sow** |
| Land to main | **harvest** |
| Rebase onto main | **graft** |
| Stack of changes | **trellis** — the structure that supports growing stacks |
| Sparse checkout paths | **plot** (`bed plot add src/...`) |
| Authoritative server | the **arboretum** — the complete collection |
| Regional object cache | the **nursery** — raises plants near where they'll be planted |
| Node-local daemon | **scarecrow** — stands in the bed and watches (fs watcher + cache) |
| Blob store | **loam** — the good earth everything grows from |
| Metadata DB | the **seedbed** |
| Derived data framework | **compost** — history recycled into nutrition for queries |
| Warmup snapshots | **seedbank** (Scion literally calls these "seeds") |
| Garbage collection | **prune** |
| Copy/move detection | **cuttings** — a cutting took root elsewhere |
| Git import/export bridge | **rootstock** — what we graft onto |
| Release branches | **heirloom** |
| Stale branches | **fallow** |
| Agent sandbox | **cloche** |
| CI fleet | the **apiary** |

Naming decisions on record:

- `gardenbed` is free on crates.io; it also publishes the `bed` binary.
- `brew install bed` is taken (itchyny's hex editor), so the Homebrew
  formula is `gardenbed`, installing the `bed` binary.
- `gdn` stays a free fallback alias if we ever want a second binary name.

## Crates

Workspace layout, package names prefixed `gardenbed-` (dirs stay short):
`model`, `store`, `derive` (compost), `protocol`, `server` (arboretum),
`scarecrow` (daemon), `bed` (CLI — package name is `gardenbed`, bin `bed`),
`rootstock` (git bridge), `mcp`.

## First 90 days

| Weeks | Milestone |
|---|---|
| 1–2 | model + canonical encoding + store traits (fs/sqlite), changeset CRUD |
| 3–4 | tree derivation + object API + diff |
| 5–6 | git import via gitoxide + differential-vs-git test harness |
| 7–8 | gRPC server + thin CLI read path (sparse checkout, log, diff) |
| 9–10 | server-side commit + CAS refs + git export → the full loop works |
| 11–12 | blame/path-history + ancestry skip lists, MCP server, v0.1 demo |

**v0.1 demo:** import a real 1–2 GB repo into one binary, sparse clone in
seconds, server-side commit visible instantly via API, export back to git
and push to GitHub, agent does blame over MCP with no checkout.

## Deliberate divergences from Scion

- **Offline grace:** allow editing and queueing changeset creation offline;
  Scion assumes always-online. Laptop users will bounce off anything else.
- **Git is a peer, not a predecessor:** import/export continuously; GitHub
  stays a valid forge target.

## Open questions

- Canonical encoding: hand-rolled (postcard-style, explicit field order) vs
  canonical CBOR — must be byte-stable across implementations.
- Hash: SHA-256 everywhere; git SHA ⇄ changeset mapping table in metadata.
- Blame semantics under the first-parent convention + cuttings detection.
- Storage economics: standalone zstd objects vs background delta compaction
  as an optional, rebuildable optimization layer.
- Offline commit queue: how far do we take it?
- Confirm Homebrew tolerance for a `gardenbed` formula installing a `bed`
  binary (formula/binaries differing is allowed; worth verifying policy).
