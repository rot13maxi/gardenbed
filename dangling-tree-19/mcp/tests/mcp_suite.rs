//! The M7 verification suite for the MCP surface (v0.1.md §M7.1–M7.4),
//! against the real `gardenbed-mcp` binary and the real fixture machinery:
//!
//! - **M7.1 parity** — every MCP answer equals the underlying
//!   `gardenbed_server::Repo` answer, and where `bed` renders the same
//!   shape (`log` entries, `file` bytes, diff letters) the rendering is
//!   byte-equal — with the one documented divergence: the MCP log prints
//!   the **full 64-hex id** (bed prints the 16-hex short form), because an
//!   agent has no bed to resolve shorts with. Diff letters are additionally
//!   checked against `git diff-tree --name-status`, blame against
//!   `git blame --first-parent`, `file` against `git show`.
//! - **M7.2 zero-checkout** — the agent's sandbox (and the fixture tree)
//!   contains no `.bed` directory before *or after* serving; all four
//!   tools answer from `--root` alone.
//! - **M7.3 scripted QA agent** — the demo (the example's exact code
//!   path) and the full ≥20-question canned suite pass at 100% against
//!   the git oracle.
//! - **M7.4 protocol conformance** — initialize negotiation, the silent
//!   `notifications/initialized`, `ping`, `tools/list` shape,
//!   `tools/call` roundtrips, `-32601` for unknown methods, `-32700` for
//!   malformed lines with the server **keeps serving** afterwards, and
//!   stdout carrying nothing but protocol lines.

use std::path::{Path, PathBuf};

use gardenbed::render::{self as bed_render, LogEntry as BedLogEntry};
use gardenbed_harness::oracle::{git_blame_first_parent, git_show};
use gardenbed_mcp::qa::{McpProcess, assert_no_bed_dir, run_canned_qa, run_demo};
use gardenbed_mcp::{handle_line, protocol};
use gardenbed_model::Mode;
use gardenbed_server::Repo;
use serde_json::{Value, json};

/// The built `gardenbed-mcp` binary (this package's own bin target).
fn mcp_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gardenbed-mcp"))
}

/// Parses one rendered protocol line.
fn resp(line: Option<String>) -> Value {
    serde_json::from_str(&line.expect("a response line")).expect("the protocol line is JSON")
}

/// The JSON-RPC error code of a parsed response.
fn code(v: &Value) -> i64 {
    v["error"]["code"].as_i64().expect("an error code")
}

/// The 64-hex id headers of a `log` tool answer (an agent-side parse).
fn log_ids(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| {
            l.len() > 66
                && l.as_bytes()[..64].iter().all(|b| b.is_ascii_hexdigit())
                && &l[64..66] == "  "
        })
        .map(|l| l[..64].to_owned())
        .collect()
}

/// `git diff-tree -r -z --name-status --no-renames shaA shaB` — the
/// tree-diff oracle for the `diff` tool (per-path status letters, no
/// rename pairs).
fn git_diff_letters(git: &Path, sha_a: &str, sha_b: &str) -> Vec<(char, String)> {
    let out = std::process::Command::new("git")
        .current_dir(git)
        .args([
            "diff-tree",
            "-r",
            "-z",
            "--name-status",
            "--no-renames",
            "--no-commit-id",
            sha_a,
            sha_b,
        ])
        .output()
        .expect("git runs")
        .stdout;
    let toks: Vec<&[u8]> = out.split(|b| *b == 0).collect();
    let mut entries = Vec::new();
    let mut i = 0;
    while i + 1 < toks.len() {
        let status = String::from_utf8_lossy(toks[i]).into_owned();
        let path = String::from_utf8_lossy(toks[i + 1]).into_owned();
        if status.is_empty() && path.is_empty() {
            break; // the trailing NUL's empty tail
        }
        let letter = status
            .chars()
            .next()
            .unwrap_or_else(|| panic!("git diff-tree printed an empty status: {status:?}"));
        entries.push((letter, path));
        i += 2;
    }
    entries
}

// ---------------------------------------------------------------- M7.4 (in process)

