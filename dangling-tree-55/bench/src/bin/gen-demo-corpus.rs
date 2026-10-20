//! `gen-demo-corpus` — builds the synthetic git repository the v0.1 north
//! star demo (`demo/run-v0.1.sh`) imports when no real corpus is pointed at
//! it via `$GARDENBED_DEMO_CORPUS`.
//!
//! The output is a *real* git repository (a single `git fast-import` stream,
//! the same writer git_bridge export uses), deterministic byte-for-byte for a
//! given commit count, with the shapes the demo asserts on:
//!
//! * ≥ 50,000 commits (default) on `refs/heads/main`, timestamps strictly
//!   increasing, eight authors with mixed timezone offsets;
//! * a `big/` subtree with ≥ 500 files (1020 by default: 800 flat files,
//!   200 nested under `big/d0..d7/`, ten executable scripts, ten symlinks)
//!   plus the `data/` symlink targets;
//! * a `story.md` rewritten every commit as a sliding 40-line window — one
//!   added line per commit — so first-parent blame has ~40 well-defined
//!   ranges to check against `git blame --first-parent`;
//! * a merge every 500 commits (a two-commit side branch folded back into
//!   main) and an occasional same-content rename (git's `-M` detects it).
//!
//! The stream ends with `git fsck` (the demo's step-1 precondition) and
//! prints a machine-readable summary: the head sha and the commit count.
//!
//! ```sh
//! cargo run -p gardenbed-bench --bin gen-demo-corpus -- --out /tmp/demo-fixture
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clap::Parser;

/// Sliding-window size of `story.md`: every commit adds one line and drops
/// the oldest, so blame over it has at most this many coalesced ranges.
const STORY_LINES: usize = 40;

/// The eight fixture authors. The timezone spread exercises raw tz-offset
/// preservation on both sides of the git bridge.
const AUTHORS: &[(&str, &str, i32)] = &[
    ("Ada Linden", "ada@garden.example", 0),
    ("Bramble Vine", "bramble@garden.example", 120),
    ("Cedar Thorn", "cedar@garden.example", -420),
    ("Dahlia Root", "dahlia@garden.example", 330),
    ("Elder Sprout", "elder@garden.example", -180),
    ("Fern Bloom", "fern@garden.example", 60),
    ("Gopher Mulch", "gopher@garden.example", 660),
    ("Hazel Vine", "hazel@garden.example", 540),
];

/// Command-line knobs. Defaults make a ≥ 50k-commit demo corpus.
#[derive(Debug, Parser)]
#[command(
    name = "gen-demo-corpus",
    about = "generate the synthetic demo git repository (git fast-import based)"
)]
struct Args {
    /// Destination repository directory; must not already exist (or must be
    /// an already-generated corpus with the exact commit count).
    #[arg(long)]
    out: PathBuf,
    /// Number of commits to write (≥ 1; the demo needs ≥ 50,000).
    #[arg(long, default_value_t = 50_000)]
    commits: u64,
    /// Flat files under `big/` (plus 200 nested, 10 exec, 10 symlinks).
    #[arg(long, default_value_t = 800)]
    big_files: u64,
    /// A merge into `main` every N commits (minimum 2).
    #[arg(long, default_value_t = 500)]
    merge_every: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.commits == 0 {
        return Err("--commits must be at least 1".into());
    }

    // Idempotency: an existing corpus with the exact commit count is reused
    // as-is (the demo reruns must not rebuild the fixture every time).
    if args.out.exists() {
        let have = rev_list_count(&args.out)?;
        if have == args.commits {
            let head = rev_parse_head(&args.out)?;
            println!("gen-demo-corpus: reuse {head} ({have} commits)");
            return Ok(());
        }
        return Err(format!(
            "{} exists with {have} commits (wanted {}); remove it first",
            args.out.display(),
            args.commits
        )
        .into());
    }

    let t0 = std::time::Instant::now();
    generate(&args)?;
    let head = rev_parse_head(&args.out)?;
    let count = rev_list_count(&args.out)?;
    println!(
        "gen-demo-corpus: {} in {:.1?} ({} commits, head {head})",
        args.out.display(),
        t0.elapsed(),
        count
    );
    Ok(())
}

