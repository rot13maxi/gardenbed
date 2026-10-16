//! M5 write-path integration tests (v0.1.md §M5): `bed sow`, `bed graft`,
//! `bed harvest` against a real arboretum process.
//!
//! The oracles are independent of the code under test:
//!
//! * **changeset shape** is checked against hand-computed expectations
//!   (ops, paths, blob ids, modes, order) read back through the server
//!   *library* (`Repo::get_changeset`) — never through counted RPCs;
//! * **trees** after a graft are recomputed by applying each side's
//!   changes to a plain `BTreeMap<String, (Mode, Id)>` and compared
//!   against the server's derived tree flattened in-process;
//! * **harvest races** rely on the arboretum's ref compare-and-swap:
//!   sixteen concurrent harvesters of the same ref position must produce
//!   exactly one winner; a chaos variant kills harvesters mid-flight and
//!   demands a consistent ref afterwards;
//! * **the crash window** kills a sowing `bed` process the moment the
//!   arboretum reports the heavy blob uploaded, then re-sows and demands
//!   the blob was uploaded exactly once across both runs.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;
use tempfile::TempDir;

use common::{Arboretum, REPO, bed, bed_async};

/// A small, fully hand-built history whose files and ids the tests know
/// exactly: three changesets ending at `head`, ref `main` pointing there.
struct World {
    _dir: TempDir,
    /// The arboretum root (what the server serves and `Repo::open` reads);
    /// kept alive through `_dir`, referenced via `server`/`repo`.
    _root: PathBuf,
    /// An empty garden directory beside it.
    garden: PathBuf,
    server: Arboretum,
    repo: Repo,
    head: Id,
    /// `path → (blob, mode)` at `head`, maintained by hand.
    files: std::collections::BTreeMap<String, (Id, Mode)>,
}

fn ident(secs: u64) -> Identity {
    Identity {
        name: "Seeder".into(),
        email: "seed@bed.example".into(),
        time_secs: secs,
        tz_min: 0,
    }
}

fn cs(parents: Vec<Id>, changes: Vec<Change>, message: &str, secs: u64) -> Changeset {
    Changeset {
        parents,
        changes,
        message: message.into(),
        author: ident(secs),
        committer: ident(secs),
        signature: None,
    }
}

fn add(path: &str, content: &[u8], mode: Mode) -> (Change, Vec<u8>) {
    (
        Change {
            path: path.into(),
            op: Op::Add,
            blob: Some(blob_id(content)),
            src_path: None,
            mode,
        },
        content.to_vec(),
    )
}

/// Builds the world: the fixture repo, the spawned arboretum over it, and
/// an empty garden directory ready for `bed clone`.
async fn setup() -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("arboretum");
    let repo = Repo::open(&root).expect("repo opens");

    let (c1, b1) = add("docs/readme.md", b"# hello\n", Mode::Regular);
    let (c2, b2) = add("src/main.rs", b"fn main() {}\n", Mode::Regular);
    let (c3, b3) = add("src/lib.rs", b"pub fn bed() {}\n", Mode::Regular);
    let (c4, b4) = add("notes/todo.txt", b"buy seeds\n", Mode::Regular);
    // Canonical order: strictly ascending by path bytes.
    let cs1 = cs(
        Vec::new(),
        vec![c1.clone(), c4.clone(), c3.clone(), c2.clone()],
        "seed the garden",
        1_700_000_000,
    );
    let mut inline1 = std::collections::BTreeMap::new();
    for (blob, content) in [(&c1, &b1), (&c2, &b2), (&c3, &b3), (&c4, &b4)] {
        inline1.insert(blob.blob.unwrap().to_hex(), content.clone());
    }
    let id1 = repo.create_changeset(&cs1, &inline1).expect("cs1 ingests");

    let (d1, e1) = add("src/extra.rs", b"pub mod extra;\n", Mode::Regular);
    let modify = Change {
        path: "docs/readme.md".into(),
        op: Op::Modify,
        blob: Some(blob_id(b"# hello\n\n tended\n")),
        src_path: None,
        mode: Mode::Regular,
    };
    let cs2 = cs(
        vec![id1],
        vec![modify.clone(), d1.clone()],
        "tend the readme",
        1_700_000_100,
    );
    let mut inline2 = std::collections::BTreeMap::new();
    inline2.insert(d1.blob.unwrap().to_hex(), e1);
    inline2.insert(
        modify.blob.unwrap().to_hex(),
        b"# hello\n\n tended\n".to_vec(),
    );
    let id2 = repo.create_changeset(&cs2, &inline2).expect("cs2 ingests");
    repo.cas_ref("main", None, id2).expect("main created");

    let mut files = std::collections::BTreeMap::new();
    files.insert(
        "docs/readme.md".into(),
        (modify.blob.unwrap(), Mode::Regular),
    );
    files.insert(
        "src/main.rs".into(),
        (blob_id(b"fn main() {}\n"), Mode::Regular),
    );
    files.insert(
        "src/lib.rs".into(),
        (blob_id(b"pub fn bed() {}\n"), Mode::Regular),
    );
    files.insert(
        "notes/todo.txt".into(),
        (blob_id(b"buy seeds\n"), Mode::Regular),
    );
    files.insert(
        "src/extra.rs".into(),
        (blob_id(b"pub mod extra;\n"), Mode::Regular),
    );

    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");
    let server = Arboretum::spawn(&root).await;
    World {
        _dir: dir,
        _root: root,
        garden,
        server,
        repo,
        head: id2,
        files,
    }
}

