//! The scripted example agent (M7.3): a minimal MCP client that spawns
//! the `gardenbed-mcp` binary, speaks the stdio protocol hand-rolled in
//! [`crate::protocol`], and asks canned questions against a
//! harness-generated fixture whose ground truth is real git (the same
//! oracle the differential suites use).
//!
//! Two entry points:
//!
//! - [`run_demo`] — the first three canned questions, answers printed
//!   (the `examples/qa_agent.rs` walkthrough);
//! - [`run_canned_qa`] — the full ≥20-question verification suite
//!   ("who last touched line N of X", "what changed between A and B",
//!   "show file X at rev R", "list the last K commits touching path P"),
//!   100% correctness required. The integration test runs both — that is
//!   how the example is wired into CI.
//!
//! Every answer is computed by *composing the five tools* the way a real
//! agent would (blame → log to attribute an author; log → diff to filter
//! commits touching a path), never by reaching into gardenbed internals —
//! that is what makes the suite a fair proxy for a real MCP client.
//!
//! Ground truth comes from subprocess git: `git blame --first-parent`
//! (mapped through `Materialized.mapping`), `git diff-tree -r
//! --name-status`, `git show`, and `git log`. The fixture ingests the
//! generated history through the server's own `Repo` (the M4/M5
//! pattern), so the MCP server serves exactly the history git rendered.
//!
//! The agent's current directory is an empty sandbox asserted to contain
//! no `.bed` directory anywhere (the zero-checkout contract, v0.1.md
//! §M7.2) — all answers come from `--root` alone.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use gardenbed_harness::generator::{GenOptions, GeneratedHistory, files_at, generate};
use gardenbed_harness::materialize::{Materialized, materialize};
use gardenbed_harness::oracle::{git_blame_first_parent, git_diff_tree, git_log, git_show};
use gardenbed_model::{Id, Identity, Mode, blob_id};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::tools::render_log_entry;

// ------------------------------------------------------------------ fixture

/// One test world: a server root with an ingested synthetic history,
/// the *same* history materialized into a real git repository (the
/// oracle), and the id ⇄ sha mapping between them.
pub struct Fixture {
    /// Holds root and git; kept alive for the fixture's lifetime.
    _dir: TempDir,
    /// The server's data dir (`objects/` + `metadata.sqlite3`) — what
    /// `gardenbed-mcp --root` serves.
    pub root: PathBuf,
    /// The git oracle repository.
    pub git: PathBuf,
    /// The generated history.
    pub history: GeneratedHistory,
    /// `(changeset id, git sha)` aligned with `history.changesets`.
    pub mapping: Vec<(Id, String)>,
    sha_of: HashMap<Id, String>,
    cs_of: HashMap<String, Id>,
    index_of: HashMap<Id, usize>,
}

impl Fixture {
    /// Generates `commits` changesets, ingests them into a fresh temp
    /// server root, and materializes the same history into a git
    /// oracle repo. Panics on failure (fixture construction).
    pub fn build(seed: u64, commits: u32) -> Fixture {
        let opts = GenOptions {
            commits,
            crlf_content: true,
            empty_files: true,
            ..GenOptions::default()
        };
        let history = generate(seed, &opts);
        let dir = tempfile::tempdir().expect("fixture tempdir");
        let root = dir.path().join("server");
        let git = dir.path().join("git-oracle");
        ingest(&history, &root);
        let materialized = materialize(&history, &git).expect("git oracle materializes");
        Self::from_parts(dir, root, git, history, materialized)
    }

    fn from_parts(
        dir: TempDir,
        root: PathBuf,
        git: PathBuf,
        history: GeneratedHistory,
        materialized: Materialized,
    ) -> Fixture {
        let mut sha_of = HashMap::new();
        let mut cs_of = HashMap::new();
        let mut index_of = HashMap::new();
        for (i, (id, sha)) in materialized.mapping.iter().enumerate() {
            sha_of.insert(*id, sha.clone());
            cs_of.insert(sha.clone(), *id);
            index_of.insert(*id, i);
        }
        Fixture {
            _dir: dir,
            root,
            git,
            history,
            mapping: materialized.mapping,
            sha_of,
            cs_of,
            index_of,
        }
    }

    /// The head (last) changeset — the fixture's `main` tip.
    pub fn head(&self) -> Id {
        self.history.head
    }

    /// The head's git sha.
    pub fn head_sha(&self) -> String {
        self.sha_of[&self.history.head].clone()
    }

    /// The git sha of a changeset.
    pub fn sha_of(&self, cs: Id) -> String {
        self.sha_of[&cs].clone()
    }

    /// The changeset a git sha belongs to.
    pub fn cs_of(&self, sha: &str) -> Id {
        self.cs_of[sha]
    }

    /// The changeset's index in the generated history.
    pub fn cs_index(&self, cs: Id) -> usize {
        self.index_of[&cs]
    }

    /// The changeset at `index`.
    pub fn cs(&self, index: usize) -> &gardenbed_model::Changeset {
        &self.history.changesets[index]
    }

    /// A blob's raw content (the generator carries every blob it made).
    pub fn blob(&self, id: Id) -> &[u8] {
        self.history.blob(&id).expect("fixture blob")
    }

    /// The working files at changeset `index`, as `path → (blob, mode)`.
    pub fn files_at(&self, index: usize) -> BTreeMap<String, (Id, Mode)> {
        files_at(&self.history, index)
    }
}

/// A fixture whose history is guaranteed to exercise every question type:
/// at head it has at least one content file, one empty file, one symlink
/// and one exec file. Deterministic: the first qualifying seed in `1..`.
pub fn qualifying_fixture(commits: u32) -> Fixture {
    let opts = GenOptions {
        commits,
        crlf_content: true,
        empty_files: true,
        ..GenOptions::default()
    };
    for seed in 1..200u64 {
        let history = generate(seed, &opts);
        if qualifies(&history) {
            let dir = tempfile::tempdir().expect("fixture tempdir");
            let root = dir.path().join("server");
            let git = dir.path().join("git-oracle");
            ingest(&history, &root);
            let materialized = materialize(&history, &git).expect("git oracle materializes");
            return Fixture::from_parts(dir, root, git, history, materialized);
        }
    }
    panic!("no seed in 1..200 produced a history with content + empty + symlink + exec files");
}

