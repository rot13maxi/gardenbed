//! `bed` — the binary. Parses the CLI and dispatches; all behavior lives
//! in the `gardenbed` library so other surfaces (the MCP server) reuse it
//! unchanged.
//!
//! Every v0.1 command is wired: the read path (`clone`, `plot add`,
//! `goto`, `file`, `log`, `diff`, `blame`) and the write path (`sow`,
//! `graft`, `harvest`) run against the arboretum; `status` and `plot
//! list/remove` are fully offline.

use clap::Parser as _;

use gardenbed::bed::Bed;
use gardenbed::cli::Cli;
use gardenbed::cli::Command;
use gardenbed::cli::PlotCommand;
use gardenbed::error::BedError;
use gardenbed::plot;
use gardenbed::revspec::CsOrRef;

fn main() {
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

async fn run(cli: &Cli) -> Result<i32, BedError> {
    match &cli.command {
        Command::Clone {
            server,
            repo,
            ref_name,
            plot,
        } => {
            let out = gardenbed::ops::clone_with_ref(
                &cli.bed_dir,
                server,
                repo.as_deref(),
                ref_name.as_deref(),
                plot.clone(),
            )
            .await?;
            print!("{out}");
            Ok(0)
        }

        Command::Plot { command } => run_plot(cli, command).await,

        Command::Status { full_scan } => {
            let bed = Bed::open(&cli.bed_dir)?;
            let plot = bed.config().plot.clone();
            if plot.is_empty() {
                println!("the plot is empty — plant a prefix with `bed plot add <path>`");
                return Ok(0);
            }
            let treestate = bed.treestate()?;
            let report =
                gardenbed::status::scan_status(&bed.garden_root(), &plot, &treestate, *full_scan)?;
            let rendered = report.render();
            if rendered.is_empty() {
                println!("the plot is clean — nothing modified, deleted, or untracked.");
            } else {
                print!("{rendered}");
            }
            Ok(0)
        }

        Command::Goto { cs_or_ref } => {
            CsOrRef::parse(cs_or_ref)?;
            let out = gardenbed::ops::goto(&cli.bed_dir, cs_or_ref).await?;
            print!("{out}");
            Ok(0)
        }

        Command::File { path, cs } => {
            validate_path_arg(path)?;
            if let Some(cs) = cs {
                CsOrRef::parse(cs)?;
            }
            let mut out = std::io::stdout().lock();
            gardenbed::ops::file(&cli.bed_dir, path, cs.as_deref(), &mut out).await?;
            Ok(0)
        }

        Command::Log { limit, cs } => {
            if let Some(cs) = cs {
                CsOrRef::parse(cs)?;
            }
            let out = gardenbed::ops::log(&cli.bed_dir, *limit, cs.as_deref()).await?;
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
            let out = gardenbed::ops::diff(&cli.bed_dir, a.as_deref(), b.as_deref()).await?;
            if !out.is_empty() {
                print!("{out}");
            }
            Ok(0)
        }

        Command::Sow { message, author } => {
            if message.trim().is_empty() {
                return Err(BedError::EmptyMessage);
            }
            let outcome = gardenbed::sow::sow(&cli.bed_dir, message, author.as_deref()).await?;
            print!("{}", gardenbed::sow::render_outcome(&outcome));
            Ok(0)
        }

        Command::Graft { new_parent } => {
            let _bed = Bed::open(&cli.bed_dir)?;
            CsOrRef::parse(new_parent)?;
            let out = gardenbed::ops::graft(&cli.bed_dir, new_parent).await?;
            print!("{out}");
            Ok(0)
        }

        Command::Harvest => {
            let _bed = Bed::open(&cli.bed_dir)?;
            let out = gardenbed::ops::harvest(&cli.bed_dir).await?;
            print!("{out}");
            Ok(0)
        }

        Command::Blame { path, cs } => {
            validate_path_arg(path)?;
            if let Some(cs) = cs {
                CsOrRef::parse(cs)?;
            }
            let out = gardenbed::ops::blame(&cli.bed_dir, path, cs.as_deref()).await?;
            // render_blame already terminates every line with a newline.
            if !out.is_empty() {
                print!("{out}");
            }
            Ok(0)
        }
    }
}

/// `bed plot` — `list` and `remove` are fully local; `add` records the
/// prefixes and then fetches + materializes them from the arboretum.
async fn run_plot(cli: &Cli, command: &PlotCommand) -> Result<i32, BedError> {
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

        PlotCommand::Add { paths, concurrency } => {
            // Normalize everything first: a bad prefix is rejected before
            // any connection is attempted.
            for raw in paths {
                plot::normalize_prefix(raw)?;
            }
            let out = gardenbed::ops::plot_add(&cli.bed_dir, paths, *concurrency).await?;
            print!("{out}");
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
