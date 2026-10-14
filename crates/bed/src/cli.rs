//! The clap-derived CLI surface for `bed` — every v0.1 subcommand from
//! docs/v0.1.md §1, with complete argument parsing.
//!
//! The read-path commands (`clone`, `plot add`, `goto`, `file`, `log`,
//! `diff`) run against the arboretum; `sow`, `graft`, `harvest` and
//! `blame` are later milestones and stop at the "not wired yet" stub in
//! `main.rs` (exit 2).

use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;

use crate::rpc::DEFAULT_CONCURRENCY;

/// bed — tend your gardenbed: a sparse, server-backed working copy.
#[derive(Parser, Debug)]
#[command(
    name = "bed",
    version,
    about = "tend your gardenbed — a sparse, server-backed working copy",
    after_help = after_help()
)]
pub struct Cli {
    /// Where this bed's local state lives (config, treestate, object
    /// cache, heads).
    #[arg(long, global = true, value_name = "DIR", default_value = "./.bed")]
    pub bed_dir: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

/// Every v0.1 subcommand (docs/v0.1.md §1 scope).
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Plant a new bed: fetch an arboretum's roots, materializing nothing.
    Clone {
        /// The arboretum to plant from: `http(s)://host:port` or
        /// `unix:///path/to/socket`.
        server: String,
        /// Repo to tend; the server's choice when omitted.
        #[arg(long)]
        repo: Option<String>,
        /// Plot prefixes to plant immediately (repeat or list several).
        #[arg(long, value_name = "PATH", num_args = 1..)]
        plot: Vec<String>,
    },

    /// Tend the plot: which path prefixes this bed materializes.
    Plot {
        #[command(subcommand)]
        command: PlotCommand,
    },

    /// Move the bed to a changeset id or ref.
    Goto {
        /// A 64-hex changeset id, or a ref name.
        cs_or_ref: String,
    },

    /// Compare the working files under the plot against the treestate.
    ///
    /// Works fully offline: mtime/size hints guide the scan; only suspects
    /// are hashed.
    Status {
        /// Hash every tracked file, ignoring the hints.
        #[arg(long)]
        full_scan: bool,
    },

    /// Print one file's content at a changeset (or the current one).
    File {
        /// The file's repo-relative path.
        path: String,
        /// Changeset id or ref to read from (default: the current one).
        #[arg(long = "cs", value_name = "ID-OR-REF")]
        cs: Option<String>,
    },

    /// First-parent history from a changeset or ref, newest first.
    Log {
        /// Show at most N changesets (default: all).
        #[arg(long, value_name = "N")]
        limit: Option<u32>,
        /// Where to start (default: the current changeset).
        #[arg(long = "cs", value_name = "ID-OR-REF")]
        cs: Option<String>,
    },

    /// Diff two changesets (default: the current changeset vs its first
    /// parent).
    Diff {
        /// The older side: changeset id or ref. With `b` omitted: diffs
        /// against this changeset's first parent.
        a: Option<String>,
        /// The newer side: changeset id or ref.
        b: Option<String>,
    },

    /// Sow the plot's changes as a new changeset.
    Sow {
        /// The changeset's message.
        #[arg(short = 'm', long, value_name = "MESSAGE")]
        message: String,
        /// Author as one `Name <email>` identity (default: the
        /// GARDENBED_AUTHOR environment variable, else the bed's own).
        #[arg(long, value_name = "NAME <EMAIL>")]
        author: Option<String>,
    },

    /// Replay the current changeset onto a new parent (server-side rebase).
    Graft {
        /// The new parent: changeset id or ref.
        new_parent: String,
    },

    /// Fast-forward `main` to the current changeset.
    Harvest,

    /// First-parent lineage blame of a path (docs/v0.1.md D7).
    Blame {
        /// The file's repo-relative path.
        path: String,
    },
}

/// Subcommands of `bed plot`.
#[derive(Subcommand, Debug)]
pub enum PlotCommand {
    /// Add path prefixes to the plot (directory or exact file) and
    /// materialize them at the current changeset.
    Add {
        /// Repo-relative prefixes; `.` plants the whole garden.
        #[arg(required = true, num_args = 1.., value_name = "PATH")]
        paths: Vec<String>,
        /// Bound on concurrent object fetches from the arboretum.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_CONCURRENCY)]
        concurrency: usize,
    },
    /// Remove path prefixes from the plot.
    Remove {
        /// The prefixes to unplant.
        #[arg(required = true, num_args = 1.., value_name = "PATH")]
        paths: Vec<String>,
    },
    /// List the plot's path prefixes.
    List,
}

