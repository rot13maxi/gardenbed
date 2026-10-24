//! The server: the authoritative gardenbed server binary.
//!
//! Modes:
//!
//! * `gardenbed-server [--addr ADDR] --root DIR [--max-repos N]` (or
//!   `serve`, the default subcommand) — serves the `Gardenbed` gRPC API
//!   (v0.1.md §5) over TCP for every repository under the root:
//!   `<root>/<repo>/objects` + `<root>/<repo>/metadata.sqlite3`
//!   (v0.2.md §5). A v0.1 root (`metadata.sqlite3` or `objects/` at the
//!   top level) keeps serving unchanged as repository `default`.
//!   Flags:
//!
//!     * `--addr` — listen address, default `127.0.0.1:9418` (`:0` binds an
//!       ephemeral port; the bound address is printed to stdout).
//!     * `--root` — the server root directory (required): one
//!       subdirectory per repository, created if missing.
//!     * `--max-repos` — cap on simultaneously open repositories
//!       (LRU eviction above it), default 256.
//!     * `--auth` — optional token auth config (`auth.toml`; `serve`
//!       subcommand only): bearer-token access to repositories. Without
//!       it the server is open.
//!
//! * `gardenbed-server import --from <repo> --root DIR [--repo NAME]
//!   [--limit N] [--renames on|off] [--ref <pattern>...] [--allow-rewind]`
//!   — ingests a git repository into the named repository's stores
//!   (git_bridge; `--repo` defaults to `default`). `--from` may be a
//!   local path or a URL; URLs are cloned/refreshed into a mirror at
//!   `<root>/<repo>/git-mirror.git` first. Prints the import report on
//!   success; a backward ref move that would lose changesets fails
//!   unless `--allow-rewind` is given, and any backward move prints a
//!   loud warning to stderr.
//!
//! * `gardenbed-server export --root DIR [--repo NAME] --into GIT-DIR
//!   [--ref <pattern>...] [--mapping FILE]` — renders the named
//!   repository's changeset graph into a real git repository via one
//!   `git fast-import` stream ([`gardenbed_git::export_to_path`]).
//!   Prints the export report; with `--mapping` also writes a TSV
//!   `<changeset id> <tab> <git sha>` file (sorted by changeset id) for
//!   roundtrip verification. This is scaffolding over the library API,
//!   not engine code.
//!
//! * `gardenbed-server move --root DIR --from NAME --into NEW-NAME` —
//!   renames a repository directory (the two renames: stores and mirror).
//!   Both stores are path-free and self-contained, so history is
//!   byte-identical after the move.
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
    /// The server root directory: one subdirectory per repository.
    #[arg(long)]
    root: Option<PathBuf>,
    /// Cap on simultaneously open repositories; least-recently-used
    /// repositories above the cap are closed. Local (`--root`) mode only.
    #[arg(long, default_value_t = gardenbed_server::repos::DEFAULT_MAX_REPOS, global = true)]
    max_repos: usize,
    /// Storage configuration file (TOML) for the scaled backends
    /// (Postgres metadata + S3 objects); takes precedence over `--root`.
    /// `GARDENBED_STORAGE` is the env fallback.
    #[arg(long)]
    storage: Option<PathBuf>,
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
        /// The server root directory: one subdirectory per repository
        /// (`<root>/<repo>/objects` + `<root>/<repo>/metadata.sqlite3`).
        /// A v0.1 root serves unchanged as repository `default`.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Storage configuration file (TOML) for the scaled backends
        /// (Postgres metadata + S3 objects); takes precedence over
        /// `--root`. `GARDENBED_STORAGE` is the env fallback.
        #[arg(long)]
        storage: Option<PathBuf>,
        /// Token auth config (`auth.toml`): `[tokens.<token>]` entries with
        /// `repos` (names, or `*` for any) and `role` (read/write/admin).
        /// Without this flag the server is open.
        #[arg(long, value_name = "PATH")]
        auth: Option<PathBuf>,
    },
    /// Import Git history into a repository.
    Import {
        /// Git repository: a local path (bare or working tree) or a URL.
        /// URLs are cloned (mirror) into the repository's
        /// `git-mirror.git` and refreshed on re-use.
        #[arg(long)]
        from: String,
        /// The server root directory (one subdirectory per repository).
        /// Local mode; required unless `--storage` selects the scaled
        /// backends (a URL import, which is local-only, uses `--root`).
        #[arg(long)]
        root: Option<PathBuf>,
        /// Repository name under the root (default: `default`); local
        /// mode only. Refused in scaled mode, where one repository
        /// serves under any name.
        #[arg(long)]
        repo: Option<String>,
        /// Storage configuration file (TOML) for the scaled backends
        /// (Postgres metadata + S3 objects); takes precedence over
        /// `--root`. `GARDENBED_STORAGE` is the env fallback.
        #[arg(long)]
        storage: Option<PathBuf>,
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
        /// Permit ref moves that would lose changesets (force-push
        /// parity). By default such an import fails loudly — before any
        /// ref is touched — instead of silently rewinding published
        /// refs; backward moves that lose nothing still only warn.
        /// `--limit` imports cannot compute the loss, so they refuse
        /// every non-descendant ref move by default.
        #[arg(long)]
        allow_rewind: bool,
    },
    /// Export repository history to Git.
    Export {
        /// The server root directory (one subdirectory per repository).
        /// Local mode; required unless `--storage` selects the scaled
        /// backends.
        #[arg(long)]
        root: Option<PathBuf>,
        /// Repository name under the root (default: `default`); local
        /// mode only.
        #[arg(long)]
        repo: Option<String>,
        /// Storage configuration file (TOML) for the scaled backends
        /// (Postgres metadata + S3 objects); takes precedence over
        /// `--root`. `GARDENBED_STORAGE` is the env fallback.
        #[arg(long)]
        storage: Option<PathBuf>,
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
        /// Persist the computed changeset↔git-SHA mapping rows into the
        /// metadata store after a successful export (dogfood #26: removes
        /// manual landing-tip bookkeeping). Never overwrites: a pair whose
        /// sha or changeset is already bound differently (a native vs
        /// re-imported twin) is refused with a warning, and the
        /// import-written canonical mapping survives. Idempotent: pairs
        /// already recorded are left as they are.
        #[arg(long)]
        record_mappings: bool,
    },
    /// Rename a repository under the server root.
    Move {
        /// The server root directory.
        #[arg(long)]
        root: PathBuf,
        /// The repository's current name.
        #[arg(long)]
        from: String,
        /// The new name (must not exist).
        #[arg(long)]
        into: String,
    },
    /// Garbage-collect unreachable objects (v0.2.md §7).
    Gc {
        /// Gardenbed repository directory (holds `objects/` and
        /// `metadata.sqlite3`).
        #[arg(long)]
        root: PathBuf,
        /// Report what would be collected; delete nothing.
        #[arg(long)]
        dry_run: bool,
        /// Run live (no exclusive lock); the sweep aborts if an ingest
        /// lands mid-sweep. Default is offline: fail if the server (or
        /// another gc) holds the lock.
        #[arg(long)]
        force: bool,
        /// Minimum age (seconds) of collectible objects; default 86400.
        #[arg(long)]
        min_age: Option<u64>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Keep the build fingerprint in this binary's rodata: the test
    // harness greps the located executable for it, proving the file was
    // built from the tree it claims (dogfood #12's staleness class).
    // `black_box` keeps the reference alive through optimization.
    std::hint::black_box(gardenbed_server::SRC_DIGEST_MARKER);
    // Default to `info` when RUST_LOG is unset.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = Args::parse();
    match args.command {
        Some(Command::Import {
            from,
            root,
            repo,
            storage,
            limit,
            renames,
            refs,
            allow_rewind,
        }) => {
            let detect_renames = match renames.as_str() {
                "on" => true,
                "off" => false,
                other => {
                    eprintln!("error: --renames must be `on` or `off`, got {other:?}");
                    std::process::exit(2);
                }
            };
            // Refuse before any directory is created: with neither
            // --root, --storage, nor GARDENBED_STORAGE, resolve_storage
            // would fall back to an empty root and the import would
            // scatter objects/ into the CWD before failing.
            if storage.is_none() && root.is_none() && env_storage().is_none() {
                eprintln!(
                    "error: import requires either --root (local mode) or \
                     --storage (scaled mode)"
                );
                std::process::exit(2);
            }
            let selection = resolve_storage(storage.as_deref(), root.as_deref());
            match selection {
                StorageSelection::Root(root) => {
                    if let Err(e) = run_import(
                        &from,
                        &root,
                        repo.as_deref(),
                        detect_renames,
                        limit,
                        refs,
                        allow_rewind,
                    ) {
                        eprintln!("error: import failed: {e}");
                        std::process::exit(1);
                    }
                }
                StorageSelection::Config(_) => {
                    if repo.is_some() {
                        eprintln!(
                            "error: --repo names a repository under a server root; it \
                             does not apply to --storage (scaled) mode)"
                        );
                        std::process::exit(2);
                    }
                    let selection = resolve_storage(storage.as_deref(), root.as_deref());
                    if let Err(e) = run_import_scaled(
                        &from,
                        selection,
                        detect_renames,
                        limit,
                        refs,
                        allow_rewind,
                    ) {
                        eprintln!("error: import failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Ok(())
        }
        Some(Command::Serve {
            addr,
            root,
            storage,
            auth,
        }) => serve(addr, root, storage, args.max_repos, auth).await,
        Some(Command::Export {
            root,
            repo,
            storage,
            into,
            refs,
            mapping,
            record_mappings,
        }) => {
            // Same pre-open refusal as import: no stores, no side effects.
            if storage.is_none() && root.is_none() && env_storage().is_none() {
                eprintln!(
                    "error: export requires either --root (local mode) or \
                     --storage (scaled mode)"
                );
                std::process::exit(2);
            }
            let selection = resolve_storage(storage.as_deref(), root.as_deref());
            match selection {
                StorageSelection::Root(root) => {
                    if let Err(e) = run_export(
                        &root,
                        repo.as_deref(),
                        &into,
                        refs,
                        mapping,
                        record_mappings,
                    ) {
                        eprintln!("error: export failed: {e}");
                        std::process::exit(1);
                    }
                }
                StorageSelection::Config(_) => {
                    if repo.is_some() {
                        eprintln!(
                            "error: --repo names a repository under a server root; it \
                             does not apply to --storage (scaled) mode)"
                        );
                        std::process::exit(2);
                    }
                    let selection = resolve_storage(storage.as_deref(), root.as_deref());
                    if let Err(e) =
                        run_export_scaled(selection, &into, refs, mapping, record_mappings)
                    {
                        eprintln!("error: export failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Ok(())
        }
        Some(Command::Move { root, from, into }) => {
            if let Err(e) = run_move(&root, &from, &into) {
                eprintln!("error: move failed: {e}");
                std::process::exit(1);
            }
            Ok(())
        }
        Some(Command::Gc {
            root,
            dry_run,
            force,
            min_age,
        }) => {
            let code = run_gc(&root, dry_run, force, min_age);
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        None => serve(args.addr, args.root, args.storage, args.max_repos, None).await,
    }
}

/// Which backing stores a subcommand runs against: either a
/// [`gardenbed_store::StorageConfig`] file (scaled mode) or a local
/// repository `--root` (the default, unchanged).
#[derive(Debug)]
enum StorageSelection {
    /// A storage configuration file path (from `--storage` or
    /// `GARDENBED_STORAGE`).
    Config(PathBuf),
    /// Local mode: the `--root` repository directory.
    Root(PathBuf),
}

/// The `GARDENBED_STORAGE` env fallback, when set to a non-empty path.
fn env_storage() -> Option<PathBuf> {
    std::env::var("GARDENBED_STORAGE")
        .ok()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

/// Resolves the storage selection: an explicit `--storage` flag first,
/// then the `GARDENBED_STORAGE` env var (a path to the same TOML file),
/// then local `--root` mode.
fn resolve_storage(storage_flag: Option<&Path>, root: Option<&Path>) -> StorageSelection {
    if let Some(p) = storage_flag {
        return StorageSelection::Config(p.to_path_buf());
    }
    if let Some(p) = env_storage() {
        return StorageSelection::Config(p);
    }
    StorageSelection::Root(root.unwrap_or(Path::new("")).to_path_buf())
}

/// Opens the repository named by the selection: local mode via
/// [`Repo::open`] (layout validation + fs/sqlite stores, unchanged), scaled
/// mode via [`gardenbed_store::StorageConfig`] and [`Repo::open_stores`].
fn open_repo(selection: &StorageSelection) -> Result<Repo, Box<dyn std::error::Error>> {
    match selection {
        StorageSelection::Root(root) => Ok(Repo::open(root)?),
        StorageSelection::Config(path) => {
            let (objects, metadata) = gardenbed_store::StorageConfig::from_path(path)?.open()?;
            Ok(Repo::open_stores(objects, metadata))
        }
    }
}

/// One-shot git export from the named repository's local stores into the
/// git repository at `into` (git_bridge's `git fast-import` writer).
/// Prints the report; optionally writes the changeset→git-SHA mapping as
/// TSV; optionally persists the mapping rows into the metadata store
/// (`--record-mappings`, dogfood #26).
fn run_export(
    root: &Path,
    repo: Option<&str>,
    into: &Path,
    refs: Vec<String>,
    mapping: Option<PathBuf>,
    record_mappings: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use gardenbed_store::{FsObjectStore, SqliteMetadataStore};

    let name = repo.unwrap_or(gardenbed_server::repos::DEFAULT_REPO);
    let set =
        gardenbed_server::repos::RepoSet::new(root, gardenbed_server::repos::DEFAULT_MAX_REPOS);
    let dir = set.existing_repo_dir_for(name)?;
    Repo::validate_layout(&dir)?;
    // Open the same stores the server would: a pre-rename root
    // (`seedbed.sqlite3` + `loam/objects`) must not be read as an empty
    // current-layout repository, and must not be shadowed by one.
    let layout = gardenbed_server::resolve_layout(&dir);
    let objects = Arc::new(FsObjectStore::open(&layout.objects)?);
    let metadata = Arc::new(SqliteMetadataStore::open(&layout.metadata)?);
    let opts = gardenbed_git::ExportOptions {
        refs,
        record_mappings,
    };
    let report = gardenbed_git::export_to_path(into, objects, metadata, &opts)?;
    write_mapping_tsv(&report, mapping.as_deref())?;
    report_conflicts(&report);
    println!("{report}");
    Ok(())
}

/// Writes the TSV side-file (`--mapping`), when requested: one
/// `<64-hex changeset id>\t<git sha>` line per exported commit, sorted by
/// changeset id (deterministic and diffable).
fn write_mapping_tsv(
    report: &gardenbed_git::ExportReport,
    path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = path else {
        return Ok(());
    };
    use std::io::Write as _;
    let mut rows: Vec<(String, &String)> = report
        .commits
        .iter()
        .map(|(id, sha)| (id.to_hex(), sha))
        .collect();
    rows.sort();
    let f = std::fs::File::create(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    let mut f = std::io::BufWriter::new(f);
    for (cs, sha) in rows {
        writeln!(f, "{cs}\t{sha}")?;
    }
    f.flush()?;
    Ok(())
}

/// Prints every mapping conflict on stderr with a `warning:` prefix: the
/// report line alone only carries the count, while each refusal names the
/// sha, the changeset, and the surviving canonical row — the operator
/// needs the identities to decide whether a re-union is required
/// (dogfood #26/#23 class).
fn report_conflicts(report: &gardenbed_git::ExportReport) {
    let Some(mappings) = &report.mappings else {
        return;
    };
    for conflict in &mappings.conflicts {
        eprintln!("warning: export mapping conflict: {conflict}");
    }
}

/// One-shot git import into the stores behind `selection`.
fn run_import(
    from: &str,
    root: &Path,
    repo: Option<&str>,
    detect_renames: bool,
    limit: Option<usize>,
    refs: Vec<String>,
    allow_rewind: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use gardenbed_git as git_bridge;
    use gardenbed_store::{FsObjectStore, SqliteMetadataStore};

    let name = repo.unwrap_or(gardenbed_server::repos::DEFAULT_REPO);
    let set =
        gardenbed_server::repos::RepoSet::new(root, gardenbed_server::repos::DEFAULT_MAX_REPOS);
    let dir = set.repo_dir_for(name)?;
    Repo::validate_layout(&dir)?;
    // An import mutates the object store and metadata in the same way a
    // running server does. Hold the shared repo.lock for its duration so
    // an offline gc cannot interleave: gc's exclusive (non-blocking)
    // acquisition refuses to start while this import runs, and this
    // import refuses to start while gc holds the lock — the same
    // contract serve already honors.
    let _lock = gardenbed_server::gc::lock_shared_blocking(&dir)?;
    let layout = gardenbed_server::resolve_layout(&dir);
    let objects = Arc::new(FsObjectStore::open(&layout.objects)?);
    let metadata = Arc::new(SqliteMetadataStore::open(&layout.metadata)?);
    let engine = gardenbed_derive::Engine::new(objects.clone(), metadata.clone());
    let opts = git_bridge::ImportOptions {
        detect_renames,
        limit,
        refs,
        allow_rewind,
    };
    // URL imports clone into a mirror under the local repository root;
    // scaled mode has no root, so URLs must be cloned locally first.
    let report = if from.contains("://") {
        let mirror = gardenbed_server::repos::mirror_dir(root, name);
        git_bridge::import_from_url(from, &mirror, objects, metadata, engine, &opts)?
    } else {
        git_bridge::import_from_path(Path::new(from), objects, metadata, engine, &opts)?
    };
    // A backward ref move is never silent: name the ref, both tips, and
    // the changesets that fell off it. (With `allow_rewind` unset the
    // engine already refused moves that would lose changesets, so what
    // lands here lost nothing — but the operator still must see it.)
    for rewind in &report.refs_rewound {
        eprintln!(
            "warning: import moved {name} backward: {old} -> {new} ({lost})",
            name = rewind.name,
            old = rewind.old_tip.short(),
            new = rewind.new_tip.short(),
            lost = match rewind.fell_off {
                Some(0) => "no changesets lost".to_owned(),
                Some(n) => format!("{n} changeset(s) may no longer be reachable from the ref"),
                None => "changeset loss not counted (limit import)".to_owned(),
            }
        );
    }
    // Machine- and human-readable one-line report on stdout.
    println!("{report}");
    Ok(())
}

/// Renames a repository under the server root: the two renames (stores,
/// then the git mirror when present). Refuses an occupied destination.
fn run_move(root: &Path, from: &str, into: &str) -> Result<(), Box<dyn std::error::Error>> {
    let set =
        gardenbed_server::repos::RepoSet::new(root, gardenbed_server::repos::DEFAULT_MAX_REPOS);
    let to_dir = set.move_repo(from, into)?;
    println!(
        "moved repository {from} to {into} ({}), history byte-identical",
        to_dir.display()
    );
    Ok(())
}

/// One-shot garbage collection over the repository under `root`.
/// Exit code 0 on success (including an aborted --force sweep), 1 on
/// lock contention, 2 on usage/IO errors.
fn run_gc(root: &Path, dry_run: bool, force: bool, min_age: Option<u64>) -> i32 {
    use gardenbed_server::gc;

    let opts = gc::Options {
        dry_run,
        force,
        min_age: std::time::Duration::from_secs(min_age.unwrap_or(86_400)),
    };
    if dry_run || force {
        // Dry run never mutates, force runs live by design: no lock.
        return match gc::run(root, opts, None) {
            Ok(report) => {
                println!("{report}");
                0
            }
            Err(e) => {
                eprintln!("error: gc failed: {e}");
                1
            }
        };
    }
    // Offline mode: exclusive lock, non-blocking, with a bounded backoff.
    // Linux flock has no FIFO fairness, so steady short-lived serve churn
    // (each import now holds the shared lock too) could starve a single
    // non-blocking attempt forever. Retry with exponential backoff up to
    // a total of ~60s, then fail with the actionable message.
    let mut backoff = std::time::Duration::from_millis(100);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let lock = loop {
        match gc::lock_exclusive(root) {
            Ok(lock) => break lock,
            Err(gc::LockError::Held) => {
                if std::time::Instant::now() + backoff >= deadline {
                    eprintln!(
                        "error: repository is in use (repo.lock is held); stop the server \
                         or run with --force for live gc"
                    );
                    return 1;
                }
                std::thread::sleep(backoff);
                backoff *= 2;
            }
            Err(e) => {
                eprintln!("error: cannot lock {}: {e}", root.display());
                return 1;
            }
        }
    };
    match gc::run(root, opts, Some(&lock)) {
        Ok(report) => {
            println!("{report}");
            0
        }
        Err(e) => {
            eprintln!("error: gc failed: {e}");
            1
        }
    }
}

/// One-shot scaled-mode import (`--storage`): the repository named by the
/// selection's `StorageConfig`. `--repo` is refused earlier, at parse.
fn run_import_scaled(
    from: &str,
    selection: StorageSelection,
    detect_renames: bool,
    limit: Option<usize>,
    refs: Vec<String>,
    allow_rewind: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use gardenbed_git as git_bridge;

    let repo = open_repo(&selection)?;
    let objects = repo.objects_handle();
    let metadata = repo.metadata_handle();
    let engine = repo.engine_handle();
    let opts = git_bridge::ImportOptions {
        detect_renames,
        limit,
        refs,
        allow_rewind,
    };
    // URL imports clone into a mirror under the local repository root;
    // scaled mode has no root, so URLs must be cloned locally first.
    let report = if from.contains("://") {
        return Err("URL import requires local mode (--root); clone the                      repository locally and import the path with --storage"
            .into());
    } else {
        git_bridge::import_from_path(Path::new(from), objects, metadata, engine, &opts)?
    };
    for rewind in &report.refs_rewound {
        eprintln!(
            "warning: import moved {name} backward: {old} -> {new} ({lost})",
            name = rewind.name,
            old = rewind.old_tip.short(),
            new = rewind.new_tip.short(),
            lost = match rewind.fell_off {
                Some(0) => "no changesets lost".to_owned(),
                Some(n) => format!("{n} changeset(s) may no longer be reachable from the ref"),
                None => "changeset loss not counted (limit import)".to_owned(),
            }
        );
    }
    println!("{report}");
    Ok(())
}

/// One-shot scaled-mode export (`--storage`).
fn run_export_scaled(
    selection: StorageSelection,
    into: &Path,
    refs: Vec<String>,
    mapping: Option<PathBuf>,
    record_mappings: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let repo = open_repo(&selection)?;
    let objects = repo.objects_handle();
    let metadata = repo.metadata_handle();
    let opts = gardenbed_git::ExportOptions {
        refs,
        record_mappings,
    };
    let report = gardenbed_git::export_to_path(into, objects, metadata, &opts)?;
    write_mapping_tsv(&report, mapping.as_deref())?;
    report_conflicts(&report);
    println!("{report}");
    Ok(())
}

async fn serve(
    addr_arg: String,
    root_arg: Option<PathBuf>,
    storage_arg: Option<PathBuf>,
    max_repos: usize,
    auth: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Local mode keeps the historical "explicit non-empty --root" check.
    let selection = match resolve_storage(storage_arg.as_deref(), root_arg.as_deref()) {
        StorageSelection::Config(p) => StorageSelection::Config(p),
        StorageSelection::Root(root) => {
            if root.as_os_str().is_empty() {
                eprintln!("error: --root is required (the server root directory)");
                std::process::exit(2);
            }
            StorageSelection::Root(root)
        }
    };
    if max_repos == 0 {
        eprintln!("error: --max-repos must be at least 1");
        std::process::exit(2);
    }

    // The root lock must outlive the match: bound here (not inside the
    // arm) so it is held for the whole serve lifetime. Dropped only when
    // serve returns — the fd stays open (flock(2): the lock lives as long
    // as any descriptor referencing the open file description).
    let _root_lock = match &selection {
        StorageSelection::Root(root) => {
            // The root is the registry: created when missing, one directory per
            // repository. A v0.1 root (stores at the top level) serves unchanged
            // as repository `default`.
            std::fs::create_dir_all(root).map_err(|e| {
                eprintln!("error: cannot create server root {}: {e}", root.display());
                e
            })?;
            // Hold the root's advisory lock for the process lifetime: it makes a
            // second server over the same root fail fast, and it lets
            // `gardenbed-server move` refuse while this server is live (renaming
            // repository files under open sqlite connections strands the WAL).
            // Taken without waiting — a second server must refuse, not queue.
            Some(match gardenbed_server::repos::try_lock_root_shared(root) {
                Ok(lock) => lock,
                Err(e) => {
                    eprintln!(
                        "error: cannot lock the server root (is another gardenbed-server \
                         serving {}?): {e}",
                        root.display()
                    );
                    std::process::exit(2);
                }
            })
        }
        StorageSelection::Config(_) => {
            // Scaled mode has no root directory to lock.
            None
        }
    };
    let set: Arc<gardenbed_server::repos::RepoSet> = match &selection {
        StorageSelection::Root(root) => {
            let set = Arc::new(gardenbed_server::repos::RepoSet::new(root, max_repos));
            if gardenbed_server::repos::RepoSet::is_legacy_root(root) {
                log::info!(
                    "v0.1 root detected: serving {} as repository `default`",
                    root.display()
                );
            }
            let served = set.served_names();
            log::info!(
                "server root={} repositories={} max_repos={max_repos}",
                root.display(),
                if served.is_empty() {
                    "(none yet)".to_owned()
                } else {
                    served.join(", ")
                }
            );
            set
        }
        StorageSelection::Config(path) => {
            // Scaled mode: one repository over off-box stores; the request's
            // `repo` name is a label (per-name counters), not a directory.
            let (objects, metadata) = gardenbed_store::StorageConfig::from_path(path)?.open()?;
            let repo = Arc::new(Repo::open_stores(objects, metadata));
            log::info!(
                "repository opened storage={:?} (single mode; any valid repo name serves)",
                path
            );
            Arc::new(gardenbed_server::repos::RepoSet::single(repo))
        }
    };

    // Shared hold on each served repository's repo.lock for the whole
    // serving lifetime: a GC's exclusive (offline) lock can then never be
    // taken while the server runs (v0.2.md §7). Blocking acquire: a GC in
    // flight is finite. Scaled mode has no root directory to lock.
    let _repo_locks = match &selection {
        StorageSelection::Root(_) => {
            let mut locks = Vec::new();
            for name in set.served_names() {
                let dir = set.repo_dir(&name);
                locks.push(
                    gardenbed_server::gc::lock_shared_blocking(&dir).map_err(|e| {
                        eprintln!("error: cannot lock {} (repo.lock): {e}", dir.display());
                        e
                    })?,
                );
            }
            locks
        }
        StorageSelection::Config(_) => Vec::new(),
    };

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
    // Auth is loaded before the listening line: a config error must abort
    // startup without ever advertising an address.
    let service = match auth {
        Some(path) => {
            let policy = gardenbed_server::auth::AuthPolicy::load(&path).unwrap_or_else(|msg| {
                eprintln!("error: {msg}");
                std::process::exit(2);
            });
            log::info!("token auth enabled ({} tokens)", policy.token_count());
            GardenbedService::with_auth(set, policy)
        }
        None => GardenbedService::new(set),
    };
    // The bound address goes to stdout: it is the contract for tests and
    // scripts that need to learn an ephemeral port.
    println!("listening on {bound}");
    log::info!("gardenbed-server listening addr={bound}");
    Server::builder()
        .add_service(gardenbed_protocol::gardenbed_server::GardenbedServer::new(
            service,
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
