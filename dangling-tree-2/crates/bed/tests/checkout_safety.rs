//! User-facing materialization refusals must preserve files and bed state.
mod common;

use std::{collections::BTreeMap, fs, path::PathBuf};

<<<<<<< HEAD
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;
use common::{Arboretum, REPO, bed};
=======
use common::{Arboretum, REPO, bed};
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;
>>>>>>> codex/integrity-fixes

struct World {
    _root: tempfile::TempDir,
    garden: PathBuf,
    server: Arboretum,
    target: Id,
}

fn create(repo: &Repo, parents: Vec<Id>, files: &[(&str, Op, &[u8])]) -> Id {
<<<<<<< HEAD
    let identity = Identity { name: "test".into(), email: "t@example.com".into(), time_secs: 1, tz_min: 0 };
    let changes = files.iter().map(|(path, op, bytes)| Change {
        path: path.to_string(), op: *op, blob: Some(blob_id(bytes)), mode: Mode::Regular, src_path: None,
    }).collect();
    let inline = files.iter().map(|(_, _, bytes)| (blob_id(bytes).to_hex(), bytes.to_vec())).collect::<BTreeMap<_, _>>();
    repo.create_changeset(&Changeset { parents, changes, message: "fixture".into(), author: identity.clone(), committer: identity, signature: None }, &inline).unwrap()
}

async fn world(extra: Option<&str>, state_name: &str) -> World {
    let root = tempfile::tempdir().unwrap();
    let garden = root.path().join("garden");
    fs::create_dir(&garden).unwrap();
    let store_root = root.path().join("server");
    let repo = Repo::open(&store_root.join(REPO)).unwrap();
    let mut files = vec![("a", Op::Add, b"base".as_slice()), ("z", Op::Add, b"base".as_slice())];
    if let Some(path) = extra { files.push((path, Op::Add, b"repo")); }
    files.sort_by_key(|(path, _, _)| *path);
    let base = create(&repo, vec![], &files);
    let local = create(&repo, vec![base], &[("x", Op::Add, b"local branch")]);
    let target = create(&repo, vec![base], &[("a", Op::Modify, b"target"), ("z", Op::Modify, b"target")]);
    repo.cas_ref("main", None, local).unwrap();
    let server = Arboretum::spawn(&store_root).await;
    let (code, _, err) = bed(&garden, &["--bed-dir", state_name, "clone", &server.http(), "--repo", REPO]);
    assert_eq!(code, 0, "{err}");
    World { _root: root, garden, server, target }
=======
    let identity = Identity {
        name: "test".into(),
        email: "t@example.com".into(),
        time_secs: 1,
        tz_min: 0,
    };
    let changes = files
        .iter()
        .map(|(path, op, bytes)| Change {
            path: path.to_string(),
            op: *op,
            blob: Some(blob_id(bytes)),
            mode: Mode::Regular,
            src_path: None,
        })
        .collect();
    let inline = files
        .iter()
        .map(|(_, _, bytes)| (blob_id(bytes).to_hex(), bytes.to_vec()))
        .collect::<BTreeMap<_, _>>();
    repo.create_changeset(
        &Changeset {
            parents,
            changes,
            message: "fixture".into(),
            author: identity.clone(),
            committer: identity,
            signature: None,
        },
        &inline,
    )
    .unwrap()
}

async fn world(extra: Option<&str>, state_name: &str) -> World {
    world_with_external_state(extra, state_name, None).await
}

async fn world_with_external_state(
    extra: Option<&str>,
    state_name: &str,
    external: Option<&std::path::Path>,
) -> World {
    let root = tempfile::tempdir().unwrap();
    let garden = root.path().join("garden");
    fs::create_dir(&garden).unwrap();
    if let Some(external) = external {
        std::os::unix::fs::symlink(external, garden.join(state_name)).unwrap();
    }
    let store_root = root.path().join("server");
    let repo = Repo::open(&store_root).unwrap();
    let mut files = vec![
        ("a", Op::Add, b"base".as_slice()),
        ("z", Op::Add, b"base".as_slice()),
    ];
    if let Some(path) = extra {
        files.push((path, Op::Add, b"repo"));
    }
    files.sort_by_key(|(path, _, _)| *path);
    let base = create(&repo, vec![], &files);
    let local = create(&repo, vec![base], &[("x", Op::Add, b"local branch")]);
    let target = create(
        &repo,
        vec![base],
        &[("a", Op::Modify, b"target"), ("z", Op::Modify, b"target")],
    );
    repo.cas_ref("main", None, local).unwrap();
    let server = Arboretum::spawn(&store_root).await;
    let (code, _, err) = bed(
        &garden,
        &[
            "--bed-dir",
            state_name,
            "clone",
            &server.http(),
            "--repo",
            REPO,
        ],
    );
    assert_eq!(code, 0, "{err}");
    World {
        _root: root,
        garden,
        server,
        target,
    }
>>>>>>> codex/integrity-fixes
}

