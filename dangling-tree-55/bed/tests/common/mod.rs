//! Shared helpers for working copy's integration tests: run the real `bed`
//! binary, spawn the real `gardenbed-server` binary on an ephemeral
//! port, ingest a generated history into its repo (in-process, through
//! the server library — fixture setup must not pollute the request
//! counters the tests assert on), and compute the exact want-list a sparse
//! should fetch.
//!
//! The server binary is not a bin target of this package, so
//! `CARGO_BIN_EXE_gardenbed-server` is unavailable here; it is located
//! under the workspace `target/` directory (or overridden with
//! `BED_SERVER_BIN`). Build the workspace first.

// Every integration test binary compiles this module; the helpers each
// binary doesn't use are dead code from its point of view.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use gardenbed_model::{Id, Mode, TreeKind, blob_id};
use gardenbed_protocol::client::GardenbedClient;
use gardenbed_server::Repo;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::{Duration, timeout};

/// The repo name every fixture is ingested under (and the working copy's default).
pub const REPO: &str = "garden";

/// Runs the real `bed` binary in `dir`, returning (exit code, stdout,
/// stderr).
pub fn bed(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("bed binary spawns");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs the real `bed` binary in `dir`, returning raw (exit code, stdout,
/// stderr) bytes — for byte-exact output comparisons (`bed file`).
pub fn bed_raw(dir: &Path, args: &[&str]) -> (i32, Vec<u8>, Vec<u8>) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("bed binary spawns");
    (out.status.code().unwrap_or(-1), out.stdout, out.stderr)
}

/// Runs the real `bed` binary in `dir` asynchronously (for tests that
/// must kill it mid-run or poll the server while it works). Returns the
/// spawned child.
pub fn bed_async(dir: &Path, args: &[&str]) -> tokio::process::Child {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("bed binary spawns")
}

/// Locates the built `gardenbed-server` binary. Order: the
/// `BED_SERVER_BIN` override, then the workspace `target/{debug,release}`
/// directory (the profile is probed both ways, debug first in debug
/// builds). Panics with actionable advice when absent.
pub fn server_bin() -> PathBuf {
    if let Ok(path) = std::env::var("BED_SERVER_BIN") {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "BED_SERVER_BIN points at {path:?}, not a file"
        );
        return path;
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // crates/bed
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
         `cargo build --workspace` first (or set BED_SERVER_BIN)",
        workspace_target.display()
    )
}

/// One running server: the spawned `gardenbed-server` process, its
/// bound address, and a client for stats polling.
pub struct Server {
    pub child: tokio::process::Child,
    /// The `ip:port` the server announced.
    pub addr: String,
    pub root: PathBuf,
}

impl Server {
    /// Spawns the server binary over `root` and waits (≤ 30 s) for its
    /// `listening on <addr>` stdout line.
    pub async fn spawn(root: &Path) -> Self {
        let mut child = tokio::process::Command::new(server_bin())
            .arg("--addr")
            .arg("127.0.0.1:0")
            .arg("--root")
            .arg(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("gardenbed-server binary spawns");
        let stdout = child.stdout.take().expect("server stdout piped");
        let line = timeout(Duration::from_secs(30), async {
            let mut lines = BufReader::new(stdout).lines();
            lines
                .next_line()
                .await
                .expect("server stdout open")
                .expect("server prints a line")
        })
        .await
        .expect("server announces itself within 30s");
        let addr = line
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("expected a listening line, got {line:?}"))
            .to_owned();
        Server {
            child,
            addr,
            root: root.to_path_buf(),
        }
    }

