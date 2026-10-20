//! `bed` — the binary. Parses the CLI and dispatches; all behavior lives
//! in the `gardenbed` library so other surfaces (the MCP server) reuse it
//! unchanged.
//!
//! Every v0.1 command is wired: the read path (`clone`, `sparse add`,
//! `switch`, `file`, `log`, `diff`, `blame`) and the write path (`commit`,
//! `rebase`, `publish`) run against the server; `status` and `sparse
//! list/remove` are fully offline.

use clap::Parser as _;

use gardenbed::cli::Cli;
use gardenbed::cli::Command;
use gardenbed::cli::SparseCommand;
use gardenbed::error::ClientError;
use gardenbed::revspec::CsOrRef;
use gardenbed::sparse;
use gardenbed::workspace::WorkingCopy;

fn main() {
    // A closed pipe is a graceful end, not a crash: `bed log | head` must
    // die silently the way git and rg do (SIGPIPE's default disposition,
    // shell reports 141) instead of panicking on the next print — Rust
    // ignores SIGPIPE at startup, which turns every later `print!` into a
    // `failed printing to stdout` panic. Must happen before the runtime
    // (and any thread) exists; signal dispositions are process-wide.
    #[cfg(unix)]
    restore_default_sigpipe().expect("reset SIGPIPE to its default disposition");
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = match runtime.block_on(run(&cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("bed: {e}");
            1
        }
    };
    std::process::exit(code);
}

