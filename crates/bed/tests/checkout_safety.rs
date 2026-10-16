//! User-facing materialization refusals must preserve files and bed state.
mod common;

use std::{collections::BTreeMap, fs, path::PathBuf};

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;
use common::{Arboretum, REPO, bed};

struct World {
    _root: tempfile::TempDir,
    garden: PathBuf,
    server: Arboretum,
    target: Id,
}

fn create(repo: &Repo, parents: Vec<Id>, files: &[(&str, Op, &[u8])]) -> Id {
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
    assert!(!w.garden.join("a").exists(), "preflight precedes all writes");
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
    assert_eq!(fs::read(w.garden.join("state/config.toml")).unwrap(), config);
    let (code, out, err) = bed(&w.garden, &["--bed-dir", "state", "status", "--full-scan"]);
    assert_eq!(code, 0, "{err}");
    assert!(!out.contains("config.toml"), "{out}");
    w.server.shutdown().await;
}