/// Writes the whole repository: `git init`, one `git fast-import` stream,
/// `git fsck`.
fn generate(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let out = &args.out;
    let mut init = Command::new("git");
    init.args(["init", "-q", "-b", "main"])
        .arg(out)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0");
    let status = init.status()?;
    if !status.success() {
        return Err("git init failed".into());
    }

    let mut child = Command::new("git")
        .current_dir(out)
        .args(["fast-import", "--quiet"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stream = Stream {
        w: std::io::BufWriter::with_capacity(
            1 << 20,
            child.stdin.take().expect("fast-import stdin"),
        ),
        mark: 0,
        big: args.big_files,
    };

    // The first commit plants the whole tree; every later one touches a
    // couple of files. `i` counts commits 1..=N.
    let total = args.commits;
    let mut main_mark = 0u64;
    let mut story: Vec<String> = Vec::with_capacity(STORY_LINES);
    for i in 1..=total {
        let author = ((i - 1) % AUTHORS.len() as u64) as usize;
        let secs = 1_600_000_000u64 + i * 7;
        let merge = args.merge_every.max(2);
        let is_merge = i > 1 && i % merge == 0;
        // The story gains one line per commit; the sliding window keeps the
        // file bounded so blame over it has a small, well-defined answer.
        story.push(format!(
            "line {i}: {} tended the repository",
            AUTHORS[author].0
        ));

        main_mark = if is_merge {
            // Fold a one-commit side branch back into main: `reset side` to
            // the current main tip, commit side work, then a real merge.
            stream.reset("refs/heads/side", main_mark)?;
            let touched = round_robin_paths(i, 2, args.big_files);
            let side_mark = stream.commit(
                "refs/heads/side",
                Some(main_mark),
                None,
                author,
                secs + 1,
                &format!("side work {i}\n"),
            )?;
            stream.file_commands(&touched)?;
            stream.end_commit()?;
            let touched = round_robin_paths(i, 1, args.big_files);
            let mark = stream.commit(
                "refs/heads/main",
                Some(main_mark),
                Some(side_mark),
                author,
                secs + 2,
                &format!("merge side branch at commit {i}\n"),
            )?;
            // The merge also advances the story (first-parent blame
            // attributes the merged-in state to the merge commit).
            story.push(format!("line {i}b: merged side branch {}", i / merge));
            stream.modify("100644", "story.md", window(&story).as_bytes())?;
            for (path, mode, content) in &touched {
                stream.modify(mode, path, content)?;
            }
            stream.end_commit()?;
            mark
        } else if i == 1 {
            // The root commit plants everything.
            let mark = stream.commit(
                "refs/heads/main",
                None,
                None,
                author,
                secs,
                "commit 1: plant the repository\n",
            )?;
            stream.initial_tree()?;
            stream.end_commit()?;
            mark
        } else {
            let mark = stream.commit(
                "refs/heads/main",
                Some(main_mark),
                None,
                author,
                secs,
                &format!("commit {i}: tend big/ and story.md\n"),
            )?;
            stream.modify("100644", "story.md", window(&story).as_bytes())?;
            let touched = round_robin_paths(i, 1, args.big_files);
            stream.file_commands(&touched)?;
            // Same-content rename every 97 commits (git -M sees R100; the
            // bridge imports a rename and re-emits M+D — same tree, same SHA).
            if i % 97 == 0 {
                stream.del("big/moved.txt")?;
                stream.modify("100644", "big/moved.txt.new", &moved_content(i))?;
            } else if i % 97 == 1 {
                stream.del("big/moved.txt.new")?;
                stream.modify("100644", "big/moved.txt", &moved_content(i))?;
            }
            // Exec-bit churn and symlink retargeting every 11th/13th commit.
            if i % 11 == 0 {
                let k = ((i / 11) % 10) as usize;
                stream.modify("100755", &format!("big/exec-{k}.sh"), &exec_content(i, k))?;
            }
            if i % 13 == 0 {
                let k = ((i / 13) % 10) as usize;
                let target = format!("../data/target-{}.txt", (i / 13) % 10);
                stream.modify("120000", &format!("big/link-{k}"), target.as_bytes())?;
            }
            stream.end_commit()?;
            mark
        };
    }
    drop(stream.w); // flush + close stdin
    let out_bytes = child.wait_with_output()?;
    if !out_bytes.status.success() {
        return Err(format!(
            "git fast-import failed: {}",
            String::from_utf8_lossy(&out_bytes.stderr)
        )
        .into());
    }

    // The fixture must be fsck-clean before the demo trusts it.
    let fsck = Command::new("git")
        .current_dir(out)
        .args(["fsck", "--no-progress"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()?;
    if !fsck.status.success() {
        return Err(format!(
            "generated corpus failed git fsck: {}",
            String::from_utf8_lossy(&fsck.stderr)
        )
        .into());
    }
    Ok(())
}

/// The story keeps only the newest [`STORY_LINES`] lines (a sliding window).
fn window(story: &[String]) -> String {
    let start = story.len().saturating_sub(STORY_LINES);
    let mut s = String::new();
    for line in &story[start..] {
        s.push_str(line);
        s.push('\n');
    }
    s
}

/// One or two deterministic file touches for commit `i` (round-robin over
/// the flat `big/` files, with nested files folded in).
fn round_robin_paths(i: u64, offset: u64, big_files: u64) -> Vec<(String, String, Vec<u8>)> {
    let mut out = Vec::with_capacity(2);
    for k in 0..offset {
        let idx = (i * 3 + k) % (big_files + 200).max(1);
        let (path, mode, content) = if idx < big_files {
            (format!("big/f{idx:05}.txt"), "100644", big_content(idx, i))
        } else {
            let n = idx - big_files;
            (
                format!("big/d{}/g{:02}.md", n / 25, n % 25),
                "100644",
                nested_content(n, i),
            )
        };
        out.push((path, mode.to_owned(), content));
    }
    out
}

fn big_content(k: u64, rev: u64) -> Vec<u8> {
    format!(
        "file big/f{k:05}.txt\nrev {rev}\nthe quick brown fox tends row {k}\npadding {}\n",
        (rev * 7919) % 100_000
    )
    .into_bytes()
}

fn nested_content(n: u64, rev: u64) -> Vec<u8> {
    format!(
        "nested file {n} at rev {rev}\nsecond line of g{}.md\n",
        n % 25
    )
    .into_bytes()
}

fn exec_content(rev: u64, k: usize) -> Vec<u8> {
    format!("#!/bin/sh\n# exec-{k} at rev {rev}\necho demo {rev}\n").into_bytes()
}

fn moved_content(rev: u64) -> Vec<u8> {
    format!("this file periodically moves; rev {rev}\n").into_bytes()
}

/// The fast-import stream writer: one child process, marks shared across
/// blobs and commits, `data <n>` byte-exact.
struct Stream<W: Write> {
    w: W,
    mark: u64,
    big: u64,
}

impl<W: Write> Stream<W> {
    fn next_mark(&mut self) -> u64 {
        self.mark += 1;
        self.mark
    }

    /// A file command for one touched file: `M <mode> inline <path>` + data
    /// or `D <path>`. (Renames are expressed as delete + modify, exactly
    /// like git_bridge export does.)
    fn file_commands(
        &mut self,
        touched: &[(String, String, Vec<u8>)],
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Sort for a deterministic stream (fast-import does not care).
        let mut sorted: Vec<&(String, String, Vec<u8>)> = touched.iter().collect();
        sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        for (path, mode, content) in sorted {
            self.modify(mode, path, content)?;
        }
        Ok(())
    }

    /// Every file of the initial tree, in ascending path order.
    fn initial_tree(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut files: Vec<(String, String, Vec<u8>)> = vec![
            (
                "README.md".to_owned(),
                "100644".to_owned(),
                b"# gardenbed demo corpus\n\nGenerated by gen-demo-corpus; deterministic.\n"
                    .to_vec(),
            ),
            (
                "data/target-0.txt".to_owned(),
                "100644".to_owned(),
                b"symlink target zero\n".to_vec(),
            ),
        ];
        for k in 1..10 {
            files.push((
                format!("data/target-{k}.txt"),
                "100644".to_owned(),
                format!("symlink target {k}\n").into_bytes(),
            ));
        }
        for k in 0..self.big {
            files.push((
                format!("big/f{k:05}.txt"),
                "100644".to_owned(),
                big_content(k, 1),
            ));
        }
        for n in 0..200u64 {
            files.push((
                format!("big/d{}/g{:02}.md", n / 25, n % 25),
                "100644".to_owned(),
                nested_content(n, 1),
            ));
        }
        for k in 0..10usize {
            files.push((
                format!("big/exec-{k}.sh"),
                "100755".to_owned(),
                exec_content(1, k),
            ));
            files.push((
                format!("big/link-{k}"),
                "120000".to_owned(),
                format!("../data/target-{k}.txt").into_bytes(),
            ));
        }
        files.push((
            "big/moved.txt".to_owned(),
            "100644".to_owned(),
            moved_content(1),
        ));
        files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        for (path, mode, content) in &files {
            self.modify(mode, path, content)?;
        }
        Ok(())
    }

    fn modify(
        &mut self,
        mode: &str,
        path: &str,
        content: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        write!(self.w, "M {mode} inline {path}\ndata {}\n", content.len())?;
        self.w.write_all(content)?;
        self.w.write_all(b"\n")?;
        Ok(())
    }

    fn del(&mut self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        writeln!(self.w, "D {path}")?;
        Ok(())
    }

    fn reset(&mut self, branch: &str, from_mark: u64) -> Result<(), Box<dyn std::error::Error>> {
        writeln!(self.w, "reset {branch}\nfrom :{from_mark}")?;
        Ok(())
    }

    /// Opens a commit (author/committer identical, as gardenbed models it)
    /// and returns nothing — file commands follow, then [`Stream::
    /// end_commit`]. `parent` of 0 means "no from line" (a root commit or
    /// the branch's current tip is intended by the caller).
    fn commit(
        &mut self,
        branch: &str,
        parent: Option<u64>,
        merge: Option<u64>,
        author: usize,
        secs: u64,
        message: &str,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let mark = self.next_mark();
        let (name, email, tz) = AUTHORS[author];
        let tz_s = format_tz(tz);
        write!(self.w, "commit {branch}\nmark :{mark}\n")?;
        writeln!(self.w, "author {name} <{email}> {secs} {tz_s}")?;
        writeln!(self.w, "committer {name} <{email}> {secs} {tz_s}")?;
        write!(self.w, "data {}\n{message}\n", message.len())?;
        if let Some(p) = parent {
            writeln!(self.w, "from :{p}")?;
        }
        if let Some(m) = merge {
            writeln!(self.w, "merge :{m}")?;
        }
        Ok(mark)
    }

    /// Terminates a commit's file-command block with the blank line.
    fn end_commit(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        writeln!(self.w)?;
        Ok(())
    }
}

/// git's `±hhmm` timezone rendering from an offset in minutes.
fn format_tz(tz_min: i32) -> String {
    let sign = if tz_min < 0 { '-' } else { '+' };
    let abs = tz_min.unsigned_abs();
    format!("{sign}{:02}{:02}", abs / 60, abs % 60)
}

/// `git rev-list --branches --count` (0 for a directory without a repo).
fn rev_list_count(dir: &Path) -> Result<u64, Box<dyn std::error::Error>> {
    if !dir.join(".git").exists() && !dir.join("HEAD").exists() {
        return Ok(0);
    }
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-list", "--branches", "--count"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()?;
    if !out.status.success() {
        return Ok(0);
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0))
}

/// The sha `refs/heads/main` points at.
fn rev_parse_head(dir: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", "refs/heads/main"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()?;
    if !out.status.success() {
        return Err("git rev-parse refs/heads/main failed".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
