//! M5 write-path integration tests (v0.1.md §M5): `bed commit`, `bed rebase`,
//! `bed publish` against a real server process.
//!
//! The oracles are independent of the code under test:
//!
//! * **changeset shape** is checked against hand-computed expectations
//!   (ops, paths, blob ids, modes, order) read back through the server
//!   *library* (`Repo::get_changeset`) — never through counted RPCs;
//! * **trees** after a rebase are recomputed by applying each side's
//!   changes to a plain `BTreeMap<String, (Mode, Id)>` and compared
//!   against the server's derived tree flattened in-process;
//! * **publish races** rely on the server's ref compare-and-swap:
//!   sixteen concurrent publishers of the same ref position must produce
//!   exactly one winner; a chaos variant kills publishers mid-flight and
//!   demands a consistent ref afterwards;
//! * **the crash window** kills a committing `bed` process the moment the
//!   server reports the heavy blob uploaded, then re-commits and demands
//!   the blob was uploaded exactly once across both runs.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;
use tempfile::TempDir;

use common::{REPO, Server, bed, bed_async};

/// A small, fully hand-built history whose files and ids the tests know
/// exactly: three changesets ending at `head`, ref `main` pointing there.
struct World {
    _dir: TempDir,
    /// The server root (what the server serves and `Repo::open` reads);
    /// kept alive through `_dir`, referenced via `server`/`repo`.
    _root: PathBuf,
    /// An empty garden directory beside it.
    garden: PathBuf,
    server: Server,
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

/// Builds the world: the fixture repo, the spawned server over it, and
/// an empty garden directory ready for `bed clone`.
async fn setup() -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("server");
    let repo = Repo::open(&root).expect("repo opens");

    let (c1, b1) = add("docs/readme.md", b"# hello\n", Mode::Regular);
    let (c2, b2) = add("src/main.rs", b"fn main() {}\n", Mode::Regular);
    let (c3, b3) = add("src/lib.rs", b"pub fn bed() {}\n", Mode::Regular);
    let (c4, b4) = add("notes/todo.txt", b"buy seeds\n", Mode::Regular);
    // Canonical order: strictly ascending by path bytes.
    let cs1 = cs(
        Vec::new(),
        vec![c1.clone(), c4.clone(), c3.clone(), c2.clone()],
        "seed the repository",
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
    let server = Server::spawn(&root).await;
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

/// Clones a working copy into `garden` and sparses everything, via the real CLI.
fn plant(w: &World) {
    let (code, _, stderr) = bed(&w.garden, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "clone: {stderr}");
    let (code, out, stderr) = bed(&w.garden, &["sparse", "add", "."]);
    assert_eq!(code, 0, "sparse add: {stderr}\n{out}");
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(out.contains("working copy is clean"), "status: {out}");
}

/// The working copy's current changeset, read from its config (the source of truth
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
/// the independent tree oracle for rebase checks.
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

/// M5 write path 1: a commit carrying every shape — add (regular, exec,
/// symlink), modify, delete, pure rename with a mode change — produces the
/// exact expected changeset, uploads its blobs, and leaves a clean sparse.
#[tokio::test]
async fn commit_covers_every_change_shape_and_leaves_status_clean() {
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
            "commit",
            "-m",
            "tend the rows",
            "--author",
            "A. Gardener <a@bed.example>",
        ],
    );
    assert_eq!(code, 0, "commit: {stderr}\n{out}");
    assert!(out.contains("created changeset "), "out: {out}");
    assert!(
        out.contains("3 added, 1 modified, 1 deleted, 1 renamed"),
        "out: {out}"
    );
    assert!(out.contains("working copy at "), "out: {out}");

    // The changeset, read back through the library (uncounted):
    let cs_id = current_cs(&w.garden);
    let created = w.repo.get_changeset(cs_id).expect("changeset readable");
    assert_eq!(
        created.parents,
        vec![w.head],
        "parent is the working copy's previous tip"
    );
    assert_eq!(created.message, "tend the rows");
    assert_eq!(created.author.name, "A. Gardener");
    assert_eq!(created.author.email, "a@bed.example");
    assert_eq!(
        created.committer, created.author,
        "committer == author in v0.1"
    );

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
        created.changes, expect,
        "the created delta must match, byte for byte"
    );

    // Every created blob is a real objects object with the right content.
    for c in &created.changes {
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
                other => panic!("unexpected created path {other}"),
            },
            Op::Rename => b"pub fn bed() {}\n".to_vec(),
        };
        assert_eq!(stored, want, "blob for {}", c.path);
    }

    // The sparse is clean afterwards; the renamed file really moved on disk.
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(out.contains("working copy is clean"), "status: {out}");
    assert!(w.garden.join("src/moved.rs").is_file());
    assert!(!w.garden.join("src/lib.rs").exists());

    w.server.shutdown().await;
}