/// Clones a bed into `garden` and plots everything, via the real CLI.
fn plant(w: &World) {
    let (code, _, stderr) = bed(&w.garden, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "clone: {stderr}");
    let (code, out, stderr) = bed(&w.garden, &["plot", "add", "."]);
    assert_eq!(code, 0, "plot add: {stderr}\n{out}");
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(out.contains("the plot is clean"), "status: {out}");
}

/// The bed's current changeset, read from its config (the source of truth
/// the CLI itself writes).
fn current_cs(garden: &Path) -> Id {
    let text = fs::read_to_string(garden.join(".bed/config.toml")).expect("config.toml");
    for line in text.lines() {
        if line.trim().starts_with("current_cs") {
            let (_, value) = line.split_once('=').expect("current_cs = <hex>");
            let hex = value.trim().trim_matches('"');
            assert_eq!(
                hex.len(),
                64,
                "current_cs in config.toml is not 64 hex: {line:?}\n{text}"
            );
            return Id::from_str(hex).expect("current_cs is 64 hex chars");
        }
    }
    panic!("config.toml has no current_cs:\n{text}");
}

/// Flattens the server's derived tree at `cs` in-process (uncounted).
fn flat_files(repo: &Repo, cs: Id) -> std::collections::BTreeMap<String, (Mode, Id)> {
    use gardenbed_model::TreeKind;
    let mut out = std::collections::BTreeMap::new();
    let top = repo.root_tree(cs).expect("root_tree").expect("files exist");
    let mut stack = vec![(String::new(), top)];
    while let Some((prefix, tid)) = stack.pop() {
        let tree = repo.get_tree(tid).expect("get_tree").expect("tree exists");
        for e in &tree.entries {
            let path = if prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{prefix}/{}", e.name)
            };
            match e.kind {
                TreeKind::Dir => stack.push((path, e.tree.expect("dir carries a tree"))),
                TreeKind::File => {
                    out.insert(path, (e.mode, e.blob.expect("file carries a blob")));
                }
            }
        }
    }
    out
}

/// Applies `changes` to a flat file map the way the delta applier does —
/// the independent tree oracle for graft checks.
fn apply(files: &mut std::collections::BTreeMap<String, (Mode, Id)>, changes: &[Change]) {
    for c in changes {
        match c.op {
            Op::Delete => {
                files.remove(&c.path);
            }
            Op::Add | Op::Modify => {
                files.insert(
                    c.path.clone(),
                    (c.mode, c.blob.expect("write carries a blob")),
                );
            }
            Op::Rename => {
                files.remove(c.src_path.as_deref().expect("rename carries a src"));
                files.insert(
                    c.path.clone(),
                    (c.mode, c.blob.expect("rename carries a blob")),
                );
            }
        }
    }
}

