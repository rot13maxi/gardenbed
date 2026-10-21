//! `gardenbed-ab` — paired Gardenbed-vs-Git benchmark harness.
//!
//! The question this answers: *for many agents working in one project, which
//! operations cost less time, CPU, bytes, and disk in Gardenbed than in Git,
//! and by how much?* Every scenario runs both sides against the **same
//! history** and records the same four quantities, so a result can never be
//! "bed is faster because Git was configured badly".
//!
//! # Sides and modes
//!
//! * `bed` — `bed clone` (a ref lookup, no files) + `bed sparse add <path>`.
//! * `git-partial` — `git clone --filter=blob:none --no-checkout --sparse`,
//!   then `git sparse-checkout set` and `git checkout`. This is Git's best
//!   modern cold-worker configuration: commits and trees, no blobs until
//!   they are asked for.
//! * `git-full` — a full clone followed by a sparse checkout: the
//!   conservative configuration most projects actually use.
//! * `git-worktree` — one shared clone, then `git worktree add` per
//!   worker. The realistic "many agents on one machine" Git setup: object
//!   storage is shared, checkouts are not.
//!
//! The harness **fails** a `git-partial` run when the server ignored
//! `--filter` (Git prints `warning: filtering not recognized by server,
//! ignoring`) or when the clone did not record the promisor filter. A
//! silent upgrade to a full clone is exactly the strawman these
//! measurements must not contain.
//!
//! # What is measured
//!
//! Every spawned command is timed with `wait4(2)`, which yields wall clock,
//! user + system CPU, and `ru_maxrss` for that process. Network bytes are
//! counted by a transparent TCP relay in front of the server
//! (`client→server`, `server→client`), which works for both `git://` and
//! Gardenbed's h2c gRPC. Disk is the worker tree's apparent and allocated
//! size after provisioning.
//!
//! Fixtures live on tmpfs by default (`/dev/shm`) — the same rule the
//! criterion benches follow: the object store fsyncs, and the benches
//! measure the design, not the host disk. `GARDENBED_FSYNC` is never set.
//!
//! # Scenarios
//!
//! * `provision` — N workers, each needing one sparse subtree. Per-worker
//!   wall/CPU/RSS, aggregate network, total and marginal disk.
//! * `query` — per query kind (`log`, `file`, `blame`), a **fresh** worker
//!   provisions and then runs the query. This is where a partial clone's
//!   lazy blob fetch shows up as network amplification, and where
//!   Gardenbed's server-side history answers with no checkout at all.
//! * `land` — N lanes prepare one commit each, then a release queue lands
//!   them **serially**: every landing rebases the lane onto the newest base
//!   and fast-forwards. Total and per-lane costs.
//! * `contend` — N lanes commit, then publish simultaneously. First-wave
//!   outcomes (accepted / refused) and the serialized retry rounds needed
//!   to land everyone under each side's conflict policy.
//!
//! # Example
//!
//! ```sh
//! # fixture: the same history in a git daemon and a Gardenbed server
//! gardenbed-ab provision \
//!   --bed-url http://127.0.0.1:9421/demo \
//!   --git-url git://127.0.0.1:9431/demo.git \
//!   --branch main --sparse rust/kernel --workers 8
//! ```
//!
//! Every scenario prints a markdown table and (with `--json <path>`) a
//! machine-readable record.

use parking_lot::Mutex;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use serde_json::json;

// ---------------------------------------------------------------- CLI

#[derive(Debug, Parser)]
#[command(
    name = "gardenbed-ab",
    about = "Paired Gardenbed-vs-Git benchmark harness",
    after_help = "Sides: bed | git-partial | git-full | git-worktree.\n\
                  Every measurement is wall/CPU/RSS via wait4, network via a TCP relay,\n\
                  and disk via the worker tree. See the module docs (src/bin/ab.rs)."
)]
struct Cli {
    #[command(subcommand)]
    scenario: Scenario,
}

#[derive(Debug, Subcommand)]
enum Scenario {
    /// N cold workers, each materializing one sparse subtree.
    Provision(ProvisionArgs),
    /// Fresh worker per query kind: log, file, blame.
    Query(QueryArgs),
    /// N lanes commit, then land serially (the release queue).
    Land(LandArgs),
    /// N lanes publish simultaneously; report first-wave outcomes and retries.
    Contend(ContendArgs),
    /// N existing workers pick up a base that moved (the refresh cycle).
    Refresh(RefreshArgs),
    /// History queries with no working copy at all (API-vs-checkout).
    History(HistoryArgs),
    /// Build a reproducible fixture pair (bounded git history + matching
    /// Gardenbed store) and start both servers. Prints the URLs to pass as
    /// `--bed-url` / `--git-url`.
    Fixture(FixtureArgs),
    /// Build a fresh fixture and run every scenario against it in one
    /// command. The repeatable entry point: no servers to start by hand,
    /// no state carried between runs.
    Suite(SuiteArgs),
}

#[derive(Debug, Clone, Args)]
struct Fixture {
    /// Gardenbed endpoint including the repository path
    /// (e.g. `http://127.0.0.1:9421/demo`).
    #[arg(long)]
    bed_url: String,
    /// Git daemon URL (e.g. `git://127.0.0.1:9431/demo.git`). A local path
    /// works too, but then network bytes read as 0.
    #[arg(long)]
    git_url: String,
    /// Local path to the same history, used only to verify that both sides
    /// return identical content. Without it, a `git://` fixture skips the
    /// equivalence check (the answer is still timed).
    #[arg(long)]
    git_reference: Option<PathBuf>,
    /// The branch both sides start from (`main`, `master`, …).
    #[arg(long, default_value = "main")]
    branch: String,
    /// Repository-relative sparse prefix every worker materializes.
    #[arg(long)]
    sparse: String,
    /// Number of workers/lanes.
    #[arg(long, default_value_t = 4)]
    workers: usize,
    /// Workers provisioned in parallel (provision scenario).
    #[arg(long, default_value_t = 1)]
    concurrency: usize,
    /// Scratch root; defaults to a fresh tmpfs directory.
    #[arg(long)]
    workdir: Option<PathBuf>,
    /// Write the full report as JSON here.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Keep the scratch root (default: remove it).
    #[arg(long)]
    keep: bool,
}

#[derive(Debug, Args)]
struct FixtureArgs {
    /// Source history: a git repository path or cloneable URL. Any real
    /// repository works; `gen-demo-corpus` is the synthetic default.
    #[arg(long)]
    source: String,
    /// Keep only the newest N commits, re-rooted so the slice has a real
    /// root commit. Full history otherwise.
    #[arg(long, default_value_t = 200)]
    commits: usize,
    /// Directory to build in (created; refused if non-empty).
    #[arg(long)]
    out: PathBuf,
    /// Port for the Gardenbed server (0 = pick one).
    #[arg(long, default_value_t = 0)]
    bed_port: u16,
    /// Port for the git daemon (0 = pick one).
    #[arg(long, default_value_t = 0)]
    git_port: u16,
    /// Branch to slice and serve.
    #[arg(long, default_value = "main")]
    branch: String,
}

#[derive(Debug, Args)]
struct SuiteArgs {
    /// Source history (a git repository path or URL).
    #[arg(long)]
    source: String,
    /// Newest N commits to slice into the fixture.
    #[arg(long, default_value_t = 200)]
    commits: usize,
    /// Directory for the fixture and all scenario output. Refused if
    /// non-empty, so a run can never inherit another run's state.
    #[arg(long)]
    out: PathBuf,
    /// Branch to exercise.
    #[arg(long, default_value = "main")]
    branch: String,
    /// Worker counts to sweep for `provision`, comma-separated.
    #[arg(long, default_value = "1,4")]
    workers: String,
    /// Sparse prefix for the provisioning/query scenarios (the large one).
    #[arg(long, default_value = "big")]
    sparse: String,
    /// Small sparse prefix for the landing/contention scenarios.
    #[arg(long, default_value = "data")]
    small_sparse: String,
    /// File queried by the `query` and `history` scenarios.
    #[arg(long)]
    query_path: Option<String>,
    /// Skip the slower scenarios (`land`, `contend`) — useful in CI.
    #[arg(long)]
    quick: bool,
}

#[derive(Debug, Args)]
struct ProvisionArgs {
    #[command(flatten)]
    fixture: Fixture,
    /// Comma-separated sides (default: bed,git-partial,git-full,git-worktree).
    #[arg(long)]
    sides: Option<String>,
}

#[derive(Debug, Args)]
struct QueryArgs {
    #[command(flatten)]
    fixture: Fixture,
    /// Comma-separated sides (default: bed,git-partial,git-full).
    #[arg(long)]
    sides: Option<String>,
    /// File to query; defaults to the first regular file under the sparse path.
    #[arg(long)]
    path: Option<String>,
}

#[derive(Debug, Args)]
struct LandArgs {
    #[command(flatten)]
    fixture: Fixture,
    /// Comma-separated sides (default: bed,git-partial,git-worktree).
    #[arg(long)]
    sides: Option<String>,
}

#[derive(Debug, Args)]
struct RefreshArgs {
    #[command(flatten)]
    fixture: Fixture,
    /// Comma-separated sides (default: bed,git-partial,git-worktree).
    #[arg(long)]
    sides: Option<String>,
}

