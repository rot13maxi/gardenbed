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

fn commit(repo: &Repo, cs: &Changeset) -> gardenbed_model::Id {
    repo.create_changeset(cs, &BTreeMap::new()).unwrap()
}

#[test]
fn invalid_changesets_never_become_visible_or_publishable() {
    let store = Arc::new(MemoryStore::new());
    let repo = Repo::new(store.clone(), store.clone());
    let root = commit(&repo, &changeset(vec![], vec![], "root"));
    repo.publish(root).unwrap();
    let invalid = changeset(
        vec![root],
        vec![change("absent", Op::Delete, None, Mode::Regular, None)],
        "bad deletion",
    );
    let invalid_id = invalid.id().unwrap();
    for _ in 0..2 {
        assert!(repo.create_changeset(&invalid, &BTreeMap::new()).is_err());
        assert!(store.get_changeset(invalid_id).unwrap().is_none());
        assert!(repo.publish(invalid_id).is_err());
        assert!(repo.cas_ref("invalid-branch", None, invalid_id).is_err());
        assert_eq!(repo.get_ref("main").unwrap(), Some(root));
        assert_eq!(repo.get_ref("invalid-branch").unwrap(), None);
    }
}

#[test]
fn legacy_invalid_history_cannot_be_published_even_as_a_nonfirst_parent() {
    let store = Arc::new(MemoryStore::new());
    let repo = Repo::new(store.clone(), store.clone());
    let root = commit(&repo, &changeset(vec![], vec![], "root"));
    repo.publish(root).unwrap();
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
        assert!(repo.publish(id).is_err());
        assert!(repo.cas_ref("invalid-branch", None, id).is_err());
    }
    assert!(repo.create_changeset(&merge, &BTreeMap::new()).is_err());
    assert_eq!(repo.get_ref("main").unwrap(), Some(root));
    assert_eq!(repo.get_ref("invalid-branch").unwrap(), None);
}

#[test]
fn rebase_preserves_concurrent_edits_modes_deletions_and_rename_destinations() {
    let store = Arc::new(MemoryStore::new());
    let repo = Repo::new(store.clone(), store);
    for content in [b"old".as_slice(), b"new", b"concurrent", b"destination"] {
        repo.put_blob(content).unwrap();
    }
    let base = commit(
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
    let delete = commit(
        &repo,
        &changeset(
            vec![base],
            vec![change("f", Op::Delete, None, Mode::Regular, None)],
            "delete",
        ),
    );
    let modify = commit(
        &repo,
        &changeset(
            vec![base],
            vec![change("f", Op::Modify, Some(b"new"), Mode::Regular, None)],
            "modify",
        ),
    );
    let rename = commit(
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
    let edit = commit(
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
    let mode = commit(
        &repo,
        &changeset(
            vec![base],
            vec![change("f", Op::Modify, Some(b"old"), Mode::Exec, None)],
            "concurrent mode",
        ),
    );
    let dest_edit = commit(
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
            repo.rebase(source, target),
            Err(ServerError::RebaseConflict(_))
        ));
        assert!(repo.get_changeset(replay.id().unwrap()).is_err());
    }
    // An untouched source and an unchanged, distinct destination can be renamed.
    assert!(repo.rebase(rename, base).is_ok());
    assert!(repo.rebase(delete, base).is_ok());
}

#[test]
fn rebase_directory_replacement_checks_all_swallowed_paths() {
    let store = Arc::new(MemoryStore::new());
    let repo = Repo::new(store.clone(), store);
    repo.put_blob(b"old").unwrap();
    repo.put_blob(b"new").unwrap();
    let base = commit(
        &repo,
        &changeset(
            vec![],
            vec![change("dir/f", Op::Add, Some(b"old"), Mode::Regular, None)],
            "base",
        ),
    );
    let replacement = commit(
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
    assert!(repo.rebase(replacement, base).is_ok());
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
        let target = commit(
            &repo,
            &changeset(vec![base], changes, "concurrent subtree change"),
        );
        assert!(matches!(
            repo.rebase(replacement, target),
            Err(ServerError::RebaseConflict(_))
        ));
    }
}

/// Count actual history reads through a newly opened backend, independently
/// of the engine's process caches and elapsed-time noise.
struct CountingMetadata {
    inner: Arc<dyn MetadataStore>,
    reads: std::sync::atomic::AtomicUsize,
    certificate_reads: std::sync::atomic::AtomicUsize,
    certificates_written: std::sync::atomic::AtomicUsize,
}
impl CountingMetadata {
    fn new(inner: Arc<dyn MetadataStore>) -> Self {
        Self {
            inner,
            reads: 0.into(),
            certificate_reads: 0.into(),
            certificates_written: 0.into(),
        }
    }
}
impl MetadataStore for CountingMetadata {
    fn put_changeset(
        &self,
        cs: &Changeset,
    ) -> Result<gardenbed_model::Id, gardenbed_store::StoreError> {
        self.inner.put_changeset(cs)
    }
    fn get_changeset(
        &self,
        id: gardenbed_model::Id,
    ) -> Result<Option<(Changeset, Vec<u8>)>, gardenbed_store::StoreError> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.get_changeset(id)
    }
    fn resolve_ref(
        &self,
        name: &str,
    ) -> Result<Option<gardenbed_model::Id>, gardenbed_store::StoreError> {
        self.inner.resolve_ref(name)
    }
    fn list_refs(&self) -> Result<Vec<(String, gardenbed_model::Id)>, gardenbed_store::StoreError> {
        self.inner.list_refs()
    }
    fn cas_ref(
        &self,
        name: &str,
        expected: Option<gardenbed_model::Id>,
        new: gardenbed_model::Id,
    ) -> Result<gardenbed_store::RefCas, gardenbed_store::StoreError> {
        self.inner.cas_ref(name, expected, new)
    }
    fn record_git_mapping(
        &self,
        sha: &str,
        cs: gardenbed_model::Id,
    ) -> Result<(), gardenbed_store::StoreError> {
        self.inner.record_git_mapping(sha, cs)
    }
    fn cs_for_git(
        &self,
        sha: &str,
    ) -> Result<Option<gardenbed_model::Id>, gardenbed_store::StoreError> {
        self.inner.cs_for_git(sha)
    }
    fn git_for_cs(
        &self,
        id: gardenbed_model::Id,
    ) -> Result<Option<String>, gardenbed_store::StoreError> {
        self.inner.git_for_cs(id)
    }
    fn derived_put(
        &self,
        kind: &str,
        cs: gardenbed_model::Id,
        bytes: &[u8],
    ) -> Result<(), gardenbed_store::StoreError> {
        self.inner.derived_put(kind, cs, bytes)
    }
    fn derived_get(
        &self,
        kind: &str,
        cs: gardenbed_model::Id,
    ) -> Result<Option<Vec<u8>>, gardenbed_store::StoreError> {
        self.inner.derived_get(kind, cs)
    }
    fn validation_tree(
        &self,
        cs: gardenbed_model::Id,
    ) -> Result<Option<Option<gardenbed_model::Id>>, gardenbed_store::StoreError> {
        self.certificate_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.validation_tree(cs)
    }
    fn put_validated_changeset(
        &self,
        cs: &Changeset,
        root: Option<gardenbed_model::Id>,
        rows: &[gardenbed_store::DerivedRow],
        git: Option<&str>,
    ) -> Result<gardenbed_model::Id, gardenbed_store::StoreError> {
        let id = self.inner.put_validated_changeset(cs, root, rows, git)?;
        self.certificates_written
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(id)
    }
}

