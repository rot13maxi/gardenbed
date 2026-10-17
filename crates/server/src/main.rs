//! The server: the authoritative gardenbed server binary.
//!
//! Two modes:
//!
//! * `gardenbed-server [--addr ADDR] --root DIR` (or `serve`, the default
//!   subcommand) — serves the `Gardenbed` gRPC API (v0.1.md §5) over TCP.
//!   Flags:
//!
//!     * `--addr` — listen address, default `127.0.0.1:9418` (`:0` binds an
//!       ephemeral port; the bound address is printed to stdout).
//!     * `--root` — repository directory (required): holds `<root>/objects/`
//!       and `<root>/metadata.sqlite3`, created if missing.
//!
//! * `gardenbed-server import --from <repo> --root DIR [--limit N]
//!   [--renames on|off] [--ref <pattern>...]` — ingests a git repository
//!   into the same stores (git_bridge). `--from` may be a local path or a
//!   URL; URLs are cloned/refreshed into a mirror under the Gardenbed
//!   root first. Prints the import report on success.
//!
//! * `gardenbed-server export --root DIR --into GIT-DIR [--ref <pattern>...]
//!   [--mapping FILE]` — renders the changeset graph into a real git
//!   repository via one `git fast-import` stream
//!   ([`gardenbed_git::export_to_path`]). Prints the export report;
//!   with `--mapping` also writes a TSV `<changeset id> <tab> <git sha>`
//!   file (sorted by changeset id) for roundtrip verification. This is
//!   scaffolding over the library API, not engine code.
//!
//! Logging goes to stderr via `env_logger` (control with `RUST_LOG`,
//! default `info`). The server runs until SIGINT/SIGTERM, then drains.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use gardenbed_server::{GardenbedService, Repo};
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;

