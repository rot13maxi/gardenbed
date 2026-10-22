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
        .env_remove("GARDENBED_AUTHOR")
        .output()
        .expect("bed binary spawns");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs the real `bed` binary in `dir` with `GARDENBED_TOKEN` set (an
/// authed server's bearer token). Returns (exit code, stdout, stderr).
pub fn bed_with_token(dir: &Path, args: &[&str], token: &str) -> (i32, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .env("GARDENBED_TOKEN", token)
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
        .env_remove("GARDENBED_AUTHOR")
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
        .env_remove("GARDENBED_AUTHOR")
        .stderr(Stdio::piped())
        .spawn()
        .expect("bed binary spawns")
}

/// Locates the built `gardenbed-server` binary. Order: the
/// `BED_SERVER_BIN` override, then the workspace `target/{debug,release}`
/// directory (the profile is probed both ways, debug first in debug
/// builds). Panics with actionable advice when absent or stale — the
/// binary is not a bin target of this package, so `cargo test -p
/// gardenbed` never rebuilds it, and a stale binary fails
/// protocol-dependent tests in ways that look like code regressions
/// (dogfood finding #12). Two staleness oracles, both loud:
///
/// * mtime: the binary must be no older than the newest source;
/// * content fingerprint read FROM THE BINARY'S OWN BYTES: the located
///   executable must contain the marker
///   `GARDENBED_SERVER_SRC_DIGEST:<hex>` (a `&'static str` in
///   `gardenbed_server::SRC_DIGEST_MARKER`, referenced from the bin's
///   `main` so it lands in rodata), and `<hex>` must equal the digest
///   computed over the *current* sources.
///
/// The fingerprint must be read from the file: the crate's `SRC_DIGEST`
/// const is linked into the **test** binary, so comparing it would always
/// compare the working copy with itself and accept a binary built from
/// other tree state — `bed switch` materializes sources with commit-time
/// mtimes, so mtime alone cannot see that either.
///
/// `BED_SERVER_BIN` overrides skip both — an externally provided binary
/// is trusted as-is.
pub fn server_bin() -> PathBuf {
    locate_guarded_bin(&BinSpec {
        name: "gardenbed-server",
        override_var: "BED_SERVER_BIN",
        crate_dir: "server",
        digest_marker_prefix: DIGEST_MARKER_PREFIX_SERVER,
        build_cmd: "cargo build -p gardenbed-server",
    })
}

/// The `..`-free marker prefixes the build scripts' digest is embedded
/// under; `locate_guarded_bin` scans the located binary for one of them.
pub const DIGEST_MARKER_PREFIX_SERVER: &str = "GARDENBED_SERVER_SRC_DIGEST:";
pub const DIGEST_MARKER_PREFIX_DAEMON: &str = "GARDENBED_DAEMON_SRC_DIGEST:";

/// One sibling binary located by the harness: its name, the env override
/// that bypasses the staleness oracles, the crate whose `src/` is the
/// source of truth, and the marker prefix its build embeds.
struct BinSpec {
    name: &'static str,
    override_var: &'static str,
    /// Crate directory under `crates/` (its `src/` is hashed).
    crate_dir: &'static str,
    /// The prefix of the fingerprint marker embedded in the binary
    /// (`GARDENBED_<CRATE>_SRC_DIGEST:`), directly followed by the hex.
    digest_marker_prefix: &'static str,
    /// The exact rebuild command the STALE panic text names.
    build_cmd: &'static str,
}

/// The one implementation of the two-oracle staleness guard shared by
/// `server_bin()` and `daemon_bin()`. The override in `spec.override_var`
/// is trusted as-is (existence only); otherwise each present candidate is
/// checked for mtime and tree-state freshness before being returned.
fn locate_guarded_bin(spec: &BinSpec) -> PathBuf {
    let BinSpec {
        name,
        override_var,
        crate_dir,
        digest_marker_prefix,
        build_cmd,
    } = *spec;
    if let Ok(path) = std::env::var(override_var) {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "{override_var} points at {path:?}, not a file"
        );
        return path;
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // crates/bed
    let workspace_target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("..").join("..").join("target"));
    let src = manifest
        .join("..")
        .join(crate_dir)
        .join("src")
        .canonicalize()
        .unwrap_or_else(|e| panic!("crates/{crate_dir}/src exists next to crates/bed: {e}"));
    let profiles: &[&str] = if cfg!(debug_assertions) {
        &["debug", "release"]
    } else {
        &["release", "debug"]
    };
    for profile in profiles {
        let candidate = workspace_target.join(profile).join(name);
        if !candidate.is_file() {
            continue;
        }
        if let Err(msg) = check_binary_against_sources(
            name,
            crate_dir,
            build_cmd,
            digest_marker_prefix,
            &candidate,
            &src,
            profile,
        ) {
            panic!("{msg}");
        }
        return candidate;
    }
    panic!(
        "{name} binary not found under {} — run `{build_cmd}` first (or set \
         {override_var})",
        workspace_target.display()
    )
}