    /// The endpoint bed expects: `http://ip:port`.
    pub fn http(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// A connected typed client.
    pub async fn client(&self) -> GardenbedClient {
        GardenbedClient::connect(&self.http())
            .await
            .expect("client connects to the server")
    }

    /// A snapshot of the server's counted RPCs: `(get_tree, get_blob)`.
    pub async fn counted(&self, client: &GardenbedClient) -> (u64, u64) {
        let stats: BTreeMap<String, u64> = client.get_stats(REPO).await.expect("GetStats works");
        (
            *stats.get("get_tree").expect("get_tree counter"),
            *stats.get("get_blob").expect("get_blob counter"),
        )
    }

    /// Terminates the server and waits for it to exit.
    pub async fn shutdown(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// True if a blob id is a submodule pointer: 12 zero bytes followed by the
/// 20-byte upstream git SHA-1 (canonical spec §3 — the pointer's "content"
/// is the padded pointer itself). Such ids are never objects objects.
pub fn is_gitlink_pointer(
    id: &Id,
    history: &gardenbed_harness::generator::GeneratedHistory,
) -> bool {
    id.as_bytes()[..12].iter().all(|b| *b == 0)
        && history.blob(id) == Some(id.as_bytes().as_slice())
}

/// Ingests a generated history into the repo rooted at `root` (the same
/// layout the spawned server serves): every changeset with its blobs
/// inline, then `main` fast-forwarded to the head. Runs through the
/// server library, so **no request counters are bumped**. Returns the
/// head changeset id.
pub fn ingest(history: &gardenbed_harness::generator::GeneratedHistory, root: &Path) -> Id {
    let repo = Repo::open(root).expect("repo opens");
    for cs in &history.changesets {
        let mut inline: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for change in &cs.changes {
            let Some(blob) = change.blob else { continue };
            if change.mode == Mode::Gitlink || is_gitlink_pointer(&blob, history) {
                continue; // opaque upstream pointer, never a objects object
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

/// should fetch at `head` under `prefixes`:
/// `(distinct tree ids, distinct non-gitlink blob ids)`. Computed by
/// walking the server's derived trees in-process (uncounted), mirroring
/// `plan_sparse`'s traversal exactly.
pub fn want_list(root: &Path, head: Id, prefixes: &[&str]) -> (HashSet<Id>, HashSet<Id>) {
    let repo = Repo::open(&root.join(REPO)).expect("repo opens");
    let mut trees: HashSet<Id> = HashSet::new();
    let mut blobs: HashSet<Id> = HashSet::new();
    let top = repo
        .root_tree(head)
        .expect("root_tree works")
        .expect("the head changeset has files");

    let walk_tree = |id: Id, trees: &mut HashSet<Id>, blobs: &mut HashSet<Id>| {
        let mut stack = vec![id];
        while let Some(tid) = stack.pop() {
            if !trees.insert(tid) {
                continue;
            }
            let tree = repo
                .get_tree(tid)
                .expect("tree readable")
                .expect("derived tree exists");
            for entry in &tree.entries {
                match entry.kind {
                    TreeKind::Dir => stack.push(entry.tree.expect("dir carries a tree id")),
                    TreeKind::File => {
                        if entry.mode != Mode::Gitlink {
                            blobs.insert(entry.blob.expect("file carries a blob id"));
                        }
                    }
                }
            }
        }
    };

    for &prefix in prefixes {
        if prefix == "." {
            walk_tree(top, &mut trees, &mut blobs);
            continue;
        }
        let mut cur = top;
        let comps: Vec<&str> = prefix.split('/').collect();
        for (i, comp) in comps.iter().enumerate() {
            let tree = repo
                .get_tree(cur)
                .expect("tree readable")
                .expect("derived tree exists");
            let Some(entry) = tree.entries.iter().find(|e| e.name == *comp) else {
                break; // names nothing — nothing to want
            };
            let last = i + 1 == comps.len();
            match entry.kind {
                TreeKind::Dir => {
                    let id = entry.tree.expect("dir carries a tree id");
                    if last {
                        walk_tree(id, &mut trees, &mut blobs);
                    }
                    cur = id;
                }
                TreeKind::File => {
                    if last && entry.mode != Mode::Gitlink {
                        blobs.insert(entry.blob.expect("file carries a blob id"));
                    }
                    break;
                }
            }
        }
    }
    (trees, blobs)
}

/// How many of `ids` the working copy's local object cache already holds.
pub fn cached_count(state_dir: &Path, ids: &HashSet<Id>) -> usize {
    // The store root is `.bed` (FsObjectStore appends `objects/` itself).
    let store = gardenbed_store::FsObjectStore::open(state_dir).expect("bed object cache opens");
    use gardenbed_store::ObjectStore as _;
    ids.iter().filter(|id| store.has(**id).unwrap()).count()
}