/// Does the history's head carry the file shapes the QA suite asks about?
fn qualifies(history: &GeneratedHistory) -> bool {
    let files = files_at(history, history.changesets.len() - 1);
    let mut has_content = false;
    let mut has_empty = false;
    let mut has_symlink = false;
    let mut has_exec = false;
    for (id, mode) in files.values() {
        let empty = history.blob(id).unwrap().is_empty();
        match mode {
            Mode::Symlink => has_symlink = true,
            Mode::Exec if !empty => has_exec = true,
            _ if empty => has_empty = true,
            _ => has_content = true,
        }
    }
    has_content && has_empty && has_symlink && has_exec
}

/// True if `id` is a submodule pointer: 12 zero bytes followed by the
/// 20-byte upstream git SHA-1 (canonical spec §3) — such ids are never
/// objects objects and must not ride inline on ingestion.
fn is_gitlink_pointer(id: &Id, history: &GeneratedHistory) -> bool {
    id.as_bytes()[..12].iter().all(|b| *b == 0)
        && history.blob(id) == Some(id.as_bytes().as_slice())
}

/// Ingests a generated history into the repo rooted at `root` (the same
/// layout `gardenbed-mcp --root` serves): every changeset with its blobs
/// inline, then `main` fast-forwarded to the head. Runs through the
/// server library, exactly like the working copy test fixtures (the M4/M5
/// pattern). Returns the head changeset id.
pub fn ingest(history: &GeneratedHistory, root: &Path) -> Id {
    use gardenbed_server::Repo;
    let repo = Repo::open(root).expect("fixture repo opens");
    for cs in &history.changesets {
        let mut inline: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for change in &cs.changes {
            let Some(blob) = change.blob else { continue };
            if change.mode == Mode::Gitlink || is_gitlink_pointer(&blob, history) {
                continue; // opaque upstream pointer, never a objects object
            }
            let content = history
                .blob(&blob)
                .expect("generated history carries every blob it references")
                .to_vec();
            debug_assert_eq!(blob_id(&content), blob);
            inline.insert(blob.to_hex(), content);
        }
        repo.create_changeset(cs, &inline)
            .expect("generated changeset ingests cleanly");
    }
    repo.cas_ref("main", None, history.head)
        .expect("main ref created");
    history.head
}

// -------------------------------------------------------------- MCP process

/// One answer block of a `tools/call`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolAnswer {
    /// The text content block.
    pub text: String,
    /// Whether the tool reports a failure (still a result, never a
    /// protocol error).
    pub is_error: bool,
}

/// A spawned `gardenbed-mcp` process, spoken to strictly sequentially
/// (one request in, one response out — MCP stdio is sequential; the
/// server answers nothing for notifications, so the next non-empty line
/// is always the response to the last request).
pub struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    stderr_text: Arc<Mutex<String>>,
    next_id: i64,
}

impl McpProcess {
    /// Spawns `bin` serving `root`, with the agent's current directory at
    /// `cwd` (the zero-checkout sandbox).
    pub fn spawn(bin: &Path, root: &Path, cwd: &Path) -> std::io::Result<McpProcess> {
        let mut child = Command::new(bin)
            .arg("--root")
            .arg(root)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = std::io::BufReader::new(child.stdout.take().expect("stdout piped"));
        let stderr = child.stderr.take().expect("stderr piped");
        let stderr_text = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr_text);
        std::thread::spawn(move || {
            // Streamed line by line, so diagnostics are visible while the
            // server is still running (not only after it exits).
            let mut reader = std::io::BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match std::io::BufRead::read_line(&mut reader, &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => sink.lock().unwrap().push_str(&line),
                }
            }
        });
        Ok(McpProcess {
            child,
            stdin,
            stdout,
            stderr_text,
            next_id: 1,
        })
    }

    /// What the server said on stderr so far (diagnostics; never protocol).
    pub fn stderr(&self) -> String {
        self.stderr_text.lock().unwrap().clone()
    }

    fn send(&mut self, line: &str) -> Result<(), String> {
        use std::io::Write;
        writeln!(self.stdin, "{line}").map_err(|e| format!("write to the MCP server failed: {e}"))
    }

    /// Writes a notification (no response is expected or read).
    pub fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string())
    }

    /// Writes a request and reads the one response line for its id.
    /// `Err` carries the error object's message (or a framing failure).
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        use std::io::BufRead;
        let id = self.next_id;
        self.next_id += 1;
        self.send(
            &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
        )?;
        loop {
            let mut line = String::new();
            let n = self
                .stdout
                .read_line(&mut line)
                .map_err(|e| format!("read from the MCP server failed: {e}"))?;
            if n == 0 {
                return Err(format!(
                    "the MCP server closed stdout while awaiting a response to {method:?}\n\
                     stderr: {}",
                    self.stderr()
                ));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue; // transport noise
            }
            let value: Value = serde_json::from_str(trimmed)
                .map_err(|e| format!("the MCP server spoke non-JSON: {e}: {trimmed:?}"))?;
            if value.get("id") != Some(&json!(id)) {
                return Err(format!(
                    "the MCP server answered id {id} with a different id: {value}"
                ));
            }
            if let Some(error) = value.get("error") {
                return Err(format!("protocol error for {method:?}: {error}"));
            }
            return Ok(value
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }
    }

    /// Writes one raw line and reads the next non-empty stdout line as a
    /// JSON value — the protocol-conformance probe: unlike
    /// [`request`][Self::request] it surfaces error responses (`-32601`,
    /// `-32700`) and their `id` shapes instead of turning them into
    /// `Err`, and it fails if the server ever writes a non-JSON line
    /// (stdout must carry the protocol and nothing else).
    pub fn raw(&mut self, line: &str) -> Result<Value, String> {
        use std::io::BufRead;
        self.send(line)?;
        loop {
            let mut line = String::new();
            let n = self
                .stdout
                .read_line(&mut line)
                .map_err(|e| format!("read from the MCP server failed: {e}"))?;
            if n == 0 {
                return Err(format!(
                    "the MCP server closed stdout while awaiting a response\n stderr: {}",
                    self.stderr()
                ));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue; // transport noise
            }
            return serde_json::from_str(trimmed)
                .map_err(|e| format!("the MCP server spoke non-JSON: {e}: {trimmed:?}"));
        }
    }

    /// The `initialize` handshake plus `notifications/initialized`; the
    /// server's advertisement comes back.
    pub fn handshake(&mut self) -> Result<Value, String> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "gardenbed-qa-agent", "version": "0.1.0" },
            }),
        )?;
        self.notify("notifications/initialized", json!({}))?;
        Ok(result)
    }

    /// One `tools/call`, as the agent sees it: the text block and the
    /// error flag.
    pub fn call_tool(&mut self, name: &str, args: Value) -> Result<ToolAnswer, String> {
        let result = self.request("tools/call", json!({ "name": name, "arguments": args }))?;
        let content = result
            .get("content")
            .and_then(|c| c.as_array())
            .ok_or_else(|| format!("tools/call result carries no content array: {result}"))?;
        let text = content
            .first()
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
            .ok_or_else(|| format!("tools/call content[0] carries no text: {result}"))?;
        Ok(ToolAnswer {
            text: text.to_owned(),
            is_error: result
                .get("isError")
                .and_then(|b| b.as_bool())
                .unwrap_or(false),
        })
    }

    /// Closes stdin and waits for the server to exit, returning its exit
    /// status (EOF is a clean end — success expected).
    pub fn shutdown(mut self) -> std::io::Result<std::process::ExitStatus> {
        drop(self.stdin);
        self.child.wait()
    }
}