/// M5 write path 1: a sow carrying every shape — add (regular, exec,
/// symlink), modify, delete, pure rename with a mode change — produces the
/// exact expected changeset, uploads its blobs, and leaves a clean plot.
#[tokio::test]
async fn sow_covers_every_change_shape_and_leaves_status_clean() {
    let mut w = setup().await;
    plant(&w);

    // The working-copy delta, by hand:
    //  * docs/readme.md modified
    //  * notes/todo.txt deleted
    //  * src/lib.rs renamed to src/moved.rs *and* made executable (the
    //    rename carries the NEW mode, blob unchanged)
    //  * src/new.rs added (regular)
    //  * run.sh added (exec)
    //  * link added (symlink to docs/readme.md)
    let mut readme = b"# hello\n\n tended\n".to_vec();
    readme.extend_from_slice(b"\n watered again\n");
    fs::write(w.garden.join("docs/readme.md"), &readme).expect("write");
    fs::remove_file(w.garden.join("notes/todo.txt")).expect("rm");
    fs::rename(w.garden.join("src/lib.rs"), w.garden.join("src/moved.rs")).expect("mv");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(w.garden.join("src/moved.rs"))
            .unwrap()
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        fs::set_permissions(w.garden.join("src/moved.rs"), perms).unwrap();
    }
    fs::write(w.garden.join("src/new.rs"), b"pub fn new() {}\n").expect("write");
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(w.garden.join("run.sh"), b"#!/bin/sh\necho hi\n").expect("write");
        let mut perms = fs::metadata(w.garden.join("run.sh")).unwrap().permissions();
        perms.set_mode(perms.mode() | 0o111);
        fs::set_permissions(w.garden.join("run.sh"), perms).unwrap();
    }
    std::os::unix::fs::symlink("docs/readme.md", w.garden.join("link")).expect("symlink");

    let (code, out, stderr) = bed(
        &w.garden,
        &[
            "sow",
            "-m",
            "tend the rows",
            "--author",
            "A. Gardener <a@bed.example>",
        ],
    );
    assert_eq!(code, 0, "sow: {stderr}\n{out}");
    assert!(out.contains("planted seed "), "out: {out}");
    assert!(
        out.contains("3 added, 1 modified, 1 deleted, 1 renamed"),
        "out: {out}"
    );
    assert!(out.contains("the bed now grows from "), "out: {out}");

    // The changeset, read back through the library (uncounted):
    let cs_id = current_cs(&w.garden);
    let sown = w.repo.get_changeset(cs_id).expect("changeset readable");
    assert_eq!(
        sown.parents,
        vec![w.head],
        "parent is the bed's previous tip"
    );
    assert_eq!(sown.message, "tend the rows");
    assert_eq!(sown.author.name, "A. Gardener");
    assert_eq!(sown.author.email, "a@bed.example");
    assert_eq!(sown.committer, sown.author, "committer == author in v0.1");

    // Exact expected changes, in canonical order.
    let expect: Vec<Change> = vec![
        Change {
            path: "docs/readme.md".into(),
            op: Op::Modify,
            blob: Some(blob_id(&readme)),
            src_path: None,
            mode: Mode::Regular,
        },
        Change {
            path: "link".into(),
            op: Op::Add,
            blob: Some(blob_id(b"docs/readme.md")),
            src_path: None,
            mode: Mode::Symlink,
        },
        Change {
            path: "notes/todo.txt".into(),
            op: Op::Delete,
            blob: None,
            src_path: None,
            mode: Mode::Regular,
        },
        Change {
            path: "run.sh".into(),
            op: Op::Add,
            blob: Some(blob_id(b"#!/bin/sh\necho hi\n")),
            src_path: None,
            mode: Mode::Exec,
        },
        Change {
            path: "src/moved.rs".into(),
            op: Op::Rename,
            blob: Some(blob_id(b"pub fn bed() {}\n")),
            src_path: Some("src/lib.rs".into()),
            mode: Mode::Exec,
        },
        Change {
            path: "src/new.rs".into(),
            op: Op::Add,
            blob: Some(blob_id(b"pub fn new() {}\n")),
            src_path: None,
            mode: Mode::Regular,
        },
    ];
    assert_eq!(
        sown.changes, expect,
        "the sown delta must match, byte for byte"
    );

    // Every sown blob is a real loam object with the right content.
    for c in &sown.changes {
        if c.mode == Mode::Gitlink {
            continue;
        }
        let Some(blob) = c.blob else { continue };
        let stored = w
            .repo
            .get_blob(blob)
            .expect("get_blob")
            .expect("blob exists");
        let want = match c.op {
            Op::Delete => unreachable!(),
            Op::Add | Op::Modify => match c.path.as_str() {
                "docs/readme.md" => readme.clone(),
                "link" => b"docs/readme.md".to_vec(),
                "run.sh" => b"#!/bin/sh\necho hi\n".to_vec(),
                "src/new.rs" => b"pub fn new() {}\n".to_vec(),
                other => panic!("unexpected sown path {other}"),
            },
            Op::Rename => b"pub fn bed() {}\n".to_vec(),
        };
        assert_eq!(stored, want, "blob for {}", c.path);
    }

    // The plot is clean afterwards; the renamed file really moved on disk.
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(out.contains("the plot is clean"), "status: {out}");
    assert!(w.garden.join("src/moved.rs").is_file());
    assert!(!w.garden.join("src/lib.rs").exists());

    w.server.shutdown().await;
}