/// Runs the two staleness oracles for one candidate binary against one
/// source tree, returning the exact actionable message on refusal. Split
/// out so the guard's regression tests can drive it with a controlled
/// binary/source pair — the real workspace pair, or a copied tree whose
/// mtimes are forced older than the binary — instead of mutating the
/// sources every other test binary compiles against.
pub fn check_binary_against_sources(
    name: &str,
    crate_dir: &str,
    build_cmd: &str,
    digest_marker_prefix: &str,
    candidate: &Path,
    src: &Path,
    profile: &str,
) -> Result<(), String> {
    let crate_label = format!("crates/{crate_dir}");
    // The STALE branch uses plain `build_cmd` (the sources are newer than
    // the binary, so Cargo rebuilds). The DIFFERENT / markerless branches
    // fire only when every source is OLDER than the binary — exactly the
    // state in which Cargo considers the crate fresh and `build_cmd`
    // would be a NO-OP. Touching a source first makes it newer, forcing
    // the recompile. The profile suffix keeps the remedy aimed at the
    // binary actually located (a debug build cannot cure a release file).
    let force_rebuild_cmd = format!(
        "touch {crate_label}/src/lib.rs && {build_cmd}{}",
        if profile == "release" {
            " --release"
        } else {
            ""
        }
    );
    let binary_age = candidate
        .metadata()
        .and_then(|m| m.modified())
        .map_err(|e| format!("{name} binary mtime: {e}"))?;
    let mut src_age = None;
    for entry in walk_files(src) {
        let Ok(age) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if src_age.is_none_or(|age0| age > age0) {
            src_age = Some(age);
        }
    }
    let src_age = src_age.ok_or_else(|| {
        format!(
            "no source files found under {} — staleness check is broken",
            src.display()
        )
    })?;
    if src_age > binary_age {
        return Err(format!(
            "{name} binary under {} is STALE — it is older than \
             {crate_label} sources (dogfood finding #12: `cargo test -p \
             gardenbed` does not rebuild it). Rebuild first: `{build_cmd}`",
            candidate.display()
        ));
    }
    // The fingerprint MUST come from the located file. `SRC_DIGEST` is
    // compiled into this test binary (from the working copy), so a
    // comparison against it would be a no-op that accepts a binary built
    // from other tree state.
    let embedded = embedded_digest_of(candidate, digest_marker_prefix).ok_or_else(|| {
        format!(
            "{name} binary under {} carries no {digest_marker_prefix} \
             marker — it cannot be checked against {crate_label} sources. \
             Rebuild first (from the workspace root): `{force_rebuild_cmd}`",
            candidate.display()
        )
    })?;
    if current_src_digest(src) != embedded {
        return Err(format!(
            "{name} binary under {} was built from DIFFERENT \
             {crate_label} sources (the fingerprint embedded in its own \
             bytes does not match the working copy's {crate_label}/src — \
             dogfood finding #12: mtimes look fresh but the tree changed \
             under the binary, e.g. a bed switch). Rebuild first (from \
             the workspace root): `{force_rebuild_cmd}`",
            candidate.display()
        ));
    }
    Ok(())
}

/// Reads the build fingerprint out of a binary's bytes: the 64 lowercase
/// hex chars directly after `marker_prefix` (`GARDENBED_<CRATE>_SRC_
/// DIGEST:`), as embedded by the crate's `build.rs` and referenced from
/// the bin's `main` via `SRC_DIGEST_MARKER`. `None` when the marker is
/// absent (an old or hand-built binary).
pub fn embedded_digest_of(binary: &Path, marker_prefix: &str) -> Option<String> {
    let bytes = std::fs::read(binary).ok()?;
    let needle = marker_prefix.as_bytes();
    let mut at = 0;
    while let Some(rel) = find_bytes(&bytes[at..], needle) {
        let start = at + rel + needle.len();
        let end = start + 64;
        if end <= bytes.len() {
            let hex = &bytes[start..end];
            if hex.iter().all(|b| b.is_ascii_hexdigit()) {
                return Some(String::from_utf8_lossy(hex).into_owned());
            }
        }
        at = start;
    }
    None
}

