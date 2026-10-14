//! Shared bench world: tmpfs fixtures, a spawned `gardenbed-server`
//! binary, generated histories ingested in-process, and lib-side beds.
//!
//! The same rules as bed's perf tests apply (and for the same reason):
//! fixtures live on tmpfs when one exists (`/dev/shm`), because the
//! object store fsyncs every object it publishes — the dominant cost on
//! ext4, a rounding error on tmpfs — and the benches measure the *design*,
//! not the host disk. No durability toggles are touched: `GARDENBED_FSYNC`
//! stays unset, exactly as a real deployment runs.

// Every bench binary compiles this module; the helpers each one doesn't
// use are dead code from its point of view.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use gardenbed_harness::generator::{GenOptions, GeneratedHistory, files_at, generate};
use gardenbed_model::{Id, Mode, blob_id};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::{Duration, timeout};

/// The repo name every fixture is ingested under (and the bed's default).
pub const REPO: &str = "garden";

/// Reads a usize env knob, falling back to `default` (and flooring at
/// `min`). Benches keep their heavy dimensions env-tunable so CI can shrink
/// them without touching code.
pub fn env_usize(name: &str, default: usize, min: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.max(min))
        .unwrap_or(default)
}

/// The bench fixtures' temp root: tmpfs (`/dev/shm`) when it exists, the
/// standard temp dir otherwise.
pub fn temp_root() -> TempDir {
    let shm = Path::new("/dev/shm");
    if shm.is_dir() {
        tempfile::TempDir::new_in(shm)
    } else {
        tempfile::tempdir()
    }
    .expect("bench tempdir")
}

/// Locates the built `gardenbed-server` binary. Order: the
/// `BED_SERVER_BIN` override, then the workspace `target/{release,debug}`
/// directory (release first in release builds — benches run optimized).
/// Panics with actionable advice when absent.
pub fn server_bin() -> PathBuf {
    if let Ok(path) = std::env::var("BED_SERVER_BIN") {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "BED_SERVER_BIN points at {path:?}, not a file"
        );
        return path;
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // crates/bench
    let workspace_target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("..").join("..").join("target"));
    let profiles: &[&str] = if cfg!(debug_assertions) {
        &["debug", "release"]
    } else {
        &["release", "debug"]
    };
    for profile in profiles {
        let candidate = workspace_target.join(profile).join("gardenbed-server");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "gardenbed-server binary not found under {} — run \
         `cargo build --release --workspace` first (or set BED_SERVER_BIN)",
        workspace_target.display()
    )
}

/// One running arboretum: the spawned `gardenbed-server` process, its
/// bound address, and its repo root. Killed (and the tmpfs removed) when
/// the bench's world drops.
pub struct Arboretum {
    pub child: tokio::process::Child,
    /// The `ip:port` the server announced.
    pub addr: String,
    pub root: PathBuf,
}

impl Arboretum {
    /// Spawns the server binary over `root` and waits (≤ 30 s) for its
    /// `listening on <addr>` stdout line.
    pub async fn spawn(root: &Path) -> Self {
        let mut child = tokio::process::Command::new(server_bin())
            .arg("--addr")
            .arg("127.0.0.1:0")
            .arg("--root")
            .arg(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("gardenbed-server binary spawns");
        let stdout = child.stdout.take().expect("server stdout piped");
        let line = timeout(Duration::from_secs(30), async {
            BufReader::new(stdout).lines().next_line().await
        })
        .await
        .expect("server announces itself within 30s")
        .expect("server stdout open")
        .expect("server prints a line");
        let addr = line
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("expected a listening line, got {line:?}"))
            .to_owned();
        Arboretum {
            child,
            addr,
            root: root.to_path_buf(),
        }
    }