#[derive(Debug, Args)]
struct HistoryArgs {
    #[command(flatten)]
    fixture: Fixture,
    /// File to query; required (the harness will not guess).
    #[arg(long)]
    path: String,
    /// Sides (default: bed,git; `bed-mcp` needs an explicit `--repo`-capable
    /// binary and is opt-in).
    #[arg(long)]
    sides: Option<String>,
}

#[derive(Debug, Args)]
struct ContendArgs {
    #[command(flatten)]
    fixture: Fixture,
    /// Comma-separated sides (default: bed,git-worktree).
    #[arg(long)]
    sides: Option<String>,
}

// ---------------------------------------------------------------- process measurement

/// One measured process.
#[derive(Debug, Clone, Default)]
struct Measure {
    wall_ms: f64,
    user_ms: f64,
    sys_ms: f64,
    max_rss_kb: u64,
    ok: bool,
    code: Option<i32>,
}

#[cfg(unix)]
fn timeval_ms(t: libc::timeval) -> f64 {
    t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0
}

/// Runs a command to completion, recording wall/CPU/max-RSS via `wait4`.
/// stdout and stderr go to the shared scenario log (never a pipe: a
/// chatty `git clone` must not deadlock the harness).
fn run_measured(cmd: &mut Command, log: &Path) -> Measure {
    run_measured_to(cmd, log, None)
}

/// [`run_measured`] with stdout sent to `capture` (or appended to `log`
/// when `None`). stderr always goes to `log`.
fn run_measured_to(cmd: &mut Command, log: &Path, capture: Option<&Path>) -> Measure {
    let out = match fs::OpenOptions::new().create(true).append(true).open(log) {
        Ok(f) => f,
        Err(e) => panic!("cannot open {}: {e}", log.display()),
    };
    let err = out.try_clone().expect("log handle clones");
    let stdout = match capture {
        Some(path) => fs::File::create(path)
            .unwrap_or_else(|e| panic!("cannot create {}: {e}", path.display())),
        None => fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap_or_else(|e| panic!("cannot open {}: {e}", log.display())),
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(err);
    let t0 = Instant::now();
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            append_log(log, &format!("spawn failed: {e}\n"));
            return Measure {
                wall_ms: t0.elapsed().as_secs_f64() * 1000.0,
                ..Measure::default()
            };
        }
    };
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        let mut status: libc::c_int = 0;
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::wait4(pid, &mut status, 0, &mut ru) };
        let wall = t0.elapsed().as_secs_f64() * 1000.0;
        // No `Child::wait`: the process was reaped above; dropping the
        // handle only closes the stdio files.
        drop(child);
        if rc < 0 {
            append_log(
                log,
                &format!("wait4 failed: {}\n", std::io::Error::last_os_error()),
            );
            return Measure {
                wall_ms: wall,
                ..Measure::default()
            };
        }
        let exited = libc::WIFEXITED(status);
        Measure {
            wall_ms: wall,
            user_ms: timeval_ms(ru.ru_utime),
            sys_ms: timeval_ms(ru.ru_stime),
            max_rss_kb: ru.ru_maxrss as u64,
            ok: exited && libc::WEXITSTATUS(status) == 0,
            code: exited.then(|| libc::WEXITSTATUS(status)),
        }
    }
    #[cfg(not(unix))]
    {
        // No rusage accounting on Windows: wall time only, exit status
        // from `wait`.
        drop(child.stdin.take());
        let status = child.wait().ok();
        let wall = t0.elapsed().as_secs_f64() * 1000.0;
        let (ok, code) = match status {
            Some(s) => (s.success(), s.code()),
            None => (false, None),
        };
        Measure {
            wall_ms: wall,
            ok,
            code,
            ..Measure::default()
        }
    }
}

fn append_log(log: &Path, text: &str) {
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = f.write_all(text.as_bytes());
    }
}

/// Runs a command and captures stdout (for fixture inspection only — never
/// for a timed step).
fn output(cmd: &mut Command) -> Option<String> {
    let out = cmd.stdin(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------- TCP relay

/// A transparent byte-counting TCP relay. Works for `git://` and plaintext
/// HTTP/2 alike.
struct Proxy {
    port: u16,
    c2s: Arc<AtomicU64>,
    s2c: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    accept: Option<thread::JoinHandle<()>>,
}

impl Proxy {
    fn start(host: &str, port: u16) -> std::io::Result<Proxy> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let local = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let c2s = Arc::new(AtomicU64::new(0));
        let s2c = Arc::new(AtomicU64::new(0));
        let host = host.to_owned();
        let (t_stop, t_c2s, t_s2c) = (stop.clone(), c2s.clone(), s2c.clone());
        let accept = thread::spawn(move || {
            for incoming in listener.incoming() {
                if t_stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(client) = incoming else { break };
                let (host, c2s, s2c) = (host.clone(), t_c2s.clone(), t_s2c.clone());
                thread::spawn(move || relay(client, &host, port, c2s, s2c));
            }
        });
        Ok(Proxy {
            port: local,
            c2s,
            s2c,
            stop,
            accept: Some(accept),
        })
    }

    fn snapshot(&self) -> (u64, u64) {
        (
            self.c2s.load(Ordering::Relaxed),
            self.s2c.load(Ordering::Relaxed),
        )
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Unblock accept() by connecting to ourselves; the accept loop then
        // observes `stop` and exits.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

fn relay(client: TcpStream, host: &str, port: u16, c2s: Arc<AtomicU64>, s2c: Arc<AtomicU64>) {
    let Ok(upstream) = TcpStream::connect((host, port)) else {
        return;
    };
    // Without NODELAY the relay turns every small exchange into a
    // Nagle/delayed-ACK stall (~40 ms). That biases the result by protocol
    // shape — Gardenbed's fetch is many small round-trips, Git's is a few
    // bulk transfers — so the relay must not add per-message latency.
    for s in [&client, &upstream] {
        let _ = s.set_nodelay(true);
    }
    let Ok(mut cr) = client.try_clone() else {
        return;
    };
    let Ok(mut ur) = upstream.try_clone() else {
        return;
    };
    let mut cw = client;
    let mut uw = upstream;
    let up = thread::spawn(move || copy_count(&mut cr, &mut uw, &c2s));
    let down = thread::spawn(move || copy_count(&mut ur, &mut cw, &s2c));
    let _ = up.join();
    let _ = down.join();
}

fn copy_count(read: &mut TcpStream, write: &mut TcpStream, count: &AtomicU64) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match read.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                count.fetch_add(n as u64, Ordering::Relaxed);
                if write.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let _ = write.shutdown(Shutdown::Write);
}

// ---------------------------------------------------------------- endpoints

fn split_http(url: &str) -> Option<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse().ok()?),
        None => (authority.to_owned(), 80),
    };
    Some((host, port, path.to_owned()))
}

fn split_git(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("git://")?;
    let (authority, path) = rest.split_once('/')?;
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse().ok()?),
        None => (authority.to_owned(), 9418),
    };
    Some((host, port, format!("/{path}")))
}

// ---------------------------------------------------------------- disk

#[derive(Debug, Clone, Copy, Default)]
struct Disk {
    apparent: u64,
    allocated: u64,
}

impl Disk {
    fn add(&mut self, other: Disk) {
        self.apparent += other.apparent;
        self.allocated += other.allocated;
    }
}

fn disk_tree(path: &Path) -> Disk {
    let mut total = Disk::default();
    let Ok(entries) = fs::read_dir(path) else {
        return total;
    };
    for entry in entries.flatten() {
        let meta = match fs::symlink_metadata(entry.path()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            total.add(disk_tree(&entry.path()));
        } else {
            total.apparent += meta.len();
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                total.allocated += meta.blocks() * 512;
            }
        }
    }
    total
}

// ---------------------------------------------------------------- harness context

struct Ctx {
    fixture: Fixture,
    /// Unique per invocation: lane content carries it so a rerun against the
    /// same fixture commits real changes instead of finding its own
    /// previous run already at HEAD.
    run_token: String,
    root: PathBuf,
    log: PathBuf,
    bed_proxy: Option<Proxy>,
    git_proxy: Option<Proxy>,
    rows: Mutex<Vec<Row>>,
    notes: Mutex<Vec<String>>,
    bed_url: String,
    git_url: String,
}

#[derive(Debug, Clone)]
struct Row {
    side: String,
    mode: String,
    op: String,
    n: u64,
    wall_ms: f64,
    user_ms: f64,
    sys_ms: f64,
    max_rss_kb: u64,
    c2s: u64,
    s2c: u64,
}