#[test]
fn restart_publication_uses_durable_frontier_after_native_writes_and_legacy_migration() {
    use gardenbed_store::{FsObjectStore, SqliteMetadataStore};
    use std::sync::atomic::Ordering::Relaxed;
    const COMMITS: usize = 10_000;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("metadata.sqlite3");
    let head = {
        let repo = Repo::open(dir.path()).unwrap();
        let mut head = None;
        for index in 0..COMMITS {
            head = Some(commit(
                &repo,
                &changeset(head.into_iter().collect(), vec![], &format!("seed {index}")),
            ));
        }
        head.unwrap()
    };
    let reopen = || {
        let metadata = Arc::new(CountingMetadata::new(Arc::new(
            SqliteMetadataStore::open(&db).unwrap(),
        )));
        let repo = Repo::new(
            Arc::new(FsObjectStore::open(dir.path()).unwrap()),
            metadata.clone(),
        );
        (repo, metadata)
    };
    for index in 0..3 {
        let (repo, metadata) = reopen();
        repo.cas_ref(&format!("publication-{index}"), None, head)
            .unwrap();
        assert_eq!(
            metadata.reads.load(Relaxed),
            0,
            "restart must not load certified ancestors"
        );
        assert_eq!(metadata.certificate_reads.load(Relaxed), 1);
        commit(
            &repo,
            &changeset(vec![head], vec![], &format!("child {index}")),
        );
        assert!(
            metadata.reads.load(Relaxed) < 16,
            "new commit must not scan old history"
        );
        assert!(metadata.certificate_reads.load(Relaxed) < 16);
    }
    // An old database has the same immutable commits/derived caches but no
    // certificates. Its first publication validates once and persists progress.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM validated_changesets_v1", [])
        .unwrap();
    {
        let (repo, metadata) = reopen();
        repo.cas_ref("legacy-migration", None, head).unwrap();
        assert_eq!(metadata.certificates_written.load(Relaxed), COMMITS);
        assert_eq!(metadata.reads.load(Relaxed), 2 * COMMITS);
    }
    let (repo, metadata) = reopen();
    repo.cas_ref("after-migration", None, head).unwrap();
    assert_eq!(metadata.reads.load(Relaxed), 0);
    assert_eq!(metadata.certificate_reads.load(Relaxed), 1);
}

