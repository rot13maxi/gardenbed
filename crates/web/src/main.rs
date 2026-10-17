//! gardenbed-web — the web GUI binary for one gardenbed repository.
//!
//! * `gardenbed-web [--root DIR] [--addr ADDR]` (or `serve`, the default
//!   subcommand) — serves the JSON API plus the embedded SPA over plain
//!   HTTP. Flags:
//!
//!     * `--addr` — listen address, default `127.0.0.1:8430` (`:0` binds
//!       an ephemeral port). Once bound, `listening on <addr>` is printed
//!       as stdout's first line — the same contract `gardenbed-server`
//!       follows, so scripts can read it and connect.
//!     * `--root` — repository directory (required): holds
//!       `<root>/objects/` and `<root>/metadata.sqlite3`, created if
//!       missing.
//!
//! * `gardenbed-web seed --root DIR` — writes the deterministic demo
//!   history ([`gardenbed_web::seed`]: ten changesets of a small garden
//!   planner app) and publishes it at `main`, printing
//!   `seeded tip <64-hex id>`. Re-running against an already-reffed repo
//!   prints a note and exits 0.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gardenbed_server::Repo;

#[derive(Debug, Parser)]
#[command(name = "gardenbed-web", about = "Gardenbed web GUI")]
struct Args {
    /// Listen address (`host:port`; `:0` picks an ephemeral port, the
    /// bound address is printed to stdout).
    #[arg(long, default_value = "127.0.0.1:8430")]
    addr: String,
    /// Repository directory; must be a non-empty path.
    #[arg(long)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the web GUI. This is the default when no subcommand is
    /// given (then the top-level `--addr`/`--root` flags apply).
    Serve {
        /// Listen address (`host:port`; `:0` picks an ephemeral port).
        #[arg(long, default_value = "127.0.0.1:8430")]
        addr: String,
        /// Repository directory; holds `<root>/objects` and
        /// `<root>/metadata.sqlite3`.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Seed the deterministic demo history into a repository (idempotent:
    /// a repo that already has refs is left untouched).
    Seed {
        /// Gardenbed repository directory (created if missing).
        #[arg(long)]
        root: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    match args.command {
        Some(Command::Seed { root }) => run_seed(&root),
        other => {
            let (addr, root) = match other {
                Some(Command::Serve { addr, root }) => (addr, root),
                Some(Command::Seed { .. }) => unreachable!("seed handled above"),
                None => (args.addr, args.root),
            };
            match root {
                Some(root) => serve(&addr, &root).await,
                None => {
                    eprintln!(
                        "error: the following required argument was not provided: --root <DIR>"
                    );
                    ExitCode::from(2)
                }
            }
        }
    }
}

fn run_seed(root: &std::path::Path) -> ExitCode {
    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        let repo = Repo::open(root)?;
        match gardenbed_web::seed::seed(&repo)? {
            Some(tip) => println!("seeded tip {}", tip.to_hex()),
            None => println!("repository already has refs; leaving it untouched"),
        }
        Ok(())
    })();
    report(result)
}

async fn serve(addr: &str, root: &std::path::Path) -> ExitCode {
    let result: Result<(), Box<dyn std::error::Error>> = async {
        let repo = Repo::open(root)?;
        let listener = tokio::net::TcpListener::bind(addr).await?;
        println!("listening on {}", listener.local_addr()?);
        axum::serve(listener, gardenbed_web::router(repo)).await?;
        Ok(())
    }
    .await;
    report(result)
}

/// Mirrors errors to stderr and turns them into the process exit code.
fn report(result: Result<(), Box<dyn std::error::Error>>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
