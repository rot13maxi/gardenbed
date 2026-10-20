//! The scripted QA agent walkthrough (M7.3): a minimal MCP client that
//! spawns the real `gardenbed-mcp` binary, speaks the hand-rolled stdio
//! protocol, and asks canned questions about a synthetic history — whose
//! ground truth is real git (the harness materializes the same history
//! into a git oracle repo and the agent's answers are checked against
//! `git blame`/`diff-tree`/`show`/`log`).
//!
//! This runs the suite's first three questions — the blame → log
//! authorship composition — and prints each question and answer as a
//! transcript. The full ≥20-question suite lives in the crate's
//! integration test (`tests/mcp_suite.rs`), which also asserts on the
//! tally.
//!
//! Run it:
//!
//! ```text
//! cargo build -p gardenbed-mcp
//! cargo run -p gardenbed-mcp --example qa_agent
//! ```
//!
//! (or point the `MCP_BIN` environment variable at a built
//! `gardenbed-mcp` binary).

fn main() {
    let bin = gardenbed_mcp::qa::probe_workspace_bin("gardenbed-mcp", "MCP_BIN");
    println!(
        "gardenbed-qa-agent: spawning {} (MCP over stdio)\n",
        bin.display()
    );

    let report = gardenbed_mcp::qa::run_demo(&bin);

    println!("\n{}", report.summary());
    if report.failed > 0 {
        std::process::exit(1);
    }
}
