# gardenbed web GUI

`gardenbed-web` is a read-only web interface over a repository: history,
refs, changeset detail, tree browsing, file view, blame, diff compare, and
the server's request counters. It opens the server's repository **in
process** over `--root` (like `gardenbed-mcp`) — every answer comes from
the same [`gardenbed_server::Repo`](architecture.md) service layer the gRPC
server and MCP tools use, so the GUI cannot disagree with `bed`. There is
no working copy anywhere in this path, and nothing here can write to the
repository: the GUI is an observability surface, not a client.

```sh
cargo run -p gardenbed-web -- --root /tmp/gardenbed-repo --addr 127.0.0.1:8430
# listening on 127.0.0.1:8430
```

Open <http://127.0.0.1:8430> — no build step needed; the UI is embedded in
the binary. `--addr` defaults to `127.0.0.1:8430`. The bound address is
printed on stdout's first line, the same contract `gardenbed-server`
scripts rely on.

Try it on a scratch repository with deterministic demo history:

```sh
gardenbed-web seed --root /tmp/gardenbed-demo
gardenbed-web --root /tmp/gardenbed-demo
# → prints `seeded tip <64-hex>`
```

`seed` writes the same seventeen-changeset "garden planner" history
every time: twelve published on `main` (exec file, rename, delete, a
side branch merged back with two parents, three authors) plus five
unpublished drafts — a two-author work-in-progress set, a rebase pair,
and one wholesale-rewrite stress file (6000 lines) the diff renderer
must decline with its budget note.

`seed` takes the repository's **shared `repo.lock`** for its whole run
(the same lock `serve` and `gardenbed-server import` hold), so an offline
`gardenbed-server gc` cannot interleave with the writes; a gc already
holding the exclusive lock makes `seed` wait until it finishes.

## Pages

| Route | Page |
|---|---|
| `#/` | Overview: ref cards, request counters, activity heatmap, authors chart, recent history |
| `#/log` | History timeline with an SVG commit-graph lane column |
| `#/changeset/<id>` | Changeset metadata, diffstat summary, grouped change list |
| `#/browse/<rev>/<path>` | Tree listing with breadcrumbs; a README in the directory renders below as markdown (GitHub style) — client-side renderer, escaped end to end, relative links resolve to that revision's file views |
| `#/file/<rev>/<path>` | File content with line numbers, blame toggle (age-heat gutters) |
| `#/compare?a=<rev>&b=<rev>` | Diff between two revspecs, rows expand into unified patches |
| `#/search?q=<q>` | Case-insensitive content grep across the tree at the selected ref |
| `#/review/<id>` | Reviewing one changeset: full diff, parents, publish state, and the read-only `bed` command card for landing it |

The Overview is the multi-worker surface: a **Workers** panel groups
unpublished changesets by author (and any ids on your localStorage
watchlist) with draft/landed badges, an **Events** feed records publishes
(ref moves), detected rebases, and landings as they happen, and the
activity heatmap sits underneath. Data: `GET /api/activity` lists
changesets newest-arrival-first *including* ones no ref points at —
workers commit server-side before they publish — and the UI derives
published/unpublished by diffing against the `main` walk. All of it is
read-only: the land commands are printed for copying, writes stay in
`bed`.

Changeset and compare rows expand into real unified diffs (client-side
Myers over the old/new file contents fetched at their revisions).

Keyboard: `t` file finder (fuzzy, at the selected ref), `/` focuses the
grep box, `j`/`k` + Enter walk lists, `?` shows the help overlay. The UI
polls `/api/refs` every 10 s and toasts when the selected ref moves, so
a watcher sees agents landing work live.

The **Use in agent** button (top bar) opens a dialog with a generated
markdown prompt — repository facts (tip id, refs, root), connection
instructions (MCP tools for read-only queries, `bed` CLI for read/write),
and the working rules (sparse checkout, commit → publish, rebase on
publish conflict) — ready to paste to any agent. It is assembled from
`GET /api/server-info` (version, repository root, bound address) and the
refs listing, so the instructions are accurate for this server.

Everywhere a rev is accepted (the global ref selector, compare inputs, URL
fragments) the shared revspec resolver applies: ref name, full 64-hex id,
or an unambiguous prefix of at least 8 hex chars — the same rule `bed`
gets from `GetRef`.

## JSON API

The SPA is a thin client over a same-origin JSON API (contract documented
in `crates/web/src/lib.rs`). Endpoints: `GET /api/refs`, `/api/log`,
`/api/changeset`, `/api/tree`, `/api/file`, `/api/blame`, `/api/diff`,
`/api/stats`. Errors are `{"error": "…"}` with 400 for bad input and 404
for unknown revisions or paths. `/api/file` caps payloads at 512 KiB
(`truncated: true`) and encodes non-UTF-8 content as base64.

## Verification

- `crates/web/tests/api.rs` — Rust integration tests driving the router
  against an in-memory repository (every endpoint's happy path and the
  error statuses).
- `e2e/` — Playwright end-to-end suite (`cd e2e && npm test`): builds the
  binary, seeds the deterministic demo repository, starts the server, and
  drives a real browser through every page and interaction, including
  blame attribution, ref selection, error states, and a mobile viewport.