impl Ctx {
    fn new(fixture: Fixture) -> Ctx {
        let root = match &fixture.workdir {
            Some(p) => {
                let _ = fs::remove_dir_all(p);
                fs::create_dir_all(p).expect("workdir");
                p.clone()
            }
            None => {
                let base = if Path::new("/dev/shm").is_dir() {
                    Path::new("/dev/shm")
                } else {
                    Path::new("/tmp")
                };
                let p = base.join(format!(
                    "gardenbed-ab-{}-{}",
                    std::process::id(),
                    Instant::now().elapsed().as_nanos()
                ));
                fs::create_dir_all(&p).expect("tmp workdir");
                p
            }
        };
        let log = root.join("scenario.log");
        fs::write(&log, b"").expect("log created");

        // A full tmpfs makes git fail fast on write and reports
        // implausibly small numbers. Refuse up front instead.
        if let Some(free) = free_bytes(&root) {
            const MIN: u64 = 2 << 30;
            if free < MIN {
                panic!(
                    "{} has {} free; the harness needs at least {} for fixtures \
                     (clear /dev/shm or pass --workdir elsewhere)",
                    root.display(),
                    human(free),
                    human(MIN)
                );
            }
        }

        // Byte-counting relays in front of both servers. A local-path git
        // fixture bypasses the relay (and reports 0 network bytes).
        let bed_proxy = split_http(&fixture.bed_url).and_then(|(h, p, _)| Proxy::start(&h, p).ok());
        let git_proxy = split_git(&fixture.git_url).and_then(|(h, p, _)| Proxy::start(&h, p).ok());

        let bed_url = match (&bed_proxy, split_http(&fixture.bed_url)) {
            (Some(proxy), Some((_, _, path))) => {
                format!("http://127.0.0.1:{}{path}", proxy.port)
            }
            _ => fixture.bed_url.clone(),
        };
        let git_url = match (&git_proxy, split_git(&fixture.git_url)) {
            (Some(proxy), Some((_, _, path))) => {
                format!("git://127.0.0.1:{}{path}", proxy.port)
            }
            // A local path silently ignores `--filter` unless it is spelled
            // `file://` (Git even warns, but proceeds with a FULL clone).
            // That would make git-partial a strawman.
            _ if Path::new(&fixture.git_url).is_dir() => format!(
                "file://{}",
                Path::new(&fixture.git_url)
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(&fixture.git_url))
                    .display()
            ),
            _ => fixture.git_url.clone(),
        };

        let run_token = format!(
            "{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        );
        let ctx = Ctx {
            fixture,
            run_token,
            root,
            log,
            bed_proxy,
            git_proxy,
            rows: Mutex::new(Vec::new()),
            notes: Mutex::new(Vec::new()),
            bed_url,
            git_url,
        };
        if ctx.git_proxy.is_none() && ctx.fixture.git_url.starts_with("git://") {
            ctx.note("git relay failed to start; git network bytes will read as 0");
        }
        if ctx.fixture.git_url.starts_with("file://") || !ctx.fixture.git_url.contains("://") {
            ctx.note("git fixture is a local path; its network bytes read as 0");
        }
        ctx
    }

    fn note(&self, text: impl Into<String>) {
        self.notes.lock().push(text.into());
    }

    fn dir(&self, name: &str) -> PathBuf {
        let p = self.root.join(name);
        fs::create_dir_all(&p).expect("scenario dir");
        p
    }

    /// Runs one measured step, attributing the network delta to it.
    fn step(&self, side: &str, mode: &str, op: &str, n: u64, cmd: &mut Command) -> Measure {
        self.step_inner(side, mode, op, n, cmd, None)
    }

    /// Like [`Ctx::step`], but redirects stdout to `out` so the answer can
    /// be checked for equivalence with the other side.
    fn step_captured(
        &self,
        side: &str,
        mode: &str,
        op: &str,
        cmd: &mut Command,
        out: &Path,
    ) -> Measure {
        self.step_inner(side, mode, op, 1, cmd, Some(out))
    }

    fn step_inner(
        &self,
        side: &str,
        mode: &str,
        op: &str,
        n: u64,
        cmd: &mut Command,
        out: Option<&Path>,
    ) -> Measure {
        let before = self.net_snapshot(family(side).0);
        let log = &self.log;
        // stdout goes to the log (or the capture file); stderr always to
        // the log, so a failing command is diagnosable.
        let m = run_measured_to(cmd, log, out);
        let after = self.net_snapshot(family(side).0);
        self.rows.lock().push(Row {
            side: side.to_owned(),
            mode: mode.to_owned(),
            op: op.to_owned(),
            n,
            wall_ms: m.wall_ms,
            user_ms: m.user_ms,
            sys_ms: m.sys_ms,
            max_rss_kb: m.max_rss_kb,
            c2s: after.0.saturating_sub(before.0),
            s2c: after.1.saturating_sub(before.1),
        });
        if !m.ok {
            let tail = tail_of(&self.log, 800);
            self.note(format!(
                "{side}/{mode} {op} failed (exit {:?}) — log tail:\n{tail}",
                m.code
            ));
        }
        m
    }

    fn net_snapshot(&self, side: &str) -> (u64, u64) {
        if side == "bed" {
            self.bed_proxy
                .as_ref()
                .map(Proxy::snapshot)
                .unwrap_or((0, 0))
        } else {
            self.git_proxy
                .as_ref()
                .map(Proxy::snapshot)
                .unwrap_or((0, 0))
        }
    }

    fn print_report(&self, scenario: &str) {
        let rows = self.rows.lock().clone();
        // Persist FIRST. stdout is often a pipe (`| head`), and a closed
        // pipe panics on the next println — which would otherwise discard
        // the whole run's results.
        self.write_json(scenario, &rows);
        println!("\n## {scenario}\n");
        println!(
            "| side | mode | op | n | wall ms | cpu ms | max rss KB | net client→server | net server→client |"
        );
        println!("|---|---|---|---:|---:|---:|---:|---:|---:|");
        for r in &rows {
            println!(
                "| {} | {} | {} | {} | {:.1} | {:.1} | {} | {} | {} |",
                r.side,
                r.mode,
                r.op,
                r.n,
                r.wall_ms,
                r.user_ms + r.sys_ms,
                r.max_rss_kb,
                human(r.c2s),
                human(r.s2c)
            );
        }
        let notes = self.notes.lock();
        if !notes.is_empty() {
            println!("\nNotes:");
            for n in notes.iter() {
                println!("- {n}");
            }
        }
    }

    /// Writes the run's rows as JSON, resolving a relative path against the
    /// invocation directory (the process cwd changes per command).
    fn write_json(&self, scenario: &str, rows: &[Row]) {
        let Some(path) = &self.fixture.json else {
            return;
        };
        let path = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let payload = json!({
            "scenario": scenario,
            "fixture": {
                "bed_url": self.fixture.bed_url,
                "git_url": self.fixture.git_url,
                "branch": self.fixture.branch,
                "sparse": self.fixture.sparse,
                "workers": self.fixture.workers,
                "concurrency": self.fixture.concurrency,
            },
            "rows": rows.iter().map(|r| json!({
                "side": r.side, "mode": r.mode, "op": r.op, "n": r.n,
                "wall_ms": r.wall_ms, "user_ms": r.user_ms, "sys_ms": r.sys_ms,
                "max_rss_kb": r.max_rss_kb, "net_c2s": r.c2s, "net_s2c": r.s2c,
            })).collect::<Vec<_>>(),
            "notes": self.notes.lock().clone(),
        });
        match fs::write(&path, serde_json::to_string_pretty(&payload).unwrap()) {
            Ok(()) => println!("\nJSON: {}", path.display()),
            Err(e) => eprintln!("JSON write failed for {}: {e}", path.display()),
        }
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        if !self.fixture.keep && self.fixture.workdir.is_none() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn tail_of(log: &Path, bytes: usize) -> String {
    let Ok(mut f) = fs::File::open(log) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(bytes as u64);
    let _ = std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(start));
    let mut buf = String::new();
    let _ = f.read_to_string(&mut buf);
    buf
}

/// Free bytes on the filesystem holding `path` (0 when unknowable).
#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt as _;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail * st.f_frsize)
}

/// Windows has no statvfs; disk free space is not essential to the
/// report, so the answer is "unknown".
#[cfg(not(unix))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

fn human(n: u64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MiB", n as f64 / (1 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KiB", n as f64 / (1 << 10) as f64)
    } else {
        format!("{n} B")
    }
}

// ---------------------------------------------------------------- command builders

/// The `bed` binary under test.
///
/// Resolution order: `$BED_BIN`, then this executable's sibling (`cargo`
/// puts `bed` and `gardenbed-ab` in one directory), then `bed` from PATH.
/// A bare `bed` from PATH is the last resort and a common trap: it is
/// usually a stale installed build, and silently benchmarking it measures
/// the wrong tree. `resolve_bed_bin` reports which one was chosen.
fn bed_bin() -> PathBuf {
    if let Ok(path) = std::env::var("BED_BIN") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("bed");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("bed")
}

fn bed(state: &Path) -> Command {
    let mut c = Command::new(bed_bin());
    c.current_dir(state);
    // Daemon disabled: the embedded client is the v0.1 baseline, and a
    // shared cache would silently flatter the per-worker numbers.
    c.env("GARDENBED_DAEMON", "")
        .env("GARDENBED_AUTHOR", "Bench <bench@example.com>");
    c
}

/// Prints the binaries in play, so a recorded run can never be attributed
/// to the wrong build.
fn report_binaries() {
    let bed = bed_bin();
    let version = Command::new(&bed)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    let server = server_bin();
    println!(
        "binaries: bed={} ({version}); gardenbed-server={}",
        bed.display(),
        server.display()
    );
}