#[tokio::test]
async fn dirty_goto_and_graft_refuse_before_any_file_or_state_moves() {
    let mut w = world(None, ".bed").await;
    let (code, _, err) = bed(&w.garden, &["plot", "add", "."]);
    assert_eq!(code, 0, "{err}");
    fs::write(w.garden.join("z"), b"unsown").unwrap();
    let config = fs::read(w.garden.join(".bed/config.toml")).unwrap();
    let heads = fs::read(w.garden.join(".bed/heads")).unwrap();
    for command in ["goto", "graft"] {
        let (code, _, err) = bed(&w.garden, &[command, &w.target.to_hex()]);
        assert_ne!(code, 0, "{command} must refuse");
        assert!(err.contains("local changes"), "{err}");
        assert_eq!(fs::read(w.garden.join("a")).unwrap(), b"base");
        assert_eq!(fs::read(w.garden.join("z")).unwrap(), b"unsown");
        assert_eq!(fs::read(w.garden.join(".bed/config.toml")).unwrap(), config);
        assert_eq!(fs::read(w.garden.join(".bed/heads")).unwrap(), heads);
    }
    let (code, _, err) = bed(&w.garden, &["plot", "add", "."]);
    assert_eq!(code, 0, "an unchanged target preserves edits: {err}");
    assert_eq!(fs::read(w.garden.join("z")).unwrap(), b"unsown");
    w.server.shutdown().await;
}

#[tokio::test]
async fn ancestor_symlink_and_untracked_collisions_refuse_before_plot_config_moves() {
    let mut w = world(Some("d/f"), ".bed").await;
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("f"), b"valuable").unwrap();
    std::os::unix::fs::symlink(outside.path(), w.garden.join("d")).unwrap();
    let config = fs::read(w.garden.join(".bed/config.toml")).unwrap();
    let (code, _, err) = bed(&w.garden, &["plot", "add", "."]);
    assert_ne!(code, 0);
    assert!(err.contains("ancestor"), "{err}");
<<<<<<< HEAD
    assert!(!w.garden.join("a").exists(), "preflight precedes all writes");
=======
    assert!(
        !w.garden.join("a").exists(),
        "preflight precedes all writes"
    );
>>>>>>> codex/integrity-fixes
    assert_eq!(fs::read(outside.path().join("f")).unwrap(), b"valuable");
    assert_eq!(fs::read(w.garden.join(".bed/config.toml")).unwrap(), config);
    fs::remove_file(w.garden.join("d")).unwrap();
    fs::write(w.garden.join("a"), b"untracked").unwrap();
    let (code, _, err) = bed(&w.garden, &["plot", "add", "."]);
    assert_ne!(code, 0);
    assert!(err.contains("untracked"), "{err}");
    assert_eq!(fs::read(w.garden.join("a")).unwrap(), b"untracked");
    assert_eq!(fs::read(w.garden.join(".bed/config.toml")).unwrap(), config);
    w.server.shutdown().await;
}

#[tokio::test]
async fn repository_cannot_overwrite_custom_bed_state() {
    let mut w = world(Some("state/config.toml"), "state").await;
    let config = fs::read(w.garden.join("state/config.toml")).unwrap();
    let (code, _, err) = bed(&w.garden, &["--bed-dir", "state", "plot", "add", "."]);
    assert_ne!(code, 0);
    assert!(err.contains("active bed state"), "{err}");
<<<<<<< HEAD
    assert_eq!(fs::read(w.garden.join("state/config.toml")).unwrap(), config);
=======
    assert_eq!(
        fs::read(w.garden.join("state/config.toml")).unwrap(),
        config
    );
>>>>>>> codex/integrity-fixes
    let (code, out, err) = bed(&w.garden, &["--bed-dir", "state", "status", "--full-scan"]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("config.toml"), "{out}");
    w.server.shutdown().await;
}
<<<<<<< HEAD
=======

