# gardenbed-mcp — the MCP surface

`gardenbed-mcp` exposes repository `log`, `diff`, `blame`, `file`, and
`copies` queries over the Model Context Protocol. It opens the repository's object
store and metadata database directly; no working copy or `.bed` directory
is needed. Tool calls do not create changesets or update refs.

## 1. Quickstart

```sh
cargo build -p gardenbed-mcp
# Query repository storage over stdio:
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  | gardenbed-mcp --root /srv/repository
```

Options:

| Flag | Meaning |
|---|---|
| `--root <DIR>` (required) | The server's data dir: its `objects/` object store and `metadata.sqlite3` metadata database — the same directory `gardenbed-server --root` serves. |
| `--repo <NAME>` | Accepted for forward compatibility; v0.1 servers are single-repo and the name is validated but otherwise unused (the API carries a repo field from day one, v0.1.md §1). |

The server speaks newline-delimited JSON-RPC 2.0 on stdin/stdout. EOF on
stdin ends it cleanly (exit 0); a server that will not open exits 1
with the reason on stderr. Diagnostics go to **stderr only** — stdout
carries the protocol and nothing else.

### MCP client configuration

Any MCP client that launches stdio servers can attach. The generic shape:

```json
{
  "mcpServers": {
    "gardenbed": {
      "command": "gardenbed-mcp",
      "args": ["--root", "/srv/repository"]
    }
  }
}
```

The server is strictly sequential: one request in, one response out, one
JSON object per line. Notifications get no response, so a client can
always associate the next line with its last request.

## 2. Protocol

Hand-rolled with `serde_json` alone — no MCP SDK
(`crates/mcp/src/protocol.rs`). Served methods:

| Message | Answer |
|---|---|
| `initialize` | `{protocolVersion, capabilities: {tools: {}}, serverInfo: {name: "gardenbed-mcp", version}}`. The client's `protocolVersion` is echoed back verbatim when present (gardenbed-mcp serves one revision and trusts the client to know it); otherwise `2025-06-18`. |
| `notifications/initialized` | nothing — notifications are never answered |
| `ping` | empty result `{}` |
| `tools/list` | the five tools with JSON-Schema `inputSchema`s |
| `tools/call` | `{content: [{type: "text", text}], isError?}` |
| anything else with an `id` | JSON-RPC error `-32601` (method not found) |
| malformed line (not valid JSON) | JSON-RPC error `-32700` bound to `id: null`; **the server keeps serving** |
| valid JSON of the wrong shape (batch, missing method, bad `id`) | `-32600` invalid request |

A `tools/call` without a usable tool name (`-32602`), and a tool that
fails on its inputs, are distinguished: protocol errors mean "the call was
malformed", `isError` results mean "the call was fine, the answer is an
explanation the agent can read and act on".

## 3. Tools

All ids and refs are strings. The optional `cs` argument of `log`,
`blame`, and `file` accepts a 64-hex changeset id (uppercase tolerated —
parsers accept any case per the canonical-encoding spec §1) or a ref
name, resolved through the server's ref table; it defaults to the
`main` ref. Every resolution verifies existence. Display ids are always
lowercase hex.

### `log` — `{cs?, limit?}`

First-parent history from `cs`, newest first, rendered git-log-like:

```text
<64-hex changeset id>  <author name> <author email>
Date: YYYY-MM-DD HH:MM:SS ±hhmm

    <message line>
    <message line>
```

Entries are separated by one blank line. `limit` defaults to **20**;
`limit: 0` means all entries. The date is the author's own wall-clock
time in the changeset's own timezone offset (no locale, no host clock).

### `diff` — `{a, b}` (both required)

What changed between changesets `a` and `b` (a → b), as git
`--name-status` lines:

```text
<letter> <path>
```

Letters: `A` added, `M` modified, `D` deleted, `T` type change (regular ↔
symlink ↔ gitlink). Rename detection is off, matching Gardenbed's
changeset tree diffs and `git diff-tree --no-renames`. An empty diff
answers `no changes between <a> and <b>`. **Both sides are required** —
omitting either is an `isError` result saying `diff needs two changesets`,
because a headless tool has no working copy or current changeset to
default from.

### `blame` — `{cs?, path}`

First-parent lineage blame of one path (rename following), as coalesced
line ranges:

```text
<lo>-<hi> <64-hex changeset id>
```

1-based, inclusive, ascending — every line of the file is covered by
exactly one range. Edge cases: a missing or directory path is an
`isError` result; an **empty file** answers a note (`no blameable lines …`
— git prints nothing for one); a **submodule pointer** is described
instead of blamed (git errors on it — documented divergence, see
`docs/divergences.md`).