/// M5 write path 1b: the commit CLI contract — author precedence (flag over
/// env over default), the rendered outcome, and the refusals (empty
/// message, clean sparse, malformed author) all exit 1 with clean errors.
#[tokio::test]
async fn commit_cli_contract_authors_and_refusals() {
    let mut w = setup().await;
    plant(&w);

    // Nothing to commit on a clean sparse.
    let (code, _, stderr) = bed(&w.garden, &["commit", "-m", "nothing here"]);
    assert_eq!(code, 1, "clean commit must fail");
    assert!(
        stderr.contains("nothing modified, deleted, or untracked"),
        "{stderr}"
    );

    // Empty message.
    let (code, _, stderr) = bed(&w.garden, &["commit", "-m", "   "]);
    assert_eq!(code, 1);
    assert!(stderr.contains("the message is empty"), "{stderr}");

    // Malformed author.
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n changed\n").unwrap();
    let (code, _, stderr) = bed(&w.garden, &["commit", "-m", "x", "--author", "no email"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("bad author"), "{stderr}");
    assert!(stderr.contains("--author"), "{stderr}");
    // The refusal left the working copy untouched: still nothing created, sparse dirty.
    assert_eq!(current_cs(&w.garden), w.head);

    // The working copy's default identity when neither flag nor env is set.
    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "default identity"]);
    assert_eq!(code, 0, "{stderr}\n{out}");
    let cs_id = current_cs(&w.garden);
    let created = w.repo.get_changeset(cs_id).unwrap();
    assert_eq!(created.author.name, "Gardenbed User");
    assert_eq!(created.author.email, "bed@gardenbed.local");

    // GARDENBED_AUTHOR fills in when the flag is absent…
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n env\n").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(&w.garden)
        .args(["commit", "-m", "env identity"])
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
    let created = w.repo.get_changeset(current_cs(&w.garden)).unwrap();
    assert_eq!(created.author.name, "Env Keeper");

    // …and the flag beats the environment.
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n flag\n").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(&w.garden)
        .args([
            "commit",
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
    let created = w.repo.get_changeset(current_cs(&w.garden)).unwrap();
    assert_eq!(created.author.name, "Flag Keeper");

    // The rendered outcome names the seed and the previous tip.
    let (code, out, _) = bed(&w.garden, &["log", "--limit", "1"]);
    assert_eq!(code, 0);
    assert!(out.contains("Flag Keeper"), "log: {out}");

    w.server.shutdown().await;
}

/// M5 write path 2: rebase replays the working copy's changeset onto a moved `main`
/// and the resulting tree equals the independently computed union of both
/// sides' changes. A conflicting rebase refuses by naming the paths.
#[tokio::test]
async fn rebase_replays_onto_moved_main_and_conflicts_name_paths() {
    let mut w = setup().await;
    plant(&w);

    // The bed commits a change of its own: a fresh file under src/. (A
    // replayed *modify* would conflict by the documented hard-conflict
    // semantics — the delta carries no old side, so a write onto a path
    // that exists with different content is a conflict; a fresh Add
    // applies cleanly.)
    fs::write(
        w.garden.join("src/bed-note.rs"),
        b"// the working copy was here\n",
    )
    .unwrap();
    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "bed side"]);
    assert_eq!(code, 0, "{stderr}\n{out}");
    let bed_cs = current_cs(&w.garden);

    // Upstream (in-process, no counters): another working copy's change lands on
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

    // Rebase the working copy's changeset onto the new main.
    let (code, out, stderr) = bed(&w.garden, &["rebase", "main"]);
    assert_eq!(code, 0, "rebase: {stderr}\n{out}");
    let rebased = current_cs(&w.garden);
    assert_ne!(
        rebased, bed_cs,
        "the working copy moved onto the replayed changeset"
    );

    // The rebased changeset: one parent (the new main), the same changes.
    let gs = w.repo.get_changeset(rebased).expect("rebased readable");
    assert_eq!(gs.parents, vec![upstream_id]);
    let bs = w.repo.get_changeset(bed_cs).expect("bed cs readable");
    assert_eq!(gs.changes, bs.changes, "the replay carries the same delta");
    assert_eq!(gs.message, bs.message);

    // Independent tree oracle: head's files + upstream's change + the
    // working copy's change.
    let mut expected = w
        .files
        .iter()
        .map(|(p, (b, m))| (p.clone(), (*m, *b)))
        .collect::<std::collections::BTreeMap<_, _>>();
    apply(&mut expected, &upstream_cs.changes);
    apply(&mut expected, &bs.changes);
    let actual = flat_files(&w.repo, rebased);
    assert_eq!(
        actual, expected,
        "rebased tree must equal the hand-computed union"
    );

    // The working copy came along: both sides' content on disk, sparse clean.
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(out.contains("working copy is clean"), "status: {out}");
    assert_eq!(
        fs::read_to_string(w.garden.join("src/main.rs")).unwrap(),
        "fn main() { println!(\"upstream\"); }\n"
    );
    assert_eq!(
        fs::read_to_string(w.garden.join("src/bed-note.rs")).unwrap(),
        "// the working copy was here\n"
    );

    // ---- the conflict path: commit a modify of docs/readme.md while
    // upstream rewrites the very same file, then rebase → the server's
    // FAILED_PRECONDITION names the conflicting path.
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n bed clash\n").unwrap();
    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "clashing change"]);
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
        vec![rebased],
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

    let (code, _, stderr) = bed(&w.garden, &["rebase", "main"]);
    assert_eq!(code, 1, "a conflicting rebase must fail");
    assert!(stderr.contains("rebase conflicts on"), "{stderr}");
    assert!(
        stderr.contains("docs/readme.md"),
        "the conflicting path is named: {stderr}"
    );
    // The bed is untouched by the refusal: still at the clashing changeset.
    assert_eq!(current_cs(&w.garden), clashing);

    w.server.shutdown().await;
}