/// Restores the default `SIGPIPE` disposition (`SIG_DFL`): a write to a
/// closed pipe kills the process silently, exactly like git and rg,
/// instead of surfacing as an `io::Error` that `print!` turns into a
/// panic. Unix only — Windows has no `SIGPIPE`; its console writes fail
/// with ordinary errors there, so nothing to restore.
///
/// Runs once, at the very top of `main`, before the tokio runtime spawns
/// any worker thread (dispositions are process-wide, but only main runs
/// here — no thread can have changed it yet).
#[cfg(unix)]
fn restore_default_sigpipe() -> std::io::Result<()> {
    // SAFETY: `signal` only resets the disposition of one signal
    // (`SIGPIPE`) to the OS default; it touches no other memory or state
    // and, for SIGPIPE on a conforming unix, cannot fail (the only error
    // is EINVAL for an invalid signal number, which this is not).
    // `SIG_ERR` is the sentinel `signal` returns on failure.
    let previous = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    if previous == libc::SIG_ERR {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

async fn run(cli: &Cli) -> Result<i32, ClientError> {
    match &cli.command {
        Command::Clone {
            server,
            repo,
            ref_name,
            sparse,
        } => {
            let out = gardenbed::ops::clone_with_ref(
                &cli.state_dir,
                server,
                repo.as_deref(),
                ref_name.as_deref(),
                sparse.clone(),
            )
            .await?;
            print!("{out}");
            Ok(0)
        }

        Command::Sparse { command } => run_sparse(cli, command).await,

        Command::Status { full_scan } => {
            let bed = WorkingCopy::open(&cli.state_dir)?;
            let sparse = bed.config().sparse.clone();
            if sparse.is_empty() {
                println!("no sparse paths selected — add paths with `bed sparse add <path>`");
                return Ok(0);
            }
            let treestate = bed.treestate()?;
            let report = gardenbed::status::status_via_daemon(bed.dir(), *full_scan, || async {
                gardenbed::status::scan_status(
                    &bed.workspace_root(),
                    &sparse,
                    &treestate,
                    *full_scan,
                )
            })
            .await?;
            let rendered = report.render();
            if rendered.is_empty() {
                println!("working copy is clean — nothing modified, deleted, or untracked.");
            } else {
                print!("{rendered}");
            }
            Ok(0)
        }

        Command::Switch { cs_or_ref } => {
            CsOrRef::parse(cs_or_ref)?;
            let out = gardenbed::ops::switch(&cli.state_dir, cs_or_ref).await?;
            print!("{out}");
            Ok(0)
        }

        Command::File { path, cs } => {
            validate_path_arg(path)?;
            if let Some(cs) = cs {
                CsOrRef::parse(cs)?;
            }
            let mut out = std::io::stdout().lock();
            gardenbed::ops::file(&cli.state_dir, path, cs.as_deref(), &mut out).await?;
            Ok(0)
        }

        Command::Log { limit, cs } => {
            if let Some(cs) = cs {
                CsOrRef::parse(cs)?;
            }
            let out = gardenbed::ops::log(&cli.state_dir, *limit, cs.as_deref()).await?;
            // render_log already terminates every entry with a newline.
            if !out.is_empty() {
                print!("{out}");
            }
            Ok(0)
        }

        Command::Diff { a, b } => {
            for arg in [a, b].into_iter().flatten() {
                CsOrRef::parse(arg)?;
            }
            let out = gardenbed::ops::diff(&cli.state_dir, a.as_deref(), b.as_deref()).await?;
            if !out.is_empty() {
                print!("{out}");
            }
            Ok(0)
        }

        Command::Commit { message, author } => {
            if message.trim().is_empty() {
                return Err(ClientError::EmptyMessage);
            }
            let outcome =
                gardenbed::commit::commit(&cli.state_dir, message, author.as_deref()).await?;
            print!("{}", gardenbed::commit::render_outcome(&outcome));
            Ok(0)
        }

        Command::Rebase { new_parent } => {
            let _bed = WorkingCopy::open(&cli.state_dir)?;
            CsOrRef::parse(new_parent)?;
            let out = gardenbed::ops::rebase(&cli.state_dir, new_parent).await?;
            print!("{out}");
            Ok(0)
        }

        Command::Publish => {
            let _bed = WorkingCopy::open(&cli.state_dir)?;
            let out = gardenbed::ops::publish(&cli.state_dir).await?;
            print!("{out}");
            Ok(0)
        }

        Command::Merge {
            target,
            message,
            author,
        } => {
            let _bed = WorkingCopy::open(&cli.state_dir)?;
            CsOrRef::parse(target)?;
            let out = gardenbed::ops::merge(
                &cli.state_dir,
                target,
                message.as_deref(),
                author.as_deref(),
            )
            .await?;
            print!("{out}");
            Ok(0)
        }

        Command::Blame { path, cs } => {
            validate_path_arg(path)?;
            if let Some(cs) = cs {
                CsOrRef::parse(cs)?;
            }
            let out = gardenbed::ops::blame(&cli.state_dir, path, cs.as_deref()).await?;
            // render_blame already terminates every line with a newline.
            if !out.is_empty() {
                print!("{out}");
            }
            Ok(0)
        }
    }
}

/// `bed sparse` — `list` and `remove` are fully local; `add` records the
/// prefixes and then fetches + materializes them from the server.
async fn run_sparse(cli: &Cli, command: &SparseCommand) -> Result<i32, ClientError> {
    match command {
        SparseCommand::List => {
            let bed = WorkingCopy::open(&cli.state_dir)?;
            let sparse = &bed.config().sparse;
            if sparse.is_empty() {
                println!("no sparse paths selected — add paths with `bed sparse add <path>`");
            } else {
                for prefix in sparse {
                    println!("{prefix}");
                }
            }
            Ok(0)
        }

        SparseCommand::Add { paths, concurrency } => {
            // Normalize everything first: a bad prefix is rejected before
            // any connection is attempted.
            for raw in paths {
                sparse::normalize_prefix(raw)?;
            }
            let out = gardenbed::ops::sparse_add(&cli.state_dir, paths, *concurrency).await?;
            print!("{out}");
            Ok(0)
        }

        SparseCommand::Remove { paths } => {
            let mut bed = WorkingCopy::open(&cli.state_dir)?;
            let mut removed: Vec<String> = Vec::new();
            let mut missing: Vec<String> = Vec::new();
            for raw in paths {
                let prefix = sparse::normalize_prefix(raw)?;
                match bed.config().sparse.iter().position(|p| *p == prefix) {
                    Some(i) => {
                        bed.config_mut().sparse.remove(i);
                        removed.push(prefix);
                    }
                    None => missing.push(prefix),
                }
            }
            for prefix in &missing {
                println!("not selected: {prefix}");
            }
            if removed.is_empty() {
                return Ok(0);
            }
            bed.save_config()?;
            for prefix in &removed {
                println!("unselected {prefix}");
            }
            Ok(0)
        }
    }
}

/// Validates a path argument against the repo's flat-path rules (spec §3.1).
fn validate_path_arg(raw: &str) -> Result<(), ClientError> {
    gardenbed_model::codec::validate_path(raw).map_err(|e| ClientError::Path {
        raw: raw.to_owned(),
        reason: e.to_string(),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::restore_default_sigpipe;

    /// The startup reset must always succeed — and stay idempotent, since
    /// a main that ran it once (a real invocation) and a test process
    /// share the same entry point. If this fails, a closed pipe would
    /// panic `bed` instead of killing it silently.
    #[test]
    fn sigpipe_reset_succeeds_and_is_idempotent() {
        restore_default_sigpipe().unwrap();
        restore_default_sigpipe().unwrap();
    }
}
