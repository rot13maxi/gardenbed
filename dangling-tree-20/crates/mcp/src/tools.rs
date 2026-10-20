//! The five MCP tools (v0.1.md §1 + the cuttings feature): `log`, `diff`,
//! `blame`, `file`, `copies` — each
//! ([`gardenbed_server::Repo`]), which is the very layer the CLI's server
//! serves gRPC from. **Parity by construction**: an MCP answer and a
//! `bed`-over-gRPC answer come from the same function calls on the same
//! types, and the M7 tests pin the rendered text against both the
//! underlying `Repo` answers and `bed`'s own rendering.
//!
//! Every tool takes an optional `cs` — a 64-hex changeset id, a ref name,
//! or a changeset id prefix (8+ hex chars) — resolved through
//! `Repo::get_ref`, defaulting to the `main` ref.
//! There is deliberately no working copy anywhere in this path: the whole
//! toolset answers from the server's objects + metadata alone (the
//! zero-checkout contract, v0.1.md §M7.2).
//!
//! Errors are *results*: bad input (unparsable ids, invalid paths, missing
//! files) becomes an `isError` text block an agent can read and correct,
//! never a protocol-level failure. Only structurally broken calls (a
//! `tools/call` without a tool name) are protocol errors.

use std::str::FromStr;

use gardenbed_model::{Id, Mode, Op, TreeEntry, TreeKind};
use gardenbed_server::{Repo, ServerError};

/// The server info name gardenbed-mcp reports in `initialize`.
pub const SERVER_NAME: &str = "gardenbed-mcp";

/// The crate version gardenbed-mcp reports in `initialize`.
pub const SERVER_VERSION: &str = "0.1.0";

/// The ref every v0.1 tool defaults to when `cs` is omitted (the
/// server's authoritative tip; `bed clone` starts at the same ref).
pub const DEFAULT_REF: &str = "main";

/// The outcome of one tool call, as it lands in the `tools/call` result.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    /// The text block the agent sees.
    pub text: String,
    /// `true` when the text describes a failure the agent can react to
    /// (bad input, missing file, unknown ref). Tool-level failures are
    /// results, never protocol errors.
    pub is_error: bool,
}

impl ToolOutcome {
    /// A successful text answer.
    pub fn text(text: impl Into<String>) -> Self {
        ToolOutcome {
            text: text.into(),
            is_error: false,
        }
    }

    /// A failed answer: the agent gets the reason, the protocol stays up.
    pub fn error(text: impl Into<String>) -> Self {
        ToolOutcome {
            text: text.into(),
            is_error: true,
        }
    }
}

/// The `tools/list` payload: the five tools with JSON-Schema
/// `inputSchema`s. All ids and refs are strings; `cs`-taking tools accept
/// either form and default to the `main` ref.
pub fn descriptors() -> serde_json::Value {
    let cs_prop = serde_json::json!({
        "type": "string",
        "description": "changeset to answer at: a 64-hex id, a ref name, or a changeset id prefix (8+ hex chars, must be unambiguous); defaults to the \"main\" ref",
    });
    serde_json::json!({
        "tools": [
            {
                "name": "log",
                "description": "History from a changeset, newest first — every parent lineage, date-ordered, like `git log`'s default walk (git-log-like text: id, author, Date, indented message). `limit` 0 means all.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cs": cs_prop,
                        "limit": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "at most N changesets (default 20; 0 = all)",
                        },
                    },
                },
            },
            {
                "name": "diff",
                "description": "What changed between two changesets (a → b), as git --name-status lines: `<letter> <path>`, letters A (added), M (modified), D (deleted), T (type change); rename detection is off, like Gardenbed's changeset tree diffs. Both sides are required — a headless tool has no working copy to default from.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "a": {
                            "type": "string",
                            "description": "the older side: 64-hex changeset id, ref name, or unambiguous id prefix (8+ hex chars)",
                        },
                        "b": {
                            "type": "string",
                            "description": "the newer side: 64-hex changeset id, ref name, or unambiguous id prefix (8+ hex chars)",
                        },
                    },
                    "required": ["a", "b"],
                },
            },
            {
                "name": "blame",
                "description": "First-parent lineage blame of one path at a changeset (git blame --first-parent semantics, rename following): coalesced line ranges `<lo>-<hi> <64-hex changeset>`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cs": cs_prop,
                        "path": {
                            "type": "string",
                            "description": "repo-relative file path",
                        },
                    },
                    "required": ["path"],
                },
            },
            {
                "name": "file",
                "description": "One file's content at a changeset, as text (lossy UTF-8; submodule pointers are described instead of read).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cs": cs_prop,
                        "path": {
                            "type": "string",
                            "description": "repo-relative file path",
                        },
                    },
                    "required": ["path"],
                },
            },
            {
                "name": "copies",
                "description": "Exact-blob copy and rename facts of one changeset against its first parent, as JSON: entries with path, from (source path), kind (\"rename\" or \"copy\"). A rename is a move whose blob is unchanged; a copy is an add whose blob already existed at another path (source = lexicographically smallest such path). No similarity heuristic — exact blob equality only, unlike git's rename/copy detection.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "cs": cs_prop,
                    },
                    "required": ["cs"],
                },
            },
        ],
    })
}