/// `bed rebase --stack`: a 3-deep local stack replays onto a moved main
/// oldest-first, each replayed link parented on the previous one, and
/// the working copy lands at the new tip.
#[tokio::test]
async fn stack_rebase_replays_the_whole_first_parent_chain() {
    let mut w = setup().await;
    plant(&w);

    // Build a 3-deep stack of working-copy commits.
    let mut tips = Vec::new();
    for i in 1..=3 {
        fs::write(
            w.garden.join(format!("src/stack-{i}.rs")),
            format!("pub fn layer{i}() {{}}\n").as_bytes(),
        )
        .unwrap();
        let (code, out, stderr) = bed(&w.garden, &["commit", "-m", &format!("layer {i}")]);
        assert_eq!(code, 0, "layer {i}: {stderr}\n{out}");
        tips.push(current_cs(&w.garden));
    }
    let stack_bottom = w
        .repo
        .get_changeset(tips[0])
        .unwrap()
        .parents
        .first()
        .copied()
        .unwrap();

    // Upstream moves main off the stack's base: a fresh file, so every
    // stack link's Add applies cleanly on top of it.
    let (up_change, up_content) = add("src/upstream.rs", b"pub fn upstream() {}\n", Mode::Regular);
    let up_blob = up_change.blob.unwrap();
    let upstream_cs = cs(
        vec![stack_bottom],
        vec![up_change],
        "upstream",
        1_700_000_300,
    );
    let upstream_id = w
        .repo
        .create_changeset(
            &upstream_cs,
            &std::collections::BTreeMap::from([(up_blob.to_hex(), up_content)]),
        )
        .expect("upstream ingests");
    w.repo
        .cas_ref("main", Some(w.head), upstream_id)
        .expect("main moves");

    let (code, out, stderr) = bed(&w.garden, &["rebase", "--stack", "main"]);
    assert_eq!(code, 0, "stack rebase: {stderr}\n{out}");
    assert!(out.contains("rebased 3 change(s)"), "output: {out}");
    let tip = current_cs(&w.garden);
    assert_ne!(tip, tips[2], "the working copy moved to the replayed tip");

    // Chain shape: replayed tip -> replayed(2) -> replayed(1) -> upstream.
    let mut ids = Vec::new();
    let mut walk = tip;
    for _ in 0..5 {
        ids.push(walk);
        let cs = w.repo.get_changeset(walk).unwrap();
        walk = cs.parents.first().copied().unwrap();
    }
    assert_eq!(ids[3], upstream_id, "the chain roots on the new main");
    // The ORIGINAL stack links are not reused: every replayed link is a
    // fresh changeset parented on the previous replay, and none of the
    // original tips appears in the new chain.
    for t in &tips {
        assert!(
            !ids.contains(t),
            "original stack link {t:?} was replayed, not reused"
        );
    }
    for id in ids.iter().take(3) {
        assert_eq!(w.repo.get_changeset(*id).unwrap().parents.len(), 1);
    }
    // The union tree: seed + upstream + all three layers.
    assert!(flat_files(&w.repo, tip).contains_key(&"src/stack-3.rs".to_owned()));
    assert!(flat_files(&w.repo, tip).contains_key(&"src/upstream.rs".to_owned()));

    // The working copy came along and is clean.
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(out.contains("working copy is clean"), "status: {out}");

    w.server.shutdown().await;
}

/// `bed rebase --stack`: a conflict mid-stack aborts the whole stack —
/// the working copy never moves off its current changeset.
#[tokio::test]
async fn stack_rebase_conflict_mid_stack_aborts_clean() {
    let mut w = setup().await;
    plant(&w);

    // Two working-copy layers.
    fs::write(w.garden.join("src/layer-a.rs"), b"pub fn a() {}\n").unwrap();
    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "layer a"]);
    assert_eq!(code, 0, "layer a: {stderr}\n{out}");
    let first = current_cs(&w.garden);

    // Layer B modifies docs/readme.md — chosen to clash with upstream.
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n layer b\n").unwrap();
    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "layer b"]);
    assert_eq!(code, 0, "layer b: {stderr}\n{out}");
    let before = current_cs(&w.garden);
    let stack_bottom = w
        .repo
        .get_changeset(first)
        .unwrap()
        .parents
        .first()
        .copied()
        .unwrap();

    // Upstream modifies the same readme — the conflict lands when layer
    // B replays (the SECOND link), after layer A has replayed cleanly.
    let up_modify = Change {
        path: "docs/readme.md".into(),
        op: Op::Modify,
        blob: Some(blob_id(b"# hello\n\n upstream\n")),
        src_path: None,
        mode: Mode::Regular,
    };
    let up_blob = up_modify.blob.unwrap();
    let upstream_cs = cs(
        vec![stack_bottom],
        vec![up_modify],
        "upstream readme",
        1_700_000_400,
    );
    let upstream_id = w
        .repo
        .create_changeset(
            &upstream_cs,
            &std::collections::BTreeMap::from([(
                up_blob.to_hex(),
                b"# hello\n\n upstream\n".to_vec(),
            )]),
        )
        .expect("upstream ingests");
    w.repo
        .cas_ref("main", Some(w.head), upstream_id)
        .expect("main moves");

    // The stack rebase fails: layer A applies, layer B conflicts.
    let (code, out, stderr) = bed(&w.garden, &["rebase", "--stack", "main"]);
    assert_eq!(code, 1, "a mid-stack conflict must fail: {out}");
    assert!(stderr.contains("rebase"), "stderr names rebase: {stderr}");
    // The working copy never moved: still at the pre-rebase tip.
    assert_eq!(
        current_cs(&w.garden),
        before,
        "an aborted stack rebase leaves the working copy untouched"
    );
    // And the disk never moved either: layer B's content is still there.
    assert_eq!(
        fs::read_to_string(w.garden.join("docs/readme.md")).unwrap(),
        "# hello\n\n layer b\n"
    );

    w.server.shutdown().await;
}

