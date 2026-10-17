//! .gitignore integration tests: untracked-file discovery in `bed status`
//! and `bed commit` honors .gitignore files read from the working copy
//! (root + nested), while tracked files keep reporting changes whatever
//! the patterns say (git semantics).
//!
//! Follows the write_path.rs pattern: a hand-built history served by a
//! real `gardenbed-server` process, driven through the real `bed` CLI.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;
use tempfile::TempDir;

use common::{Server, bed};

/// A one-changeset history: `src/lib.rs` and `docs/readme.md`, ref
/// `main` at head — the minimum a sparse add of `.` materializes.
struct World {
    _dir: TempDir,
    _root: PathBuf,
    garden: PathBuf,
    server: Server,
    _repo: Repo,
    _head: Id,
}

fn ident(secs: u64) -> Identity {
    Identity {
        name: "Seeder".into(),
        email: "seed@bed.example".into(),
        time_secs: secs,
        tz_min: 0,
    }
}

async fn setup() -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("server");
    let repo = Repo::open(&root).expect("repo opens");

    let mut inline = std::collections::BTreeMap::new();
    let mut changes = Vec::new();
    let seeded: &[(&str, &[u8])] = &[
        ("docs/readme.md", b"# hello\n"),
        ("src/lib.rs", b"pub fn f() {}\n"),
        // Tracked content inside what the working copy will later ignore:
        // only the server's history can put it there (git cannot add into
        // an ignored directory without -f; bed commit is no different).
        ("target/tracked.rs", b"tracked\n"),
    ];
    for (path, content) in seeded {
        changes.push(Change {
            path: (*path).to_owned(),
            op: Op::Add,
            blob: Some(blob_id(content)),
            src_path: None,
            mode: Mode::Regular,
        });
        inline.insert(blob_id(content).to_hex(), content.to_vec());
    }
    // Canonical order: strictly ascending by path bytes.
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    let cs = Changeset {
        parents: Vec::new(),
        changes,
        message: "seed the repository".into(),
        author: ident(1_700_000_000),
        committer: ident(1_700_000_000),
        signature: None,
    };
    let head = repo.create_changeset(&cs, &inline).expect("cs ingests");
    repo.cas_ref("main", None, head).expect("main created");

    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");
    let server = Server::spawn(&root).await;
    World {
        _dir: dir,
        _root: root,
        garden,
        server,
        _repo: repo,
        _head: head,
    }
}

/// Clones a working copy and sparses everything, via the real CLI.
fn plant(w: &World) {
    let (code, _, stderr) = bed(&w.garden, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "clone: {stderr}");
    let (code, out, stderr) = bed(&w.garden, &["sparse", "add", "."]);
    assert_eq!(code, 0, "sparse add: {stderr}\n{out}");
}

/// The working copy's current changeset, read from its config.
fn current_cs(garden: &Path) -> Id {
    let text = fs::read_to_string(garden.join(".bed/config.toml")).expect("config.toml");
    for line in text.lines() {
        if line.trim().starts_with("current_cs") {
            let (_, value) = line.split_once('=').expect("current_cs = <hex>");
            let hex = value.trim().trim_matches('"');
            return Id::from_str(hex).expect("current_cs is 64 hex chars");
        }
    }
    panic!("no current_cs in config.toml");
}

/// The paths of the changeset a commit created, in order.
fn committed_paths(repo: &Repo, cs: Id) -> Vec<String> {
    let changeset = repo.get_changeset(cs).expect("changeset readable");
    changeset.changes.iter().map(|c| c.path.clone()).collect()
}

/// The scenario the dogfood needs: cargo drops `target/` at the checkout
/// root, `.gitignore` says `/target`, status stays clean and commit does
/// not swallow the build junk — while a tracked file's edit still
/// reports and commits.
#[tokio::test]
async fn gitignore_hides_target_from_status_and_commit() {
    let mut w = setup().await;
    plant(&w);

    fs::write(w.garden.join(".gitignore"), "/target\n").unwrap();
    fs::create_dir_all(w.garden.join("target/debug")).unwrap();
    fs::write(w.garden.join("target/debug/x"), b"build junk\n").unwrap();
    // A tracked file's real edit: this must still surface.
    fs::write(w.garden.join("src/lib.rs"), b"pub fn g() {}\n").unwrap();

    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(
        !out.contains("target/debug/x"),
        "status must not show target/: {out}"
    );
    assert!(
        out.contains("modified") && out.contains("src/lib.rs"),
        "status must show the tracked edit: {out}"
    );
    assert!(
        out.contains(".gitignore"),
        "the untracked .gitignore itself still shows (git reports it too): {out}"
    );

    // Commit only the tracked edit: the changeset must carry src/lib.rs
    // (and the .gitignore, which is untracked and NOT ignored) and
    // nothing from target/.
    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "tend lib"]);
    assert_eq!(code, 0, "commit: {stderr}\n{out}");
    let cs = current_cs(&w.garden);
    let repo = Repo::open(&w._root).unwrap();
    let paths = committed_paths(&repo, cs);
    assert!(
        !paths.iter().any(|p| p.starts_with("target/")),
        "commit must not include target/: {paths:?}"
    );
    assert!(paths.contains(&"src/lib.rs".to_owned()), "{paths:?}");
    assert!(paths.contains(&".gitignore".to_owned()), "{paths:?}");

    // After the commit the working copy is clean — target/ stays
    // invisible, the .gitignore and the edit are in.
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(out.contains("working copy is clean"), "status: {out}");

    w.server.shutdown().await;
}