/// A `tools/call` request's `(tool name, arguments)` — protocol-shaped
/// validation only (a missing or non-string name is a `-32602`, not a
/// tool outcome, since no tool ran).
pub fn extract_call(params: &serde_json::Value) -> Result<(String, serde_json::Value), String> {
    let Some(name) = params.get("name") else {
        return Err("tools/call needs a \"name\" naming one of the five tools".into());
    };
    let Some(name) = name.as_str() else {
        return Err("tools/call's \"name\" must be a string".into());
    };
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if !args.is_object() {
        return Err("tools/call's \"arguments\" must be an object".into());
    }
    Ok((name.to_owned(), args))
}

/// Runs one tool call against `repo`. Unknown tool names and bad
/// arguments come back as `isError` outcomes — the protocol stays up and
/// the agent learns what went wrong.
pub fn call_tool(repo: &Repo, name: &str, args: &serde_json::Value) -> ToolOutcome {
    match name {
        "log" => tool_log(repo, args),
        "diff" => tool_diff(repo, args),
        "blame" => tool_blame(repo, args),
        "file" => tool_file(repo, args),
        "copies" => tool_copies(repo, args),
        other => ToolOutcome::error(format!(
            "unknown tool {other:?} — the repository answers log, diff, blame, file and copies"
        )),
    }
}

// ------------------------------------------------------------ shared plumbing

/// Resolves the `cs` argument to a changeset id: absent → the `main` ref;
/// 64 hex chars → an id; anything else → a ref name, falling back to a
/// changeset id prefix (8+ hex chars) through `Repo::get_ref` — the same
/// shared resolver the server's `GetRef` RPC serves `bed`, so every
/// tools accept truncated ids exactly like the CLI. Precedence lives
/// server-side: an existing ref with that exact name wins over prefix
/// interpretation. A 64-hex id is passed through and verified by the Repo
/// call that uses it — a dangling id surfaces the server's "changeset …
/// not found" from there, never a mysterious missing-object error deeper
/// in the walk. Ambiguous / too-short / unmatched prefixes surface the
/// resolver's own errors verbatim (`prefix … is ambiguous (N changesets)`,
/// `changeset id prefix must be at least 8 hex chars`,
/// `no ref or changeset matching …`).
fn resolve_cs(repo: &Repo, cs: Option<&str>) -> Result<Id, String> {
    match cs {
        None => repo
            .get_ref(DEFAULT_REF)
            .map_err(err_text)?
            .ok_or_else(|| format!("ref {DEFAULT_REF:?} does not exist on this server")),
        Some(spec) => {
            let spec = spec.trim();
            if spec.is_empty() {
                return Err("cs is empty — give a 64-hex changeset id or a ref name".into());
            }
            if spec.len() == 64 {
                return Id::from_str(spec)
                    .map_err(|_| format!("cs is 64 characters but not valid hex: {spec:?}"));
            }
            repo.get_ref(spec)
                .map_err(err_text)?
                .ok_or_else(|| format!("ref {spec:?} does not exist on this server"))
        }
    }
}