#[test]
fn mutable_tree_cache_cannot_certify_invalid_legacy_changesets_or_override_certified_roots() {
    let store = Arc::new(MemoryStore::new());
    let repo = Repo::new(store.clone(), store.clone());
    let root = commit(&repo, &changeset(vec![], vec![], "root"));
    let invalid = changeset(
        vec![root],
        vec![change("absent", Op::Delete, None, Mode::Regular, None)],
        "invalid",
    );
    let id = store.put_changeset(&invalid).unwrap();
    store.derived_put("tree", id, &[]).unwrap();
    assert!(
        repo.root_tree(id).is_ok(),
        "legacy generic cache intentionally claims a valid empty tree"
    );
    assert!(repo.cas_ref("forged-cache", None, id).is_err());
    assert_eq!(store.validation_tree(id).unwrap(), None);
    assert_eq!(repo.get_ref("forged-cache").unwrap(), None);

    repo.put_blob(b"content").unwrap();
    let with_file = commit(
        &repo,
        &changeset(
            vec![root],
            vec![change(
                "file",
                Op::Add,
                Some(b"content"),
                Mode::Regular,
                None,
            )],
            "file",
        ),
    );
    let correct = repo.root_tree(with_file).unwrap();
    assert!(correct.is_some());
    store.derived_put("tree", with_file, &[]).unwrap();
    // Reopen the service so no process-local cache can hide the overwritten row.
    let reopened = Repo::new(store.clone(), store);
    assert_eq!(reopened.root_tree(with_file).unwrap(), correct);
    reopened.cas_ref("certified-root", None, with_file).unwrap();
}

#[test]
fn legacy_certification_rejects_a_changeset_under_the_wrong_content_address() {
    use gardenbed_store::SqliteMetadataStore;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("metadata.sqlite3");
    let store = SqliteMetadataStore::open(&db).unwrap();
    let valid = changeset(vec![], vec![], "valid bytes under wrong ID");
    let real = store.put_changeset(&valid).unwrap();
    let wrong = gardenbed_model::Id::from_bytes([19; 32]);
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE changesets SET id = ?1 WHERE id = ?2",
            rusqlite::params![wrong.as_bytes(), real.as_bytes()],
        )
        .unwrap();
    drop(store);
    let repo = Repo::open(dir.path()).unwrap();
    assert!(repo.cas_ref("wrong-address", None, wrong).is_err());
    assert_eq!(repo.get_ref("wrong-address").unwrap(), None);
    let store = SqliteMetadataStore::open(&db).unwrap();
    assert_eq!(store.validation_tree(wrong).unwrap(), None);
    assert_eq!(store.validation_tree(real).unwrap(), None);
}

#[test]
fn eager_rebuild_restores_certificate_backed_derived_tree_rows() {
    let store = Arc::new(MemoryStore::new());
    let repo = Repo::new(store.clone(), store.clone());
    repo.put_blob(b"content").unwrap();
    let root = commit(
        &repo,
        &changeset(
            vec![],
            vec![change(
                "file",
                Op::Add,
                Some(b"content"),
                Mode::Regular,
                None,
            )],
            "root",
        ),
    );
    let child = commit(&repo, &changeset(vec![root], vec![], "child"));
    let kinds = ["tree", "skiplist", "pathsummary"];
    let before: Vec<_> = [root, child]
        .into_iter()
        .flat_map(|id| kinds.map(|kind| (id, kind, store.derived_get(kind, id).unwrap().unwrap())))
        .collect();
    for (id, kind, _) in &before {
        store.derived_put(kind, *id, &[]).unwrap();
    }
    let engine = gardenbed_derive::Engine::new(store.clone(), store.clone());
    for id in [root, child] {
        engine.on_changeset_created(id).unwrap();
    }
    for (id, kind, bytes) in before {
        assert_eq!(store.derived_get(kind, id).unwrap(), Some(bytes));
    }
}

#[test]
fn obsolete_layout_is_rejected_without_creating_an_empty_repository() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("seedbed.sqlite3"), b"old metadata").unwrap();
    let error = Repo::open(dir.path()).unwrap_err();
    assert!(error.to_string().contains("obsolete storage layout"));
    assert!(!dir.path().join("metadata.sqlite3").exists());
    assert!(!dir.path().join("objects").exists());
    assert_eq!(
        std::fs::read(dir.path().join("seedbed.sqlite3")).unwrap(),
        b"old metadata"
    );
}