#[derive(Debug, Parser)]
#[command(name = "gardenbed-server", about = "Gardenbed repository server")]
struct Args {
    /// Listen address (`host:port`; `:0` picks an ephemeral port).
    #[arg(long, default_value = "127.0.0.1:9418")]
    addr: String,
    /// Repository directory; must be a non-empty path.
    #[arg(long)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the gRPC API. This is the default when no subcommand is
    /// given (then the top-level `--addr`/`--root` flags apply).
    Serve {
        /// Listen address (`host:port`; `:0` picks an ephemeral port).
        #[arg(long, default_value = "127.0.0.1:9418")]
        addr: String,
        /// Repository directory; holds `<root>/objects` and
        /// `<root>/metadata.sqlite3`.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Import Git history into a repository.
    Import {
        /// Git repository: a local path (bare or working tree) or a URL.
        /// URLs are cloned (mirror) into `<root>/git-mirror.git`
        /// and refreshed on re-use.
        #[arg(long)]
        from: String,
        /// Gardenbed repository directory (created if missing).
        #[arg(long)]
        root: PathBuf,
        /// Ingest only the newest N commits (topological suffix; slice
        /// boundaries are re-rooted as full-tree root changesets).
        #[arg(long)]
        limit: Option<usize>,
        /// Detect renames with `git diff-tree -M` (`on`, default) or
        /// import them as delete+add pairs (`off`).
        #[arg(long, default_value = "on")]
        renames: String,
        /// Only import branches matching this glob (e.g. `refs/heads/*`);
        /// repeatable, empty means every branch.
        #[arg(long = "ref")]
        refs: Vec<String>,
    },
    /// Export repository history to Git.
    Export {
        /// Gardenbed repository directory (holds `objects/` and
        /// `metadata.sqlite3`).
        #[arg(long)]
        root: PathBuf,
        /// Destination git repository: created as a bare sha1 repository
        /// when missing, accepted as-is when present.
        #[arg(long)]
        into: PathBuf,
        /// Only export refs matching this glob (e.g. `refs/heads/*`);
        /// repeatable, empty means every ref in the metadata.
        #[arg(long = "ref")]
        refs: Vec<String>,
        /// Write one `<64-hex changeset id>\t<git sha>` line per exported
        /// commit, sorted by changeset id, to this file.
        #[arg(long)]
        mapping: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Default to `info` when RUST_LOG is unset.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = Args::parse();
    match args.command {
        Some(Command::Import {
            from,
            root,
            limit,
            renames,
            refs,
        }) => {
            let detect_renames = match renames.as_str() {
                "on" => true,
                "off" => false,
                other => {
                    eprintln!("error: --renames must be `on` or `off`, got {other:?}");
                    std::process::exit(2);
                }
            };
            if let Err(e) = run_import(&from, &root, detect_renames, limit, refs) {
                eprintln!("error: import failed: {e}");
                std::process::exit(1);
            }
            Ok(())
        }
        Some(Command::Serve { addr, root }) => serve(addr, root).await,
        Some(Command::Export {
            root,
            into,
            refs,
            mapping,
        }) => {
            if let Err(e) = run_export(&root, &into, refs, mapping) {
                eprintln!("error: export failed: {e}");
                std::process::exit(1);
            }
            Ok(())
        }
        None => serve(args.addr, args.root).await,
    }
}

/// One-shot git export from the stores under `root` into the git
/// repository at `into` (git_bridge's `git fast-import` writer). Prints the
/// report; optionally writes the changeset→git-SHA mapping as TSV.
fn run_export(
    root: &Path,
    into: &Path,
    refs: Vec<String>,
    mapping: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    use gardenbed_store::{FsObjectStore, SqliteMetadataStore};

    Repo::validate_layout(root)?;
    let objects = Arc::new(FsObjectStore::open(root)?);
    let metadata = Arc::new(SqliteMetadataStore::open(&root.join("metadata.sqlite3"))?);
    let opts = gardenbed_git::ExportOptions { refs };
    let report = gardenbed_git::export_to_path(into, objects, metadata, &opts)?;
    if let Some(path) = mapping {
        use std::io::Write as _;
        // Sorted by changeset id so the file is deterministic and diffable.
        let mut rows: Vec<(String, &String)> = report
            .commits
            .iter()
            .map(|(id, sha)| (id.to_hex(), sha))
            .collect();
        rows.sort();
        let f = std::fs::File::create(&path)
            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        let mut f = std::io::BufWriter::new(f);
        for (cs, sha) in rows {
            writeln!(f, "{cs}\t{sha}")?;
        }
        f.flush()?;
    }
    println!("{report}");
    Ok(())
}

/// One-shot git import into the stores under `root`.
fn run_import(
    from: &str,
    root: &Path,
    detect_renames: bool,
    limit: Option<usize>,
    refs: Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    use gardenbed_git as git_bridge;
    use gardenbed_store::{FsObjectStore, SqliteMetadataStore};

    Repo::validate_layout(root)?;
    let objects = Arc::new(FsObjectStore::open(root)?);
    let metadata = Arc::new(SqliteMetadataStore::open(&root.join("metadata.sqlite3"))?);
    let engine = gardenbed_derive::Engine::new(objects.clone(), metadata.clone());
    let opts = git_bridge::ImportOptions {
        detect_renames,
        limit,
        refs,
    };
    let report = if from.contains("://") {
        let mirror = root.join("git-mirror.git");
        git_bridge::import_from_url(from, &mirror, objects, metadata, engine, &opts)?
    } else {
        git_bridge::import_from_path(Path::new(from), objects, metadata, engine, &opts)?
    };
    // Machine- and human-readable one-line report on stdout.
    println!("{report}");
    Ok(())
}

async fn serve(
    addr_arg: String,
    root_arg: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(root) = root_arg.filter(|p| !p.as_os_str().is_empty()) else {
        eprintln!("error: --root is required (the repository directory)");
        std::process::exit(2);
    };

    let repo = Arc::new(Repo::open(&root).map_err(|e| {
        eprintln!("error: cannot open repository at {}: {e}", root.display());
        e
    })?);
    log::info!("repository opened root={}", root.display());

    let addr: SocketAddr = addr_arg
        .parse()
        .map_err(|e| format!("invalid --addr {addr_arg}: {e}"))?;
    let incoming = TcpIncoming::bind(addr)?
        // tonic 0.14's low-level serve path bypasses tonic's default-true
        // nodelay: without this, Nagle delays every unary RPC round trip
        // (bed blame on a 670-step walk measured ~41 ms per call, 28.4 s
        // total; with nodelay, 0.66 s — see docs/release-0.1.md).
        .with_nodelay(Some(true));
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