/// The `gardenbed-server` binary: `$BED_SERVER_BIN`, then this
/// executable's sibling, then PATH.
fn server_bin() -> PathBuf {
    if let Ok(path) = std::env::var("BED_SERVER_BIN") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("gardenbed-server");
        if sibling.is_file() {
            return sibling;
        }
    }
    PathBuf::from("gardenbed-server")
}

fn git_in(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            c.env_remove(k);
        }
    }
    // Identity for every commit/`rebase` a lane makes (rebase re-writes
    // commits and needs one even when the original commit had it).
    c.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Bench")
        .env("GIT_AUTHOR_EMAIL", "bench@example.com")
        .env("GIT_COMMITTER_NAME", "Bench")
        .env("GIT_COMMITTER_EMAIL", "bench@example.com")
        .env("LC_ALL", "C")
        .env("LANG", "C");
    c
}

fn publish_args(branch: &str) -> Vec<String> {
    if branch == "main" {
        vec!["publish".to_owned()]
    } else {
        vec!["publish".to_owned(), "--ref".to_owned(), branch.to_owned()]
    }
}

/// Git's own best cold-worker configuration. The caller must verify that
/// the server honored `--filter`.
fn provision_git_partial(ctx: &Ctx, side: &str, dir: &Path, suffix: &str) -> Measure {
    let mut clone = git_in(dir);
    clone
        .args(["clone", "--filter=blob:none", "--no-checkout", "--sparse"])
        .arg(&ctx.git_url)
        .arg(suffix);
    let m = ctx.step(side, "partial", "clone", 1, &mut clone);
    if !m.ok {
        return m;
    }
    let worker = dir.join(suffix);
    let mut set = git_in(&worker);
    set.args(["sparse-checkout", "set"])
        .arg(&ctx.fixture.sparse);
    let m = ctx.step(side, "partial", "sparse-set", 1, &mut set);
    if !m.ok {
        return m;
    }
    let mut checkout = git_in(&worker);
    checkout.arg("checkout");
    let m = ctx.step(side, "partial", "checkout", 1, &mut checkout);
    if !m.ok {
        return m;
    }
    // Refuse a strawman: the server must have honored the filter.
    let filter =
        output(git_in(&worker).args(["config", "--get", "remote.origin.partialclonefilter"]));
    let log = fs::read_to_string(&ctx.log).unwrap_or_default();
    if filter.as_deref().map(str::trim) != Some("blob:none")
        || log.contains("filtering not recognized by server")
    {
        panic!(
            "git server ignored --filter (partialclonefilter={filter:?}); \
             set `uploadpack.allowFilter=true` in the fixture repository — \
             a full clone is not a valid partial-clone baseline"
        );
    }
    m
}

fn provision_git_full(ctx: &Ctx, side: &str, dir: &Path, suffix: &str) -> Measure {
    let mut clone = git_in(dir);
    clone
        .args(["clone", "--no-checkout"])
        .arg(&ctx.git_url)
        .arg(suffix);
    let m = ctx.step(side, "full", "clone", 1, &mut clone);
    if !m.ok {
        return m;
    }
    let worker = dir.join(suffix);
    let mut init = git_in(&worker);
    init.args(["sparse-checkout", "init", "--cone"]);
    let m = ctx.step(side, "full", "sparse-init", 1, &mut init);
    if !m.ok {
        return m;
    }
    let mut set = git_in(&worker);
    set.args(["sparse-checkout", "set"])
        .arg(&ctx.fixture.sparse);
    let m = ctx.step(side, "full", "sparse-set", 1, &mut set);
    if !m.ok {
        return m;
    }
    let mut checkout = git_in(&worker);
    checkout.arg("checkout");
    ctx.step(side, "full", "checkout", 1, &mut checkout)
}

fn provision_git_worktree(ctx: &Ctx, side: &str, base: &Path, dir: &Path, name: &str) -> Measure {
    let mut add = git_in(base);
    add.args(["worktree", "add", "--no-checkout", "--detach"])
        .arg(dir.join(name))
        .arg("HEAD");
    let m = ctx.step(side, "worktree", "worktree-add", 1, &mut add);
    if !m.ok {
        return m;
    }
    let worker = dir.join(name);
    let mut init = git_in(&worker);
    init.args(["sparse-checkout", "init", "--cone"]);
    let m = ctx.step(side, "worktree", "sparse-init", 1, &mut init);
    if !m.ok {
        return m;
    }
    let mut set = git_in(&worker);
    set.args(["sparse-checkout", "set"])
        .arg(&ctx.fixture.sparse);
    let m = ctx.step(side, "worktree", "sparse-set", 1, &mut set);
    if !m.ok {
        return m;
    }
    let mut checkout = git_in(&worker);
    checkout.arg("checkout");
    ctx.step(side, "worktree", "checkout", 1, &mut checkout)
}

fn provision_bed(ctx: &Ctx, dir: &Path, name: &str) -> Measure {
    let state = dir.join(name);
    fs::create_dir_all(&state).expect("bed worker dir");
    let mut clone = bed(&state);
    clone.args(["clone", &ctx.bed_url, "--ref", &ctx.fixture.branch]);
    let m = ctx.step("bed", "sparse", "clone", 1, &mut clone);
    if !m.ok {
        return m;
    }
    let mut add = bed(&state);
    add.arg("sparse").arg("add").arg(&ctx.fixture.sparse);
    ctx.step("bed", "sparse", "sparse-add", 1, &mut add)
}

fn git_worktree_base(ctx: &Ctx, side: &str, dir: &Path) -> PathBuf {
    let base = dir.join("base");
    let mut clone = git_in(dir);
    clone
        .args(["clone", "--no-checkout"])
        .arg(&ctx.git_url)
        .arg("base");
    let m = ctx.step(side, "worktree", "base-clone", 1, &mut clone);
    if !m.ok {
        panic!("worktree base clone failed — see {}", ctx.log.display());
    }
    base
}

// ---------------------------------------------------------------- scenarios

fn run_provision(args: ProvisionArgs) {
    let sides = parse_sides(
        args.sides.as_deref(),
        &["bed", "git-partial", "git-full", "git-worktree"],
    );
    let ctx = Ctx::new(args.fixture.clone());
    let workers = ctx.fixture.workers;

    for side in &sides {
        let dir = ctx.dir(&format!("provision-{side}"));
        let started = Instant::now();
        let base = if side == "git-worktree" {
            Some(git_worktree_base(&ctx, side, &dir))
        } else {
            None
        };
        let conc = ctx.fixture.concurrency.max(1);
        let next = AtomicU64::new(0);
        let errs = Mutex::new(Vec::new());
        thread::scope(|scope| {
            for _ in 0..conc {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed) as usize;
                        if i >= workers {
                            break;
                        }
                        let name = format!("w{i:03}");
                        let m = match side.as_str() {
                            "bed" => provision_bed(&ctx, &dir, &name),
                            "git-partial" => provision_git_partial(&ctx, side, &dir, &name),
                            "git-full" => provision_git_full(&ctx, side, &dir, &name),
                            "git-worktree" => provision_git_worktree(
                                &ctx,
                                side,
                                base.as_ref().unwrap(),
                                &dir,
                                &name,
                            ),
                            other => panic!("unknown side {other}"),
                        };
                        if !m.ok {
                            errs.lock().push(name);
                        }
                    }
                });
            }
        });
        let total = started.elapsed().as_secs_f64() * 1000.0;
        let disk = disk_tree(&dir);
        // Sum this side's own steps. The relay counter is shared by every
        // git configuration, so snapshotting it here would report each git
        // side's traffic as cumulative (git-full looking like partial+full).
        let (c2s, s2c) = {
            let rows = ctx.rows.lock();
            rows.iter()
                .filter(|r| r.side == *side)
                .fold((0u64, 0u64), |(a, b), r| (a + r.c2s, b + r.s2c))
        };
        let cpu = ctx
            .rows
            .lock()
            .iter()
            .filter(|r| r.side == *side)
            .fold(0.0, |acc, r| acc + r.user_ms + r.sys_ms);
        let failures = errs.lock().len();
        println!(
            "{side}: {workers} workers ({} failed) in {:.1} ms wall, {cpu:.1} CPU-ms total, \
             {} client→server, {} server→client, {} on disk ({:.1} MiB/worker marginal incl. shared state)",
            failures,
            total,
            human(c2s),
            human(s2c),
            human(disk.allocated),
            disk.allocated as f64 / (1 << 20) as f64 / workers as f64
        );
        if failures > 0 {
            ctx.note(format!(
                "{side}: {failures}/{workers} workers FAILED — the aggregate above is not a \
                 valid measurement for this side"
            ));
        }
    }
    ctx.print_report("provision");
}