#[tokio::test]
async fn cross_device_state_symlink_supports_regular_files_symlinks_dirty_refusals_and_recovery() {
    let state = match tempfile::tempdir_in("/dev/shm") {
        Ok(state) => state,
        Err(error) => {
            eprintln!("cross-device CLI test unavailable: {error}");
            return;
        }
    };
    let mut w = world_with_external_state(None, ".bed", Some(state.path())).await;
    use std::os::unix::fs::MetadataExt as _;
    if fs::metadata(&w.garden).unwrap().dev() == fs::metadata(state.path()).unwrap().dev() {
        w.server.shutdown().await;
        eprintln!("cross-device CLI test unavailable: devices are identical");
        return;
    }
    let (code, _, err) = bed(&w.garden, &["plot", "add", "."]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(fs::read(w.garden.join("a")).unwrap(), b"base");
    std::os::unix::fs::symlink("a", w.garden.join("link")).unwrap();
    let (code, _, err) = bed(&w.garden, &["sow", "-m", "symlink"]);
    assert_eq!(code, 0, "{err}");
    let saved = gardenbed::bed::Bed::open(&w.garden.join(".bed"))
        .unwrap()
        .config()
        .current_cs
        .clone()
        .unwrap();
    let (code, _, err) = bed(&w.garden, &["goto", &w.target.to_hex()]);
    assert_eq!(code, 0, "{err}");
    assert!(!w.garden.join("link").exists());
    let (code, _, err) = bed(&w.garden, &["goto", &saved]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        fs::read_link(w.garden.join("link")).unwrap(),
        std::path::Path::new("a")
    );
    fs::write(w.garden.join("a"), b"unsown").unwrap();
    let config = fs::read(state.path().join("config.toml")).unwrap();
    let (code, _, err) = bed(&w.garden, &["goto", &w.target.to_hex()]);
    assert_ne!(code, 0);
    assert!(err.contains("local changes"), "{err}");
    assert_eq!(fs::read(w.garden.join("a")).unwrap(), b"unsown");
    assert_eq!(fs::read(state.path().join("config.toml")).unwrap(), config);
    fs::write(w.garden.join("a"), b"base").unwrap();

    // Simulate a partial stage retained by a killed writer. Only the
    // registered device/inode pair is excluded from status and sow.
    let stage = ".bed-checkout-recovery";
    fs::write(w.garden.join(stage), b"partial stage bytes").unwrap();
    let meta = fs::symlink_metadata(w.garden.join(stage)).unwrap();
    let record = format!(
        "version = 1\npath = \"{stage}\"\ndevice = \"{:x}\"\ninode = \"{:x}\"\nsymlink = false\n",
        meta.dev(),
        meta.ino()
    );
    fs::write(state.path().join("checkout-stage-recovery"), record).unwrap();
    fs::write(w.garden.join("normal"), b"real work").unwrap();
    let (code, out, err) = bed(&w.garden, &["status", "--full-scan"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("normal"));
    assert!(!out.contains(stage), "{out}");
    let (code, _, err) = bed(&w.garden, &["sow", "-m", "real work"]);
    assert_eq!(code, 0, "{err}");
    let current = gardenbed::bed::Bed::open(&w.garden.join(".bed"))
        .unwrap()
        .config()
        .current_cs
        .clone()
        .unwrap();
    let repo = Repo::open(&w._root.path().join("server")).unwrap();
    let cs = repo.get_changeset(current.parse().unwrap()).unwrap();
    assert_eq!(
        cs.changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>(),
        ["normal"]
    );
    let (code, _, err) = bed(&w.garden, &["plot", "add", "."]);
    assert_eq!(code, 0, "{err}");
    assert!(!w.garden.join(stage).exists());
    assert!(!state.path().join("checkout-stage-recovery").exists());
    assert_eq!(fs::read(w.garden.join("normal")).unwrap(), b"real work");
    w.server.shutdown().await;
}
>>>>>>> codex/integrity-fixes
