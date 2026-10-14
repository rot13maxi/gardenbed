//! The arboretum: the authoritative gardenbed server binary.
//!
//! Serves the `Gardenbed` gRPC API (v0.1.md §5) over TCP. Flags:
//!
//! * `--addr` — listen address, default `127.0.0.1:9418` (`:0` binds an
//!   ephemeral port; the bound address is printed to stdout).
//! * `--root` — repository directory (required): holds `<root>/loam/` and
//!   `<root>/seedbed.sqlite3`, created if missing.
//!
//! Logging goes to stderr via `env_logger` (control with `RUST_LOG`,
//! default `info`). The process serves until SIGINT/SIGTERM, then drains.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use gardenbed_server::{GardenbedService, Repo};
use tonic::transport::server::TcpIncoming;
use tonic::transport::Server;

#[derive(Debug, Parser)]
#[command(name = "gardenbed-server", about = "The gardenbed arboretum (authoritative server)")]
struct Args {
    /// Listen address (`host:port`; `:0` picks an ephemeral port).
    #[arg(long, default_value = "127.0.0.1:9418")]
    addr: String,
    /// Repository directory; must be a non-empty path.
    #[arg(long)]
    root: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Default to `info` when RUST_LOG is unset.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = Args::parse();
    let Some(root) = args.root.filter(|p| !p.as_os_str().is_empty()) else {
        eprintln!("error: --root is required (the repository directory)");
        std::process::exit(2);
    };

    let repo = Arc::new(Repo::open(&root).map_err(|e| {
        eprintln!("error: cannot open repository at {}: {e}", root.display());
        e
    })?);
    log::info!("repository opened root={}", root.display());

    let addr: SocketAddr = args
        .addr
        .parse()
        .map_err(|e| format!("invalid --addr {}: {e}", args.addr))?;
    let incoming = TcpIncoming::bind(addr)?;
    let bound = incoming.local_addr()?;
    // The bound address goes to stdout: it is the contract for tests and
    // scripts that need to learn an ephemeral port.
    println!("listening on {bound}");
    log::info!("gardenbed-server listening addr={bound}");

    Server::builder()
        .add_service(gardenbed_protocol::gardenbed_server::GardenbedServer::new(
            GardenbedService::new(repo),
        ))
        .serve_with_incoming_shutdown(incoming, shutdown_signal())
        .await?;

    log::info!("shutdown complete");
    Ok(())
}

/// Resolves when SIGINT (ctrl-c) or SIGTERM arrives; the tokio `signal`
/// feature is required for either.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler installs");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