/// The layout tour printed under `bed --help`.
fn after_help() -> &'static str {
    "local state (under --bed-dir, default ./.bed):\n  \
     config.toml   server, repo, plot, current changeset\n  \
     treestate     sqlite index of the materialized plot\n  \
     objects/      local content-addressed cache (loam)\n  \
     heads         local stack tips\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    fn parsed(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("bed").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn cli_shape_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn clone_parses_server_repo_and_plots() {
        let cli = parsed(&["clone", "http://host:7400"]);
        match cli.command {
            Command::Clone { server, repo, plot } => {
                assert_eq!(server, "http://host:7400");
                assert_eq!(repo, None);
                assert!(plot.is_empty());
            }
            other => panic!("expected Clone, got {other:?}"),
        }

        let cli = parsed(&["clone", "unix:///tmp/a.sock", "--repo", "x", "--plot", "src", "docs"]);
        match cli.command {
            Command::Clone { server, repo, plot } => {
                assert_eq!(server, "unix:///tmp/a.sock");
                assert_eq!(repo.as_deref(), Some("x"));
                assert_eq!(plot, vec!["src", "docs"]);
            }
            other => panic!("expected Clone, got {other:?}"),
        }

        // Repeated --plot flags accumulate too.
        let cli = parsed(&["clone", "s", "--plot", "a", "--plot", "b"]);
        match cli.command {
            Command::Clone { plot, .. } => assert_eq!(plot, vec!["a", "b"]),
            other => panic!("expected Clone, got {other:?}"),
        }
    }

    #[test]
    fn plot_subcommands_parse() {
        match parsed(&["plot", "add", "src", "docs"]).command {
            Command::Plot { command: PlotCommand::Add { paths, .. } } => {
                assert_eq!(paths, vec!["src", "docs"])
            }
            other => panic!("expected plot add, got {other:?}"),
        }
        match parsed(&["plot", "add", "--concurrency", "3", "src"]).command {
            Command::Plot { command: PlotCommand::Add { paths, concurrency } } => {
                assert_eq!(paths, vec!["src"]);
                assert_eq!(concurrency, 3);
            }
            other => panic!("expected plot add, got {other:?}"),
        }
        match parsed(&["plot", "remove", "src"]).command {
            Command::Plot { command: PlotCommand::Remove { paths } } => {
                assert_eq!(paths, vec!["src"])
            }
            other => panic!("expected plot remove, got {other:?}"),
        }
        match parsed(&["plot", "list"]).command {
            Command::Plot { command: PlotCommand::List } => {}
            other => panic!("expected plot list, got {other:?}"),
        }
        // `plot add` with no paths is a usage error.
        assert!(Cli::try_parse_from(["bed", "plot", "add"]).is_err());
    }

    #[test]
    fn goto_status_file_log_parse() {
        match parsed(&["goto", "main"]).command {
            Command::Goto { cs_or_ref } => assert_eq!(cs_or_ref, "main"),
            other => panic!("expected Goto, got {other:?}"),
        }
        match parsed(&["status", "--full-scan"]).command {
            Command::Status { full_scan } => assert!(full_scan),
            other => panic!("expected Status, got {other:?}"),
        }
        match parsed(&["status"]).command {
            Command::Status { full_scan } => assert!(!full_scan),
            other => panic!("expected Status, got {other:?}"),
        }
        match parsed(&["file", "src/a.rs", "--cs", "main"]).command {
            Command::File { path, cs } => {
                assert_eq!(path, "src/a.rs");
                assert_eq!(cs.as_deref(), Some("main"));
            }
            other => panic!("expected File, got {other:?}"),
        }
        match parsed(&["log", "--limit", "5", "--cs", "HEAD"]).command {
            Command::Log { limit, cs } => {
                assert_eq!(limit, Some(5));
                assert_eq!(cs.as_deref(), Some("HEAD"));
            }
            other => panic!("expected Log, got {other:?}"),
        }
    }

    #[test]
    fn diff_sow_graft_harvest_blame_parse() {
        match parsed(&["diff"]).command {
            Command::Diff { a, b } => {
                assert!(a.is_none() && b.is_none());
            }
            other => panic!("expected Diff, got {other:?}"),
        }
        match parsed(&["diff", "abc123", "def456"]).command {
            Command::Diff { a, b } => {
                assert_eq!(a.as_deref(), Some("abc123"));
                assert_eq!(b.as_deref(), Some("def456"));
            }
            other => panic!("expected Diff, got {other:?}"),
        }
        match parsed(&["sow", "-m", "tend the rows", "--author", "G. Gardener <g@bed.dev>"])
            .command
        {
            Command::Sow { message, author } => {
                assert_eq!(message, "tend the rows");
                assert_eq!(author.as_deref(), Some("G. Gardener <g@bed.dev>"));
            }
            other => panic!("expected Sow, got {other:?}"),
        }
        // The author is optional (env var / bed default fill in).
        match parsed(&["sow", "-m", "tend the rows"]).command {
            Command::Sow { author, .. } => assert!(author.is_none()),
            other => panic!("expected Sow, got {other:?}"),
        }
        match parsed(&["graft", "main"]).command {
            Command::Graft { new_parent } => assert_eq!(new_parent, "main"),
            other => panic!("expected Graft, got {other:?}"),
        }
        match parsed(&["harvest"]).command {
            Command::Harvest => {}
            other => panic!("expected Harvest, got {other:?}"),
        }
        match parsed(&["blame", "src/lib.rs"]).command {
            Command::Blame { path } => assert_eq!(path, "src/lib.rs"),
            other => panic!("expected Blame, got {other:?}"),
        }
        // `-m` is required.
        assert!(Cli::try_parse_from(["bed", "sow"]).is_err());
        // `--author` takes exactly one value.
        assert!(Cli::try_parse_from(["bed", "sow", "-m", "x", "--author"]).is_err());
    }

    #[test]
    fn bed_dir_is_a_global_override() {
        let cli = parsed(&["--bed-dir", "/tmp/other-bed", "status"]);
        assert_eq!(cli.bed_dir, PathBuf::from("/tmp/other-bed"));
        // ...and works after the subcommand too.
        let cli = parsed(&["status", "--bed-dir", "/tmp/other-bed"]);
        assert_eq!(cli.bed_dir, PathBuf::from("/tmp/other-bed"));
        let cli = parsed(&["status"]);
        assert_eq!(cli.bed_dir, PathBuf::from("./.bed"));
    }
}