/// Renders any repo error as agent-readable text (the Display strings are
/// already precise; the agent just gets them verbatim).
fn err_text(e: ServerError) -> String {
    e.to_string()
}

/// Reads an optional string argument.
fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

/// Reads an optional non-negative integer argument (`limit`); `None` when
/// absent. A wrong shape is an error, never silently ignored.
fn arg_limit(args: &serde_json::Value) -> Result<Option<u32>, String> {
    match args.get("limit") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) if n <= u32::MAX as u64 => Ok(Some(n as u32)),
            _ => Err("limit must be a non-negative integer".into()),
        },
    }
}

/// Validates a path argument against the repo's flat-path rules
/// (canonical spec §3.1: relative, NUL-free, no empty/./.. components).
fn validated_path(args: &serde_json::Value) -> Result<String, String> {
    let Some(raw) = arg_str(args, "path") else {
        return Err("path is required (a repo-relative file path)".into());
    };
    gardenbed_model::codec::validate_path(raw)
        .map(|()| raw.to_owned())
        .map_err(|e| format!("invalid path {raw:?}: {e}"))
}

/// The tree entry at `path` under `cs`'s root tree (`None` when the path
/// names nothing). Walks the derived trees via the repo — the same walk
/// `bed`'s client performs, done server-side here.
fn entry_at(repo: &Repo, cs: Id, path: &str) -> Result<Option<TreeEntry>, ServerError> {
    let Some(root) = repo.root_tree(cs)? else {
        return Ok(None); // the empty tree: nothing anywhere
    };
    let mut cur = Some(root);
    let comps: Vec<&str> = path.split('/').collect();
    for (i, comp) in comps.iter().enumerate() {
        let Some(id) = cur else {
            return Ok(None); // below a file: nothing there
        };
        let Some(tree) = repo.get_tree(id)? else {
            return Ok(None);
        };
        let Some(entry) = tree.entries.iter().find(|e| e.name == *comp) else {
            return Ok(None);
        };
        if i + 1 == comps.len() {
            return Ok(Some(entry.clone()));
        }
        match entry.kind {
            TreeKind::Dir => cur = entry.tree,
            TreeKind::File => return Ok(None), // a file cannot contain paths
        }
    }
    unreachable!("the loop returns on the final component")
}

/// The git `--name-status` *type* a mode belongs to: regular and exec
/// files are one type, symlinks and gitlinks are their own. Matches
/// `gardenbed_bed::render`'s classification (and git's `T`).
fn type_of(mode: Mode) -> u8 {
    match mode {
        Mode::Symlink => b's',
        Mode::Gitlink => b'g',
        Mode::Exec | Mode::Regular => b'f',
    }
}

// ------------------------------------------------------------------- log

/// `log`: `{cs?, limit?}` — the history from `Repo::log` (the server's
/// date-ordered walk, git's own algorithm), rendered in the same shape
/// `bed log` prints, with one deliberate difference: the id column is the
/// **full 64-hex id**, because an MCP agent has no bed to resolve a
/// 16-hex short form with and every subsequent tool call wants a full id
/// (canonical-encoding spec §1: lowercase hex is the display form).
pub fn tool_log(repo: &Repo, args: &serde_json::Value) -> ToolOutcome {
    let limit = match arg_limit(args) {
        Ok(limit) => limit.unwrap_or(20),
        Err(e) => return ToolOutcome::error(e),
    };
    let cs = match resolve_cs(repo, arg_str(args, "cs")) {
        Ok(cs) => cs,
        Err(e) => return ToolOutcome::error(e),
    };
    let entries = match repo.log(cs, limit) {
        Ok(entries) => entries,
        Err(e) => return ToolOutcome::error(err_text(e)),
    };
    let mut out = String::new();
    for (i, (id, changeset)) in entries.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        render_log_entry(*id, &changeset.author, &changeset.message, &mut out);
    }
    ToolOutcome::text(out)
}