#[test]
fn m7_4_protocol_conformance_in_process() {
    let fixture = gardenbed_mcp::qa::Fixture::build(3, 6);
    let repo = Repo::open(&fixture.root).expect("fixture repo opens");

    // initialize: the client's protocolVersion is echoed, capabilities and
    // serverInfo advertised.
    let init = resp(handle_line(
        &repo,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}"#,
    ));
    assert_eq!(init["id"], json!(1));
    assert_eq!(init["result"]["protocolVersion"], json!("2024-11-05"));
    assert_eq!(init["result"]["capabilities"]["tools"], json!({}));
    assert_eq!(init["result"]["serverInfo"]["name"], json!("gardenbed-mcp"));
    assert!(init.get("error").is_none());

    // initialize without a version: the spec revision gardenbed-mcp serves.
    let init = resp(handle_line(
        &repo,
        r#"{"jsonrpc":"2.0","id":"a","method":"initialize"}"#,
    ));
    assert_eq!(init["id"], json!("a"));
    assert_eq!(
        init["result"]["protocolVersion"],
        json!(protocol::SPEC_PROTOCOL_VERSION)
    );

    // Notifications are never answered.
    assert_eq!(
        handle_line(
            &repo,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        ),
        None
    );
    assert_eq!(
        handle_line(
            &repo,
            r#"{"jsonrpc":"2.0","id":null,"method":"notifications/initialized"}"#
        ),
        None
    );

    // ping answers an empty result.
    let ping = resp(handle_line(
        &repo,
        r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
    ));
    assert_eq!(ping["id"], json!(2));
    assert_eq!(ping["result"], json!({}));

    // tools/list: exactly the four tools, each with an object inputSchema;
    // diff requires both sides (a headless tool has no working copy).
    let list = resp(handle_line(
        &repo,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
    ));
    let tools = list["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["log", "diff", "blame", "file"]);
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], json!("object"), "for {t}");
        assert!(t["description"].is_string(), "for {t}");
    }
    let diff = tools.iter().find(|t| t["name"] == "diff").unwrap();
    assert_eq!(diff["inputSchema"]["required"], json!(["a", "b"]));
    let blame = tools.iter().find(|t| t["name"] == "blame").unwrap();
    assert_eq!(blame["inputSchema"]["required"], json!(["path"]));
    let log = tools.iter().find(|t| t["name"] == "log").unwrap();
    assert!(log["inputSchema"]["properties"]["limit"].is_object());

    // tools/call roundtrip: one text content block, no isError on success.
    let head = fixture.head();
    let call = json!({
        "jsonrpc": "2.0", "id": 4,
        "method": "tools/call",
        "params": { "name": "log", "arguments": { "limit": 1 } },
    });
    let answer = resp(handle_line(&repo, &call.to_string()));
    assert_eq!(answer["id"], json!(4));
    assert!(answer.get("error").is_none());
    assert_eq!(answer["result"]["content"][0]["type"], json!("text"));
    assert!(answer["result"]["content"][0]["text"].is_string());
    assert!(answer["result"].get("isError").is_none());

    // A tool-level failure is still a result — isError: true, protocol up.
    let call = json!({
        "jsonrpc": "2.0", "id": 5,
        "method": "tools/call",
        "params": { "name": "diff", "arguments": { "a": head.to_hex() } },
    });
    let answer = resp(handle_line(&repo, &call.to_string()));
    assert!(answer.get("error").is_none(), "tool errors are results");
    assert_eq!(answer["result"]["isError"], json!(true));
    assert!(
        answer["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("diff needs two changesets")
    );

    // Unknown tool names are isError results too.
    let call = json!({
        "jsonrpc": "2.0", "id": 6,
        "method": "tools/call",
        "params": { "name": "merge", "arguments": {} },
    });
    let answer = resp(handle_line(&repo, &call.to_string()));
    assert_eq!(answer["result"]["isError"], json!(true));

    // Structurally broken calls are protocol errors: -32602.
    let call = json!({
        "jsonrpc": "2.0", "id": 7,
        "method": "tools/call",
        "params": { "arguments": {} },
    });
    let answer = resp(handle_line(&repo, &call.to_string()));
    assert_eq!(code(&answer), protocol::INVALID_PARAMS);
    let call = json!({
        "jsonrpc": "2.0", "id": 8,
        "method": "tools/call",
        "params": { "name": 9, "arguments": {} },
    });
    let answer = resp(handle_line(&repo, &call.to_string()));
    assert_eq!(code(&answer), protocol::INVALID_PARAMS);

    // Unknown methods: -32601, id echoed.
    let answer = resp(handle_line(
        &repo,
        r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#,
    ));
    assert_eq!(answer["id"], json!(9));
    assert_eq!(code(&answer), protocol::METHOD_NOT_FOUND);

    // Malformed lines: -32700 bound to id null — and the server keeps
    // serving (the very next request is answered).
    let answer = resp(handle_line(&repo, "{oops"));
    assert_eq!(answer["id"], json!(null));
    assert_eq!(code(&answer), protocol::PARSE_ERROR);
    let ping = resp(handle_line(
        &repo,
        r#"{"jsonrpc":"2.0","id":10,"method":"ping"}"#,
    ));
    assert_eq!(ping["result"], json!({}));

    // A JSON-RPC batch is not part of the served contract: -32600.
    let answer = resp(handle_line(
        &repo,
        r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
    ));
    assert_eq!(code(&answer), protocol::INVALID_REQUEST);
}

