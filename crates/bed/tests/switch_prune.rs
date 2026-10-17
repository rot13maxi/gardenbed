//! Regression tests for `bed switch` removing directories its removals
//! left empty (dogfood bug: after switching off a changeset that
//! introduced `crates/web/{src,static,tests}`, the empty directories
//! stayed on disk — a workspace glob `crates/*` then matched a member
//! with no `Cargo.toml` and `cargo check` failed).
//!
//! The safety rules under test:
//!
//! * a directory that still contains *anything* (files, other
//!   directories, untracked or ignored content) is never pruned;
//! * the working-copy root and the state directory are never pruned;
//! * the sparse selection is recorded in the config, independent of
//!   disk state — pruning does not touch it;
//! * switching back re-materializes the files (round-trip).

mod common;

use std::{collections::BTreeMap, fs, path::PathBuf};

use common::{REPO, Server, bed};
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;

struct World {
    _root: tempfile::TempDir,
    garden: PathBuf,
    server: Server,
    cs1: Id,
    cs2: Id,
}

fn create(repo: &Repo, parents: Vec<Id>, files: &[(&str, &[u8])]) -> Id {
    let identity = Identity {
        name: "Seeder".into(),
        email: "seed@bed.example".into(),
        time_secs: 1_700_000_000,
        tz_min: 0,
    };
    let mut changes: Vec<Change> = files
        .iter()
        .map(|(path, bytes)| Change {
            path: (*path).into(),
            op: Op::Add,
            blob: Some(blob_id(bytes)),
            src_path: None,
            mode: Mode::Regular,
        })
        .collect();
    changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let inline = files
        .iter()
        .map(|(_, bytes)| (blob_id(bytes).to_hex(), bytes.to_vec()))
        .collect::<BTreeMap<_, _>>();
    repo.create_changeset(
        &Changeset {
            parents,
            changes,
            message: "seed".into(),
            author: identity.clone(),
            committer: identity,
            signature: None,
        },
        &inline,
    )
    .unwrap()
}

/// `cs1` carries the web crate (three subdirectories with files, like the
/// dogfood repo's `crates/web`); `cs2` predates it, carrying only `a`.
async fn world() -> World {
    let root = tempfile::tempdir().unwrap();
    let garden = root.path().join("garden");
    fs::create_dir(&garden).unwrap();
    let store_root = root.path().join("server");
    let repo = Repo::open(&store_root).unwrap();
    let cs1 = create(
        &repo,
        vec![],
        &[
            ("a", b"base"),
            ("crates/web/Cargo.toml", b"[package]\nname = \"web\"\n"),
            ("crates/web/src/main.rs", b"fn main() {}\n"),
            ("crates/web/static/app.css", b"body {}\n"),
            ("crates/web/tests/it.rs", b"#[test]\nfn t() {}\n"),
        ],
    );
    let cs2 = create(&repo, vec![], &[("a", b"older")]);
    repo.cas_ref("main", None, cs1).unwrap();
    let server = Server::spawn(&store_root).await;
    let (code, _, err) = bed(
        &garden,
        &[
            "clone",
            &server.http(),
            "--repo",
            REPO,
            "--sparse",
            "crates",
        ],
    );
    assert_eq!(code, 0, "{err}");
    // Sparse was recorded at clone; materialize at cs1.
    let (code, _, err) = bed(&garden, &["sparse", "add", "crates"]);
    assert_eq!(code, 0, "{err}");
    World {
        _root: root,
        garden,
        server,
        cs1,
        cs2,
    }
}

#[tokio::test]
async fn switch_prunes_directories_left_empty_by_removals() {
    let mut w = world().await;
    let web = w.garden.join("crates/web");
    for dir in ["src", "static", "tests"] {
        assert!(web.join(dir).is_dir(), "fixture materialized {dir}");
    }

    // Switch to the changeset without crates/web: files go, and now the
    // emptied directories must go with them.
    let (code, out, err) = bed(&w.garden, &["switch", &w.cs2.to_hex()]);
    assert_eq!(code, 0, "{err}");
    assert!(!web.exists(), "crates/web pruned entirely: {out}");
    assert!(
        w.garden.join("crates").is_dir(),
        "crates stays: sparse-selected"
    );
    // A cargo-style glob must not see the vanished member.
    let members: Vec<_> = fs::read_dir(w.garden.join("crates"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("Cargo.toml").exists())
        .collect();
    assert!(members.is_empty(), "no crates/* member remains");

    // The sparse selection is untouched by pruning.
    let (code, out, err) = bed(&w.garden, &["sparse", "list"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("crates"), "sparse stays recorded: {out}");

    // Round-trip: switching back re-materializes the files.
    let (code, _, err) = bed(&w.garden, &["switch", &w.cs1.to_hex()]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        fs::read(web.join("src/main.rs")).unwrap(),
        b"fn main() {}\n"
    );
    assert_eq!(
        fs::read(web.join("Cargo.toml")).unwrap(),
        b"[package]\nname = \"web\"\n"
    );
    w.server.shutdown().await;
}

#[tokio::test]
async fn untracked_content_blocks_pruning_of_its_directory() {
    let mut w = world().await;
    let web = w.garden.join("crates/web");
    // Untracked content in a to-be-emptied directory must keep the
    // directory on disk.
    fs::write(web.join("src/scratch.rs"), b"local notes").unwrap();

    let (code, _, err) = bed(&w.garden, &["switch", &w.cs2.to_hex()]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        fs::read(web.join("src/scratch.rs")).unwrap(),
        b"local notes",
        "untracked file survives"
    );
    assert!(web.join("src").is_dir(), "its directory survives");
    // The sibling directories with no untracked content still go.
    assert!(!web.join("static").exists());
    assert!(!web.join("tests").exists());

    // Round-trip back: nothing materialized collides with the untracked
    // file, so the switch succeeds and both live side by side.
    let (code, _, err) = bed(&w.garden, &["switch", &w.cs1.to_hex()]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        fs::read(web.join("src/scratch.rs")).unwrap(),
        b"local notes",
        "untracked file survives the round-trip"
    );
    assert_eq!(
        fs::read(web.join("src/main.rs")).unwrap(),
        b"fn main() {}\n"
    );
    w.server.shutdown().await;
}