/// `memchr`-free substring search over the (≤ a few hundred MiB) binary;
/// `windows` is linear enough at this scale and keeps the test helper
/// dependency-free.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The same digest `crates/{server,daemon}/build.rs` embed: SHA-256 over
/// (sorted rel-path, length, content) of every file under `src/` —
/// deterministic, independent of readdir order and mtimes.
pub fn current_src_digest(src: &Path) -> String {
    use std::collections::BTreeMap;

    use sha2::{Digest, Sha256};

    let crate_root = src
        .parent()
        .expect("a crate's src has a parent (crates/<name>)");
    let mut files: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    for path in walk_files(src) {
        let rel = path
            .strip_prefix(crate_root)
            .expect("file under the crate root");
        files.insert(
            rel.to_path_buf(),
            std::fs::read(&path).expect("src readable"),
        );
    }
    assert!(
        !files.is_empty(),
        "no sources to hash under {}",
        src.display()
    );
    let mut hasher = Sha256::new();
    for (rel, content) in &files {
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(content.len().to_le_bytes());
        hasher.update(content);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Every regular file under `dir`, recursively (test-fixture scale:
/// a crate's `src` is a few dozen files).
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
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

    /// Spawns the server binary over `root` with token auth from
    /// `auth_path` (`serve --auth`), waiting for its `listening on
    /// <addr>` stdout line. `spawn` (no auth) stays the open-server
    /// default.
    pub async fn spawn_with_auth(root: &Path, auth_path: &Path) -> Self {
        let mut child = tokio::process::Command::new(server_bin())
            .arg("serve")
            .arg("--auth")
            .arg(auth_path)
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

    /// Spawns the server binary over `root` bound to a *fixed* address
    /// (`serve --addr ADDR`): for tests that need two server processes to
    /// be reachable under one endpoint string (a daemon restart whose
    /// scope key — a digest over the upstream endpoint — must stay stable
    /// across the restart, standing in for the same server coming back).
    pub async fn spawn_at_addr(root: &Path, addr: &str) -> Self {
        let mut child = tokio::process::Command::new(server_bin())
            .arg("serve")
            .arg("--addr")
            .arg(addr)
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
        let announced = line
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("expected a listening line, got {line:?}"))
            .to_owned();
        assert_eq!(announced, addr, "the server binds the requested address");
        Server {
            child,
            addr: addr.to_owned(),
            root: root.to_path_buf(),
        }
    }

    /// The endpoint bed expects: `http://ip:port`.
    pub fn http(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The endpoint bed expects for the fixture's repo (v0.2 URL
    /// addressing): `http://ip:port/garden`.
    pub fn http_garden(&self) -> String {
        format!("http://{}/{}", self.addr, REPO)
    }

    /// [`Server::spawn_with_auth`] bound to a fixed address (see
    /// [`Server::spawn_at_addr`]): tokened fixtures that also need a
    /// stable endpoint across server restarts.
    pub async fn spawn_with_auth_at_addr(root: &Path, auth_path: &Path, addr: &str) -> Self {
        let mut child = tokio::process::Command::new(server_bin())
            .arg("serve")
            .arg("--auth")
            .arg(auth_path)
            .arg("--addr")
            .arg(addr)
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
        let announced = line
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("expected a listening line, got {line:?}"))
            .to_owned();
        assert_eq!(announced, addr, "the server binds the requested address");
        Server {
            child,
            addr: addr.to_owned(),
            root: root.to_path_buf(),
        }
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

    /// [`Server::counted`] for a named repository (fixtures with repos
    /// other than `REPO`).
    pub async fn counted_for_repo(&self, client: &GardenbedClient, repo: &str) -> (u64, u64) {
        let stats: BTreeMap<String, u64> = client.get_stats(repo).await.expect("GetStats works");
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

/// One running gardenbed-daemon: the spawned process, its socket path,
/// and its isolated shared-cache root.
pub struct Daemon {
    pub child: tokio::process::Child,
    /// The unix socket the daemon announced.
    pub socket: PathBuf,
    /// The isolated XDG_CACHE_HOME root (per-daemon, under the tempdir).
    pub cache_home: PathBuf,
    /// The temp root keeping the socket and cache alive.
    pub root: PathBuf,
}

/// Locates the built `gardenbed-daemon` binary. Order: the
/// `BED_DAEMON_BIN` override, then the workspace `target/{debug,release}`
/// directory. Panics with actionable advice when absent — and, unless the
/// override is used, when the binary is stale by either oracle the server
/// guard applies (`locate_guarded_bin`): mtime, and the
/// `GARDENBED_DAEMON_SRC_DIGEST:<hex>` fingerprint read from the located
/// file's own bytes and compared with the current sources. A stale daemon
/// has produced a spurious 4 MiB test failure; dogfood finding #12's
/// class. `BED_DAEMON_BIN` bypasses both oracles.
pub fn daemon_bin() -> PathBuf {
    locate_guarded_bin(&BinSpec {
        name: "gardenbed-daemon",
        override_var: "BED_DAEMON_BIN",
        crate_dir: "daemon",
        digest_marker_prefix: DIGEST_MARKER_PREFIX_DAEMON,
        build_cmd: "cargo build -p gardenbed-daemon",
    })
}

impl Daemon {
    /// Spawns the daemon proxying `upstream`, with an isolated socket and
    /// shared cache under `root`. Waits (≤ 30 s) for the
    /// `listening on <path>` stdout line.
    pub async fn spawn(root: &Path, upstream: &str) -> Self {
        let cache_home = root.join("xdg-cache");
        std::fs::create_dir_all(root).expect("daemon root created");
        Self::spawn_with_socket(&root.join("daemon.sock"), &cache_home, upstream).await
    }

    /// Spawns the daemon proxying `upstream` with a *shared* cache root
    /// and caller-chosen socket path: for tests that restart the daemon
    /// over the same cache (or front a different upstream with it). The
    /// `--cache` dir is `<cache_home>/gardenbed`, matching [`Daemon::spawn`].
    pub async fn spawn_with_socket(socket: &Path, cache_home: &Path, upstream: &str) -> Self {
        let root = socket
            .parent()
            .expect("socket path has a parent")
            .to_path_buf();
        std::fs::create_dir_all(&root).expect("daemon root created");
        let mut child = tokio::process::Command::new(daemon_bin())
            .arg("serve")
            .arg("--upstream")
            .arg(upstream)
            .arg("--socket")
            .arg(socket)
            .arg("--cache")
            .arg(cache_home.join("gardenbed"))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("gardenbed-daemon binary spawns");
        let stdout = child.stdout.take().expect("daemon stdout piped");
        let line = timeout(Duration::from_secs(30), async {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let line = lines
                    .next_line()
                    .await
                    .expect("daemon stdout open")
                    .expect("daemon prints a line");
                if let Some(path) = line.strip_prefix("listening on ") {
                    return path.to_owned();
                }
                // Banner lines before the announcement are skipped.
            }
        })
        .await
        .expect("daemon announces itself within 30s");
        Daemon {
            child,
            socket: PathBuf::from(line),
            cache_home: cache_home.to_path_buf(),
            root,
        }
    }

    /// Kills the daemon without cleanup (SIGKILL semantics for the
    /// crash-fallback tests).
    pub async fn kill(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// Runs the real `bed` binary in `dir` **through the daemon**: sets
/// `GARDENBED_DAEMON` to the daemon's socket and isolates
/// `XDG_CACHE_HOME`. Returns (exit code, stdout, stderr).
pub fn bed_via_daemon(daemon: &Daemon, dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .env("GARDENBED_DAEMON", &daemon.socket)
        .env("XDG_CACHE_HOME", &daemon.cache_home)
        .output()
        .expect("bed binary spawns");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs the real `bed` binary in `dir` **through the daemon** and with
/// `GARDENBED_TOKEN` set: the token must survive bed → daemon → authed
/// server. Returns (exit code, stdout, stderr).
pub fn bed_via_daemon_with_token(
    daemon: &Daemon,
    dir: &Path,
    args: &[&str],
    token: &str,
) -> (i32, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .env("GARDENBED_DAEMON", &daemon.socket)
        .env("XDG_CACHE_HOME", &daemon.cache_home)
        .env("GARDENBED_TOKEN", token)
        .output()
        .expect("bed binary spawns");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Runs the real `bed` binary in `dir` with the daemon **disabled**
/// (GARDENBED_DAEMON=""), for direct-vs-daemon output comparisons.
pub fn bed_direct(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .env("GARDENBED_DAEMON", "")
        .output()
        .expect("bed binary spawns");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}