/// One log entry in working copy's git-log-like shape (the date rendering and
/// message indentation byte-match `gardenbed_bed::render::render_log_entry`
/// for the same author/message; that format is pinned by the M7 parity
/// tests — replicated here so the MCP server does not link the CLI crate).
pub fn render_log_entry(
    id: Id,
    author: &gardenbed_model::Identity,
    message: &str,
    out: &mut String,
) {
    out.push_str(&id.to_hex());
    out.push_str("  ");
    out.push_str(&author.name);
    out.push_str(" <");
    out.push_str(&author.email);
    out.push_str(">\n");
    out.push_str("Date: ");
    out.push_str(&render_date(author.time_secs, author.tz_min));
    out.push('\n');
    out.push('\n');
    let trimmed = message.trim_end_matches('\n');
    for line in trimmed.split('\n') {
        if line.is_empty() {
            out.push('\n');
        } else {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
    }
}

/// A git-native timestamp in its **own** timezone offset:
/// `YYYY-MM-DD HH:MM:SS ±hhmm` — no wall clock, no locale; the changeset's
/// own fields decide. Byte-identical to `gardenbed_bed::render::render_date`
/// (pinned by the M7 parity tests; Howard Hinnant's `civil_from_days`).
pub fn render_date(time_secs: u64, tz_min: i32) -> String {
    let local = time_secs as i64 + tz_min as i64 * 60;
    let days = local.div_euclid(86_400);
    let secs_of_day = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let sign = if tz_min < 0 { '-' } else { '+' };
    let a = tz_min.unsigned_abs();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}{:02}{:02}",
        y,
        m,
        d,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
        sign,
        a / 60,
        a % 60
    )
}

/// Days since 1970-01-01 → `(year, month, day)` (proleptic Gregorian).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // year of era
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year
    let mp = (5 * doy + 2) / 153; // month index [0, 11], March = 0
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ------------------------------------------------------------------ diff

/// `diff`: `{a, b}` — what changed between two changesets, as git
/// `--name-status` lines: `<letter> <path>`, plus `from <src_path>` for
/// renames (tree diffs carry none in v0.1 — rename detection is off — but
/// a changeset-level rename answer keeps the shape for future callers).
/// Letters: `A` add, `M` modify, `D` delete, `T` type change (regular ↔
/// symlink ↔ gitlink), classified from the old side's mode exactly like
/// git's diff-tree and working copy's renderer. **Both sides are required**: a
/// headless tool has no working copy or current changeset to default
/// from; omitting either is an error the agent can act on.
pub fn tool_diff(repo: &Repo, args: &serde_json::Value) -> ToolOutcome {
    let (Some(a), Some(b)) = (arg_str(args, "a"), arg_str(args, "b")) else {
        return ToolOutcome::error(
            "diff needs two changesets: give a and b (64-hex ids, ref names, \
             or 8+ hex id prefixes); a headless MCP tool has no working copy \
             to default from",
        );
    };
    let a = match resolve_cs(repo, Some(a)) {
        Ok(id) => id,
        Err(e) => return ToolOutcome::error(e),
    };
    let b = match resolve_cs(repo, Some(b)) {
        Ok(id) => id,
        Err(e) => return ToolOutcome::error(e),
    };
    let changes = match repo.diff(a, b) {
        Ok(changes) => changes,
        Err(e) => return ToolOutcome::error(err_text(e)),
    };
    if changes.is_empty() {
        return ToolOutcome::text(format!(
            "no changes between {} and {}",
            a.to_hex(),
            b.to_hex()
        ));
    }
    let mut out = String::new();
    for c in &changes {
        // git's classification needs the *old* side's mode, which a
        // tree-to-tree diff does not carry: look it up in a's tree (None
        // when the old side had no file at that path — including the
        // file↔directory swap case, where git also sees a fresh add).
        let old = match entry_at(repo, a, &c.path) {
            Ok(old) => old.filter(|e| e.kind == TreeKind::File),
            Err(e) => return ToolOutcome::error(err_text(e)),
        };
        let letter = match (old.is_some(), c.op) {
            (false, Op::Add | Op::Modify | Op::Rename) => 'A',
            (false, Op::Delete) => unreachable!("a delete always has an old side"),
            (true, Op::Delete) => 'D',
            (true, op) => {
                let old_mode = old.map(|e| e.mode).unwrap_or(Mode::Regular);
                if type_of(old_mode) != type_of(c.mode) {
                    'T'
                } else if op == Op::Rename {
                    'R'
                } else {
                    'M'
                }
            }
        };
        out.push(letter);
        out.push(' ');
        out.push_str(&c.path);
        if c.op == Op::Rename
            && let Some(src) = &c.src_path
        {
            out.push_str(" from ");
            out.push_str(src);
        }
        out.push('\n');
    }
    ToolOutcome::text(out)
}