    /// The endpoint bed expects: `http://ip:port`.
    pub fn http(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Terminates the server and waits for it to exit.
    pub async fn shutdown(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// True if a blob id is a submodule pointer: 12 zero bytes followed by the
/// 20-byte upstream git SHA-1 (canonical spec §3). Never a loam object.
pub fn is_gitlink_pointer(id: &Id, history: &GeneratedHistory) -> bool {
    id.as_bytes()[..12].iter().all(|b| *b == 0)
        && history.blob(id) == Some(id.as_bytes().as_slice())
}

/// Ingests a generated history into the repo rooted at `root` (in-process,
/// through the server library — fixture setup must not pollute request
/// counters). Returns the head changeset id.
pub fn ingest(history: &GeneratedHistory, root: &Path) -> Id {
    let repo = gardenbed_server::Repo::open(root).expect("repo opens");
    for cs in &history.changesets {
        let mut inline: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for change in &cs.changes {
            let Some(blob) = change.blob else { continue };
            if change.mode == Mode::Gitlink || is_gitlink_pointer(&blob, history) {
                continue; // opaque upstream pointer, never a loam object
            }
            let content = history
                .blob(&blob)
                .expect("generated history carries every blob it references")
                .to_vec();
            debug_assert_eq!(blob_id(&content), blob);
            inline.insert(blob.to_hex(), content);
        }
        repo.create_changeset(cs, &inline)
            .expect("generated changeset ingests cleanly");
    }
    repo.cas_ref("main", None, history.head)
        .expect("main ref created");
    history.head
}

/// A generated history whose head mixes exec, symlink, and gitlink files
/// in with the regular ones (the first qualifying seed in 1..100 — the
/// same fixture contract bed's perf tests use).
pub fn seeded_history(opts: &GenOptions) -> GeneratedHistory {
    (1..100u64)
        .map(|s| generate(s, opts))
        .find(|h| {
            let files = files_at(h, h.changesets.len() - 1);
            let has = |want: Mode| files.values().any(|(_, m)| *m == want);
            has(Mode::Exec) && has(Mode::Symlink) && has(Mode::Gitlink)
        })
        .expect("a qualifying seed exists in 1..100")
}

/// A planted bed beside its garden root: `<dir>` holds the working files,
/// `<dir>/.bed` the bed state.
pub struct Garden {
    /// The working-copy root (what `bed.garden_root()` resolves to).
    pub dir: PathBuf,
    /// The bed state directory (`<dir>/.bed`).
    pub bed: PathBuf,
}

impl Garden {
    /// Creates the directory and plants a fresh bed over `server_http`
    /// (the lib-side `ops::clone`: one ref read, nothing materialized).
    pub async fn plant(dir: &Path, server_http: &str) -> Self {
        std::fs::create_dir_all(dir).expect("garden dir");
        let bed = dir.join(".bed");
        gardenbed::ops::clone(&bed, server_http, None, Vec::new())
            .await
            .expect("bed clones");
        Garden {
            dir: dir.to_path_buf(),
            bed,
        }
    }
}

/// Ingests `total_files` bulk files (in changesets of `per_cs` adds,
/// spread over per-changeset directories so each tree apply touches a
/// handful of nodes) plus a `small/` plot of `small_files` files, and
/// points `main` at the head. The same fixture shape bed's M5 size-
/// independence budget uses; the sow benches measure the identical delta
/// against repos of different total size on top of it.
pub fn build_sized_repo(root: &Path, total_files: usize, per_cs: usize, small_files: usize) -> Id {
    use gardenbed_model::{Change, Changeset, Identity, Op};

    let repo = gardenbed_server::Repo::open(root).expect("repo opens");
    let ident = |secs: u64| Identity {
        name: "Seeder".into(),
        email: "seed@bed.example".into(),
        time_secs: secs,
        tz_min: 0,
    };
    let mut parent: Option<Id> = None;
    let mut made = 0usize;
    let mut cs_no = 0usize;
    while made < total_files {
        let n = per_cs.min(total_files - made);
        let mut changes = Vec::with_capacity(n + small_files.min(1));
        let mut inline = BTreeMap::new();
        for k in 0..n {
            let path = format!("bulk/d{cs_no:03}/f{k:05}.txt");
            let content = format!("row {} of the bulk fixture\n", made + k);
            let blob = blob_id(content.as_bytes());
            changes.push(Change {
                path,
                op: Op::Add,
                blob: Some(blob),
                src_path: None,
                mode: Mode::Regular,
            });
            inline.insert(blob.to_hex(), content.into_bytes());
        }
        if cs_no == 0 {
            for k in 0..small_files {
                let path = format!("small/f{k:03}.txt");
                let content = format!("small file {k}\n");
                let blob = blob_id(content.as_bytes());
                changes.push(Change {
                    path,
                    op: Op::Add,
                    blob: Some(blob),
                    src_path: None,
                    mode: Mode::Regular,
                });
                inline.insert(blob.to_hex(), content.into_bytes());
            }
        }
        let cs = Changeset {
            parents: parent.into_iter().collect(),
            changes,
            message: format!("bulk seed {cs_no}"),
            author: ident(1_700_000_000 + cs_no as u64),
            committer: ident(1_700_000_000 + cs_no as u64),
            signature: None,
        };
        let id = repo
            .create_changeset(&cs, &inline)
            .expect("bulk changeset ingests");
        parent = Some(id);
        made += n;
        cs_no += 1;
    }
    let head = parent.expect("at least one changeset");
    repo.cas_ref("main", None, head).expect("main created");
    head
}