/// M5 write path 3: a lone publish fast-forwards `main`; a working copy left
/// behind gets the not-fast-forward refusal with the recipe spelled out.
#[tokio::test]
async fn publish_fast_forwards_and_losers_get_the_recipe() {
    let mut w = setup().await;
    plant(&w);

    // A second bed parks itself at the base tip BEFORE anything moves.
    let other = w._dir.path().join("garden-2");
    fs::create_dir_all(&other).unwrap();
    let (code, _, stderr) = bed(&other, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = bed(&other, &["sparse", "add", "."]);
    assert_eq!(code, 0, "{stderr}");

    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n published\n").unwrap();
    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "publish me"]);
    assert_eq!(code, 0, "{stderr}\n{out}");
    let created = current_cs(&w.garden);

    // Publish publishes: main fast-forwards to the created changeset, and
    // the working copy stays where it is.
    let (code, out, stderr) = bed(&w.garden, &["publish"]);
    assert_eq!(code, 0, "publish: {stderr}\n{out}");
    assert!(out.contains("published"), "out: {out}");
    assert_eq!(w.repo.get_ref("main").expect("get_ref"), Some(created));
    assert_eq!(
        current_cs(&w.garden),
        created,
        "publishing does not move the working copy"
    );

    // The second bed (still at the old tip) commits a sibling changeset; its
    // publish is no longer a fast-forward of `main` and is refused with
    // the fix spelled out.
    fs::write(other.join("src/other.rs"), b"// other bed\n").unwrap();
    let (code, out, stderr) = bed(&other, &["commit", "-m", "other working copy's change"]);
    assert_eq!(code, 0, "{stderr}\n{out}");
    assert_ne!(current_cs(&other), created);

    let (code, _, stderr) = bed(&other, &["publish"]);
    assert_eq!(code, 1, "a non-fast-forward publish must fail");
    assert!(stderr.contains("does not fast-forward `main`"), "{stderr}");
    assert!(stderr.contains("onto the new main"), "{stderr}");

    w.server.shutdown().await;
}

/// Sixteen beds commit sixteen sibling changesets and publish concurrently:
/// the ref CAS admits exactly one winner; every loser is told the fix.
#[tokio::test]
async fn sixteen_concurrent_publishs_exactly_one_wins() {
    let mut w = setup().await;
    plant(&w);
    let base = w.head;

    // Sixteen beds, one sibling changeset each (distinct files).
    const N: usize = 16;
    let mut beds = Vec::new();
    let mut created = Vec::new();
    for i in 0..N {
        let dir = w._dir.path().join(format!("bed-{i:02}"));
        fs::create_dir_all(dir.join("row")).unwrap();
        let (code, _, stderr) = bed(&dir, &["clone", &w.server.http()]);
        assert_eq!(code, 0, "clone {i}: {stderr}");
        let (code, _, stderr) = bed(&dir, &["sparse", "add", "."]);
        assert_eq!(code, 0, "sparse add {i}: {stderr}");
        fs::write(
            dir.join(format!("row/bed-{i:02}.txt")),
            format!("bed {i}'s row\n"),
        )
        .unwrap();
        let (code, out, stderr) = bed(&dir, &["commit", "-m", &format!("bed {i} commits")]);
        assert_eq!(code, 0, "commit {i}: {stderr}\n{out}");
        created.push(current_cs(&dir));
        beds.push(dir);
    }

    // Sixteen concurrent publishs of the same ref position.
    let mut children = Vec::new();
    for dir in &beds {
        children.push(bed_async(dir, &["publish"]));
    }
    let mut winners = 0usize;
    let mut loser_messages = Vec::new();
    for (i, child) in children.into_iter().enumerate() {
        let out = child.wait_with_output().await.expect("publish finishes");
        let code = out.status.code().unwrap_or(-1);
        if code == 0 {
            winners += 1;
        } else {
            loser_messages.push((i, String::from_utf8_lossy(&out.stderr).into_owned()));
        }
    }
    assert_eq!(winners, 1, "exactly one publisher wins the CAS");
    assert_eq!(loser_messages.len(), N - 1);
    for (_, msg) in &loser_messages {
        assert!(msg.contains("does not fast-forward `main`"), "{msg}");
        assert!(msg.contains("rebase"), "{msg}");
    }

    // The ref points at the winner's changeset — one of the sixteen.
    let main = w
        .repo
        .get_ref("main")
        .expect("get_ref")
        .expect("main exists");
    assert!(
        created.contains(&main),
        "main is one of the created changesets"
    );
    assert_ne!(main, base);

    // The winner's bed can publish again (idempotent); everyone else is
    // still refused.
    for (i, dir) in beds.iter().enumerate() {
        let (code, _, _) = bed(dir, &["publish"]);
        let should_win = current_cs(dir) == main;
        assert_eq!(
            code == 0,
            should_win,
            "bed {i} publish {} after the race",
            if should_win { "must win" } else { "must lose" }
        );
    }

    w.server.shutdown().await;
}