// ----------------------------------------------------------------- blame

/// `blame`: `{cs?, path}` — first-parent lineage blame ranges as
/// `<lo>-<hi> <64-hex changeset>` lines (1-based, inclusive, ascending).
/// A missing or directory path is an error; an empty file answers with a
/// note (git prints nothing); a submodule pointer is described, not
/// blamed (the engine's documented divergence from git, which errors).
pub fn tool_blame(repo: &Repo, args: &serde_json::Value) -> ToolOutcome {
    let path = match validated_path(args) {
        Ok(path) => path,
        Err(e) => return ToolOutcome::error(e),
    };
    let cs = match resolve_cs(repo, arg_str(args, "cs")) {
        Ok(cs) => cs,
        Err(e) => return ToolOutcome::error(e),
    };
    // Pre-check the path so a blame of a missing path says so, instead of
    // the engine's generic not_found (which cannot name which of the two
    // was missing).
    let entry = match entry_at(repo, cs, &path) {
        Ok(entry) => entry,
        Err(e) => return ToolOutcome::error(err_text(e)),
    };
    let Some(entry) = entry else {
        return ToolOutcome::error(format!("no file at {path:?} in {}", cs.to_hex()));
    };
    if entry.kind != TreeKind::File {
        return ToolOutcome::error(format!("{path:?} is a directory in {}", cs.to_hex()));
    }
    if entry.mode == Mode::Gitlink {
        return ToolOutcome::text(format!(
            "submodule pointer at {path:?} in {} — a gitlink has no blameable \
             content in this garden (git errors on it; documented divergence)",
            cs.to_hex()
        ));
    }
    let ranges = match repo.blame(cs, &path) {
        Ok(ranges) => ranges,
        Err(e) => return ToolOutcome::error(err_text(e)),
    };
    if ranges.is_empty() {
        return ToolOutcome::text(format!(
            "no blameable lines for {path:?} in {} (the file is empty)",
            cs.to_hex()
        ));
    }
    let mut out = String::new();
    for (lo, hi, cs_id) in ranges {
        out.push_str(&format!("{lo}-{hi} {}\n", cs_id.to_hex()));
    }
    ToolOutcome::text(out)
}

// ------------------------------------------------------------------ file

