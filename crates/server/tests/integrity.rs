//! Regression coverage for changeset publication and three-way replay.
use std::{collections::BTreeMap, sync::Arc};

use gardenbed_model::{Change, Changeset, Identity, Mode, Op, blob_id};
use gardenbed_server::{Repo, ServerError};
use gardenbed_store::{MemoryStore, MetadataStore};

fn change(path: &str, op: Op, bytes: Option<&[u8]>, mode: Mode, src: Option<&str>) -> Change {
    Change {
        path: path.into(),
        op,
        blob: bytes.map(blob_id),
        mode,
        src_path: src.map(str::to_owned),
    }
}

fn changeset(parents: Vec<gardenbed_model::Id>, changes: Vec<Change>, message: &str) -> Changeset {
    let identity = Identity {
        name: "test".into(),
        email: "test@example.com".into(),
        time_secs: 1,
        tz_min: 0,
    };
    Changeset {
        parents,
        changes,
        message: message.into(),
        author: identity.clone(),
        committer: identity,
        signature: None,
    }
}

fn sow(repo: &Repo, cs: &Changeset) -> gardenbed_model::Id {
    repo.create_changeset(cs, &BTreeMap::new()).unwrap()
}

#[test]
fn invalid_changesets_never_become_visible_or_publishable() {
    let store = Arc::new(MemoryStore::new());
    let repo = Repo::new(store.clone(), store.clone());
    let root = sow(&repo, &changeset(vec![], vec![], "root"));
    repo.harvest(root).unwrap();
    let invalid = changeset(
        vec![root],
        vec![change("absent", Op::Delete, None, Mode::Regular, None)],
        "bad deletion",
    );
    let invalid_id = invalid.id().unwrap();
    for _ in 0..2 {
        assert!(repo.create_changeset(&invalid, &BTreeMap::new()).is_err());
        assert!(store.get_changeset(invalid_id).unwrap().is_none());
        assert!(repo.harvest(invalid_id).is_err());
        assert!(repo.cas_ref("invalid-branch", None, invalid_id).is_err());
        assert_eq!(repo.get_ref("main").unwrap(), Some(root));
        assert_eq!(repo.get_ref("invalid-branch").unwrap(), None);
    }
}

#[test]
fn legacy_invalid_history_cannot_be_published_even_as_a_nonfirst_parent() {
    let store = Arc::new(MemoryStore::new());
    let repo = Repo::new(store.clone(), store.clone());
    let root = sow(&repo, &changeset(vec![], vec![], "root"));
    repo.harvest(root).unwrap();
    // Simulate metadata retained by a server predating validation-before-put.
    let bad = store
        .put_changeset(&changeset(
            vec![root],
            vec![change("missing", Op::Delete, None, Mode::Regular, None)],
            "legacy invalid",
        ))
        .unwrap();
    let merge = changeset(vec![root, bad], vec![], "merge invalid history");
    let merge_id = store.put_changeset(&merge).unwrap();
    // Its own first-parent tree is valid, so checking only the head misses it.
    assert!(repo.root_tree(merge_id).is_ok());
    for id in [bad, merge_id] {
        assert!(repo.harvest(id).is_err());
        assert!(repo.cas_ref("invalid-branch", None, id).is_err());
    }
    assert!(repo.create_changeset(&merge, &BTreeMap::new()).is_err());
    assert_eq!(repo.get_ref("main").unwrap(), Some(root));
    assert_eq!(repo.get_ref("invalid-branch").unwrap(), None);
}

#[test]
fn graft_preserves_concurrent_edits_modes_deletions_and_rename_destinations() {
    let store = Arc::new(MemoryStore::new());
    let repo = Repo::new(store.clone(), store);
    for content in [b"old".as_slice(), b"new", b"concurrent", b"destination"] {
        repo.put_blob(content).unwrap();
    }
    let base = sow(
        &repo,
        &changeset(
            vec![],
            vec![
                change("dest", Op::Add, Some(b"destination"), Mode::Regular, None),
                change("f", Op::Add, Some(b"old"), Mode::Regular, None),
            ],
            "base",
        ),
    );
    let delete = sow(
        &repo,
        &changeset(
            vec![base],
            vec![change("f", Op::Delete, None, Mode::Regular, None)],
            "delete",
        ),
    );
    let modify = sow(
        &repo,
        &changeset(
            vec![base],
            vec![change("f", Op::Modify, Some(b"new"), Mode::Regular, None)],
            "modify",
        ),
    );
    let rename = sow(
        &repo,
        &changeset(
            vec![base],
            vec![change(
                "dest",
                Op::Rename,
                Some(b"old"),
                Mode::Regular,
                Some("f"),
            )],
            "rename",
        ),
    );
    let edit = sow(
        &repo,
        &changeset(
            vec![base],
            vec![change(
                "f",
                Op::Modify,
                Some(b"concurrent"),
                Mode::Regular,
                None,
            )],
            "concurrent content",
        ),
    );
    let mode = sow(
        &repo,
        &changeset(
            vec![base],
            vec![change("f", Op::Modify, Some(b"old"), Mode::Exec, None)],
            "concurrent mode",
        ),
    );
    let dest_edit = sow(
        &repo,
        &changeset(
            vec![base],
            vec![change(
                "dest",
                Op::Modify,
                Some(b"concurrent"),
                Mode::Regular,
                None,
            )],
            "concurrent destination",
        ),
    );
    for (source, target) in [
        (delete, edit),
        (delete, mode),
        (rename, edit),
        (rename, mode),
        (modify, delete),
        (rename, dest_edit),
    ] {
        let replay = changeset(
            vec![target],
            repo.get_changeset(source).unwrap().changes,
            &repo.get_changeset(source).unwrap().message,
        );
        assert!(matches!(
            repo.graft(source, target),
            Err(ServerError::GraftConflict(_))
        ));
        assert!(repo.get_changeset(replay.id().unwrap()).is_err());
    }
    // An untouched source and an unchanged, distinct destination can be renamed.
    assert!(repo.graft(rename, base).is_ok());
    assert!(repo.graft(delete, base).is_ok());
}

#[test]
fn graft_directory_replacement_checks_all_swallowed_paths() {
    let store = Arc::new(MemoryStore::new());
    let repo = Repo::new(store.clone(), store);
    repo.put_blob(b"old").unwrap();
    repo.put_blob(b"new").unwrap();
    let base = sow(
        &repo,
        &changeset(
            vec![],
            vec![change("dir/f", Op::Add, Some(b"old"), Mode::Regular, None)],
            "base",
        ),
    );
    let replacement = sow(
        &repo,
        &changeset(
            vec![base],
            vec![
                change("dir", Op::Add, Some(b"new"), Mode::Regular, None),
                change("dir/f", Op::Delete, None, Mode::Regular, None),
            ],
            "replace directory",
        ),
    );
    // Explicit deletion is swallowed by the earlier write during tree apply.
    assert!(repo.graft(replacement, base).is_ok());
    for changes in [
        vec![change(
            "dir/f",
            Op::Modify,
            Some(b"new"),
            Mode::Regular,
            None,
        )],
        vec![change("dir/g", Op::Add, Some(b"new"), Mode::Regular, None)],
        vec![change("dir/f", Op::Delete, None, Mode::Regular, None)],
    ] {
        let target = sow(
            &repo,
            &changeset(vec![base], changes, "concurrent subtree change"),
        );
        assert!(matches!(
            repo.graft(replacement, target),
            Err(ServerError::GraftConflict(_))
        ));
    }
}