/// The chaos variant: publishers are killed mid-flight. Whatever lands,
/// the ref ends up consistent — either still at the base or at exactly one
/// of the candidates — and a follow-up round still has exactly one winner.
#[tokio::test]
async fn chaos_killed_publishs_leave_a_consistent_ref() {
    let mut w = setup().await;
    plant(&w);
    let base = w.head;

    const N: usize = 16;
    let mut beds = Vec::new();
    let mut created = Vec::new();
    for i in 0..N {
        let dir = w._dir.path().join(format!("chaos-{i:02}"));
        fs::create_dir_all(dir.join("row")).unwrap();
        let (code, _, stderr) = bed(&dir, &["clone", &w.server.http()]);
        assert_eq!(code, 0, "clone {i}: {stderr}");
        let (code, _, stderr) = bed(&dir, &["sparse", "add", "."]);
        assert_eq!(code, 0, "sparse add {i}: {stderr}");
        fs::write(
            dir.join(format!("row/chaos-{i:02}.txt")),
            format!("chaos bed {i}'s row\n"),
        )
        .unwrap();
        let (code, out, stderr) = bed(&dir, &["commit", "-m", &format!("chaos bed {i} commits")]);
        assert_eq!(code, 0, "commit {i}: {stderr}\n{out}");
        created.push(current_cs(&dir));
        beds.push(dir);
    }

    // Stagger and kill half mid-flight; let the rest finish.
    let mut children = Vec::new();
    for dir in &beds {
        children.push((bed_async(dir, &["publish"]), tokio::time::Instant::now()));
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
            let out = child.wait_with_output().await.expect("publish finishes");
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
            m == base || created.contains(&m),
            "main is a candidate or the base"
        );
        w.repo
            .get_changeset(m)
            .expect("main names a real changeset");
    }

    // Round 2, sequential: exactly one working copy wins the (possibly fresh) ref
    // position, and the ref lands on its changeset.
    let mut round2_winners = 0usize;
    for dir in &beds {
        let (code, _, _) = bed(dir, &["publish"]);
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
    assert!(created.contains(&main));

    w.server.shutdown().await;
}

/// M5 write path 4: a commit killed in the upload window (between `PutBlob`
/// and `CreateChangeset`) recovers by re-committing — the heavy blob goes over
/// the wire exactly once across both runs (cache-first: the local cache
/// wrote it before the upload, so the rerun trusts it), and the retry
/// completes with a clean sparse.
#[tokio::test]
async fn commit_killed_between_upload_and_commit_retries_uploading_once() {
    let mut w = setup().await;
    plant(&w);

    // One heavy blob (> 1 MiB rides PutBlob, not inline) plus a small one.
    let heavy = vec![0x61u8; (1 << 20) + 17];
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n heavy\n").unwrap();
    fs::write(w.garden.join("bulk.bin"), &heavy).unwrap();

    let mut child = bed_async(&w.garden, &["commit", "-m", "heavy lift"]);
    let client = w.server.client().await;

    // Poll the server's counters until the heavy blob has fully
    // arrived (put_blob bumps only after the whole stream), then kill the
    // bed mid-flight — inside the upload→commit window.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let stats = client.get_stats(REPO).await.expect("get_stats");
        if stats.get("put_blob").copied().unwrap_or(0) >= 1 {
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            panic!("the commit exited before the upload completed");
        }
        assert!(Instant::now() < deadline, "put_blob never bumped");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    child.kill().await.expect("SIGKILL the committing bed");
    let _ = child.wait().await;

    // The local bed state never moved (it only moves after success).
    assert_eq!(
        current_cs(&w.garden),
        w.head,
        "config untouched by the crash"
    );

    // Re-commit: succeeds, and the heavy blob went over the wire exactly once
    // in total — the retry must trust the cache, never re-upload.
    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "heavy lift, retried"]);
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

    // The blob is really there, and working copy is clean.
    let blob = blob_id(&heavy);
    let stored = w
        .repo
        .get_blob(blob)
        .expect("get_blob")
        .expect("blob exists");
    assert_eq!(stored, heavy);
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(out.contains("working copy is clean"), "status: {out}");

    w.server.shutdown().await;
}