// ------------------------------------------------------------ scripted agent

/// The QA tally: every question is checked against ground truth computed
/// from git; failures carry a rendered diff.
#[derive(Debug, Default)]
pub struct QaReport {
    /// Questions answered correctly.
    pub passed: u32,
    /// Questions answered wrongly.
    pub failed: u32,
    /// Human-readable failure descriptions.
    pub failures: Vec<String>,
}

impl QaReport {
    /// A one-line assertion-friendly summary.
    pub fn summary(&self) -> String {
        format!(
            "{} passed, {} failed{}",
            self.passed,
            self.failed,
            if self.failures.is_empty() {
                String::new()
            } else {
                format!(":\n{}", self.failures.join("\n"))
            }
        )
    }
}

/// One parsed entry of the `log` tool's text, as an agent would see it.
struct LogEntryParsed {
    id: String,
    name: String,
    date: String,
    /// Message lines with the 4-space indent stripped.
    message: Vec<String>,
}

/// The runner: asks questions through one MCP session and checks answers
/// against ground truth.
struct Runner {
    mcp: McpProcess,
    report: QaReport,
    asked: usize,
    limit: usize,
}

impl Runner {
    fn count(&mut self) -> bool {
        if self.asked >= self.limit {
            return false;
        }
        self.asked += 1;
        true
    }

    /// One exact-match question, asked through the session.
    fn ask(&mut self, tool: &str, args: Value, question: &str, want: &str) {
        if !self.count() {
            return;
        }
        let got = match self.mcp.call_tool(tool, args) {
            Ok(answer) => answer.text,
            Err(e) => format!("<protocol failure: {e}>"),
        };
        self.judge_got(question, want, &got);
    }

    /// One exact-match question against a pre-computed tool answer (used
    /// for multi-tool compositions like blame → log or log → diff).
    fn judge(&mut self, question: &str, want: &str, got: &str) {
        if !self.count() {
            return;
        }
        self.judge_got(question, want, got);
    }

    /// One substring question (for answers whose exact shape legitimately
    /// varies — submodule pointers, empty-file blame notes).
    fn ask_contains(&mut self, tool: &str, args: Value, question: &str, needle: &str) {
        if !self.count() {
            return;
        }
        let got = match self.mcp.call_tool(tool, args) {
            Ok(answer) => answer.text,
            Err(e) => format!("<protocol failure: {e}>"),
        };
        if got.contains(needle) {
            self.transcript(question, &format!("ok: {}", summarize(&got)));
            self.report.passed += 1;
        } else {
            self.report.failures.push(format!(
                "Q: {question}\n   want substring: {needle:?}\n   got: {got:?}"
            ));
            self.transcript(
                question,
                &format!("FAIL: want substring {needle:?}, got {got:?}"),
            );
            self.report.failed += 1;
        }
    }

    /// One question that must answer `isError: true` with a message
    /// containing `needle` — the bad-input contract.
    fn ask_error(&mut self, tool: &str, args: Value, question: &str, needle: &str) {
        if !self.count() {
            return;
        }
        let (got, is_error) = match self.mcp.call_tool(tool, args) {
            Ok(answer) => (answer.text, answer.is_error),
            Err(e) => (format!("<protocol failure: {e}>"), false),
        };
        if is_error && got.contains(needle) {
            self.transcript(question, &format!("ok (error result): {}", summarize(&got)));
            self.report.passed += 1;
        } else {
            self.report.failures.push(format!(
                "Q: {question}\n   want isError with substring {needle:?}\n   got: {got:?} \
                 (isError: {is_error})"
            ));
            self.transcript(
                question,
                &format!("FAIL: want isError with {needle:?}, got {got:?} (isError {is_error})"),
            );
            self.report.failed += 1;
        }
    }

    fn judge_got(&mut self, question: &str, want: &str, got: &str) {
        if want == got {
            self.transcript(question, &format!("ok: {}", summarize(got)));
            self.report.passed += 1;
        } else {
            self.report.failures.push(format!(
                "Q: {question}\n   want: {want:?}\n   got:  {got:?}"
            ));
            self.transcript(question, &format!("FAIL: want {want:?}, got {got:?}"));
            self.report.failed += 1;
        }
    }

