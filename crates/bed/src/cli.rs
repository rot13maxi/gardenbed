//! The clap-derived CLI surface for `bed` — every v0.1 subcommand from
//! docs/v0.1.md §1, with complete argument parsing.
//!
//! All commands are wired: the read path (`clone`, `sparse add`, `switch`,
//! `file`, `log`, `diff`, `blame`), the write path (`commit`, `rebase`,
//! `publish`), and the fully offline ones (`status`, `sparse list/remove`).

use std::path::PathBuf;

use clap::Parser;
use clap::Subcommand;

use crate::rpc::DEFAULT_CONCURRENCY;

/// bed — source control with sparse, server-backed working copies.
#[derive(Parser, Debug)]
#[command(
    name = "bed",
    version,
    about = "Source control with sparse, server-backed working copies",
    after_help = after_help()
)]
pub struct Cli {
    /// Where this working copy's local state lives (config, index, object
    /// cache, heads).
    #[arg(long, global = true, value_name = "DIR", default_value = "./.bed")]
    pub state_dir: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

/// Every v0.1 subcommand (docs/v0.1.md §1 scope).
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a working copy connected to a server; materialize no files.
    Clone {
        /// Repository server endpoint: `http(s)://host:port` or
        /// `unix:///path/to/socket`.
        server: String,
        /// Repository name (default: default).
        #[arg(long)]
        repo: Option<String>,
        /// Starting ref. Defaults to `main`, then `master` or `trunk`.
        #[arg(long = "ref", value_name = "REF")]
        ref_name: Option<String>,
        /// Sparse paths to record; fetch files afterward with `bed sparse add`.
        #[arg(long, value_name = "PATH", num_args = 1..)]
        sparse: Vec<String>,
    },

    /// Select which repository paths to materialize in the working copy.
    Sparse {
        #[command(subcommand)]
        command: SparseCommand,
    },

    /// Switch the working copy to a changeset or ref.
    Switch {
        /// A 64-hex changeset id, or a ref name.
        cs_or_ref: String,
    },

    /// Show local changes under the selected sparse paths.
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

    /// Show changeset history, newest first.
    Log {
        /// Show at most N changesets (default: all). Like `git log`, 0
        /// means "no limit", not "show none".
        #[arg(long, value_name = "N")]
        limit: Option<u32>,
        /// Where to start (default: the current changeset).
        #[arg(long = "cs", value_name = "ID-OR-REF")]
        cs: Option<String>,
    },

    /// Diff the working copy against the current changeset (when dirty),
    /// or two changesets (default: the current one vs its first parent).
    /// Every diff prints a header naming what is being diffed.
    Diff {
        /// The older side: changeset id or ref. With `b` omitted, diffs
        /// against this changeset's first parent.
        a: Option<String>,
        /// The newer side: changeset id or ref.
        b: Option<String>,
    },

    /// Record local changes as a changeset on the server.
    Commit {
        /// The changeset's message.
        #[arg(short = 'm', long, value_name = "MESSAGE")]
        message: String,
        /// Author as one `Name <email>` identity (default: the
        /// GARDENBED_AUTHOR environment variable, else the working copy's own).
        #[arg(long, value_name = "NAME <EMAIL>")]
        author: Option<String>,
    },

    /// Rebase the current changeset onto another changeset or ref.
    Rebase {
        /// The new parent: changeset id or ref.
        new_parent: String,
    },

    /// Publish the current changeset by fast-forwarding a ref (default
    /// `main`; a named ref is the lane-agent form).
    Publish {
        /// The ref to fast-forward (default: `main`). `main` must use the
        /// plain form.
        #[arg(long = "ref", value_name = "REF")]
        ref_name: Option<String>,
        /// Permit a non-fast-forward (rewind) move of a named ref.
        #[arg(long)]
        force: bool,
    },

