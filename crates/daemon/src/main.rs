//! `gardenbed-daemon` — the local daemon binary (v0.2.md §6).
//!
//! One subcommand: `serve`.
//!
//! ```sh
//! gardenbed-daemon serve --upstream http://127.0.0.1:9418
//! ```
//!
//! The upstream server is a command-line argument (not a working-copy
//! config key): the daemon is machine-level infrastructure, not a
//! per-working-copy setting — the same way the socket path comes from the
//! environment. Working copies never name the daemon; `bed` discovers it
//! through `GARDENBED_DAEMON` / the default socket path (D19).
//!
//! The daemon never opens a working-copy index (D18): all `.bed` writes
//! stay in `bed`. It serves the full Gardenbed RPC surface as a caching
//! proxy plus the sidecar (`Ping`, `Register`, `Generation`) on one unix
//! socket.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "gardenbed-daemon", about = "gardenbed local daemon (shared cache + watchers)")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the caching proxy and sidecar on the daemon socket.
    Serve {
        /// The upstream gardenbed server endpoint (`http://host:port`).
        /// Every forwarded RPC goes here; `repo` fields ride untouched.
        #[arg(long)]
        upstream: String,
        /// Override the socket path (default: $XDG_RUNTIME_DIR/gardenbed/
        /// daemon.sock; honors GARDENBED_DAEMON the same way `bed` does).
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Override the shared object cache root (default:
        /// $XDG_CACHE_HOME/gardenbed).
        #[arg(long)]
        cache: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = Args::parse();
    match args.command {
        Command::Serve {
            upstream,
            socket,
            cache,
        } => {
            let socket = socket
                .or_else(gardenbed_daemon::paths::socket_path)
                .unwrap_or_else(|| {
                    eprintln!(
                        "error: GARDENBED_DAEMON is set to empty — the daemon is \
                         disabled by the environment; unset it or pass --socket"
                    );
                    std::process::exit(2);
                });
            let cache = cache.unwrap_or_else(gardenbed_daemon::paths::cache_root);
            let proxy = gardenbed_daemon::proxy::ProxyService::new(&upstream, &cache)?;
            let sidecar = gardenbed_daemon::sidecar::SidecarService::new();
            println!(
                "gardenbed-daemon serving {} (upstream {upstream}, cache {})",
                socket.display(),
                cache.display()
            );
            gardenbed_daemon::unix::serve(&socket, proxy, sidecar).await
        }
    }
}