/// Ignore is untracked-discovery semantics: a tracked file keeps
/// reporting changes even when a pattern matches it, and tracked content
/// defeats directory pruning (git never un-tracks through ignore).
#[tokio::test]
async fn tracked_files_inside_ignored_directories_keep_reporting() {
    let mut w = setup().await;
    plant(&w);

    // target/tracked.rs is already tracked (seeded in the served
    // history); the working copy adds a .gitignore covering target/.
    // Only history can put it there: commit prunes an ignored directory
    // with no tracked content, exactly like git cannot add into an
    // ignored directory without -f.
    fs::write(w.garden.join(".gitignore"), "/target\n").unwrap();

    // The tracked file inside the ignored directory is edited, and an
    // untracked sibling appears. The edit reports and commits; the
    // sibling stays hidden.
    fs::write(w.garden.join("target/tracked.rs"), b"edited\n").unwrap();
    fs::write(w.garden.join("target/fresh.rs"), b"untracked\n").unwrap();
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(
        out.contains("modified") && out.contains("target/tracked.rs"),
        "tracked edit inside ignored dir must report: {out}"
    );
    assert!(!out.contains("target/fresh.rs"), "status: {out}");

    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "edit tracked"]);
    assert_eq!(code, 0, "commit: {stderr}\n{out}");
    let repo = Repo::open(&w._root).unwrap();
    let cs = current_cs(&w.garden);
    let paths = committed_paths(&repo, cs);
    // The tracked edit plus the new untracked .gitignore — nothing from
    // target/ beyond the tracked edit itself.
    assert_eq!(
        paths,
        vec![".gitignore".to_owned(), "target/tracked.rs".to_owned()],
        "{paths:?}"
    );

    w.server.shutdown().await;
}

/// Nested .gitignore files scope to their own subtree, and pattern
/// flavors (basename glob, dir-only, anchored) behave like git's.
#[tokio::test]
async fn nested_gitignore_and_pattern_flavors() {
    let mut w = setup().await;
    plant(&w);

    fs::write(w.garden.join(".gitignore"), "*.tmp\nbuild/\n").unwrap();
    fs::write(w.garden.join("scratch.tmp"), b"junk\n").unwrap();
    fs::write(w.garden.join("src/deep.tmp"), b"junk\n").unwrap();
    fs::create_dir_all(w.garden.join("build")).unwrap();
    fs::write(w.garden.join("build/artifact"), b"junk\n").unwrap();
    fs::create_dir_all(w.garden.join("src")).unwrap();
    fs::write(w.garden.join("src/.gitignore"), "/local.rs\n").unwrap();
    fs::write(w.garden.join("src/local.rs"), b"hidden\n").unwrap();
    fs::write(w.garden.join("local.rs"), b"visible\n").unwrap();

    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    for hidden in [
        "scratch.tmp",
        "src/deep.tmp",
        "build/artifact",
        "src/local.rs",
    ] {
        assert!(!out.contains(hidden), "status must hide {hidden}: {out}");
    }
    for shown in ["local.rs", "src/.gitignore", ".gitignore"] {
        assert!(out.contains(shown), "status must show {shown}: {out}");
    }

    // Nothing from target-style junk commits: only the visible untracked
    // files enter the changeset.
    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "sweep"]);
    assert_eq!(code, 0, "commit: {stderr}\n{out}");
    let repo = Repo::open(&w._root).unwrap();
    let mut paths = committed_paths(&repo, current_cs(&w.garden));
    paths.sort();
    assert_eq!(
        paths,
        vec![
            ".gitignore".to_owned(),
            "local.rs".to_owned(),
            "src/.gitignore".to_owned(),
        ],
        "{paths:?}"
    );

    w.server.shutdown().await;
}