### `file` — `{cs?, path}`

One file's content at a changeset as a single text block — what
`git show <cs>:<path>` prints. Symlinks render their target bytes;
**submodule pointers** are described (`upstream commit <sha>`) instead of
read (the pointer is not a object store object); an empty file answers empty
text. Content that is not valid UTF-8 is rendered **lossily**, with a
warning on stderr (v0.1: the tool returns text blocks only — a documented
divergence).

### `copies` — `{cs}` (required)

The exact-blob copy and rename facts of one changeset against its first
parent, as JSON:

```json
{
  "changeset": "<64-hex id>",
  "entries": [
    { "path": "moved.txt", "from": "a.txt", "kind": "rename" },
    { "path": "copy/of-b.txt", "from": "b.txt", "kind": "copy" }
  ]
}
```

A **rename** is a changeset rename whose blob is unchanged from its source
path's blob in the first parent's tree (a pure move). A **copy** is an
added path whose blob already exists at another path in the first
parent's tree; the reported source is the lexicographically smallest such
path. A changeset with no facts answers an empty `entries` array. A
missing `cs` argument or an unknown changeset is an `isError` result
naming the argument and the next step.

## 4. Parity by construction

The MCP tools call the same `gardenbed_server::Repo` methods the CLI's
`gardenbed-server` serves gRPC from — `Repo::log`, `Repo::diff`,
`Repo::blame`, tree/blob reads — so an MCP answer and a `bed`-over-gRPC
answer come from the same function calls on the same types. The crate's
tests pin this three ways (`crates/mcp/tests/mcp_suite.rs`):

1. **M7.1 parity** — MCP output == `Repo`/engine answers, == `git
   diff-tree` / `git blame --first-parent` / `git show` through the
   harness oracle, and byte-equal to `bed`'s own renderers where the two
   render the same shape (log entry dates and message indentation; file
   bytes; diff letters). The one deliberate textual difference: the MCP
   log prints the **full 64-hex id** where `bed log` prints the 16-hex
   short form — an agent has no bed to resolve a short id with, and every
   follow-up tool call wants a full id.
2. **M7.2 zero-checkout** — the suite spawns the real binary with its
   working directory an empty sandbox and asserts no `.bed` directory
   exists anywhere in the sandbox or the fixture tree, before *and*
   after all five tools answer.
3. **M7.3 scripted QA agent** — `examples/qa_agent.rs` /
   `crates/mcp/src/qa.rs`: an MCP client that composes the five tools the
   way a real agent would (blame → log to attribute an author; log →
   diff to find commits touching a path) over a generated history whose
   ground truth is real git; ≥ 20 canned questions, 100% required.

Run the scripted agent walkthrough:

```sh
cargo build -p gardenbed-mcp
cargo run -p gardenbed-mcp --example qa_agent
```

## 5. Divergences from git (v0.1)

| Divergence | Why |
|---|---|
| `log` shows the full 64-hex id (bed shows the 16-hex short form) | agents must be able to copy an id straight into the next tool call |
| `diff` is name-status only (no patch hunks) | v0.1 tool surface; `bed diff` renders the full patch |
| `blame` answers coalesced ranges, not per-line annotations with authors | the author attribution is one follow-up `log` call away; ranges keep answers compact |
| submodule pointers are described, not read (`file`) or blamed (`blame`) | the pointer is not a object store object; git errors on `git blame` for one |
| non-UTF-8 file content renders lossily (with a stderr warning) | tools return text blocks in v0.1 |
| `file` on a gitlink names the upstream commit, `git show` prints the raw pointer bytes | the commit is what an agent wants |
| `copies` reports exact-blob facts only (source = smallest matching path, first parent only); git's rename/copy detection is similarity-based and searches whole commits | exact equality is deterministic and content-addressed; similarity thresholds are not reproducible derived data |

## 6. Library layout

| Module | Role |
|---|---|
| `crates/mcp/src/protocol.rs` | wire shapes: parse, response/error rendering, JSON-RPC codes |
| `crates/mcp/src/tools.rs` | the five tools over `gardenbed_server::Repo`, their schemas, renderers |
| `crates/mcp/src/lib.rs` | `handle_line` — the whole server behavior in one function |
| `crates/mcp/src/main.rs` | the binary: args, in-process `Repo`, stdin loop |
| `crates/mcp/src/qa.rs` | the scripted agent, fixture machinery, MCP process client |
