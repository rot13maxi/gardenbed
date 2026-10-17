//! `gardenbed-mcp` — the binary. Parses `--root` (the server's data
//! dir) and the accepted `--repo` name (single-repo in v0.1), opens the
//! server's [`gardenbed_server::Repo`] in process, and serves the MCP
//! stdio protocol: one JSON-RPC message per stdin line, one response per
//! request, stdout for nothing but the protocol.
//!
//! Diagnostics go to stderr. EOF on stdin ends the server cleanly
//! (exit 0); a repo that will not open exits 1 before the loop starts.

use std::io::Write;

use clap::Parser;

use gardenbed_server::Repo;

/// Command line for the Gardenbed MCP server.
#[derive(Parser, Debug)]
#[command(
    name = "gardenbed-mcp",
    version,
    about = "Query repository history over MCP without a working copy"
)]
struct Args {
    /// The server's data directory (its `objects/` object store and
    /// `metadata.sqlite3` metadata database).
    #[arg(long, value_name = "DIR")]
    root: std::path::PathBuf,

    /// Repo name; accepted and validated for forward compatibility
    /// (v0.1 servers are single-repo; the API carries a repo field
    /// from day one, docs/v0.1.md §1).
    #[arg(long, value_name = "NAME")]
    repo: Option<String>,
}

fn main() {
    let args = Args::parse();
    let repo_name = match &args.repo {
        Some(name) if !name.trim().is_empty() => name.trim().to_owned(),
        Some(_) => {
            eprintln!("gardenbed-mcp: --repo must not be empty");
            std::process::exit(1);
        }
        None => "default".to_owned(),
    };
    let repo = match Repo::open(&args.root) {
        Ok(repo) => repo,
        Err(e) => {
            eprintln!(
                "gardenbed-mcp: cannot open repository at {}: {e}",
                args.root.display()
            );
            std::process::exit(1);
        }
    };
    eprintln!(
        "gardenbed-mcp: serving repo {repo_name:?} from {} (root's objects/ + \
         metadata.sqlite3); MCP protocol on stdin/stdout, diagnostics here",
        args.root.display()
    );

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    let mut line = String::new();
    loop {
        // A line is capped at 1 MiB: protocol messages are tiny, and a
        // hostile client's "line" must not be buffered without bound.
        // An oversized line is answered with -32600 and skipped, then
        // serving continues.
        let in_cap = match gardenbed_mcp::protocol::read_capped_line(&mut reader, &mut line) {
            Ok(None) => return, // EOF: the client is done
            Ok(Some(true)) => true,
            Ok(Some(false)) => false,
            Err(e) => {
                eprintln!("gardenbed-mcp: stdin failed: {e}");
                std::process::exit(1);
            }
        };
        if !in_cap {
            let response = gardenbed_mcp::protocol::error_line(
                &serde_json::Value::Null,
                gardenbed_mcp::protocol::INVALID_REQUEST,
                format!(
                    "request line exceeds the {}-byte limit and was skipped",
                    gardenbed_mcp::protocol::MAX_LINE_LEN
                ),
            );
            if writeln!(writer, "{response}").is_err() {
                return; // stdout closed: the client hung up; end cleanly
            }
            let _ = writer.flush();
            continue;
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue; // transport noise, not a message
        }
        if let Some(response) = gardenbed_mcp::handle_line(&repo, trimmed) {
            if writeln!(writer, "{response}").is_err() {
                return; // stdout closed: the client hung up; end cleanly
            }
            let _ = writer.flush();
        }
    }
}