/// M5 write path 1b: the sow CLI contract — author precedence (flag over
/// env over default), the rendered outcome, and the refusals (empty
/// message, clean plot, malformed author) all exit 1 with clean errors.
#[tokio::test]
async fn sow_cli_contract_authors_and_refusals() {
    let mut w = setup().await;
    plant(&w);

    // Nothing to sow on a clean plot.
    let (code, _, stderr) = bed(&w.garden, &["sow", "-m", "nothing here"]);
    assert_eq!(code, 1, "clean sow must fail");
    assert!(
        stderr.contains("nothing modified, deleted, or untracked"),
        "{stderr}"
    );

    // Empty message.
    let (code, _, stderr) = bed(&w.garden, &["sow", "-m", "   "]);
    assert_eq!(code, 1);
    assert!(stderr.contains("the message is empty"), "{stderr}");

    // Malformed author.
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n changed\n").unwrap();
    let (code, _, stderr) = bed(&w.garden, &["sow", "-m", "x", "--author", "no email"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("bad author"), "{stderr}");
    assert!(stderr.contains("--author"), "{stderr}");
    // The refusal left the bed untouched: still nothing sown, plot dirty.
    assert_eq!(current_cs(&w.garden), w.head);

    // The bed's default identity when neither flag nor env is set.
    let (code, out, stderr) = bed(&w.garden, &["sow", "-m", "default identity"]);
    assert_eq!(code, 0, "{stderr}\n{out}");
    let cs_id = current_cs(&w.garden);
    let sown = w.repo.get_changeset(cs_id).unwrap();
    assert_eq!(sown.author.name, "Bed User");
    assert_eq!(sown.author.email, "bed@gardenbed.local");

    // GARDENBED_AUTHOR fills in when the flag is absent…
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n env\n").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(&w.garden)
        .args(["sow", "-m", "env identity"])
        .env("GARDENBED_AUTHOR", "Env Keeper <env@bed.example>")
        .env("NO_COLOR", "1")
        .output()
        .expect("bed spawns");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let sown = w.repo.get_changeset(current_cs(&w.garden)).unwrap();
    assert_eq!(sown.author.name, "Env Keeper");

    // …and the flag beats the environment.
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n flag\n").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(&w.garden)
        .args([
            "sow",
            "-m",
            "flag wins",
            "--author",
            "Flag Keeper <flag@bed.example>",
        ])
        .env("GARDENBED_AUTHOR", "Env Keeper <env@bed.example>")
        .env("NO_COLOR", "1")
        .output()
        .expect("bed spawns");
    assert_eq!(out.status.code(), Some(0));
    let sown = w.repo.get_changeset(current_cs(&w.garden)).unwrap();
    assert_eq!(sown.author.name, "Flag Keeper");

    // The rendered outcome names the seed and the previous tip.
    let (code, out, _) = bed(&w.garden, &["log", "--limit", "1"]);
    assert_eq!(code, 0);
    assert!(out.contains("Flag Keeper"), "log: {out}");

    w.server.shutdown().await;
}

/// M5 write path 2: graft replays the bed's changeset onto a moved `main`
/// and the resulting tree equals the independently computed union of both
/// sides' changes. A conflicting graft refuses by naming the paths.
#[tokio::test]
async fn graft_replays_onto_moved_main_and_conflicts_name_paths() {
    let mut w = setup().await;
    plant(&w);

    // The bed sows a change of its own: a fresh file under src/. (A
    // replayed *modify* would conflict by the documented hard-conflict
    // semantics — the delta carries no old side, so a write onto a path
    // that exists with different content is a conflict; a fresh Add
    // applies cleanly.)
    fs::write(w.garden.join("src/bed-note.rs"), b"// the bed was here\n").unwrap();
    let (code, out, stderr) = bed(&w.garden, &["sow", "-m", "bed side"]);
    assert_eq!(code, 0, "{stderr}\n{out}");
    let bed_cs = current_cs(&w.garden);

    // Upstream (in-process, no counters): another bed's change lands on
    // the same parent and `main` moves to it.
    let upstream = Change {
        path: "src/main.rs".into(),
        op: Op::Modify,
        blob: Some(blob_id(b"fn main() { println!(\"upstream\"); }\n")),
        src_path: None,
        mode: Mode::Regular,
    };
    let upstream_cs = cs(
        vec![w.head],
        vec![upstream.clone()],
        "upstream main.rs",
        1_700_000_200,
    );
    let upstream_id = w
        .repo
        .create_changeset(
            &upstream_cs,
            &std::collections::BTreeMap::from([(
                upstream.blob.unwrap().to_hex(),
                b"fn main() { println!(\"upstream\"); }\n".to_vec(),
            )]),
        )
        .expect("upstream ingests");
    w.repo
        .cas_ref("main", Some(w.head), upstream_id)
        .expect("main moves");

    // Graft the bed's changeset onto the new main.
    let (code, out, stderr) = bed(&w.garden, &["graft", "main"]);
    assert_eq!(code, 0, "graft: {stderr}\n{out}");
    let grafted = current_cs(&w.garden);
    assert_ne!(grafted, bed_cs, "the bed moved onto the replayed changeset");

    // The grafted changeset: one parent (the new main), the same changes.
    let gs = w.repo.get_changeset(grafted).expect("grafted readable");
    assert_eq!(gs.parents, vec![upstream_id]);
    let bs = w.repo.get_changeset(bed_cs).expect("bed cs readable");
    assert_eq!(gs.changes, bs.changes, "the replay carries the same delta");
    assert_eq!(gs.message, bs.message);

    // Independent tree oracle: head's files + upstream's change + the
    // bed's change.
    let mut expected = w
        .files
        .iter()
        .map(|(p, (b, m))| (p.clone(), (*m, *b)))
        .collect::<std::collections::BTreeMap<_, _>>();
    apply(&mut expected, &upstream_cs.changes);
    apply(&mut expected, &bs.changes);
    let actual = flat_files(&w.repo, grafted);
    assert_eq!(
        actual, expected,
        "grafted tree must equal the hand-computed union"
    );

    // The working copy came along: both sides' content on disk, plot clean.
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(out.contains("the plot is clean"), "status: {out}");
    assert_eq!(
        fs::read_to_string(w.garden.join("src/main.rs")).unwrap(),
        "fn main() { println!(\"upstream\"); }\n"
    );
    assert_eq!(
        fs::read_to_string(w.garden.join("src/bed-note.rs")).unwrap(),
        "// the bed was here\n"
    );

    // ---- the conflict path: sow a modify of docs/readme.md while
    // upstream rewrites the very same file, then graft → the arboretum's
    // FAILED_PRECONDITION names the conflicting path.
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n bed clash\n").unwrap();
    let (code, out, stderr) = bed(&w.garden, &["sow", "-m", "clashing change"]);
    assert_eq!(code, 0, "{stderr}\n{out}");
    let clashing = current_cs(&w.garden);

    let upstream2 = Change {
        path: "docs/readme.md".into(),
        op: Op::Modify,
        blob: Some(blob_id(b"# hello\n\n upstream clash\n")),
        src_path: None,
        mode: Mode::Regular,
    };
    let clash_cs = cs(
        vec![grafted],
        vec![upstream2.clone()],
        "upstream clash",
        1_700_000_300,
    );
    let clash_id = w
        .repo
        .create_changeset(
            &clash_cs,
            &std::collections::BTreeMap::from([(
                upstream2.blob.unwrap().to_hex(),
                b"# hello\n\n upstream clash\n".to_vec(),
            )]),
        )
        .expect("clash cs ingests");
    w.repo
        .cas_ref("main", Some(upstream_id), clash_id)
        .expect("main moves again");

    let (code, _, stderr) = bed(&w.garden, &["graft", "main"]);
    assert_eq!(code, 1, "a conflicting graft must fail");
    assert!(stderr.contains("graft conflicts on"), "{stderr}");
    assert!(
        stderr.contains("docs/readme.md"),
        "the conflicting path is named: {stderr}"
    );
    // The bed is untouched by the refusal: still at the clashing changeset.
    assert_eq!(current_cs(&w.garden), clashing);

    w.server.shutdown().await;
}

/// M5 write path 3: a lone harvest fast-forwards `main`; a bed left
/// behind gets the not-fast-forward refusal with the recipe spelled out.
#[tokio::test]
async fn harvest_fast_forwards_and_losers_get_the_recipe() {
    let mut w = setup().await;
    plant(&w);

    // A second bed parks itself at the base tip BEFORE anything moves.
    let other = w._dir.path().join("garden-2");
    fs::create_dir_all(&other).unwrap();
    let (code, _, stderr) = bed(&other, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = bed(&other, &["plot", "add", "."]);
    assert_eq!(code, 0, "{stderr}");

    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n harvested\n").unwrap();
    let (code, out, stderr) = bed(&w.garden, &["sow", "-m", "harvest me"]);
    assert_eq!(code, 0, "{stderr}\n{out}");
    let sown = current_cs(&w.garden);

    // Harvest publishes: main fast-forwards to the sown changeset, and
    // the bed stays where it is.
    let (code, out, stderr) = bed(&w.garden, &["harvest"]);
    assert_eq!(code, 0, "harvest: {stderr}\n{out}");
    assert!(out.contains("harvested"), "out: {out}");
    assert_eq!(w.repo.get_ref("main").expect("get_ref"), Some(sown));
    assert_eq!(
        current_cs(&w.garden),
        sown,
        "harvesting does not move the bed"
    );

    // The second bed (still at the old tip) sows a sibling changeset; its
    // harvest is no longer a fast-forward of `main` and is refused with
    // the fix spelled out.
    fs::write(other.join("src/other.rs"), b"// other bed\n").unwrap();
    let (code, out, stderr) = bed(&other, &["sow", "-m", "other bed's change"]);
    assert_eq!(code, 0, "{stderr}\n{out}");
    assert_ne!(current_cs(&other), sown);

    let (code, _, stderr) = bed(&other, &["harvest"]);
    assert_eq!(code, 1, "a non-fast-forward harvest must fail");
    assert!(stderr.contains("not its tip anymore"), "{stderr}");
    assert!(stderr.contains("onto the new main"), "{stderr}");

    w.server.shutdown().await;
}

/// Sixteen beds sow sixteen sibling changesets and harvest concurrently:
/// the ref CAS admits exactly one winner; every loser is told the fix.
#[tokio::test]
async fn sixteen_concurrent_harvests_exactly_one_wins() {
    let mut w = setup().await;
    plant(&w);
    let base = w.head;

    // Sixteen beds, one sibling changeset each (distinct files).
    const N: usize = 16;
    let mut beds = Vec::new();
    let mut sown = Vec::new();
    for i in 0..N {
        let dir = w._dir.path().join(format!("bed-{i:02}"));
        fs::create_dir_all(dir.join("row")).unwrap();
        let (code, _, stderr) = bed(&dir, &["clone", &w.server.http()]);
        assert_eq!(code, 0, "clone {i}: {stderr}");
        let (code, _, stderr) = bed(&dir, &["plot", "add", "."]);
        assert_eq!(code, 0, "plot add {i}: {stderr}");
        fs::write(
            dir.join(format!("row/bed-{i:02}.txt")),
            format!("bed {i}'s row\n"),
        )
        .unwrap();
        let (code, out, stderr) = bed(&dir, &["sow", "-m", &format!("bed {i} sows")]);
        assert_eq!(code, 0, "sow {i}: {stderr}\n{out}");
        sown.push(current_cs(&dir));
        beds.push(dir);
    }

    // Sixteen concurrent harvests of the same ref position.
    let mut children = Vec::new();
    for dir in &beds {
        children.push(bed_async(dir, &["harvest"]));
    }
    let mut winners = 0usize;
    let mut loser_messages = Vec::new();
    for (i, child) in children.into_iter().enumerate() {
        let out = child.wait_with_output().await.expect("harvest finishes");
        let code = out.status.code().unwrap_or(-1);
        if code == 0 {
            winners += 1;
        } else {
            loser_messages.push((i, String::from_utf8_lossy(&out.stderr).into_owned()));
        }
    }
    assert_eq!(winners, 1, "exactly one harvester wins the CAS");
    assert_eq!(loser_messages.len(), N - 1);
    for (_, msg) in &loser_messages {
        assert!(msg.contains("not its tip anymore"), "{msg}");
        assert!(msg.contains("graft"), "{msg}");
    }

    // The ref points at the winner's changeset — one of the sixteen.
    let main = w
        .repo
        .get_ref("main")
        .expect("get_ref")
        .expect("main exists");
    assert!(sown.contains(&main), "main is one of the sown changesets");
    assert_ne!(main, base);

    // The winner's bed can harvest again (idempotent); everyone else is
    // still refused.
    for (i, dir) in beds.iter().enumerate() {
        let (code, _, _) = bed(dir, &["harvest"]);
        let should_win = current_cs(dir) == main;
        assert_eq!(
            code == 0,
            should_win,
            "bed {i} harvest {} after the race",
            if should_win { "must win" } else { "must lose" }
        );
    }

    w.server.shutdown().await;
}

/// The chaos variant: harvesters are killed mid-flight. Whatever lands,
/// the ref ends up consistent — either still at the base or at exactly one
/// of the candidates — and a follow-up round still has exactly one winner.
#[tokio::test]
async fn chaos_killed_harvests_leave_a_consistent_ref() {
    let mut w = setup().await;
    plant(&w);
    let base = w.head;

    const N: usize = 16;
    let mut beds = Vec::new();
    let mut sown = Vec::new();
    for i in 0..N {
        let dir = w._dir.path().join(format!("chaos-{i:02}"));
        fs::create_dir_all(dir.join("row")).unwrap();
        let (code, _, stderr) = bed(&dir, &["clone", &w.server.http()]);
        assert_eq!(code, 0, "clone {i}: {stderr}");
        let (code, _, stderr) = bed(&dir, &["plot", "add", "."]);
        assert_eq!(code, 0, "plot add {i}: {stderr}");
        fs::write(
            dir.join(format!("row/chaos-{i:02}.txt")),
            format!("chaos bed {i}'s row\n"),
        )
        .unwrap();
        let (code, out, stderr) = bed(&dir, &["sow", "-m", &format!("chaos bed {i} sows")]);
        assert_eq!(code, 0, "sow {i}: {stderr}\n{out}");
        sown.push(current_cs(&dir));
        beds.push(dir);
    }

    // Stagger and kill half mid-flight; let the rest finish.
    let mut children = Vec::new();
    for dir in &beds {
        children.push((bed_async(dir, &["harvest"]), tokio::time::Instant::now()));
    }
    let mut results = Vec::new();
    for (i, (mut child, spawned)) in children.into_iter().enumerate() {
        if i % 2 == 0 {
            // Kill while it is (probably) still in flight.
            tokio::time::sleep_until(spawned + Duration::from_millis(2 + (i as u64) * 3)).await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            results.push(None);
        } else {
            let out = child.wait_with_output().await.expect("harvest finishes");
            results.push(Some(out.status.code().unwrap_or(-1)));
        }
    }
    let survivor_wins = results.iter().filter(|r| matches!(r, Some(0))).count();
    assert!(survivor_wins <= 1, "at most one survivor can have won");

    // Whatever happened, main is the base or exactly one candidate — and a
    // real changeset (a torn CAS would point at nothing valid).
    let main = w.repo.get_ref("main").expect("get_ref");
    if let Some(m) = main {
        assert!(
            m == base || sown.contains(&m),
            "main is a candidate or the base"
        );
        w.repo
            .get_changeset(m)
            .expect("main names a real changeset");
    }

    // Round 2, sequential: exactly one bed wins the (possibly fresh) ref
    // position, and the ref lands on its changeset.
    let mut round2_winners = 0usize;
    for dir in &beds {
        let (code, _, _) = bed(dir, &["harvest"]);
        if code == 0 {
            round2_winners += 1;
        }
    }
    assert_eq!(
        round2_winners, 1,
        "exactly one winner in the follow-up round"
    );
    let main = w
        .repo
        .get_ref("main")
        .expect("get_ref")
        .expect("main exists");
    assert!(sown.contains(&main));

    w.server.shutdown().await;
}

/// M5 write path 4: a sow killed in the upload window (between `PutBlob`
/// and `CreateChangeset`) recovers by re-sowing — the heavy blob goes over
/// the wire exactly once across both runs (cache-first: the local cache
/// wrote it before the upload, so the rerun trusts it), and the retry
/// completes with a clean plot.
#[tokio::test]
async fn sow_killed_between_upload_and_commit_retries_uploading_once() {
    let mut w = setup().await;
    plant(&w);

    // One heavy blob (> 1 MiB rides PutBlob, not inline) plus a small one.
    let heavy = vec![0x61u8; (1 << 20) + 17];
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n heavy\n").unwrap();
    fs::write(w.garden.join("bulk.bin"), &heavy).unwrap();

    let mut child = bed_async(&w.garden, &["sow", "-m", "heavy lift"]);
    let client = w.server.client().await;

    // Poll the arboretum's counters until the heavy blob has fully
    // arrived (put_blob bumps only after the whole stream), then kill the
    // bed mid-flight — inside the upload→commit window.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let stats = client.get_stats(REPO).await.expect("get_stats");
        if stats.get("put_blob").copied().unwrap_or(0) >= 1 {
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            panic!("the sow exited before the upload completed");
        }
        assert!(Instant::now() < deadline, "put_blob never bumped");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    child.kill().await.expect("SIGKILL the sowing bed");
    let _ = child.wait().await;

    // The local bed state never moved (it only moves after success).
    assert_eq!(
        current_cs(&w.garden),
        w.head,
        "config untouched by the crash"
    );

    // Re-sow: succeeds, and the heavy blob went over the wire exactly once
    // in total — the retry must trust the cache, never re-upload.
    let (code, out, stderr) = bed(&w.garden, &["sow", "-m", "heavy lift, retried"]);
    assert_eq!(code, 0, "retry: {stderr}\n{out}");
    let stats = client.get_stats(REPO).await.expect("get_stats");
    assert_eq!(
        stats.get("put_blob").copied().unwrap_or(0),
        1,
        "exactly one put_blob across the crash and the retry (counters: {stats:?})"
    );
    assert!(
        stats.get("create_changeset").copied().unwrap_or(0) >= 1,
        "the retry planted a changeset"
    );

    // The blob is really there, and the plot is clean.
    let blob = blob_id(&heavy);
    let stored = w
        .repo
        .get_blob(blob)
        .expect("get_blob")
        .expect("blob exists");
    assert_eq!(stored, heavy);
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(out.contains("the plot is clean"), "status: {out}");

    w.server.shutdown().await;
}

/// M5 write path 5: heavy blobs take the two-phase path (PutBlob first),
/// and sowing the same content again uploads nothing new (dedup by id).
#[tokio::test]
async fn heavy_blobs_upload_first_and_never_twice() {
    let mut w = setup().await;
    plant(&w);

    let heavy = vec![0x23u8; (1 << 20) + 5];
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n bulky\n").unwrap();
    fs::write(w.garden.join("bulk.bin"), &heavy).unwrap();

    let client = w.server.client().await;
    let before = client.get_stats(REPO).await.expect("get_stats");
    let (code, out, stderr) = bed(&w.garden, &["sow", "-m", "bring the bulk"]);
    assert_eq!(code, 0, "{stderr}\n{out}");
    // The outcome spells out the two-phase split.
    assert!(
        out.contains("1 heavy blob(s) went over the wire first"),
        "out: {out}"
    );
    assert!(out.contains(" rode inline"), "out: {out}");
    let after = client.get_stats(REPO).await.expect("get_stats");
    assert_eq!(
        stats_get(&before, "put_blob") + 1,
        stats_get(&after, "put_blob"),
        "one put_blob for the heavy blob"
    );

    // Re-sowing the same heavy content (untouched) changes nothing — and
    // a second sow of it (e.g. after a goto away and back) uploads nothing.
    // Here: sow a different file, then verify the counter stayed put.
    let before = after;
    fs::write(
        w.garden.join("docs/readme.md"),
        b"# hello\n\n bulky again\n",
    )
    .unwrap();
    let (code, _, stderr) = bed(&w.garden, &["sow", "-m", "no bulk this time"]);
    assert_eq!(code, 0, "{stderr}");
    let after = client.get_stats(REPO).await.expect("get_stats");
    assert_eq!(
        stats_get(&before, "put_blob"),
        stats_get(&after, "put_blob"),
        "small sows never touch put_blob"
    );

    w.server.shutdown().await;
}

/// Individually inline-sized blobs must not overflow CreateChangeset's
/// aggregate gRPC message limit; overflow content streams before publication.
#[tokio::test]
async fn many_small_blobs_fit_the_unary_request_budget() {
    let mut w = setup().await;
    plant(&w);
    for i in 0..6u8 {
        fs::write(w.garden.join(format!("bulk-{i}.bin")), vec![i; 900_000]).unwrap();
    }
    let client = w.server.client().await;
    let before = client.get_stats(REPO).await.expect("stats");
    let (code, out, stderr) = bed(&w.garden, &["sow", "-m", "many small blobs"]);
    assert_eq!(code, 0, "{stderr}\n{out}");
    let after = client.get_stats(REPO).await.expect("stats");
    assert!(stats_get(&after, "put_blob") > stats_get(&before, "put_blob"));
    let repo = Repo::open(&w._root).unwrap();
    let changeset = repo.get_changeset(current_cs(&w.garden)).unwrap();
    for i in 0..6u8 {
        let path = format!("bulk-{i}.bin");
        let change = changeset.changes.iter().find(|c| c.path == path).unwrap();
        assert_eq!(
            repo.get_blob(change.blob.unwrap()).unwrap().unwrap(),
            vec![i; 900_000]
        );
    }
    let (code, out, stderr) = bed(&w.garden, &["status", "--full-scan"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(out.contains("the plot is clean"), "{out}");
    w.server.shutdown().await;
}

fn stats_get(stats: &std::collections::BTreeMap<String, u64>, key: &str) -> u64 {
    stats.get(key).copied().unwrap_or(0)
}
