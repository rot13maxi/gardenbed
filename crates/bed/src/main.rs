//! `bed` — the binary. Parses the CLI and dispatches; all local state
//! lives in the `gardenbed` library so later waves (RPC read path, MCP)
//! can reuse it unchanged.
//!
//! Commands that need the arboretum parse and validate fully, then stop at
//! [`todo_rpc`] until the server-facing half of M4 lands.

use clap::Parser as _;

use gardenbed::bed::Bed;
use gardenbed::cli::Cli;
use gardenbed::cli::Command;
use gardenbed::cli::PlotCommand;
use gardenbed::error::BedError;
use gardenbed::plot;
use gardenbed::revspec::CsOrRef;
use gardenbed::status;

fn main() {
    let cli = Cli::parse();
    std::process::exit(match run(&cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("bed: {e}");
            1
        }
    });
}

/// Exit code for "everything is parsed, but the server half is not wired
/// yet" — distinct from errors (1).
const NOT_WIRED: i32 = 2;

/// The single stub for every command that needs the arboretum. Argument
/// parsing and local validation are complete; the RPC call itself is a
/// later wave.
///
/// Extension points for the read-path agent: each match arm below that
/// ends in `todo_rpc("<command>")` is where the client wiring goes
/// (`gardenbed_protocol::client::GardenbedClient`).
fn todo_rpc(command: &str) -> ! {
    eprintln!(
        "bed {command}: requires a running arboretum — not wired yet."
    );
    eprintln!(
        "The server-facing half of this command lands in a later wave. \
         The local bed state (.bed/config.toml, .bed/treestate, .bed/objects, \
         .bed/heads) is in place, and `bed status` and `bed plot` work offline today."
    );
    std::process::exit(NOT_WIRED);
}

fn run(cli: &Cli) -> Result<i32, BedError> {
    match &cli.command {
        Command::Clone { server: _, repo, plot } => {
            // Validate everything we can before the (future) network hop:
            // repo name, and every requested plot prefix.
            if repo.as_ref().is_some_and(|r| r.trim().is_empty()) {
                return Err(BedError::ConfigInvalid("repo name is empty".into()));
            }
            for raw in plot {
                plot::normalize_prefix(raw)?;
            }
            todo_rpc("clone")
        }

        Command::Plot { command } => run_plot(cli, command),

        Command::Status { full_scan } => {
            let bed = Bed::open(&cli.bed_dir)?;
            let plot = bed.config().plot.clone();
            if plot.is_empty() {
                println!("the plot is empty — plant a prefix with `bed plot add <path>`");
                return Ok(0);
            }
            let treestate = bed.treestate()?;
            let report = status::scan_status(
                &bed.garden_root(),
                &plot,
                &treestate,
                *full_scan,
            )?;
            let rendered = report.render();
            if rendered.is_empty() {
                println!("the plot is clean — nothing modified, deleted, or untracked.");
            } else {
                print!("{rendered}");
            }
            Ok(0)
        }

        Command::Goto { cs_or_ref } => {
            let _bed = Bed::open(&cli.bed_dir)?;
            CsOrRef::parse(cs_or_ref)?;
            todo_rpc("goto")
        }

        Command::File { path, cs } => {
            let _bed = Bed::open(&cli.bed_dir)?;
            validate_path_arg(path)?;
            if let Some(cs) = cs {
                CsOrRef::parse(cs)?;
            }
            todo_rpc("file")
        }

        Command::Log { limit: _, cs } => {
            let _bed = Bed::open(&cli.bed_dir)?;
            if let Some(cs) = cs {
                CsOrRef::parse(cs)?;
            }
            todo_rpc("log")
        }

        Command::Diff { a, b } => {
            let _bed = Bed::open(&cli.bed_dir)?;
            for arg in [a, b].into_iter().flatten() {
                CsOrRef::parse(arg)?;
            }
            todo_rpc("diff")
        }

        Command::Sow { message, author } => {
            let _bed = Bed::open(&cli.bed_dir)?;
            if message.trim().is_empty() {
                return Err(BedError::ConfigInvalid(
                    "the message is empty — sow with -m <message>".into(),
                ));
            }
            if let Some(name_email) = author {
                for field in name_email {
                    if field.trim().is_empty() {
                        return Err(BedError::ConfigInvalid(
                            "--author needs a name and an email, both non-empty".into(),
                        ));
                    }
                }
            }
            todo_rpc("sow")
        }

        Command::Graft { new_parent } => {
            let _bed = Bed::open(&cli.bed_dir)?;
            CsOrRef::parse(new_parent)?;
            todo_rpc("graft")
        }

        Command::Harvest => {
            let _bed = Bed::open(&cli.bed_dir)?;
            todo_rpc("harvest")
        }

        Command::Blame { path } => {
            let _bed = Bed::open(&cli.bed_dir)?;
            validate_path_arg(path)?;
            todo_rpc("blame")
        }
    }
}

/// `bed plot` — fully local: reads and writes `.bed/config.toml`.
fn run_plot(cli: &Cli, command: &PlotCommand) -> Result<i32, BedError> {
    match command {
        PlotCommand::List => {
            let bed = Bed::open(&cli.bed_dir)?;
            let plot = &bed.config().plot;
            if plot.is_empty() {
                println!("the plot is empty — plant a prefix with `bed plot add <path>`");
            } else {
                for prefix in plot {
                    println!("{prefix}");
                }
            }
            Ok(0)
        }

        PlotCommand::Add { paths } => {
            let mut bed = Bed::open(&cli.bed_dir)?;
            let mut added: Vec<String> = Vec::new();
            for raw in paths {
                let prefix = plot::normalize_prefix(raw)?;
                if !bed.config().plot.contains(&prefix) {
                    bed.config_mut().plot.push(prefix.clone());
                    added.push(prefix);
                }
            }
            if added.is_empty() {
                println!("nothing to add — those prefixes are already in the plot");
                return Ok(0);
            }
            bed.save_config()?;
            for prefix in &added {
                println!("plotted {prefix}");
            }
            println!(
                "materializing these prefixes needs a running arboretum — not wired yet; \
                 until then `bed status` will show their files as untracked"
            );
            Ok(0)
        }

        PlotCommand::Remove { paths } => {
            let mut bed = Bed::open(&cli.bed_dir)?;
            let mut removed: Vec<String> = Vec::new();
            let mut missing: Vec<String> = Vec::new();
            for raw in paths {
                let prefix = plot::normalize_prefix(raw)?;
                match bed.config().plot.iter().position(|p| *p == prefix) {
                    Some(i) => {
                        bed.config_mut().plot.remove(i);
                        removed.push(prefix);
                    }
                    None => missing.push(prefix),
                }
            }
            for prefix in &missing {
                println!("not plotted: {prefix}");
            }
            if removed.is_empty() {
                return Ok(0);
            }
            bed.save_config()?;
            for prefix in &removed {
                println!("unplotted {prefix}");
            }
            Ok(0)
        }
    }
}

/// Validates a path argument against the repo's flat-path rules (spec §3.1).
fn validate_path_arg(raw: &str) -> Result<(), BedError> {
    gardenbed_model::codec::validate_path(raw).map_err(|e| BedError::Path {
        raw: raw.to_owned(),
        reason: e.to_string(),
    })
}