/// `file`: `{cs?, path}` — the file's content at a changeset as one text
/// block. Symlinks render their target bytes (what `git show` prints);
/// submodule pointers are described instead of read (their pointer is not
/// a objects object); content that is not UTF-8 renders lossily with a
/// stderr note (v0.1, documented in docs/mcp.md).
pub fn tool_file(repo: &Repo, args: &serde_json::Value) -> ToolOutcome {
    let path = match validated_path(args) {
        Ok(path) => path,
        Err(e) => return ToolOutcome::error(e),
    };
    let cs = match resolve_cs(repo, arg_str(args, "cs")) {
        Ok(cs) => cs,
        Err(e) => return ToolOutcome::error(e),
    };
    let entry = match entry_at(repo, cs, &path) {
        Ok(entry) => entry,
        Err(e) => return ToolOutcome::error(err_text(e)),
    };
    let Some(entry) = entry else {
        return ToolOutcome::error(format!("no file at {path:?} in {}", cs.to_hex()));
    };
    if entry.kind != TreeKind::File {
        return ToolOutcome::error(format!("{path:?} is a directory in {}", cs.to_hex()));
    }
    if entry.mode == Mode::Gitlink {
        // The pointer rides in the blob field: 12 zero bytes padding a
        // 20-byte git SHA-1 (canonical spec §3). The upstream commit is
        // what an agent wants to know.
        let blob = entry.blob.expect("gitlink entries carry their pointer");
        let sha = blob.as_bytes()[12..]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        return ToolOutcome::text(format!(
            "submodule pointer at {path:?} in {}: upstream commit {sha}",
            cs.to_hex()
        ));
    }
    let Some(blob) = entry.blob else {
        return ToolOutcome::error(format!(
            "the tree entry at {path:?} carries no blob (server bug)"
        ));
    };
    let content = match repo.get_blob(blob) {
        Ok(Some(content)) => content,
        Ok(None) => {
            return ToolOutcome::error(format!(
                "blob {} is referenced but missing from objects",
                blob.to_hex()
            ));
        }
        Err(e) => return ToolOutcome::error(err_text(e)),
    };
    match String::from_utf8(content) {
        Ok(text) => ToolOutcome::text(text),
        Err(e) => {
            eprintln!(
                "gardenbed-mcp: file {path:?} is not valid UTF-8; answering a lossy rendering"
            );
            ToolOutcome::text(String::from_utf8_lossy(e.as_bytes()).into_owned())
        }
    }
}

// ------------------------------------------------------------------ copies

/// `copies`: `{cs}` (required) — the exact-blob copy/rename facts of one
/// changeset against its first parent, as pretty JSON:
/// `{"changeset": "<hex>", "entries": [{"path", "from", "kind"}]}`.
/// A changeset with no facts answers an empty `entries` array. Facts are
/// exact-blob only (no similarity heuristic; a documented divergence
/// from git's rename/copy detection).
pub fn tool_copies(repo: &Repo, args: &serde_json::Value) -> ToolOutcome {
    let Some(cs_arg) = arg_str(args, "cs") else {
        return ToolOutcome::error(
            "copies needs a \"cs\" argument — a 64-hex changeset id, a ref name, \
             or a changeset id prefix (8+ hex chars); give the changeset to list \
             the copies and renames of",
        );
    };
    let cs = match resolve_cs(repo, Some(cs_arg)) {
        Ok(cs) => cs,
        Err(e) => return ToolOutcome::error(e),
    };
    let cuttings = match repo.copies(cs) {
        Ok(c) => c,
        Err(e) => return ToolOutcome::error(err_text(e)),
    };
    let entries: Vec<serde_json::Value> = cuttings
        .entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "path": e.path,
                "from": e.src,
                "kind": match e.kind {
                    gardenbed_derive::CuttingKind::Rename => "rename",
                    gardenbed_derive::CuttingKind::Copy => "copy",
                },
            })
        })
        .collect();
    let out = serde_json::json!({
        "changeset": cs.to_hex(),
        "entries": entries,
    });
    match serde_json::to_string_pretty(&out) {
        Ok(text) => ToolOutcome::text(text),
        Err(e) => ToolOutcome::error(format!("copies could not render its answer: {e}")),
    }
}

#[cfg(test)]
mod prefix_tests {
    use super::*;
    use gardenbed_model::{Change, Changeset, Identity, blob_id};
    use gardenbed_server::Repo;
    use serde_json::json;
    use std::collections::BTreeMap;

    /// One Add/Modify change.
    fn chg(path: &str, op: Op, content: &[u8], mode: Mode) -> Change {
        Change {
            path: path.into(),
            op,
            blob: Some(blob_id(content)),
            src_path: None,
            mode,
        }
    }