    /// Show first-parent line attribution, following renames.
    Blame {
        /// The file's repo-relative path.
        path: String,
        /// Changeset id or ref to blame at (default: the current one).
        #[arg(long = "cs", value_name = "ID-OR-REF")]
        cs: Option<String>,
    },
}

/// Subcommands of `bed sparse`.
#[derive(Subcommand, Debug)]
pub enum SparseCommand {
    /// Select repository paths (directories or files) and
    /// materialize them at the current changeset.
    Add {
        /// Repository-relative paths; `.` selects the whole repository.
        #[arg(required = true, num_args = 1.., value_name = "PATH")]
        paths: Vec<String>,
        /// Bound on concurrent object fetches from the server.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_CONCURRENCY)]
        concurrency: usize,
    },
    /// Stop selecting paths; keep existing working files.
    Remove {
        /// The path prefixes to stop selecting (working files are kept).
        #[arg(required = true, num_args = 1.., value_name = "PATH")]
        paths: Vec<String>,
    },
    /// List selected sparse path prefixes.
    List,
}

/// The layout tour printed under `bed --help`.
fn after_help() -> &'static str {
    "local state (under --state-dir, default ./.bed):\n  \
     config.toml   server, repository, sparse paths, current changeset\n  \
     index.sqlite3 SQLite index of tracked files\n  \
     objects/      local content-addressed object cache\n  \
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
    fn retired_command_names_and_flags_are_rejected() {
        for command in ["sow", "graft", "harvest", "plot", "goto"] {
            assert!(Cli::try_parse_from(["bed", command]).is_err(), "{command}");
        }
        assert!(Cli::try_parse_from(["bed", "--bed-dir", "x", "status"]).is_err());
        assert!(
            Cli::try_parse_from(["bed", "clone", "http://host:7400", "--plot", "src"]).is_err()
        );
    }

    #[test]
    fn cli_shape_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn clone_parses_server_repo_and_sparses() {
        let cli = parsed(&["clone", "http://host:7400"]);
        match cli.command {
            Command::Clone {
                server,
                repo,
                ref_name,
                sparse,
            } => {
                assert_eq!(server, "http://host:7400");
                assert_eq!(repo, None);
                assert_eq!(ref_name, None);
                assert!(sparse.is_empty());
            }
            other => panic!("expected Clone, got {other:?}"),
        }

        let cli = parsed(&[
            "clone",
            "unix:///tmp/a.sock",
            "--repo",
            "x",
            "--sparse",
            "src",
            "docs",
        ]);
        match cli.command {
            Command::Clone {
                server,
                repo,
                sparse,
                ..
            } => {
                assert_eq!(server, "unix:///tmp/a.sock");
                assert_eq!(repo.as_deref(), Some("x"));
                assert_eq!(sparse, vec!["src", "docs"]);
            }
            other => panic!("expected Clone, got {other:?}"),
        }

        // Repeated --sparse flags accumulate too.
        let cli = parsed(&["clone", "s", "--sparse", "a", "--sparse", "b"]);
        match cli.command {
            Command::Clone { sparse, .. } => assert_eq!(sparse, vec!["a", "b"]),
            other => panic!("expected Clone, got {other:?}"),
        }

        // `--ref` names the starting ref exactly.
        let cli = parsed(&["clone", "http://host:7400", "--ref", "master"]);
        match cli.command {
            Command::Clone { ref_name, .. } => assert_eq!(ref_name.as_deref(), Some("master")),
            other => panic!("expected Clone, got {other:?}"),
        }
    }

    #[test]
    fn sparse_subcommands_parse() {
        match parsed(&["sparse", "add", "src", "docs"]).command {
            Command::Sparse {
                command: SparseCommand::Add { paths, .. },
            } => {
                assert_eq!(paths, vec!["src", "docs"])
            }
            other => panic!("expected sparse add, got {other:?}"),
        }
        match parsed(&["sparse", "add", "--concurrency", "3", "src"]).command {
            Command::Sparse {
                command: SparseCommand::Add { paths, concurrency },
            } => {
                assert_eq!(paths, vec!["src"]);
                assert_eq!(concurrency, 3);
            }
            other => panic!("expected sparse add, got {other:?}"),
        }
        match parsed(&["sparse", "remove", "src"]).command {
            Command::Sparse {
                command: SparseCommand::Remove { paths },
            } => {
                assert_eq!(paths, vec!["src"])
            }
            other => panic!("expected sparse remove, got {other:?}"),
        }
        match parsed(&["sparse", "list"]).command {
            Command::Sparse {
                command: SparseCommand::List,
            } => {}
            other => panic!("expected sparse list, got {other:?}"),
        }
        // `sparse add` with no paths is a usage error.
        assert!(Cli::try_parse_from(["bed", "sparse", "add"]).is_err());
    }

    #[test]
    fn switch_status_file_log_parse() {
        match parsed(&["switch", "main"]).command {
            Command::Switch { cs_or_ref } => assert_eq!(cs_or_ref, "main"),
            other => panic!("expected Switch, got {other:?}"),
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
    fn diff_commit_rebase_publish_blame_parse() {
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
        match parsed(&[
            "commit",
            "-m",
            "tend the rows",
            "--author",
            "G. Gardener <g@bed.dev>",
        ])
        .command
        {
            Command::Commit { message, author } => {
                assert_eq!(message, "tend the rows");
                assert_eq!(author.as_deref(), Some("G. Gardener <g@bed.dev>"));
            }
            other => panic!("expected Commit, got {other:?}"),
        }
        // The author is optional (env var / bed default fill in).
        match parsed(&["commit", "-m", "tend the rows"]).command {
            Command::Commit { author, .. } => assert!(author.is_none()),
            other => panic!("expected Commit, got {other:?}"),
        }
        match parsed(&["rebase", "main"]).command {
            Command::Rebase { new_parent } => assert_eq!(new_parent, "main"),
            other => panic!("expected Rebase, got {other:?}"),
        }
        match parsed(&["publish"]).command {
            Command::Publish { ref_name, force } => {
                assert!(ref_name.is_none());
                assert!(!force);
            }
            other => panic!("expected Publish, got {other:?}"),
        }
        match parsed(&["publish", "--ref", "feat/scale-out", "--force"]).command {
            Command::Publish { ref_name, force } => {
                assert_eq!(ref_name.as_deref(), Some("feat/scale-out"));
                assert!(force);
            }
            other => panic!("expected Publish, got {other:?}"),
        }
        match parsed(&["blame", "src/lib.rs"]).command {
            Command::Blame { path, cs } => {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(cs, None);
            }
            other => panic!("expected Blame, got {other:?}"),
        }
        match parsed(&["blame", "src/lib.rs", "--cs", "main"]).command {
            Command::Blame { path, cs } => {
                assert_eq!(path, "src/lib.rs");
                assert_eq!(cs.as_deref(), Some("main"));
            }
            other => panic!("expected Blame, got {other:?}"),
        }
        // `-m` is required.
        assert!(Cli::try_parse_from(["bed", "commit"]).is_err());
        // `--author` takes exactly one value.
        assert!(Cli::try_parse_from(["bed", "commit", "-m", "x", "--author"]).is_err());
    }

    #[test]
    fn state_dir_is_a_global_override() {
        let cli = parsed(&["--state-dir", "/tmp/other-bed", "status"]);
        assert_eq!(cli.state_dir, PathBuf::from("/tmp/other-bed"));
        // ...and works after the subcommand too.
        let cli = parsed(&["status", "--state-dir", "/tmp/other-bed"]);
        assert_eq!(cli.state_dir, PathBuf::from("/tmp/other-bed"));
        let cli = parsed(&["status"]);
        assert_eq!(cli.state_dir, PathBuf::from("./.bed"));
    }
}