// ---------------------------------------------------------------- M7.4 (subprocess)

#[test]
fn m7_4_protocol_conformance_over_stdio() {
    let fixture = gardenbed_mcp::qa::Fixture::build(4, 6);
    let sandbox = tempfile::tempdir().expect("sandbox tempdir");
    assert_no_bed_dir(sandbox.path());

    let mut mcp =
        McpProcess::spawn(&mcp_bin(), &fixture.root, sandbox.path()).expect("gardenbed-mcp spawns");

    // The handshake: version negotiation + the silent initialized
    // notification (request ids are sequential; the next response is the
    // ping's, proving the notification was never answered).
    let init = mcp.handshake().expect("handshake");
    assert_eq!(init["protocolVersion"], json!("2025-06-18"));
    assert_eq!(init["serverInfo"]["name"], json!("gardenbed-mcp"));
    let ping = mcp
        .raw(r#"{"jsonrpc":"2.0","id":"string-ids-too","method":"ping"}"#)
        .expect("ping answered");
    assert_eq!(ping["id"], json!("string-ids-too"));
    assert_eq!(ping["result"], json!({}));

    // Unknown method: -32601 with the id echoed.
    let answer = mcp
        .raw(r#"{"jsonrpc":"2.0","id":"u1","method":"resources/read","params":{"uri":"x"}}"#)
        .expect("unknown method answered");
    assert_eq!(answer["id"], json!("u1"));
    assert_eq!(code(&answer), protocol::METHOD_NOT_FOUND);

    // Malformed line: -32700 bound to id null; the server keeps serving.
    let answer = mcp.raw("{oops").expect("malformed line answered");
    assert_eq!(answer["id"], json!(null));
    assert_eq!(code(&answer), protocol::PARSE_ERROR);
    let answer = mcp
        .raw(r#"{"jsonrpc":"2.0","id":77,"method":"ping"}"#)
        .expect("the server keeps serving after a malformed line");
    assert_eq!(answer["id"], json!(77));
    assert_eq!(answer["result"], json!({}));

    // stdout carried only protocol lines (raw parses every line it
    // reads); diagnostics went to stderr.
    assert!(
        mcp.stderr().contains("gardenbed-mcp"),
        "diagnostics belong on stderr: {:?}",
        mcp.stderr()
    );

    // EOF is a clean end: exit success.
    let status = mcp.shutdown().expect("wait for the server");
    assert!(
        status.success(),
        "EOF must end the server cleanly: {status}"
    );

    // The sandbox is still empty — the server never wrote a checkout.
    assert_no_bed_dir(sandbox.path());
}

// ---------------------------------------------------------------- M7.1 parity

#[test]
fn m7_1_parity_with_repo_engine_and_bed() {
    let fixture = gardenbed_mcp::qa::qualifying_fixture(12);
    let repo = Repo::open(&fixture.root).expect("fixture repo opens");
    let head = fixture.head();
    let head_index = fixture.history.changesets.len() - 1;
    let head_sha = fixture.head_sha();

    let sandbox = tempfile::tempdir().expect("sandbox tempdir");
    assert_no_bed_dir(sandbox.path());
    assert_no_bed_dir(&fixture.root);
    assert_no_bed_dir(&fixture.git);

    let mut mcp =
        McpProcess::spawn(&mcp_bin(), &fixture.root, sandbox.path()).expect("gardenbed-mcp spawns");
    mcp.handshake().expect("handshake");

    let files = fixture.files_at(head_index);

    // ---- log: the id sequence equals Repo::log's, and the text equals
    // bed's renderer byte for byte — after widening bed's 16-hex id
    // column to the documented full 64-hex MCP form.
    let repo_log = repo.log(head, 0).expect("repo log");
    let answer = mcp
        .call_tool("log", json!({ "limit": 0 }))
        .expect("log call");
    assert!(!answer.is_error);
    let want_ids: Vec<String> = repo_log.iter().map(|(id, _)| id.to_hex()).collect();
    assert_eq!(
        log_ids(&answer.text),
        want_ids,
        "MCP log order == Repo::log order"
    );
    let mut want = String::new();
    for (i, (id, cs)) in repo_log.iter().enumerate() {
        if i > 0 {
            want.push('\n');
        }
        let bed_entry = BedLogEntry {
            id: *id,
            author_name: cs.author.name.clone(),
            author_email: cs.author.email.clone(),
            time_secs: cs.author.time_secs,
            tz_min: cs.author.tz_min,
            message: cs.message.clone(),
        };
        let mut entry = String::new();
        bed_render::render_log_entry(&bed_entry, &mut entry);
        entry.replace_range(0..16, &id.to_hex()); // the documented divergence
        want.push_str(&entry);
    }
    assert_eq!(
        answer.text, want,
        "MCP log text == bed's rendering (64-hex ids)"
    );

    // ---- diff: letters == git diff-tree --name-status; paths == Repo::diff.
    let n = fixture.history.changesets.len();
    for (ai, bi) in [(0usize, n - 1), (1, 2), (n / 2, n - 1), (n - 1, n - 1)] {
        let (a, b) = (fixture.mapping[ai].0, fixture.mapping[bi].0);
        let answer = mcp
            .call_tool("diff", json!({ "a": a.to_hex(), "b": b.to_hex() }))
            .expect("diff call");
        let letters = git_diff_letters(&fixture.git, &fixture.sha_of(a), &fixture.sha_of(b));
        let want = if letters.is_empty() {
            format!("no changes between {} and {}", a.to_hex(), b.to_hex())
        } else {
            letters
                .iter()
                .map(|(l, p)| format!("{l} {p}\n"))
                .collect::<String>()
        };
        assert_eq!(answer.text, want, "MCP diff {ai}→{bi} == git's tree diff");
        let changes = repo.diff(a, b).expect("repo diff");
        let got_paths: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
        let want_paths: Vec<String> = letters.iter().map(|(_, p)| p.clone()).collect();
        assert_eq!(
            got_paths, want_paths,
            "Repo::diff paths track git's for {ai}→{bi}"
        );
    }

    // ---- blame: the wire text == Repo::blame's ranges, and every range
    // matches `git blame --first-parent` through the id ⇄ sha mapping.
    let blameable: Vec<(String, gardenbed_model::Id)> = files
        .iter()
        .filter(|(_, (_, mode))| *mode != Mode::Gitlink)
        .map(|(p, (id, _))| (p.clone(), *id))
        .collect();
    assert!(
        !blameable.is_empty(),
        "the fixture has a blameable file at head"
    );
    for (path, _) in blameable.iter().take(2) {
        let answer = mcp
            .call_tool("blame", json!({ "path": path }))
            .expect("blame call");
        let ranges = repo.blame(head, path).expect("repo blame");
        let want: String = ranges
            .iter()
            .map(|(lo, hi, cs)| format!("{lo}-{hi} {}\n", cs.to_hex()))
            .collect();
        assert_eq!(
            answer.text, want,
            "MCP blame text == Repo::blame for {path}"
        );
        let oracle =
            git_blame_first_parent(&fixture.git, &head_sha, path).expect("git blame oracle");
        let want_git: String = oracle
            .iter()
            .map(|r| {
                format!(
                    "{}-{} {}\n",
                    r.lo,
                    r.hi,
                    fixture.cs_of(&r.commit_sha).to_hex()
                )
            })
            .collect();
        assert_eq!(
            answer.text, want_git,
            "MCP blame == git blame --first-parent for {path}"
        );
    }

    // ---- file: text == the repo's blob bytes == `git show` bytes, for a
    // content file, a symlink (target bytes), and an empty file.
    let content = files
        .iter()
        .find(|(_, (id, mode))| {
            *mode != Mode::Gitlink && *mode != Mode::Symlink && !fixture.blob(*id).is_empty()
        })
        .map(|(p, (id, _))| (p.clone(), *id))
        .expect("the fixture has a content file at head");
    let answer = mcp
        .call_tool("file", json!({ "path": content.0 }))
        .expect("file call");
    let blob = repo
        .get_blob(content.1)
        .expect("blob readable")
        .expect("blob exists");
    assert_eq!(
        answer.text,
        String::from_utf8_lossy(&blob),
        "MCP file == Repo bytes"
    );
    let shown = git_show(&fixture.git, &head_sha, &content.0).expect("git show");
    let want = String::from_utf8_lossy(&shown);
    assert_eq!(answer.text, want, "MCP file == git show for {}", content.0);

    if let Some((path, _)) = files.iter().find(|(_, (_, mode))| *mode == Mode::Symlink) {
        let answer = mcp
            .call_tool("file", json!({ "path": path }))
            .expect("file call");
        let shown = git_show(&fixture.git, &head_sha, path).expect("git show");
        let want = String::from_utf8_lossy(&shown);
        assert_eq!(answer.text, want, "MCP file == git show for symlink {path}");
    }
    let empty = files
        .iter()
        .find(|(_, (id, mode))| *mode != Mode::Gitlink && fixture.blob(*id).is_empty())
        .map(|(p, _)| p.clone());
    if let Some(path) = empty {
        let answer = mcp
            .call_tool("file", json!({ "path": path }))
            .expect("file call");
        assert_eq!(answer.text, "", "an empty file answers empty text");
    }

    let _ = mcp.shutdown().expect("server exits");
}

// ---------------------------------------------------------------- M7.2 zero checkout

#[test]
fn m7_2_zero_checkout_all_tools_from_root_alone() {
    let fixture = gardenbed_mcp::qa::qualifying_fixture(10);
    let head = fixture.head();
    let head_index = fixture.history.changesets.len() - 1;
    let files = fixture.files_at(head_index);

    // The agent's sandbox: an empty directory, asserted checkout-free
    // before the server even starts.
    let sandbox = tempfile::tempdir().expect("sandbox tempdir");
    assert_no_bed_dir(sandbox.path());
    assert_no_bed_dir(&fixture.root);
    assert_no_bed_dir(&fixture.git);

    let mut mcp =
        McpProcess::spawn(&mcp_bin(), &fixture.root, sandbox.path()).expect("gardenbed-mcp spawns");
    mcp.handshake().expect("handshake");

    // All four tools answer — with the cwd holding nothing at all.
    let log = mcp
        .call_tool("log", json!({ "limit": 2 }))
        .expect("log call");
    assert!(!log.is_error);
    let (a, b) = (fixture.mapping[0].0, head);
    let diff = mcp
        .call_tool("diff", json!({ "a": a.to_hex(), "b": b.to_hex() }))
        .expect("diff call");
    assert!(!diff.is_error);
    let blameable = files
        .iter()
        .find(|(_, (_, mode))| *mode != Mode::Gitlink)
        .expect("a blameable file at head");
    let blame = mcp
        .call_tool("blame", json!({ "path": blameable.0 }))
        .expect("blame call");
    assert!(!blame.is_error, "blame: {}", blame.text);
    let content = files
        .iter()
        .find(|(_, (id, mode))| *mode != Mode::Gitlink && !fixture.blob(*id).is_empty())
        .expect("a content file at head");
    let file = mcp
        .call_tool("file", json!({ "path": content.0 }))
        .expect("file call");
    assert!(!file.is_error, "file: {}", file.text);

    // And nothing appeared anywhere: no checkout was made to answer.
    assert_no_bed_dir(sandbox.path());
    assert_no_bed_dir(&fixture.root);
    assert_no_bed_dir(&fixture.git);

    let _ = mcp.shutdown().expect("server exits");
}

// ---------------------------------------------------------------- M7.3 canned QA

#[test]
fn m7_3_scripted_qa_agent_answers_like_git() {
    let bin = mcp_bin();

    // The example's exact code path: the first three questions, printed.
    let demo = run_demo(&bin);
    assert_eq!(demo.failed, 0, "demo: {}", demo.summary());

    // The full suite: at least 20 questions, every one correct.
    let report = run_canned_qa(&bin);
    assert!(
        report.passed + report.failed >= 20,
        "the suite must ask at least 20 questions: {}",
        report.summary()
    );
    assert_eq!(report.failed, 0, "QA: {}", report.summary());
}