fn run_query(args: QueryArgs) {
    let sides = parse_sides(args.sides.as_deref(), &["bed", "git-partial", "git-full"]);
    let ctx = Ctx::new(args.fixture.clone());
    let path = match &args.path {
        Some(p) => p.clone(),
        None => panic!(
            "--path is required: the harness will not guess which file to query \
             (pass e.g. --path {}/SOME_FILE)",
            ctx.fixture.sparse.trim_end_matches('/')
        ),
    };
    ctx.note(format!("query path: {path}"));

    // Reference bytes straight from the fixture, not from a worker: an
    // equivalence check must not share a cache with either side.
    let reference_source = ctx
        .fixture
        .git_reference
        .as_deref()
        .unwrap_or(Path::new(&ctx.fixture.git_url));
    let reference = git_side_bytes(reference_source, &path);
    if reference.is_none() {
        ctx.note(format!(
            "could not read {path} from the git fixture; content equivalence unchecked"
        ));
    }

    for side in &sides {
        for kind in ["log", "file", "blame"] {
            let dir = ctx.dir(&format!("query-{side}-{kind}"));
            let worker = dir.join("w");
            let m = match side.as_str() {
                "bed" => provision_bed(&ctx, &dir, "w"),
                "git-partial" => provision_git_partial(&ctx, side, &dir, "w"),
                "git-full" => provision_git_full(&ctx, side, &dir, "w"),
                other => panic!("unknown side {other}"),
            };
            if !m.ok {
                continue;
            }
            let mut timed = match (side.as_str(), kind) {
                ("bed", "log") => {
                    let mut c = bed(&worker);
                    c.args(["log", "--limit", "50"]);
                    c
                }
                ("bed", "file") => {
                    let mut c = bed(&worker);
                    c.args(["file", &path]);
                    c
                }
                ("bed", "blame") => {
                    let mut c = bed(&worker);
                    c.args(["blame", &path]);
                    c
                }
                (_, "log") => {
                    let mut c = git_in(&worker);
                    c.args(["log", "-n", "50"]);
                    c
                }
                (_, "file") => {
                    let mut c = git_in(&worker);
                    c.args(["show", &format!("HEAD:{path}")]);
                    c
                }
                (_, "blame") => {
                    let mut c = git_in(&worker);
                    c.args(["blame", "--first-parent", "-L", "1,50", "HEAD", "--", &path]);
                    c
                }
                _ => unreachable!(),
            };

            // One run, captured. A warm-up run would hide exactly what the
            // query scenario exists to measure (a partial clone's lazy blob
            // fetch, bed's cold object fetch).
            let out_path = ctx.root.join(format!("query-{side}-{kind}.out"));
            let m = ctx.step_captured(side, "sparse", kind, &mut timed, &out_path);
            if !m.ok {
                continue;
            }
            let text = fs::read_to_string(&out_path).unwrap_or_default();
            match kind {
                "file" => {
                    if let Some(bytes) = &reference
                        && bytes.as_slice() != text.as_bytes()
                    {
                        ctx.note(format!(
                            "{side}/{kind}: content differs from `git show` for {path}"
                        ));
                    }
                }
                "log" => {
                    let subjects = text.lines().filter(|l| l.starts_with("    ")).count();
                    if subjects != 50 {
                        ctx.note(format!("{side}/{kind}: {subjects} subjects, expected 50"));
                    }
                }
                "blame" => {
                    // One blame line per file line: compare against the
                    // reference blob's line count, not a made-up constant.
                    let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
                    if let Some(bytes) = &reference {
                        let want = bytes.iter().filter(|b| **b == b'\n').count()
                            + usize::from(bytes.last().is_some_and(|b| *b != b'\n'));
                        if lines != want {
                            ctx.note(format!(
                                "{side}/{kind}: {lines} blame lines for a {want}-line file"
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    ctx.print_report("query");
}

/// The same file's bytes straight from the git fixture, independent of
/// either side's worker cache.
///
/// A local repository path is read directly. A `git://` URL needs a
/// fetch: `git archive --remote` is not served by `git daemon`, so the
/// reference comes from a one-shot `git cat-file` over a throwaway
/// blobjectless clone... which would itself download history. Instead the
/// fixture is expected to be readable locally when it is local; for URL
/// fixtures the equivalence check is skipped and said so.
fn git_side_bytes(path_or_url: &Path, path: &str) -> Option<Vec<u8>> {
    let raw = path_or_url.to_string_lossy();
    let local = raw.strip_prefix("file://").unwrap_or(&raw);
    if !Path::new(local).is_dir() {
        return None;
    }
    let git_url = local;
    let mut c = Command::new("git");
    c.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    c.args(["--git-dir", git_url, "show"])
        .arg(format!("HEAD:{path}"));
    let out = c.stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then_some(out.stdout)
}

fn run_land(args: LandArgs) {
    let sides = parse_sides(
        args.sides.as_deref(),
        &["bed", "git-partial", "git-worktree"],
    );
    let ctx = Ctx::new(args.fixture.clone());
    let n = ctx.fixture.workers;

    for side in &sides {
        let dir = ctx.dir(&format!("land-{side}"));
        let base = if side == "git-worktree" {
            Some(git_worktree_base(&ctx, side, &dir))
        } else {
            None
        };
        // Prepare every lane: provision, edit one unique file, commit.
        let mut lanes = Vec::new();
        for i in 0..n {
            let name = format!("lane{i:03}");
            let root = dir.join(&name);
            let m = match side.as_str() {
                "bed" => provision_bed(&ctx, &dir, &name),
                "git-partial" => provision_git_partial(&ctx, side, &dir, &name),
                "git-worktree" => {
                    let root = dir.join(&name);
                    provision_git_worktree(&ctx, side, base.as_ref().unwrap(), &dir, &name);
                    // `git worktree add --detach` leaves HEAD detached;
                    // give the lane its own branch so rebase has a name.
                    let mut b = git_in(&root);
                    b.args(["switch", "-c", &name]);
                    ctx.step(side, "worktree", "branch", 1, &mut b);
                    Measure {
                        ok: true,
                        ..Measure::default()
                    }
                }
                other => panic!("unknown side {other}"),
            };
            if !m.ok {
                ctx.note(format!("{side}: lane {name} did not provision"));
                continue;
            }
            // Inside the sparse prefix on purpose: both sides materialize
            // and track exactly the same tree. The side name is in the
            // path because scenarios share one fixture: a second run must
            // not find the first run's file already committed at HEAD.
            let content = format!("lane {i} work for {side} run {}\n", ctx.run_token);
            let rel = format!(
                "{}/ab-land-{side}/{name}.txt",
                ctx.fixture.sparse.trim_end_matches('/')
            );
            let file = root.join(&rel);
            if let Some(parent) = file.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(&file, content);
            let committed = match side.as_str() {
                "bed" => {
                    let mut c = bed(&root);
                    c.args(["commit", "-m", &format!("lane {i}")]);
                    ctx.step(side, "sparse", "prepare-commit", 1, &mut c).ok
                }
                _ => {
                    let mut add = git_in(&root);
                    add.args(["add", &rel]);
                    let added = ctx.step(side, "sparse", "prepare-add", 1, &mut add).ok;
                    let mut c = git_in(&root);
                    c.args(["commit", "-q", "-m", &format!("lane {i}")]);
                    added && ctx.step(side, "sparse", "prepare-commit", 1, &mut c).ok
                }
            };
            if !committed {
                ctx.note(format!(
                    "{side}: lane {name} failed to commit; excluded from the landing set"
                ));
                continue;
            }
            lanes.push((root, name));
        }
        if lanes.len() != n {
            ctx.note(format!("{side}: only {}/{n} lanes prepared", lanes.len()));
        }

        // Serialized landing — the release queue: each lane rebases onto
        // the newest base, then fast-forwards it.
        let started = Instant::now();
        let mut landed = 0;
        for (root, name) in &lanes {
            match side.as_str() {
                "bed" => {
                    let mut r = bed(root);
                    r.args(["rebase", &ctx.fixture.branch]);
                    let m = ctx.step(side, "sparse", "land-rebase", 1, &mut r);
                    if !m.ok {
                        continue;
                    }
                    let mut p = bed(root);
                    p.args(publish_args(&ctx.fixture.branch));
                    let m = ctx.step(side, "sparse", "land-publish", 1, &mut p);
                    if m.ok {
                        landed += 1;
                    }
                }
                _ => {
                    let mut f = git_in(root);
                    f.args(["fetch", "-q", "origin"]);
                    let m = ctx.step(side, "sparse", "land-fetch", 1, &mut f);
                    if !m.ok {
                        continue;
                    }
                    let mut r = git_in(root);
                    r.args(["rebase", "-q", &format!("origin/{}", ctx.fixture.branch)]);
                    let m = ctx.step(side, "sparse", "land-rebase", 1, &mut r);
                    if !m.ok {
                        continue;
                    }
                    let mut p = git_in(root);
                    p.args([
                        "push",
                        "-q",
                        "origin",
                        &format!("HEAD:{}", ctx.fixture.branch),
                    ]);
                    let m = ctx.step(side, "sparse", "land-push", 1, &mut p);
                    if m.ok {
                        landed += 1;
                    }
                }
            }
            let _ = name;
        }
        println!(
            "{side}: landed {landed}/{n} lanes serially in {:.1} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    ctx.print_report("land");
}

fn run_contend(args: ContendArgs) {
    let sides = parse_sides(args.sides.as_deref(), &["bed", "git-worktree"]);
    let ctx = Ctx::new(args.fixture.clone());
    let n = ctx.fixture.workers;

    for side in &sides {
        let dir = ctx.dir(&format!("contend-{side}"));
        let base = if side == "git-worktree" {
            Some(git_worktree_base(&ctx, side, &dir))
        } else {
            None
        };
        let mut lanes = Vec::new();
        for i in 0..n {
            let name = format!("lane{i:03}");
            let root = dir.join(&name);
            let m = match side.as_str() {
                "bed" => provision_bed(&ctx, &dir, &name),
                "git-worktree" => {
                    provision_git_worktree(&ctx, side, base.as_ref().unwrap(), &dir, &name);
                    let mut b = git_in(&root);
                    b.args(["switch", "-c", &name]);
                    ctx.step(side, "worktree", "branch", 1, &mut b);
                    Measure {
                        ok: true,
                        ..Measure::default()
                    }
                }
                other => panic!("unknown side {other}"),
            };
            if !m.ok {
                ctx.note(format!("{side}: lane {name} did not provision"));
                continue;
            }
            let rel = format!(
                "{}/ab-contend-{side}/{name}.txt",
                ctx.fixture.sparse.trim_end_matches('/')
            );
            let file = root.join(&rel);
            let _ = fs::create_dir_all(file.parent().unwrap());
            let _ = fs::write(
                &file,
                format!("lane {i} for {side} run {}\n", ctx.run_token),
            );
            let committed = match side.as_str() {
                "bed" => {
                    let mut c = bed(&root);
                    c.args(["commit", "-m", &format!("lane {i}")]);
                    ctx.step(side, "sparse", "prepare-commit", 1, &mut c).ok
                }
                _ => {
                    let mut add = git_in(&root);
                    add.args(["add", &rel]);
                    let added = ctx.step(side, "sparse", "prepare-add", 1, &mut add).ok;
                    let mut c = git_in(&root);
                    c.args(["commit", "-q", "-m", &format!("lane {i}")]);
                    added && ctx.step(side, "sparse", "prepare-commit", 1, &mut c).ok
                }
            };
            if !committed {
                ctx.note(format!(
                    "{side}: lane {name} failed to commit; excluded from the contention set"
                ));
                continue;
            }
            lanes.push((root, name));
        }

        // First wave: publish/push all at once.
        let outcomes = Mutex::new(Vec::new());
        let branch = ctx.fixture.branch.clone();
        let log = &ctx.log;
        let side_ref = side.as_str();
        thread::scope(|scope| {
            for (root, name) in &lanes {
                let outcomes = &outcomes;
                let (branch, log) = (&branch, log);
                scope.spawn(move || {
                    let ok = match side_ref {
                        "bed" => {
                            let mut p = bed(root);
                            p.args(publish_args(branch));
                            run_measured(&mut p, log).ok
                        }
                        _ => {
                            let mut p = git_in(root);
                            p.args(["push", "-q", "origin", &format!("HEAD:{branch}")]);
                            run_measured(&mut p, log).ok
                        }
                    };
                    outcomes.lock().push((name.clone(), ok));
                });
            }
        });
        let mut accepted = 0;
        let mut refused = Vec::new();
        for (name, ok) in outcomes.into_inner() {
            if ok {
                accepted += 1;
            } else {
                refused.push(name);
            }
        }
        println!(
            "{side}: first wave accepted {accepted}/{n}, refused {}",
            refused.len()
        );

        // Serialized retry rounds: rebase onto the newest base, try again.
        let mut rounds = 0;
        while !refused.is_empty() && rounds < 3 {
            rounds += 1;
            let mut still = Vec::new();
            for name in refused {
                let root = dir.join(&name);
                let rebased = match side.as_str() {
                    "bed" => {
                        let mut r = bed(&root);
                        r.args(["rebase", &ctx.fixture.branch]);
                        run_measured(&mut r, &ctx.log).ok
                    }
                    _ => {
                        let mut f = git_in(&root);
                        f.args(["fetch", "-q", "origin"]);
                        let fok = run_measured(&mut f, &ctx.log).ok;
                        let mut r = git_in(&root);
                        r.args(["rebase", "-q", &format!("origin/{}", ctx.fixture.branch)]);
                        fok && run_measured(&mut r, &ctx.log).ok
                    }
                };
                if !rebased {
                    still.push(name);
                    continue;
                }
                let ok = match side.as_str() {
                    "bed" => {
                        let mut p = bed(&root);
                        p.args(publish_args(&ctx.fixture.branch));
                        run_measured(&mut p, &ctx.log).ok
                    }
                    _ => {
                        let mut p = git_in(&root);
                        p.args([
                            "push",
                            "-q",
                            "origin",
                            &format!("HEAD:{}", ctx.fixture.branch),
                        ]);
                        run_measured(&mut p, &ctx.log).ok
                    }
                };
                if !ok {
                    still.push(name);
                }
            }
            refused = still;
        }
        println!(
            "{side}: {} lane(s) unresolved after {rounds} retry round(s)",
            refused.len()
        );
        if !refused.is_empty() {
            ctx.note(format!("{side}: unresolved lanes {refused:?}"));
        }
    }
    ctx.print_report("contend");
}

/// Row attribution: every row names its binary family and its configuration.
fn family(side: &str) -> (&'static str, &'static str) {
    match side {
        "bed" => ("bed", "sparse"),
        "git-partial" => ("git", "partial"),
        "git-full" => ("git", "full"),
        "git-worktree" => ("git", "worktree"),
        // No-checkout history queries: a distinct family row, not a mode.
        "git" => ("git", "history"),
        other => panic!("unknown side {other}"),
    }
}

/// One command, one fresh fixture, every scenario. The repeatable entry
/// point: each run rebuilds the fixture from `--source`, so results are
/// never contaminated by a previous run's pushes or lane commits.
fn run_suite(args: SuiteArgs) {
    let out = args.out.clone();
    if out.exists() && fs::read_dir(&out).map(|d| d.count() > 0).unwrap_or(false) {
        panic!(
            "{} is not empty; a suite run needs a fresh --out",
            out.display()
        );
    }
    fs::create_dir_all(&out).expect("suite root");
    let fixture_root = out.join("fixture");
    // `run_fixture` owns the build; it panics on any failure.
    run_fixture(FixtureArgs {
        source: args.source.clone(),
        commits: args.commits,
        out: fixture_root.clone(),
        bed_port: 0,
        git_port: 0,
        branch: args.branch.clone(),
    });
    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture_root.join("fixture.json")).expect("manifest readable"),
    )
    .expect("manifest parses");
    let bed_url = manifest["bed_url"].as_str().expect("bed_url").to_owned();
    let git_url = manifest["git_url"].as_str().expect("git_url").to_owned();
    let git_ref = fixture_root.join("slice.git");
    let branch = args.branch.clone();
    let query_path = args.query_path.clone().unwrap_or_else(|| {
        // Deterministic pick: the first file under the small prefix.
        let mut c = Command::new("git");
        c.args(["--git-dir"])
            .arg(&git_ref)
            .args(["ls-tree", "-r", "--name-only"]);
        c.arg(format!("refs/heads/{branch}"));
        output(&mut c)
            .unwrap_or_default()
            .lines()
            .find(|l| l.starts_with(&format!("{}/", args.small_sparse)))
            .unwrap_or("README.md")
            .to_owned()
    });

    let base = |name: &str| Fixture {
        bed_url: bed_url.clone(),
        git_url: git_url.clone(),
        git_reference: Some(git_ref.clone()),
        branch: branch.clone(),
        sparse: args.sparse.clone(),
        workers: 2,
        concurrency: 1,
        workdir: Some(out.join(name)),
        json: Some(out.join(format!("{name}.json"))),
        keep: true,
    };

    println!("\n=== suite: fixture built, running scenarios ===\n");

    for n in args
        .workers
        .split(',')
        .filter_map(|v| v.trim().parse::<usize>().ok())
    {
        run_provision(ProvisionArgs {
            fixture: Fixture {
                workers: n,
                ..base(&format!("provision-{n}"))
            },
            sides: None,
        });
    }

    run_query(QueryArgs {
        fixture: base("query"),
        sides: None,
        path: Some(query_path.clone()),
    });
    run_history(HistoryArgs {
        fixture: base("history"),
        path: query_path.clone(),
        sides: None,
    });
    run_refresh(RefreshArgs {
        fixture: Fixture {
            sparse: args.small_sparse.clone(),
            workers: 4,
            ..base("refresh-small")
        },
        sides: None,
    });
    run_refresh(RefreshArgs {
        fixture: Fixture {
            workers: 3,
            ..base("refresh-large")
        },
        sides: None,
    });

    if args.quick {
        println!("\n--quick: skipping land and contend\n");
    } else {
        run_land(LandArgs {
            fixture: Fixture {
                sparse: args.small_sparse.clone(),
                workers: 4,
                ..base("land")
            },
            sides: None,
        });
        run_contend(ContendArgs {
            fixture: Fixture {
                sparse: args.small_sparse.clone(),
                workers: 6,
                ..base("contend")
            },
            sides: None,
        });
    }

    println!(
        "\nsuite complete: {} (results beside it as JSON; kill the fixture servers with \
         `kill $(cat {}/fixture/*.pid)`)",
        out.display(),
        out.display()
    );
}

/// Builds a reproducible fixture pair and starts both servers.
///
/// A bounded history with a **true root commit** is the part that makes a
/// benchmark repeatable: `--limit` in the importer re-roots slice
/// boundaries to full-tree root changesets, and Git refuses to serve a
/// shallow clone to a tool that wants complete history. The construction
/// here is the one both sides accept:
///
/// 1. clone the source (bare) to `out/source.git`;
/// 2. mark the cut point with `git replace --graft` so the boundary commit
///    has no parents — a genuine root, not a dangling parent;
/// 3. `git fast-export` the branch into a fresh bare repository (data
///    included, so every blob is present);
/// 4. import that into a Gardenbed root and start both servers.
///
/// The result is byte-identical history on both sides, bounded by
/// `--commits`, and buildable from any repository — the property the
/// recorded runs need and the kernel corpus lacked.
fn run_fixture(args: FixtureArgs) {
    let out = &args.out;
    if out.exists() && fs::read_dir(out).map(|d| d.count() > 0).unwrap_or(false) {
        panic!(
            "{} is not empty; pass a fresh --out (fixtures are rebuilt, never reused)",
            out.display()
        );
    }
    fs::create_dir_all(out).expect("fixture root");
    let log = out.join("fixture.log");

    let source = out.join("source.git");
    let mut clone = Command::new("git");
    clone
        .args(["clone", "--bare", "-q"])
        .arg(&args.source)
        .arg(&source);
    let m = run_measured(&mut clone, &log);
    if !m.ok {
        panic!("clone failed — see {}", log.display());
    }

    // Graft the cut point so the slice starts at a real root.
    let mut rev = Command::new("git");
    rev.current_dir(&source)
        .args(["rev-list", "-1"])
        .arg(format!("refs/heads/{}~{}", args.branch, args.commits));
    let base = output(&mut rev)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            panic!(
                "cannot find a commit {args_commits} back",
                args_commits = args.commits
            )
        });
    let mut graft = Command::new("git");
    graft
        .current_dir(&source)
        .args(["replace", "--graft", &base]);
    let m = run_measured(&mut graft, &log);
    if !m.ok {
        panic!("graft failed — see {}", log.display());
    }

    // Export just that branch into a fresh bare repository.
    let slice = out.join("slice.git");
    let mut init = Command::new("git");
    init.args(["init", "-q", "--bare"]).arg(&slice);
    let m = run_measured(&mut init, &log);
    if !m.ok {
        panic!("slice init failed — see {}", log.display());
    }
    let export = Command::new("git")
        .current_dir(&source)
        .args(["fast-export", &format!("refs/heads/{}", args.branch)])
        .output()
        .expect("fast-export runs");
    if !export.status.success() {
        panic!(
            "fast-export failed: {}",
            String::from_utf8_lossy(&export.stderr)
        );
    }
    let mut import = Command::new("git");
    import
        .args(["--git-dir"])
        .arg(&slice)
        .args(["fast-import", "--quiet"]);
    import.stdin(Stdio::piped());
    let mut child = import.spawn().expect("fast-import spawns");
    child
        .stdin
        .take()
        .expect("fast-import stdin")
        .write_all(&export.stdout)
        .expect("stream written");
    let status = child.wait().expect("fast-import finishes");
    if !status.success() {
        panic!("fast-import failed — see {}", log.display());
    }

    let count = {
        let mut c = Command::new("git");
        c.args(["--git-dir"])
            .arg(&slice)
            .args(["rev-list", "--count"])
            .arg(format!("refs/heads/{}", args.branch));
        output(&mut c).unwrap_or_default().trim().to_owned()
    };
    let root_parents = {
        let mut c = Command::new("git");
        c.args(["--git-dir"]).arg(&slice).args([
            "rev-list",
            "--max-parents=0",
            &format!("refs/heads/{}", args.branch),
        ]);
        output(&mut c).unwrap_or_default()
    };
    let mut root_check = Command::new("git");
    root_check.args(["--git-dir"]).arg(&slice).args([
        "log",
        "-1",
        "--format=%P",
        root_parents.trim(),
    ]);
    let parents = output(&mut root_check).unwrap_or_default();
    if !parents.trim().is_empty() {
        panic!(
            "slice root {} still names parents {:?} — the slice is not self-contained",
            root_parents.trim(),
            parents.trim()
        );
    }

    // `git fast-import` creates the branch but leaves the fresh bare
    // repository's HEAD on its default (unborn) name; a clone of that
    // reports "You are on a branch yet to be born".
    let mut head = Command::new("git");
    head.args(["--git-dir"])
        .arg(&slice)
        .args(["symbolic-ref", "HEAD"])
        .arg(format!("refs/heads/{}", args.branch));
    let m = run_measured(&mut head, &log);
    if !m.ok {
        panic!(
            "could not point HEAD at {} — see {}",
            args.branch,
            log.display()
        );
    }

    // The servers need these.
    for (dir, cfg, val) in [
        (&slice, "uploadpack.allowFilter", "true"),
        (&slice, "receive.denyNonFastForwards", "true"),
    ] {
        let mut c = Command::new("git");
        c.args(["--git-dir"]).arg(dir).args(["config", cfg, val]);
        run_measured(&mut c, &log);
    }

    // Gardenbed store.
    let bed_root = out.join("bed");
    fs::create_dir_all(&bed_root).expect("bed root");
    let mut imp = Command::new(server_bin());
    imp.args(["import", "--from"])
        .arg(&slice)
        .arg("--root")
        .arg(&bed_root)
        .args(["--repo", "demo"]);
    let m = run_measured(&mut imp, &log);
    if !m.ok {
        panic!("gardenbed-server import failed — see {}", log.display());
    }

    // Start both servers.
    let git_port = if args.git_port == 0 {
        free_port()
    } else {
        args.git_port
    };
    let bed_port = if args.bed_port == 0 {
        free_port()
    } else {
        args.bed_port
    };
    start_git_daemon(out, git_port);
    start_bed_server(&bed_root, bed_port, out);
    wait_for_port(git_port, Duration::from_secs(10));
    wait_for_port(bed_port, Duration::from_secs(10));

    let manifest = json!({
        "branch": args.branch,
        "commits": count,
        "git_url": format!("git://127.0.0.1:{git_port}/slice.git"),
        "git_reference": slice,
        "bed_url": format!("http://127.0.0.1:{bed_port}/demo"),
        "out": out,
    });
    fs::write(
        out.join("fixture.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .expect("manifest written");
    println!("{}", serde_json::to_string_pretty(&manifest).unwrap());
    println!(
        "\nfixture ready (pid files under {}); both servers stay up until killed",
        out.display()
    );
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("free port")
}

fn start_git_daemon(out: &Path, port: u16) {
    let log = fs::File::create(out.join("git-daemon.log")).expect("git daemon log");
    let child = Command::new("git")
        .args([
            "daemon",
            "--reuseaddr",
            "--export-all",
            "--enable=receive-pack",
        ])
        .arg(format!("--port={port}"))
        .arg(format!("--base-path={}", out.display()))
        .arg(out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("git daemon starts");
    fs::write(out.join("git-daemon.pid"), child.id().to_string()).expect("pid file");
    // Deliberately leaked: the fixture outlives this process so the caller
    // can run several scenarios against it.
    std::mem::forget(child);
}

fn start_bed_server(root: &Path, port: u16, out: &Path) {
    let log = fs::File::create(out.join("bed-server.log")).expect("bed log");
    let child = Command::new(server_bin())
        .args(["--root"])
        .arg(root)
        .args(["--addr"])
        .arg(format!("127.0.0.1:{port}"))
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(Stdio::null())
        .spawn()
        .expect("gardenbed-server starts");
    fs::write(out.join("bed-server.pid"), child.id().to_string()).expect("pid file");
    std::mem::forget(child);
}

fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("server on port {port} did not come up within {timeout:?}");
}

/// N warm workers pick up a base that moved: the steady-state agent loop.
///
/// The base advances once with a change *outside* every worker's sparse
/// prefix (a docs edit), then every worker refreshes. This isolates the
/// refresh protocol — what a worker pays per iteration just to stay
/// current — from any checkout work that the change itself would cause.
fn run_refresh(args: RefreshArgs) {
    let sides = parse_sides(
        args.sides.as_deref(),
        &["bed", "git-partial", "git-worktree"],
    );
    let ctx = Ctx::new(args.fixture.clone());
    let n = ctx.fixture.workers;

    for side in &sides {
        let dir = ctx.dir(&format!("refresh-{side}"));
        let base = if side == "git-worktree" {
            Some(git_worktree_base(&ctx, side, &dir))
        } else {
            None
        };
        let mut workers = Vec::new();
        for i in 0..n {
            let name = format!("w{i:03}");
            let root = dir.join(&name);
            let m = match side.as_str() {
                "bed" => provision_bed(&ctx, &dir, &name),
                "git-partial" => provision_git_partial(&ctx, side, &dir, &name),
                "git-worktree" => {
                    provision_git_worktree(&ctx, side, base.as_ref().unwrap(), &dir, &name);
                    Measure {
                        ok: true,
                        ..Measure::default()
                    }
                }
                other => panic!("unknown side {other}"),
            };
            if !m.ok {
                ctx.note(format!("{side}: worker {name} did not provision"));
                continue;
            }
            workers.push(root);
        }
        if workers.is_empty() {
            continue;
        }

        // Advance the base once, outside every sparse prefix, from a
        // dedicated advancer clone. The workers must not be the ones
        // making the change: this measures refresh, not the commit.
        // Inside the sparse prefix on purpose: a base move that a worker
        // must actually pick up is the steady-state agent case (fetch +
        // materialize one file), not a no-op ref update.
        let outside = format!(
            "{}/ab-refresh-{side}/base-moved.txt",
            ctx.fixture.sparse.trim_end_matches('/')
        );
        match side.as_str() {
            "bed" => {
                let adv = dir.join("advancer");
                let m = provision_bed(&ctx, &dir, "advancer");
                if m.ok {
                    let file = adv.join(&outside);
                    let _ = fs::create_dir_all(file.parent().unwrap());
                    let _ = fs::write(&file, format!("moved run {}\n", ctx.run_token));
                    let mut c = bed(&adv);
                    c.args(["commit", "-m", "advance base"]);
                    let _ = ctx.step(side, "sparse", "advance-commit", 1, &mut c);
                    let mut p = bed(&adv);
                    p.args(publish_args(&ctx.fixture.branch));
                    let _ = ctx.step(side, "sparse", "advance-publish", 1, &mut p);
                }
            }
            _ => {
                // A plain (non-sparse) advancer: the outside path must be
                // materialized to be committed by Git.
                let adv = dir.join("advancer");
                let mut clone = git_in(&dir);
                clone
                    .args([
                        "clone",
                        "-q",
                        "--filter=blob:none",
                        "--no-checkout",
                        "--sparse",
                    ])
                    .arg(&ctx.git_url)
                    .arg("advancer");
                let m = ctx.step(side, "sparse", "advance-clone", 1, &mut clone);
                if m.ok {
                    let mut set = git_in(&adv);
                    set.args(["sparse-checkout", "set"])
                        .arg(&ctx.fixture.sparse);
                    let _ = ctx.step(side, "sparse", "advance-sparse-set", 1, &mut set);
                    let mut co = git_in(&adv);
                    co.arg("checkout");
                    let _ = ctx.step(side, "sparse", "advance-checkout", 1, &mut co);
                    let file = adv.join(&outside);
                    let _ = fs::create_dir_all(file.parent().unwrap());
                    let _ = fs::write(&file, format!("moved run {}\n", ctx.run_token));
                    let mut add = git_in(&adv);
                    add.args(["add", &outside]);
                    let _ = ctx.step(side, "sparse", "advance-add", 1, &mut add);
                    let mut c = git_in(&adv);
                    c.args(["commit", "-q", "-m", "advance base"]);
                    let _ = ctx.step(side, "sparse", "advance-commit", 1, &mut c);
                    let mut p = git_in(&adv);
                    p.args([
                        "push",
                        "-q",
                        "origin",
                        &format!("HEAD:{}", ctx.fixture.branch),
                    ]);
                    let _ = ctx.step(side, "sparse", "advance-push", 1, &mut p);
                }
            }
        }

        // Now every *other* worker refreshes.
        let mut refreshed = 0;
        for (i, root) in workers.iter().enumerate().skip(1) {
            let ok = match side.as_str() {
                "bed" => {
                    let mut c = bed(root);
                    c.arg("switch").arg(&ctx.fixture.branch);
                    ctx.step(side, "sparse", "refresh-switch", 1, &mut c).ok
                }
                "git-partial" => {
                    let mut f = git_in(root);
                    f.args(["fetch", "-q", "origin"]);
                    let m = ctx.step(side, "sparse", "refresh-fetch", 1, &mut f);
                    if !m.ok {
                        false
                    } else {
                        let mut r = git_in(root);
                        r.args(["checkout", "-q", &format!("origin/{}", ctx.fixture.branch)]);
                        let _ = ctx.step(side, "sparse", "refresh-checkout", 1, &mut r);
                        true
                    }
                }
                _ => {
                    let mut r = git_in(root);
                    r.args([
                        "reset",
                        "-q",
                        "--hard",
                        &format!("origin/{}", ctx.fixture.branch),
                    ]);
                    let m = ctx.step(side, "sparse", "refresh-reset", 1, &mut r);
                    if !m.ok {
                        false
                    } else {
                        let mut f = git_in(root);
                        f.args(["fetch", "-q", "origin"]);
                        ctx.step(side, "sparse", "refresh-fetch", 1, &mut f).ok
                    }
                }
            };
            if ok {
                refreshed += 1;
            }
            let _ = i;
        }
        println!(
            "{side}: {refreshed}/{} workers refreshed",
            workers.len() - 1
        );
    }
    ctx.print_report("refresh");
}

/// History queries with **no working copy**: the agent that asks "what
/// changed, who wrote this line" without checking anything out.
///
/// * `bed` — `bed log` / `bed file` / `bed blame` against the server from a
///   throwaway state directory: no clone, no sparse selection.
/// * `git` — the same questions against a bare mirror over the wire; `git
///   blame` has no server-side form, so it is measured after fetching the
///   minimal history the query needs.
fn run_history(args: HistoryArgs) {
    let sides = parse_sides(args.sides.as_deref(), &["bed", "git"]);
    let ctx = Ctx::new(args.fixture.clone());
    let path = args.path.clone();

    // One fresh connection and one fresh worker per query kind. Sharing a
    // warm clone would let an earlier query (say `file`) prefetch the blob
    // that `blame` then reads, which flatters whichever side ran second.
    for side in &sides {
        for kind in ["log", "file", "blame"] {
            let dir = ctx.dir(&format!("history-{side}-{kind}"));
            match side.as_str() {
                "bed" => {
                    let mut clone = bed(&dir);
                    clone
                        .arg("clone")
                        .arg(&ctx.bed_url)
                        .args(["--ref", &ctx.fixture.branch]);
                    let m = ctx.step("bed", "history", "connect", 1, &mut clone);
                    if !m.ok {
                        continue;
                    }
                    let (op, mut cmd) = history_query_bed(&dir, &path, kind);
                    ctx.step_captured(
                        "bed",
                        "history",
                        op,
                        &mut cmd,
                        &dir.join(format!("{op}.out")),
                    );
                }
                "git" => {
                    let mirror = dir.join("mirror.git");
                    let mut clone = git_in(&dir);
                    clone
                        .args(["clone", "--bare", "--filter=blob:none"])
                        .arg(&ctx.git_url)
                        .arg("mirror.git");
                    let m = ctx.step("git", "history", "connect", 1, &mut clone);
                    if !m.ok {
                        continue;
                    }
                    let (op, mut cmd) = history_query_git(&mirror, &path, kind);
                    ctx.step_captured(
                        "git",
                        "history",
                        op,
                        &mut cmd,
                        &dir.join(format!("{op}.out")),
                    );
                }
                other => panic!("unknown side {other}"),
            }
        }
    }
    ctx.print_report("history");
}

fn history_query_bed(state: &Path, path: &str, kind: &str) -> (&'static str, Command) {
    match kind {
        "log" => {
            let mut log = bed(state);
            log.args(["log", "--limit", "50"]);
            ("log", log)
        }
        "file" => {
            let mut file = bed(state);
            file.args(["file", path]);
            ("file", file)
        }
        "blame" => {
            let mut blame = bed(state);
            blame.args(["blame", path]);
            ("blame", blame)
        }
        other => panic!("unknown query {other}"),
    }
}

fn history_query_git(mirror: &Path, path: &str, kind: &str) -> (&'static str, Command) {
    match kind {
        "log" => {
            let mut log = git_in(mirror);
            log.args(["--git-dir", "."]).args(["log", "-n", "50"]);
            ("log", log)
        }
        "file" => {
            let mut file = git_in(mirror);
            file.args(["--git-dir", "."])
                .args(["show"])
                .arg(format!("HEAD:{path}"));
            ("file", file)
        }
        "blame" => {
            let mut blame = git_in(mirror);
            blame.args(["--git-dir", "."]).args([
                "blame",
                "--first-parent",
                "-L",
                "1,50",
                "HEAD",
                "--",
                path,
            ]);
            ("blame", blame)
        }
        other => panic!("unknown query {other}"),
    }
}

fn parse_sides(raw: Option<&str>, default: &[&str]) -> Vec<String> {
    raw.map(|s| s.split(',').map(str::trim).map(str::to_owned).collect())
        .unwrap_or_else(|| default.iter().map(|s| (*s).to_owned()).collect())
}

fn main() {
    let cli = Cli::parse();
    if !matches!(cli.scenario, Scenario::Fixture(_)) {
        report_binaries();
    }
    match cli.scenario {
        Scenario::Provision(a) => run_provision(a),
        Scenario::Query(a) => run_query(a),
        Scenario::Land(a) => run_land(a),
        Scenario::Contend(a) => run_contend(a),
        Scenario::Refresh(a) => run_refresh(a),
        Scenario::History(a) => run_history(a),
        Scenario::Fixture(a) => run_fixture(a),
        Scenario::Suite(a) => run_suite(a),
    }
}