    fn transcript(&self, question: &str, verdict: &str) {
        println!("Q{:<3} {}", self.asked, question);
        println!("     {verdict}");
    }
}

/// A short single-line summary of an answer for the transcript.
fn summarize(text: &str) -> String {
    let one_line = text.replace('\n', " ⏎ ");
    let mut s: String = one_line.chars().take(160).collect();
    if one_line.chars().count() > 160 {
        s.push('…');
    }
    s
}

/// A short display form (first 8 hex) for question text only.
fn short(hex: &str) -> String {
    hex[..8].to_owned()
}

/// Lossily renders raw bytes as text, exactly as the `file` tool does.
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The number of lines in a file's content (a trailing newline does not
/// add a line).
fn line_count(content: &[u8]) -> usize {
    if content.is_empty() {
        return 0;
    }
    let mut n = content.iter().filter(|b| **b == b'\n').count();
    if !content.ends_with(b"\n") {
        n += 1;
    }
    n
}

/// Ground truth: the changeset git attributes to `line` (1-based) of
/// `path` at the head, via `git blame --first-parent --porcelain`,
/// mapped through `Materialized.mapping`.
fn blame_line_cs(fixture: &Fixture, path: &str, line: u64) -> Id {
    blame_line_cs_at(fixture, fixture.head(), path, line)
}

/// [`blame_line_cs`] at an explicit changeset.
fn blame_line_cs_at(fixture: &Fixture, cs: Id, path: &str, line: u64) -> Id {
    let sha = fixture.sha_of(cs);
    let ranges = git_blame_first_parent(&fixture.git, &sha, path)
        .unwrap_or_else(|e| panic!("git blame oracle works for {path}: {e}"));
    let range = ranges
        .iter()
        .find(|r| (r.lo as u64..=r.hi as u64).contains(&line))
        .unwrap_or_else(|| panic!("git blame covers line {line} of {path} at {sha}"));
    fixture.cs_of(&range.commit_sha)
}

/// The agent-side composition for the line-attribution question: call the
/// `blame` tool, read its `<lo>-<hi> <changeset>` ranges, and return the
/// changeset covering `line` — exactly what an MCP client without
/// gardenbed internals would do. `Err` only for protocol-level failures.
fn blame_line_via_tools(
    mcp: &mut McpProcess,
    path: &str,
    cs: Option<&str>,
    line: u64,
) -> Result<String, String> {
    let mut args = serde_json::json!({ "path": path });
    if let Some(cs) = cs {
        args["cs"] = serde_json::json!(cs);
    }
    let answer = mcp.call_tool("blame", args)?;
    if answer.is_error {
        return Err(format!("the blame tool errored: {}", answer.text));
    }
    for l in answer.text.lines() {
        let Some((range, hex)) = l.split_once(' ') else {
            continue;
        };
        let Some((lo, hi)) = range.split_once('-') else {
            continue;
        };
        if let (Ok(lo), Ok(hi)) = (lo.parse::<u64>(), hi.parse::<u64>())
            && (lo..=hi).contains(&line)
        {
            return Ok(hex.to_owned());
        }
    }
    // No range covers the line: an answer, but a wrong one — the judge
    // fails it against ground truth.
    Ok(String::new())
}

/// `git diff-tree -r -z --name-status --no-renames shaA shaB` — the tree
/// diff between two commits (per-path status letters; no rename pairs).
fn git_diff_tree_ab(git: &Path, sha_a: &str, sha_b: &str) -> Vec<(char, String)> {
    let out = std::process::Command::new("git")
        .current_dir(git)
        .args([
            "diff-tree",
            "--root",
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
            .unwrap_or_else(|| panic!("git diff-tree printed an empty status token: {status:?}"));
        entries.push((letter, path));
        i += 2;
    }
    entries
}

/// Ground truth for the `diff` tool's exact answer text between two
/// changesets: git's per-path letters (A/M/D/T — rename detection off on
/// both sides), or the tool's empty-diff sentence.
fn git_diff_answer(fixture: &Fixture, a: Id, b: Id) -> String {
    let entries = git_diff_tree_ab(&fixture.git, &fixture.sha_of(a), &fixture.sha_of(b));
    if entries.is_empty() {
        return format!("no changes between {} and {}", a.to_hex(), b.to_hex());
    }
    let mut out = String::new();
    for (letter, path) in entries {
        assert!(
            matches!(letter, 'A' | 'M' | 'D' | 'T'),
            "git produced letter {letter:?} with renames off — oracle drift"
        );
        out.push_str(&format!("{letter} {path}\n"));
    }
    out
}

/// Ground truth: the last `k` changesets (newest first) whose recorded
/// first-parent change set touches `path` — `git log` order filtered by
/// `git diff-tree` per commit (equivalent to `git log -- path` on the
/// fixture's first-parent chain, without history-simplification
/// ambiguity on merges).
fn commits_touching(fixture: &Fixture, path: &str, k: usize) -> Vec<Id> {
    let log = git_log(&fixture.git, &fixture.head_sha(), 10_000).expect("git log oracle");
    let mut out = Vec::new();
    for e in log {
        let touches = git_diff_tree(&fixture.git, &e.sha, false)
            .expect("git diff-tree oracle")
            .iter()
            .any(|d| d.path == path);
        if touches {
            out.push(fixture.cs_of(&e.sha));
            if out.len() == k {
                break;
            }
        }
    }
    out
}

/// The agent-side composition for the touching question: walk the log
/// newest-first and diff each entry against the one before it (on the
/// fixture's chain, consecutive log entries are consecutive first-parent
/// commits — committer times strictly increase along the chain).
fn touching_via_tools(
    mcp: &mut McpProcess,
    full_log: &[LogEntryParsed],
    path: &str,
    k: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    for w in full_log.windows(2) {
        let answer = match mcp.call_tool("diff", json!({ "a": w[1].id, "b": w[0].id })) {
            Ok(a) => a,
            Err(_) => return out, // a protocol failure truncates; the judge fails
        };
        if touches_path(&answer.text, path) {
            out.push(w[0].id.clone());
            if out.len() == k {
                break;
            }
        }
    }
    out
}

/// Does a diff-tool answer (letter/path lines) touch `path`?
fn touches_path(diff_text: &str, path: &str) -> bool {
    diff_text.lines().any(|l| match l.split_once(' ') {
        Some((letter, p)) => matches!(letter, "A" | "M" | "D" | "T") && p == path,
        None => false,
    })
}

/// Parses the `log` tool's rendered text into entries (an agent-side
/// read of the text the tool produces).
fn parse_log_text(text: &str) -> Vec<LogEntryParsed> {
    fn is_header(line: &str) -> bool {
        line.len() > 66
            && line.as_bytes()[..64].iter().all(|b| b.is_ascii_hexdigit())
            && &line[64..66] == "  "
    }
    fn author_name(rest: &str) -> String {
        // `Name <email>` — the name is everything before the last " <".
        match rest.rsplit_once(" <") {
            Some((name, _email)) => name.to_owned(),
            None => rest.to_owned(),
        }
    }
    let mut entries: Vec<LogEntryParsed> = Vec::new();
    for line in text.lines() {
        if is_header(line) {
            entries.push(LogEntryParsed {
                id: line[..64].to_owned(),
                name: author_name(&line[66..]),
                date: String::new(),
                message: Vec::new(),
            });
        } else if let Some(entry) = entries.last_mut() {
            if let Some(date) = line.strip_prefix("Date: ") {
                entry.date = date.to_owned();
            } else {
                entry
                    .message
                    .push(line.strip_prefix("    ").unwrap_or(line).to_owned());
            }
        }
    }
    entries
}

/// The `log` tool's parsed entries, fetched through the session.
fn log_entries(mcp: &mut McpProcess, args: Value) -> Result<Vec<LogEntryParsed>, String> {
    let answer = mcp.call_tool("log", args)?;
    if answer.is_error {
        return Err(format!("the log tool errored: {}", answer.text));
    }
    Ok(parse_log_text(&answer.text))
}

/// Ground truth for an exact `log` question: the first `count` entries of
/// git's log, rendered with the same entry renderer the tool uses (the
/// id comes from the mapping; the author identity from git; the author
/// *time* from the generated changeset — the same fields the
/// materializer wrote into git).
fn expected_log_text(
    fixture: &Fixture,
    git_entries: &[gardenbed_harness::oracle::LogEntry],
    count: usize,
) -> String {
    let mut out = String::new();
    for (i, e) in git_entries.iter().take(count).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let cs = fixture.cs_of(&e.sha);
        let generated = fixture.cs(fixture.cs_index(cs)).author.clone();
        let identity = Identity {
            name: e.author_name.clone(),
            email: e.author_email.clone(),
            time_secs: generated.time_secs,
            tz_min: generated.tz_min,
        };
        render_log_entry(cs, &identity, e.message.trim_end_matches('\n'), &mut out);
    }
    out
}