    /// A two-commit fixture repo: root (a.txt, dir/b.txt) and a child that
    /// rewrites a.txt, published onto `main`.
    fn repo_fixture() -> (tempfile::TempDir, Repo, Id, Id) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::open(dir.path()).expect("repo opens");
        let identity = |secs: u64| Identity {
            name: "amy".into(),
            email: "amy@garden.example".into(),
            time_secs: 1_700_000_000 + secs,
            tz_min: 0,
        };
        let a: &[u8] = b"hello garden\n";
        let a2: &[u8] = b"hello garden v2\n";
        let world: &[u8] = b"world\n";
        let inline = |blobs: &[&[u8]]| {
            blobs
                .iter()
                .map(|b| (blob_id(b).to_hex(), b.to_vec()))
                .collect::<BTreeMap<_, _>>()
        };
        let root = Changeset {
            parents: vec![],
            changes: vec![
                chg("a.txt", Op::Add, a, Mode::Regular),
                chg("dir/b.txt", Op::Add, world, Mode::Regular),
            ],
            message: "root".into(),
            author: identity(10),
            committer: identity(10),
            signature: None,
        };
        let root_id = repo
            .create_changeset(&root, &inline(&[a, world]))
            .expect("root commits");
        let child = Changeset {
            parents: vec![root_id],
            changes: vec![chg("a.txt", Op::Modify, a2, Mode::Regular)],
            message: "update a".into(),
            author: identity(20),
            committer: identity(20),
            signature: None,
        };
        let child_id = repo
            .create_changeset(&child, &inline(&[a2]))
            .expect("child commits");
        repo.publish(child_id, None, false).expect("main published");
        (dir, repo, root_id, child_id)
    }

    /// Runs one tool call, asserting it succeeded.
    fn ok_tool(repo: &Repo, name: &str, args: &serde_json::Value) -> String {
        let out = call_tool(repo, name, args);
        assert!(!out.is_error, "{name} failed: {}", out.text);
        out.text
    }

    /// Runs one tool call, asserting it failed with `needle` in the text.
    fn err_tool(repo: &Repo, name: &str, args: &serde_json::Value, needle: &str) {
        let out = call_tool(repo, name, args);
        assert!(out.is_error, "{name} unexpectedly succeeded: {}", out.text);
        assert!(
            out.text.contains(needle),
            "want {needle:?} in: {}",
            out.text
        );
    }

    #[test]
    fn tools_answer_at_an_id_prefix() {
        let (_dir, repo, root, child) = repo_fixture();
        let p_root = &root.to_hex()[..8];
        let p_child = &child.to_hex()[..16]; // working copy's short() form

        // `file` at an 8-hex prefix.
        assert_eq!(
            ok_tool(&repo, "file", &json!({ "path": "a.txt", "cs": p_root })),
            "hello garden\n"
        );
        // `log` from a 16-hex short form lists the child first.
        let log = ok_tool(&repo, "log", &json!({ "cs": p_child, "limit": 2 }));
        assert!(log.contains(&child.to_hex()), "full ids printed: {log}");
        assert!(log.contains("update a"));
        // `diff` takes prefixes on both sides.
        assert_eq!(
            ok_tool(
                &repo,
                "diff",
                &json!({ "a": p_root, "b": &child.to_hex()[..8] })
            ),
            "M a.txt\n"
        );
        // `blame` through a prefix attributes the rewritten line to the child.
        assert_eq!(
            ok_tool(&repo, "blame", &json!({ "path": "a.txt", "cs": p_child })),
            format!("1-1 {}\n", child.to_hex())
        );
    }

    #[test]
    fn prefix_errors_are_agent_readable() {
        let (_dir, repo, _root, _child) = repo_fixture();

        // Too short: the resolver's minimum-length hint.
        err_tool(
            &repo,
            "file",
            &json!({ "path": "a.txt", "cs": "abc123" }),
            "changeset id prefix must be at least 8 hex chars",
        );
        // Well-formed but matching nothing: neither a ref nor a prefix.
        err_tool(
            &repo,
            "file",
            &json!({ "path": "a.txt", "cs": "1d050f877" }),
            "no ref or changeset matching 1d050f877",
        );
        // Non-hex names keep the ref-shaped message.
        err_tool(
            &repo,
            "log",
            &json!({ "cs": "no-such-ref" }),
            "ref \"no-such-ref\" does not exist on this server",
        );
        // A full 64-hex id passes through and is verified by the call.
        err_tool(
            &repo,
            "log",
            &json!({ "cs": Id::from_bytes([9; 32]).to_hex() }),
            "changeset",
        );
    }
}
