//! Roundtrip verification for native merges (v0.2.md M10): changesets
//! created by a native two-parent merge export to a real git repository
//! (`fsck --strict` clean, parent structure verbatim) and re-import into
//! fresh stores reproduces the identical changeset ids.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_git::{ExportOptions, ImportOptions, export_to_path, import_from_path};
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;
use gardenbed_store::{MemoryStore, MetadataStore, ObjectStore};

fn ident(name: &str, secs: u64) -> Identity {
    Identity {
        name: name.into(),
        email: format!("{name}@garden.example"),
        time_secs: 1_700_000_000 + secs,
        tz_min: 0,
    }
}

fn add(path: &str, content: &[u8]) -> Change {
    Change {
        path: path.into(),
        op: Op::Add,
        blob: Some(blob_id(content)),
        src_path: None,
        mode: Mode::Regular,
    }
}

fn modify(path: &str, content: &[u8]) -> Change {
    Change {
        path: path.into(),
        op: Op::Modify,
        blob: Some(blob_id(content)),
        src_path: None,
        mode: Mode::Regular,
    }
}

fn inline(items: &[&[u8]]) -> BTreeMap<String, Vec<u8>> {
    items
        .iter()
        .map(|b| (blob_id(b).to_hex(), b.to_vec()))
        .collect()
}

/// The export-facing stores plus the ids of interest.
struct Fixture {
    objects: Arc<dyn ObjectStore>,
    metadata: Arc<dyn MetadataStore>,
    root: Id,
    ours: Id,
    theirs: Id,
    merged: Id,
}

/// Builds the fixture: root → (ours, theirs) → one native merge commit,
/// all in MemoryStores, with `refs/heads/main` at the merge.
fn fixture() -> Fixture {
    let objects = Arc::new(MemoryStore::new()) as Arc<dyn ObjectStore>;
    let metadata = Arc::new(MemoryStore::new()) as Arc<dyn MetadataStore>;
    let repo = Repo::new(objects.clone(), metadata.clone());

    let base_text: Vec<u8> = (1..=12)
        .flat_map(|i| format!("base line {i}\n").into_bytes())
        .collect();
    let root = repo
        .create_changeset(
            &Changeset {
                parents: vec![],
                changes: vec![add("doc.txt", &base_text)],
                message: "root".into(),
                author: ident("amy", 0),
                committer: ident("amy", 0),
                signature: None,
            },
            &inline(&[&base_text]),
        )
        .expect("root");
    let mut ours_text = base_text.clone();
    ours_text.splice(0.."base line 1\n".len(), b"ours top\n".iter().copied());
    let ours = repo
        .create_changeset(
            &Changeset {
                parents: vec![root],
                changes: vec![
                    modify("doc.txt", &ours_text),
                    add("ours.txt", b"ours file\n"),
                ],
                message: "ours".into(),
                author: ident("amy", 1),
                committer: ident("amy", 1),
                signature: None,
            },
            &inline(&[&ours_text, b"ours file\n"]),
        )
        .expect("ours");
    let mut theirs_text = base_text.clone();
    let cut = theirs_text.len() - b"base line 12\n".len();
    theirs_text.splice(cut.., b"theirs bottom\n".iter().copied());
    let theirs = repo
        .create_changeset(
            &Changeset {
                parents: vec![root],
                changes: vec![
                    modify("doc.txt", &theirs_text),
                    add("theirs.txt", b"theirs file\n"),
                ],
                message: "theirs".into(),
                author: ident("bob", 2),
                committer: ident("bob", 2),
                signature: None,
            },
            &inline(&[&theirs_text, b"theirs file\n"]),
        )
        .expect("theirs");
    let (merged, up_to_date) = repo
        .merge(
            ours,
            theirs,
            "merge theirs into ours",
            &ident("amy", 3),
            &ident("amy", 3),
        )
        .expect("clean merge");
    assert!(!up_to_date);

    metadata
        .cas_ref("refs/heads/main", None, merged)
        .expect("ref created");
    Fixture {
        objects,
        metadata,
        root,
        ours,
        theirs,
        merged,
    }
}

#[test]
fn native_merge_roundtrips_through_git() {
    let f = fixture();

    // ---------------------------------------------------------- export
    let git_dir = tempfile::tempdir().expect("git tempdir");
    let git_path = git_dir.path().join("repo.git");
    let report = export_to_path(
        &git_path,
        f.objects.clone(),
        f.metadata.clone(),
        &ExportOptions {
            refs: vec![],
            record_mappings: false,
        },
    )
    .expect("export runs");

    // fsck --strict clean.
    git_out(git_path.as_path(), &["fsck", "--strict"]);

    // Parent structure verbatim: the exported merge commit has exactly
    // [ours, theirs] as parents, in order.
    let (_, merge_sha) = report
        .refs
        .iter()
        .find(|(name, _)| name == "refs/heads/main")
        .expect("main exported")
        .clone();
    let parents = git_parents(git_path.as_path(), &merge_sha);
    assert_eq!(parents.len(), 2, "merge commit keeps two parents");
    let ours_sha = report
        .commits
        .iter()
        .find(|(id, _)| *id == f.ours)
        .map(|(_, sha)| sha.clone())
        .expect("ours in commits mapping");
    let theirs_sha = report
        .commits
        .iter()
        .find(|(id, _)| *id == f.theirs)
        .map(|(_, sha)| sha.clone())
        .expect("theirs in commits mapping");
    assert_eq!(
        parents,
        vec![ours_sha, theirs_sha],
        "parent order [ours, theirs]"
    );
    // And the merge changeset exported to the sha the ref points at.
    assert!(
        report
            .commits
            .iter()
            .any(|(id, sha)| *id == f.merged && sha == &merge_sha)
    );

    // ---------------------------------------------------------- import
    let objects2 = Arc::new(MemoryStore::new()) as Arc<dyn ObjectStore>;
    let metadata2 = Arc::new(MemoryStore::new()) as Arc<dyn MetadataStore>;
    let engine2 = Engine::new(objects2.clone(), metadata2.clone());
    let report2 = import_from_path(
        git_path.as_path(),
        objects2.clone(),
        metadata2.clone(),
        engine2,
        &ImportOptions::default(),
    )
    .expect("re-import runs");
    assert!(report2.commits_new >= 4, "whole graph re-imported");

    // Identical changeset ids: every exported changeset re-imports to
    // its original id (the git-SHA mapping is the join; ids are
    // content-addressed).
    for id in [f.root, f.ours, f.theirs, f.merged] {
        let reimported = metadata2
            .get_changeset(id)
            .expect("re-imported store readable")
            .unwrap_or_else(|| panic!("changeset {id} re-imported"));
        let original = f
            .metadata
            .get_changeset(id)
            .expect("original readable")
            .expect("present");
        assert_eq!(
            reimported.0.parents, original.0.parents,
            "parents preserved for {id}"
        );
        assert_eq!(
            reimported.0.changes, original.0.changes,
            "changes preserved for {id}"
        );
        assert_eq!(reimported.0.message, original.0.message, "message for {id}");
        assert_eq!(reimported.1, original.1, "canonical bytes for {id}");
    }
}

/// Scrubbed-env git subprocess (crate test convention).
fn git_out(repo: &Path, args: &[&str]) -> String {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(repo);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    let out = cmd.args(args).output().expect("git runs");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

/// `git rev-list --parents -n1 <sha>` → parent SHAs.
fn git_parents(repo: &Path, sha: &str) -> Vec<String> {
    git_out(repo, &["rev-list", "--parents", "-n1", sha])
        .split_whitespace()
        .skip(1)
        .map(str::to_owned)
        .collect()
}