/// M5 write path 5: heavy blobs take the two-phase path (PutBlob first),
/// and committing the same content again uploads nothing new (dedup by id).
#[tokio::test]
async fn heavy_blobs_upload_first_and_never_twice() {
    let mut w = setup().await;
    plant(&w);

    let heavy = vec![0x23u8; (1 << 20) + 5];
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n bulky\n").unwrap();
    fs::write(w.garden.join("bulk.bin"), &heavy).unwrap();

    let client = w.server.client().await;
    let before = client.get_stats(REPO).await.expect("get_stats");
    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "bring the bulk"]);
    assert_eq!(code, 0, "{stderr}\n{out}");
    // The outcome spells out the two-phase split.
    assert!(out.contains("1 blob(s) uploaded separately"), "out: {out}");
    assert!(out.contains(" sent inline"), "out: {out}");
    let after = client.get_stats(REPO).await.expect("get_stats");
    assert_eq!(
        stats_get(&before, "put_blob") + 1,
        stats_get(&after, "put_blob"),
        "one put_blob for the heavy blob"
    );

    // Re-committing the same heavy content (untouched) changes nothing — and
    // a second commit of it (e.g. after a switch away and back) uploads nothing.
    // Here: commit a different file, then verify the counter stayed put.
    let before = after;
    fs::write(
        w.garden.join("docs/readme.md"),
        b"# hello\n\n bulky again\n",
    )
    .unwrap();
    let (code, _, stderr) = bed(&w.garden, &["commit", "-m", "no bulk this time"]);
    assert_eq!(code, 0, "{stderr}");
    let after = client.get_stats(REPO).await.expect("get_stats");
    assert_eq!(
        stats_get(&before, "put_blob"),
        stats_get(&after, "put_blob"),
        "small commits never touch put_blob"
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
    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "many small blobs"]);
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
    assert!(out.contains("working copy is clean"), "{out}");
    w.server.shutdown().await;
}

fn stats_get(stats: &std::collections::BTreeMap<String, u64>, key: &str) -> u64 {
    stats.get(key).copied().unwrap_or(0)
}

// ------------------------------------------------------------ diff