// ------------------------------------------------------------------ entries

/// The demo: three canned questions, printed. The example binary runs
/// this; the integration test runs it too (so the example's exact code
/// path is exercised) and asserts it.
pub fn run_demo(bin: &Path) -> QaReport {
    run_questions(bin, 3)
}

/// The full verification suite: every canned question below. 100%
/// correctness required (v0.1.md §M7.3).
pub fn run_canned_qa(bin: &Path) -> QaReport {
    run_questions(bin, usize::MAX)
}

/// Runs the first `limit` questions of the full scripted suite (the demo
/// is the suite's first three).
fn run_questions(bin: &Path, limit: usize) -> QaReport {
    let fixture = qualifying_fixture(40);
    let head = fixture.head();
    let head_sha = fixture.head_sha();
    let head_index = fixture.history.changesets.len() - 1;
    let root_cs = fixture.mapping[0].0;

    // The zero-checkout sandbox: the agent's cwd holds nothing at all.
    let sandbox = tempfile::tempdir().expect("sandbox tempdir");
    assert_no_state_dir(sandbox.path());

    let mcp = McpProcess::spawn(bin, &fixture.root, sandbox.path()).expect("gardenbed-mcp spawns");
    let mut r = Runner {
        mcp,
        report: QaReport::default(),
        asked: 0,
        limit,
    };
    let handshake = r.mcp.handshake().expect("initialize handshake");
    assert_eq!(
        handshake["protocolVersion"], "2025-06-18",
        "server negotiates the spec revision: {handshake}"
    );

    // The file inventory at head.
    let files = fixture.files_at(head_index);
    let blameable: Vec<(String, Vec<u8>)> = files
        .iter()
        .filter(|(_, (_, mode))| *mode != Mode::Gitlink)
        .map(|(p, (id, _))| (p.clone(), fixture.blob(*id).to_vec()))
        .collect();
    let content_files: Vec<String> = files
        .iter()
        .filter(|(_, (id, mode))| {
            *mode != Mode::Gitlink && *mode != Mode::Symlink && !fixture.blob(*id).is_empty()
        })
        .map(|(p, _)| p.clone())
        .collect();
    let empty_files: Vec<String> = files
        .iter()
        .filter(|(_, (id, mode))| *mode != Mode::Gitlink && fixture.blob(*id).is_empty())
        .map(|(p, _)| p.clone())
        .collect();
    let symlink_file: Option<String> = files
        .iter()
        .find(|(_, (_, mode))| *mode == Mode::Symlink)
        .map(|(p, _)| p.clone());
    let exec_file: Option<String> = files
        .iter()
        .find(|(_, (id, mode))| *mode == Mode::Exec && !fixture.blob(*id).is_empty())
        .map(|(p, _)| p.clone());
    let gitlink_file: Option<String> = files
        .iter()
        .find(|(_, (_, mode))| *mode == Mode::Gitlink)
        .map(|(p, _)| p.clone());
    let crlf_file: Option<String> = files
        .iter()
        .find(|(_, (id, mode))| {
            *mode == Mode::Regular && fixture.blob(*id).windows(2).any(|w| w == b"\r\n")
        })
        .map(|(p, _)| p.clone());

    // The full log, once (the agent composes later questions against it).
    let full_log = log_entries(&mut r.mcp, json!({ "limit": 0 })).expect("log(0) parses");

    // ------------------------------------------------- blame + authorship
    // "Which changeset last touched line N of P?" — ground truth: `git
    // blame --first-parent` mapped through Materialized.mapping. The
    // author question is then answered the way an agent must: blame, then
    // find the blamed changeset in `log` and read its author.
    let mut blamed = 0;
    for (path, content) in &blameable {
        let lines = line_count(content);
        if lines == 0 {
            continue;
        }
        let line = (lines / 2).max(1) as u64;
        let want_cs = blame_line_cs(&fixture, path, line);
        let got_cs = match blame_line_via_tools(&mut r.mcp, path, None, line) {
            Ok(hex) => hex,
            Err(e) => format!("<failure: {e}>"),
        };
        r.judge(
            &format!("Which changeset last touched line {line} of {path}?"),
            &want_cs.to_hex(),
            &got_cs,
        );
        let author = full_log
            .iter()
            .find(|e| e.id == want_cs.to_hex())
            .map(|e| e.name.clone())
            .unwrap_or_default();
        let want_author = fixture.cs(fixture.cs_index(want_cs)).author.name.clone();
        assert_eq!(
            author, want_author,
            "the log tool must carry the true author of {want_cs}"
        );
        r.judge(
            &format!("Who last touched line {line} of {path}? (blame + log)"),
            &want_author,
            &author,
        );
        blamed += 1;
        if blamed == 3 {
            break;
        }
    }

    // Blame at an explicit mid changeset (the `cs` argument, not the main
    // default): a content path that exists there, at its middle line.
    let mid_index = head_index / 2;
    let (mid_cs, mid_sha) = fixture.mapping[mid_index].clone();
    let mid_files = fixture.files_at(mid_index);
    let mid_content: Vec<(String, Id)> = mid_files
        .iter()
        .filter(|(_, (id, mode))| *mode != Mode::Gitlink && !fixture.blob(*id).is_empty())
        .map(|(p, (id, _))| (p.clone(), *id))
        .collect();
    if let Some((path, blob)) = mid_content.first() {
        let line = (line_count(fixture.blob(*blob)) / 2).max(1) as u64;
        let want_cs = blame_line_cs_at(&fixture, mid_cs, path, line);
        let got_cs = match blame_line_via_tools(&mut r.mcp, path, Some(&mid_cs.to_hex()), line) {
            Ok(hex) => hex,
            Err(e) => format!("<failure: {e}>"),
        };
        r.judge(
            &format!(
                "Which changeset last touched line {line} of {path} at {}?",
                short(&mid_cs.to_hex())
            ),
            &want_cs.to_hex(),
            &got_cs,
        );
    }

    // ------------------------------------------------------------- diff
    let n = fixture.history.changesets.len();
    for (ai, bi) in [(1usize, n - 1), (n / 2, n - 1), (2, n - 3)] {
        let (a, b) = (fixture.mapping[ai].0, fixture.mapping[bi].0);
        let want = git_diff_answer(&fixture, a, b);
        r.ask(
            "diff",
            json!({ "a": a.to_hex(), "b": b.to_hex() }),
            &format!(
                "What changed between changesets {} and {}?",
                short(&a.to_hex()),
                short(&b.to_hex())
            ),
            &want,
        );
    }
    r.ask(
        "diff",
        json!({ "a": head.to_hex(), "b": head.to_hex() }),
        "What changed between the head changeset and itself?",
        &format!("no changes between {} and {}", head.to_hex(), head.to_hex()),
    );
    let want_root_head = git_diff_answer(&fixture, root_cs, head);
    r.ask(
        "diff",
        json!({ "a": root_cs.to_hex(), "b": head.to_hex() }),
        "What changed between the first changeset and main?",
        &want_root_head,
    );

    // ------------------------------------------------------------- file
    if let Some(path) = content_files.first() {
        let want = lossy(&git_show(&fixture.git, &head_sha, path).expect("git show oracle"));
        r.ask(
            "file",
            json!({ "path": path }),
            &format!("Show {path} at main."),
            &want,
        );
    }
    if let Some((path, _)) = mid_content.first() {
        let want = lossy(&git_show(&fixture.git, &mid_sha, path).expect("git show oracle"));
        r.ask(
            "file",
            json!({ "path": path, "cs": mid_cs.to_hex() }),
            &format!("Show {path} at {}.", short(&mid_cs.to_hex())),
            &want,
        );
        // Reading the same content through the `main` ref name.
        let want_via_ref =
            lossy(&git_show(&fixture.git, &head_sha, path).expect("git show oracle"));
        r.ask(
            "file",
            json!({ "path": path, "cs": "main" }),
            &format!("Show {path} at the main ref."),
            &want_via_ref,
        );
    }
    if let Some(path) = &symlink_file {
        let want = lossy(&git_show(&fixture.git, &head_sha, path).expect("git show oracle"));
        r.ask(
            "file",
            json!({ "path": path }),
            &format!("Show the symlink {path} at main."),
            &want,
        );
    }
    if let Some(path) = &exec_file {
        let want = lossy(&git_show(&fixture.git, &head_sha, path).expect("git show oracle"));
        r.ask(
            "file",
            json!({ "path": path }),
            &format!("Show the executable {path} at main."),
            &want,
        );
    }
    if let Some(path) = &crlf_file {
        let want = lossy(&git_show(&fixture.git, &head_sha, path).expect("git show oracle"));
        r.ask(
            "file",
            json!({ "path": path }),
            &format!("Show {path} at main (CRLF file)."),
            &want,
        );
    }
    if let Some(path) = empty_files.first() {
        r.ask(
            "file",
            json!({ "path": path }),
            &format!("Show {path} at main (an empty file)."),
            "",
        );
    }
    if let Some(path) = &gitlink_file {
        r.ask_contains(
            "file",
            json!({ "path": path }),
            &format!("Show {path} at main (a submodule pointer)."),
            "submodule pointer",
        );
    }

    // -------------------------------------------------------------- log
    let git_log_all = git_log(&fixture.git, &head_sha, 10_000).expect("git log oracle");
    r.ask(
        "log",
        json!({ "limit": 5 }),
        "Show the last 5 commits on main.",
        &expected_log_text(&fixture, &git_log_all, 5),
    );
    // The default limit is 20 entries, newest first.
    let default_answer = r
        .mcp
        .call_tool("log", json!({}))
        .expect("log with defaults");
    let parsed = parse_log_text(&default_answer.text);
    r.judge(
        "How many commits does the log tool show by default?",
        "20",
        &parsed.len().to_string(),
    );
    if let (Some(newest), Some(want)) = (parsed.first(), git_log_all.first()) {
        r.judge(
            "Which changeset is the tip of main?",
            &fixture.cs_of(&want.sha).to_hex(),
            &newest.id,
        );
    }
    // Who wrote the most recent commit? (agent: log limit 1, read the author)
    let one = log_entries(&mut r.mcp, json!({ "limit": 1 })).expect("log(1) parses");
    r.judge(
        "Who wrote the most recent commit on main?",
        &git_log_all[0].author_name,
        one.first().map(|e| e.name.as_str()).unwrap_or(""),
    );

    // ------------------------------------------ commits touching a path
    // The agent composes: walk the log newest-first, diff each entry
    // against the one before, keep entries whose diff touches the path.
    let mut touching_qs = 0;
    'touching: for path in &content_files {
        for k in [3usize, 2] {
            let want = commits_touching(&fixture, path, k);
            // The composition cannot see the root changeset (it has no
            // log predecessor to diff against); only ask when the answer
            // does not need it.
            if want.len() != k || want.contains(&root_cs) {
                continue;
            }
            let want_text = want
                .iter()
                .map(|id| id.to_hex())
                .collect::<Vec<_>>()
                .join("\n");
            let got = touching_via_tools(&mut r.mcp, &full_log, path, k);
            r.judge(
                &format!("List the last {k} commits touching {path}."),
                &want_text,
                &got.join("\n"),
            );
            touching_qs += 1;
            if touching_qs == 2 {
                break 'touching;
            }
        }
    }
    assert!(
        touching_qs >= 1 || r.asked >= r.limit,
        "the fixture must support at least one touching question"
    );

    // -------------------------------------------------- second history
    // A second, smaller fixture over a second MCP process: crlf and empty
    // files, the error contract, and the bad-input contract.
    let fixture_b = qualifying_fixture(35);
    let head_b = fixture_b.head();
    let head_sha_b = fixture_b.head_sha();
    let head_index_b = fixture_b.history.changesets.len() - 1;
    let files_b = fixture_b.files_at(head_index_b);
    let blameable_b: Vec<(String, Vec<u8>)> = files_b
        .iter()
        .filter(|(_, (_, mode))| *mode != Mode::Gitlink)
        .map(|(p, (id, _))| (p.clone(), fixture_b.blob(*id).to_vec()))
        .collect();
    let empty_b: Vec<String> = files_b
        .iter()
        .filter(|(_, (id, mode))| *mode != Mode::Gitlink && fixture_b.blob(*id).is_empty())
        .map(|(p, _)| p.clone())
        .collect();
    let content_b: Vec<(String, Id)> = files_b
        .iter()
        .filter(|(_, (id, mode))| *mode != Mode::Gitlink && !fixture_b.blob(*id).is_empty())
        .map(|(p, (id, _))| (p.clone(), *id))
        .collect();
    let gitlink_b: Option<String> = files_b
        .iter()
        .find(|(_, (_, mode))| *mode == Mode::Gitlink)
        .map(|(p, _)| p.clone());
    let crlf_b: Option<String> = files_b
        .iter()
        .find(|(_, (id, mode))| {
            *mode == Mode::Regular && fixture_b.blob(*id).windows(2).any(|w| w == b"\r\n")
        })
        .map(|(p, _)| p.clone());

    let mcp_b =
        McpProcess::spawn(bin, &fixture_b.root, sandbox.path()).expect("gardenbed-mcp spawns");
    let mut b = Runner {
        mcp: mcp_b,
        report: QaReport::default(),
        asked: 0,
        limit,
    };
    b.mcp.handshake().expect("second initialize handshake");

    let full_log_b = log_entries(&mut b.mcp, json!({ "limit": 0 })).expect("log(0) parses");
    for (path, content) in blameable_b.iter().take(2) {
        let lines = line_count(content);
        if lines == 0 {
            continue;
        }
        let line = (lines / 2).max(1) as u64;
        let want_cs = blame_line_cs(&fixture_b, path, line);
        let got_cs = match blame_line_via_tools(&mut b.mcp, path, None, line) {
            Ok(hex) => hex,
            Err(e) => format!("<failure: {e}>"),
        };
        b.judge(
            &format!("Which changeset last touched line {line} of {path}? (second history)"),
            &want_cs.to_hex(),
            &got_cs,
        );
        let author = full_log_b
            .iter()
            .find(|e| e.id == want_cs.to_hex())
            .map(|e| e.name.clone())
            .unwrap_or_default();
        let want_author = fixture_b
            .cs(fixture_b.cs_index(want_cs))
            .author
            .name
            .clone();
        b.judge(
            &format!("Who last touched line {line} of {path}? (second history)"),
            &want_author,
            &author,
        );
    }
    if let Some(path) = empty_b.first() {
        b.ask(
            "file",
            json!({ "path": path }),
            &format!("Show {path} at main (an empty file)."),
            "",
        );
        b.ask_contains(
            "blame",
            json!({ "path": path }),
            &format!("Blame {path} at main (an empty file)."),
            "no blameable lines",
        );
    }
    if let Some(path) = &crlf_b {
        let want = lossy(&git_show(&fixture_b.git, &head_sha_b, path).expect("git show oracle"));
        b.ask(
            "file",
            json!({ "path": path }),
            &format!("Show {path} at main (CRLF file, second history)."),
            &want,
        );
    }
    if let Some(path) = &gitlink_b {
        b.ask_contains(
            "file",
            json!({ "path": path }),
            &format!("Show {path} at main (a submodule pointer)."),
            "submodule pointer",
        );
    }
    // A diff spanning the whole history, and a log slice from a mid cs.
    let root_b = fixture_b.mapping[0].0;
    let want_root_head_b = git_diff_answer(&fixture_b, root_b, head_b);
    b.ask(
        "diff",
        json!({ "a": root_b.to_hex(), "b": head_b.to_hex() }),
        "What changed between the first changeset and main? (second history)",
        &want_root_head_b,
    );
    let mid_index_b = head_index_b / 2;
    let mid_b = fixture_b.mapping[mid_index_b].clone();
    let mid_files_b = fixture_b.files_at(mid_index_b);
    let mid_content_b: Vec<(String, Id)> = mid_files_b
        .iter()
        .filter(|(_, (id, mode))| *mode != Mode::Gitlink && !fixture_b.blob(*id).is_empty())
        .map(|(p, (id, _))| (p.clone(), *id))
        .collect();
    let git_log_mid = git_log(&fixture_b.git, &mid_b.1, 3).expect("git log oracle");
    b.ask(
        "log",
        json!({ "cs": mid_b.0.to_hex(), "limit": 3 }),
        &format!(
            "Show the last 3 commits up to {}.",
            short(&mid_b.0.to_hex())
        ),
        &expected_log_text(&fixture_b, &git_log_mid, 3),
    );
    // The touching composition once more on the second history.
    'touching_b: for (path, _) in &content_b {
        let want = commits_touching(&fixture_b, path, 2);
        if want.len() != 2 || want.contains(&root_b) {
            continue;
        }
        let want_text = want
            .iter()
            .map(|id| id.to_hex())
            .collect::<Vec<_>>()
            .join("\n");
        let got = touching_via_tools(&mut b.mcp, &full_log_b, path, 2);
        b.judge(
            &format!("List the last 2 commits touching {path}. (second history)"),
            &want_text,
            &got.join("\n"),
        );
        break 'touching_b;
    }

    // ------------------------------------------------- the bad-input contract
    // Errors are isError *results* an agent can read and act on — the
    // protocol never crashes on bad input.
    if let Some((path, _)) = content_b.first() {
        b.ask_error(
            "file",
            json!({ "path": format!("{path}.no-such-file") }),
            "Show a file that does not exist.",
            "no file at",
        );
        b.ask(
            "file",
            json!({ "path": path, "cs": "no-such-ref" }),
            "Show a file at a ref that does not exist.",
            "ref \"no-such-ref\" does not exist on this server",
        );
        b.ask_error(
            "blame",
            json!({ "path": format!("{path}.no-such-file") }),
            "Blame a file that does not exist.",
            "no file at",
        );
    }
    b.ask_error(
        "file",
        json!({ "path": "/absolute/path.txt" }),
        "Show a file by an absolute path.",
        "invalid path",
    );
    b.ask_error(
        "blame",
        json!({ "path": "dir/../escape.txt" }),
        "Blame a path that escapes upward.",
        "invalid path",
    );
    b.ask_error(
        "diff",
        json!({ "a": head_b.to_hex() }),
        "Diff with only one side given.",
        "diff needs two changesets",
    );
    // A 64-character token that is not hex: the resolver sees the length,
    // tries to parse it as an id, and refuses.
    b.ask_error(
        "log",
        json!({ "cs": "z".repeat(64) }),
        "Log from a 64-character token that is not hex.",
        "not valid hex",
    );
    // Uppercase hex is accepted (canonical spec §1: parsers accept any
    // case, display is lowercase) — asked through the blame composition
    // at a path that exists at that changeset.
    if let Some((path, blob)) = mid_content_b.first() {
        let line = (line_count(fixture_b.blob(*blob)) / 2).max(1) as u64;
        let want_cs = blame_line_cs_at(&fixture_b, mid_b.0, path, line);
        let upper = mid_b.0.to_hex().to_uppercase();
        let got_cs = match blame_line_via_tools(&mut b.mcp, path, Some(&upper), line) {
            Ok(hex) => hex,
            Err(e) => format!("<failure: {e}>"),
        };
        b.judge(
            &format!(
                "Blame with an UPPERCASE changeset id: which changeset last \
                 touched line {line} of {path}?"
            ),
            &want_cs.to_hex(),
            &got_cs,
        );
    }

    let _ = b.mcp.shutdown();
    let _ = r.mcp.shutdown();

    let mut report = std::mem::take(&mut r.report);
    report.passed += b.report.passed;
    report.failed += b.report.failed;
    report.failures.extend(b.report.failures);
    report
}