/// No-argument `bed diff` is context-aware and always says what it
/// diffed: a dirty working copy diffs local bytes against the current
/// changeset; a clean one falls back to the committed current-vs-parent
/// diff. Both modes are distinguishable at a glance by the header, and
/// the explicit `A B` form is unchanged (its body is the pinned
/// regression shape — only the header line is new).
#[tokio::test]
async fn no_arg_diff_names_working_copy_when_dirty_and_changesets_when_clean() {
    let mut w = setup().await;
    plant(&w);
    // w.head is cs2: "tend the readme" modified docs/readme.md and added
    // src/extra.rs, both against cs1.

    // Clean tree: the no-arg diff is cs2 vs cs1, header names changesets.
    let (code, out, err) = bed(&w.garden, &["diff"]);
    assert_eq!(code, 0, "{err}");
    let cs2 = current_cs(&w.garden);
    assert!(
        out.starts_with(&format!(
            "diff of {} against its first parent\n",
            cs2.short()
        )),
        "header names the changeset: {out}"
    );
    let clean_body = out.lines().skip(1).collect::<Vec<_>>().join("\n");
    assert!(
        clean_body.contains("docs/readme.md") && clean_body.contains("src/extra.rs"),
        "committed diff body: {clean_body}"
    );

    // Dirty tree: the no-arg diff is the working copy against cs2, header
    // names the working copy. Untracked files are not part of it.
    fs::write(
        w.garden.join("src/lib.rs"),
        b"pub fn bed() { /* local edit */ }\n",
    )
    .unwrap();
    fs::write(w.garden.join("src/untracked.rs"), b"not part of the diff\n").unwrap();
    fs::remove_file(w.garden.join("notes/todo.txt")).unwrap();
    let (code, out, err) = bed(&w.garden, &["diff"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.starts_with(&format!("diff of working copy against {}\n", cs2.short())),
        "header names the working copy: {out}"
    );
    assert!(
        !out.contains("untracked.rs"),
        "untracked files are not diffed: {out}"
    );
    let body = out.lines().skip(1).collect::<Vec<_>>().join("\n");
    assert!(
        body.contains("diff --git a/src/lib.rs b/src/lib.rs"),
        "edit rendered: {body}"
    );
    assert!(
        body.contains("diff --git a/notes/todo.txt b/notes/todo.txt"),
        "local deletion rendered: {body}"
    );
    assert!(
        body.contains("+++ /dev/null"),
        "deletion renders to /dev/null: {body}"
    );

    // Explicit A B form: unchanged semantics, plus the header line. The
    // body must be byte-identical to the clean no-arg body (both are the
    // cs1→cs2 diff).
    let cs1 = {
        // cs1 is the only parent of cs2.
        w.repo
            .get_changeset(cs2)
            .unwrap()
            .parents
            .first()
            .copied()
            .unwrap()
    };
    let (code, out, err) = bed(&w.garden, &["diff", &cs1.to_hex(), &cs2.to_hex()]);
    assert_eq!(code, 0, "{err}");
    let explicit_body = out.lines().skip(1).collect::<Vec<_>>().join("\n");
    assert_eq!(explicit_body, clean_body, "explicit A B body is unchanged");

    w.server.shutdown().await;
}

/// A tracked symlink retargeted in the working copy: the new side of
/// `bed diff` shows the *link-target bytes* (what `bed commit` records),
/// never the target file's content — and a dangling retarget renders too
/// instead of failing to read.
#[tokio::test]
async fn diff_retargeted_symlink_shows_link_target_bytes_not_target_content() {
    let mut w = setup().await;
    plant(&w);

    // Plant a tracked symlink first; a fresh link is merely untracked.
    fs::write(w.garden.join("docs/other.md"), b"target file content\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("docs/readme.md", w.garden.join("docs/link.txt")).unwrap();
    let (code, _, err) = bed(&w.garden, &["commit", "-m", "add a link"]);
    assert_eq!(code, 0, "{err}");

    // The link bytes bed commit would record for the new target, computed
    // exactly the way commit does: the link-target bytes.
    let new_link_bytes = b"docs/other.md".to_vec();
    #[cfg(unix)]
    {
        fs::remove_file(w.garden.join("docs/link.txt")).unwrap();
        std::os::unix::fs::symlink("docs/other.md", w.garden.join("docs/link.txt")).unwrap();
    }

    let (code, out, err) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("modified") && out.contains("docs/link.txt"),
        "retargeted link is modified: {out}"
    );

    let cs = current_cs(&w.garden);
    let (code, out, err) = bed(&w.garden, &["diff"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.starts_with(&format!("diff of working copy against {}\n", cs.short())),
        "header names the working copy: {out}"
    );
    assert!(
        out.contains("diff --git a/docs/link.txt b/docs/link.txt"),
        "link retarget rendered: {out}"
    );
    // The + side must carry the link-target bytes, not the target's
    // content (git parity: the symlink's "content" is its target path).
    let body = out.lines().skip(1).collect::<Vec<_>>().join("\n");
    assert!(
        body.contains("+docs/other.md"),
        "new side is the link-target bytes: {body}"
    );
    assert!(
        !body.contains("target file content"),
        "new side is NOT the target file's content: {body}"
    );

    // What commit records for the same state must agree byte-for-byte.
    let (code, _, err) = bed(&w.garden, &["commit", "-m", "retarget the link"]);
    assert_eq!(code, 0, "{err}");
    let committed = w
        .repo
        .get_changeset(current_cs(&w.garden))
        .expect("committed changeset");
    let change = committed
        .changes
        .iter()
        .find(|c| c.path == "docs/link.txt")
        .expect("link.txt change");
    assert_eq!(change.blob, Some(gardenbed_model::blob_id(&new_link_bytes)));
    assert_eq!(change.mode, Mode::Symlink);

    w.server.shutdown().await;
}

/// A tracked symlink retargeted to a nonexistent path (dangling): the
/// diff still renders — the link-target bytes exist regardless of
/// whether the target does — instead of exiting 1 on read.
#[tokio::test]
async fn diff_dangling_symlink_renders_instead_of_failing() {
    let mut w = setup().await;
    plant(&w);

    fs::remove_file(w.garden.join("docs/readme.md")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("docs/no-such-file.md", w.garden.join("docs/readme.md")).unwrap();

    let (code, out, err) = bed(&w.garden, &["diff"]);
    assert_eq!(code, 0, "dangling link renders: {err}");
    assert!(
        out.contains("diff --git a/docs/readme.md b/docs/readme.md"),
        "dangling retarget rendered: {out}"
    );
    assert!(
        out.contains("+docs/no-such-file.md"),
        "new side is the link-target bytes: {out}"
    );

    w.server.shutdown().await;
}

/// A tracked file replaced by a directory: status says deleted, and the
/// diff agrees — it renders as a deletion instead of exiting 1 'Is a
/// directory'.
#[tokio::test]
async fn diff_tracked_file_replaced_by_directory_renders_as_deletion() {
    let mut w = setup().await;
    plant(&w);

    fs::remove_file(w.garden.join("notes/todo.txt")).unwrap();
    fs::create_dir(w.garden.join("notes/todo.txt")).unwrap();

    let (code, out, err) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("deleted") && out.contains("notes/todo.txt"),
        "status says deleted: {out}"
    );

    let (code, out, err) = bed(&w.garden, &["diff"]);
    assert_eq!(code, 0, "directory replacement renders: {err}");
    assert!(
        out.contains("diff --git a/notes/todo.txt b/notes/todo.txt"),
        "deletion rendered: {out}"
    );
    assert!(
        out.contains("deleted file mode") || out.contains("+++ /dev/null"),
        "renders as a removal: {out}"
    );

    w.server.shutdown().await;
}

/// The two-argument header claims "first parent" only when it is true:
/// a non-parent pair names both sides (`diff of <older> against
/// <newer>`), a real parent pair keeps the first-parent wording.
#[tokio::test]
async fn diff_two_arg_header_names_both_sides_for_non_parent_pair() {
    let mut w = setup().await;
    plant(&w);

    // A third changeset on top of cs2, so cs1→cs3 is a non-parent pair
    // while cs2→cs3 is a parent pair.
    fs::write(
        w.garden.join("docs/readme.md"),
        b"# hello\n\n tended more\n",
    )
    .unwrap();
    let (code, _, err) = bed(&w.garden, &["commit", "-m", "third"]);
    assert_eq!(code, 0, "{err}");
    let cs3 = current_cs(&w.garden);
    let cs2 = w.head;
    let cs1 = w
        .repo
        .get_changeset(cs2)
        .unwrap()
        .parents
        .first()
        .copied()
        .unwrap();

    // Parent pair: first-parent wording.
    let (code, out, err) = bed(&w.garden, &["diff", &cs2.to_hex(), &cs3.to_hex()]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.starts_with(&format!(
            "diff of {} against its first parent\n",
            cs3.short()
        )),
        "parent pair keeps first-parent wording: {out}"
    );

    // Non-parent pair: both sides named, no ancestry claim.
    let (code, out, err) = bed(&w.garden, &["diff", &cs1.to_hex(), &cs3.to_hex()]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.starts_with(&format!(
            "diff of {} against {}\n",
            cs1.short(),
            cs3.short()
        )),
        "non-parent pair names both sides: {out}"
    );

    w.server.shutdown().await;
}

/// Untracked-only dirt does not hijack the no-argument diff: nothing is
/// modified or deleted, so the diff is the committed current-vs-parent
/// one (self-describing header included), not a header-only empty
/// working-copy diff — git parity.
#[tokio::test]
async fn diff_untracked_only_dirt_falls_back_to_committed_diff() {
    let mut w = setup().await;
    plant(&w);

    fs::write(w.garden.join("src/untracked.rs"), b"not part of the diff\n").unwrap();
    let (code, out, err) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("untracked") && !out.contains("working copy is clean"),
        "status is dirty by untracked only: {out}"
    );

    // The diff must be the committed cs2-vs-cs1 one, header and all.
    let cs2 = current_cs(&w.garden);
    let (code, out, err) = bed(&w.garden, &["diff"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.starts_with(&format!(
            "diff of {} against its first parent\n",
            cs2.short()
        )),
        "untracked-only dirt falls back to the committed diff: {out}"
    );
    assert!(
        out.contains("diff --git a/docs/readme.md b/docs/readme.md"),
        "committed body renders: {out}"
    );
    assert!(
        !out.contains("untracked.rs"),
        "untracked files are not diffed: {out}"
    );

    w.server.shutdown().await;
}

/// The dogfood #10 repro: a lane agent commits off-main and publishes with
/// `bed publish --ref <branch>`; a FRESH clone's `bed switch <branch>` must
/// then resolve the branch to the committed tip, not the stale imported one
/// the ref carried before the publish.
#[tokio::test]
async fn publish_ref_advances_lane_and_fresh_switch_sees_it() {
    let mut w = setup().await;
    plant(&w);

    // The stale imported lane ref: the branch exists on the server, pointing
    // at the history the working copy cloned from (the dogfood's
    // `feat/scale-out` resolving to the imported tip).
    let lane = "feat/scale-out";
    w.repo
        .cas_ref(lane, None, w.head)
        .expect("lane ref imported");

    // The lane agent commits off-main.
    fs::write(w.garden.join("docs/readme.md"), b"# hello\n\n lane work\n").unwrap();
    let (code, out, stderr) = bed(&w.garden, &["commit", "-m", "lane work"]);
    assert_eq!(code, 0, "commit: {stderr}\n{out}");
    let committed = current_cs(&w.garden);

    // `bed publish --ref <branch>` advances the lane ref.
    let (code, out, stderr) = bed(&w.garden, &["publish", "--ref", lane]);
    assert_eq!(code, 0, "publish --ref: {stderr}\n{out}");
    assert!(
        out.contains(&format!("`{lane}` now points to it")),
        "the output names the lane ref: {out}"
    );
    assert_eq!(
        w.repo.get_ref(lane).expect("get_ref"),
        Some(committed),
        "the lane ref advanced to the committed tip"
    );
    // main is untouched by a lane publish.
    assert_eq!(w.repo.get_ref("main").expect("get_ref"), Some(w.head));

    // The fresh clone: `bed switch <branch>` must land on the committed tip.
    let fresh = w._dir.path().join("garden-fresh");
    fs::create_dir_all(&fresh).unwrap();
    let (code, _, stderr) = bed(&fresh, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "fresh clone: {stderr}");
    let (code, out, stderr) = bed(&fresh, &["sparse", "add", "."]);
    assert_eq!(code, 0, "fresh sparse add: {stderr}\n{out}");
    let (code, out, stderr) = bed(&fresh, &["switch", lane]);
    assert_eq!(code, 0, "fresh switch: {stderr}\n{out}");
    assert_eq!(
        current_cs(&fresh),
        committed,
        "switch resolves the lane ref to the published tip, not the stale imported one"
    );
    // The materialized file is the lane agent's edit.
    assert_eq!(
        fs::read(fresh.join("docs/readme.md")).unwrap(),
        b"# hello\n\n lane work\n",
        "the fresh switch materialized the committed content"
    );

    // `--ref main` is reserved: main publishes stay the plain form.
    let (code, _, stderr) = bed(&w.garden, &["publish", "--ref", "main"]);
    assert_eq!(code, 1, "--ref main must be rejected: {stderr}");
    assert!(
        stderr.contains("plain form"),
        "the reserved error names the plain form: {stderr}"
    );

    w.server.shutdown().await;
}