// --------------------------------------------------------- workspace helpers

/// Locates a built workspace binary. Order: the env override (`MCP_BIN`
/// for `gardenbed-mcp`, following the working copy tests' `BED_SERVER_BIN`
/// pattern), then the workspace `target/{debug,release}` directory.
/// Panics with actionable advice when absent.
pub fn probe_workspace_bin(name: &str, env_override: &str) -> PathBuf {
    if let Ok(path) = std::env::var(env_override) {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "{env_override} points at {path:?}, not a file"
        );
        return path;
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // crates/mcp
    let workspace_target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("..").join("..").join("target"));
    let profiles: &[&str] = if cfg!(debug_assertions) {
        &["debug", "release"]
    } else {
        &["release", "debug"]
    };
    for profile in profiles {
        let candidate = workspace_target.join(profile).join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "binary {name} not found under {} — run `cargo build --workspace` \
         first (or set {env_override})",
        workspace_target.display()
    )
}

/// Asserts the zero-checkout contract for one directory tree: no `.bed`
/// directory anywhere beneath it.
pub fn assert_no_state_dir(dir: &Path) {
    fn walk(dir: &Path) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.expect("directory entry readable");
            if entry.file_name() == ".bed" {
                panic!(
                    "the zero-checkout contract is violated: found a .bed \
                     directory at {}",
                    entry.path().display()
                );
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                walk(&entry.path());
            }
        }
    }
    walk(dir);
}
