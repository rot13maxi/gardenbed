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
    repo.publish(root, None, false).unwrap();
    let invalid = changeset(
        vec![root],
        vec![change("absent", Op::Delete, None, Mode::Regular, None)],
        "bad deletion",
    );
    let invalid_id = invalid.id().unwrap();
    for _ in 0..2 {
        assert!(repo.create_changeset(&invalid, &BTreeMap::new()).is_err());
        assert!(store.get_changeset(invalid_id).unwrap().is_none());
        assert!(repo.publish(invalid_id, None, false).is_err());
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
    repo.publish(root, None, false).unwrap();
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
        assert!(repo.publish(id, None, false).is_err());
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
    fn recent_changesets(
        &self,
        limit: u32,
    ) -> Result<Vec<gardenbed_model::Id>, gardenbed_store::StoreError> {
        // Not a per-changeset history read: the counter assertions are
        // about get_changeset traffic, so this stays uncounted.
        self.inner.recent_changesets(limit)
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
        raw: Option<&[u8]>,
        _verify: Option<&dyn Fn() -> Result<(), gardenbed_store::StoreError>>,
    ) -> Result<gardenbed_model::Id, gardenbed_store::StoreError> {
        let id = self
            .inner
            .put_validated_changeset(cs, root, rows, git, raw, None)?;
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
fn pre_rename_layout_opens_in_place() {
    // `seedbed.sqlite3` + `loam/objects` is the same store format under the
    // historical directory names: it must open, not be refused.
    use gardenbed_store::{FsObjectStore, SqliteMetadataStore};

    let dir = tempfile::tempdir().unwrap();
    let loam = dir.path().join("loam");
    std::fs::create_dir_all(loam.join("objects")).unwrap();
    let seedbed = dir.path().join("seedbed.sqlite3");
    // Create a real (empty) store at the historical paths, then write one
    // changeset into it through the ordinary stores.
    let objects = std::sync::Arc::new(FsObjectStore::open(&loam).unwrap());
    let metadata = std::sync::Arc::new(SqliteMetadataStore::open(&seedbed).unwrap());
    let repo = Repo::new(objects, metadata);
    let head = commit(&repo, &changeset(vec![], vec![], "pre-rename seed"));
    repo.cas_ref("main", None, head).unwrap();
    drop(repo);

    // Reopening by root must find the same repository.
    let reopened = Repo::open(dir.path()).expect("pre-rename layout opens");
    assert_eq!(
        reopened.get_ref("main").unwrap(),
        Some(head),
        "the seed written under the old paths must be visible"
    );
    // Opening must not migrate files or create a second, empty store.
    assert!(!dir.path().join("metadata.sqlite3").exists());
    assert!(!dir.path().join("objects").exists());
    assert!(seedbed.exists());
}

#[test]
fn a_root_with_foreign_content_is_refused_instead_of_silently_empty() {
    // A wrong `--root` must not produce an empty repository beside the
    // user's data: that reads as "all history vanished".
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Makefile"), b"not a repository").unwrap();
    let error = Repo::open(dir.path()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("refusing to create an empty repository"),
        "unexpected error: {error}"
    );
    assert!(!dir.path().join("metadata.sqlite3").exists());
    assert!(!dir.path().join("objects").exists());
    assert_eq!(
        std::fs::read(dir.path().join("Makefile")).unwrap(),
        b"not a repository"
    );
}

#[test]
fn an_empty_root_still_initializes_as_the_current_layout() {
    let dir = tempfile::tempdir().unwrap();
    let repo = Repo::open(dir.path()).unwrap();
    assert!(dir.path().join("metadata.sqlite3").exists());
    assert!(dir.path().join("objects").is_dir());
    assert!(repo.get_ref("main").unwrap().is_none());
}

// ------------------------------------------------- pre-rename layout (upstream)
//
// The pre-rename layout (`seedbed.sqlite3` + `loam/objects`) is the same store
// under historical names. `Repo::open` resolves it; every OTHER entry point
// must resolve it the same way, or it reads (and creates) an empty current
// layout store beside the real one. These tests pin the entry points that had
// drifted: export/import, gc's mark and sweep walk, and `move`.

/// Builds a seeded pre-rename store under `root` and returns it.
fn pre_rename_store(root: &std::path::Path) {
    let loam = root.join("loam");
    std::fs::create_dir_all(loam.join("objects")).unwrap();
    // Create the pre-rename database first: the FILE is the discriminator
    // (a bare `loam/objects` is an ordinary repository store, not a layout).
    drop(gardenbed_store::SqliteMetadataStore::open(&root.join("seedbed.sqlite3")).unwrap());
    // `Repo::open` resolves the pre-rename layout and writes real objects
    // (a bare `Repo::new` over fresh stores cannot resolve blob content).
    let repo = Repo::open(root).expect("pre-rename root opens");
    // `commit` passes no inline blobs, so the content must be in the store.
    repo.put_blob(b"pre-rename").unwrap();
    let head = commit(
        &repo,
        &changeset(
            vec![],
            vec![change(
                "f",
                Op::Add,
                Some(b"pre-rename"),
                Mode::Regular,
                None,
            )],
            "pre-rename seed",
        ),
    );
    repo.cas_ref("main", None, head).unwrap();
    // The seed must have gone to the historical paths, not a new store.
    assert!(root.join("seedbed.sqlite3").is_file());
    assert!(!root.join("metadata.sqlite3").exists());
}

#[test]
fn exported_pre_rename_root_is_not_read_as_an_empty_repository() {
    // `gardenbed-server export` used to hardcode `<dir>/metadata.sqlite3` and
    // `<dir>/objects`, so a pre-rename root exported zero commits and left a
    // shadow current-layout store behind.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("server");
    let repo_root = root.join("default");
    std::fs::create_dir_all(&repo_root).unwrap();
    pre_rename_store(&repo_root);

    let into = dir.path().join("out.git");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
        .args(["export", "--root"])
        .arg(&root)
        .args(["--repo", "default", "--into"])
        .arg(&into)
        .output()
        .expect("export runs");
    assert!(
        out.status.success(),
        "export failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.starts_with("0 commits"),
        "the pre-rename store must export its history, got: {stdout}"
    );
    // And it must not shadow the real store with an empty one.
    assert!(
        !repo_root.join("metadata.sqlite3").exists(),
        "export created a shadow current-layout database beside seedbed.sqlite3"
    );
    assert!(
        !repo_root.join("objects").exists(),
        "export created a shadow current-layout object store beside loam/objects"
    );
}

#[test]
fn gc_mark_and_sweep_walk_the_pre_rename_object_store() {
    // The mark used to read `<root>/metadata.sqlite3` and the sweep walked
    // `<root>/objects`, so a pre-rename store reported 0 scanned / 0 live.
    let dir = tempfile::tempdir().unwrap();
    let repo_root = dir.path().join("repo");
    std::fs::create_dir_all(&repo_root).unwrap();
    pre_rename_store(&repo_root);
    let on_disk = walk_count(&repo_root.join("loam").join("objects"));
    assert!(on_disk > 0, "the fixture has object files");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
        .args(["gc", "--root"])
        .arg(&repo_root)
        .args(["--dry-run"])
        .output()
        .expect("gc runs");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let scanned: u64 = text
        .split("object(s) scanned")
        .next()
        .and_then(|s| s.trim().rsplit(' ').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert_eq!(
        scanned, on_disk,
        "gc must scan the pre-rename object store (report: {text})"
    );
    assert!(
        text.contains(&format!("{on_disk} live")),
        "every object must be live in the pre-rename mark (report: {text})"
    );
}

/// Number of object files under an `objects/` root.
fn walk_count(objects: &std::path::Path) -> u64 {
    let mut n = 0u64;
    let mut stack = vec![objects.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|f| f != "pack") {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn moving_a_pre_rename_root_moves_the_real_store() {
    // `move` renamed only the current-layout names, so a pre-rename root
    // produced an empty destination while printing "history byte-identical".
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("server");
    std::fs::create_dir_all(&root).unwrap();
    pre_rename_store(&root);

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
        .args(["move", "--root"])
        .arg(&root)
        .args(["--from", "default", "--into", "moved"])
        .output()
        .expect("move runs");
    assert!(
        out.status.success(),
        "move failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let moved = root.join("moved");
    assert!(
        moved.join("seedbed.sqlite3").is_file() || moved.join("metadata.sqlite3").is_file(),
        "the moved repository has no metadata store"
    );
    assert!(
        walk_count(&moved.join("loam").join("objects")) > 0
            || walk_count(&moved.join("objects")) > 0,
        "the moved repository has no objects"
    );
    // The moved repository must open and keep its history.
    let reopened = Repo::open(&moved).expect("moved repository opens");
    assert!(
        reopened.get_ref("main").unwrap().is_some(),
        "history must survive the move"
    );
}

#[test]
fn a_repository_named_loam_does_not_masquerade_as_the_pre_rename_layout() {
    // `loam/objects` is a normal repository store, not proof of the
    // pre-rename layout: keying on it made `default` resolve to the
    // multi-repo root and hid every sibling from the served list.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("server");
    for name in ["alpha", "loam"] {
        let repo_root = root.join(name);
        std::fs::create_dir_all(&repo_root).unwrap();
        let repo = Repo::open(&repo_root).expect("repository opens");
        repo.put_blob(b"x").unwrap();
        let head = commit(
            &repo,
            &changeset(
                vec![],
                vec![change("f", Op::Add, Some(b"x"), Mode::Regular, None)],
                "seed",
            ),
        );
        repo.cas_ref("main", None, head).unwrap();
    }
    let set = gardenbed_server::repos::RepoSet::new(&root, 8);
    let served = set.served_names();
    assert_eq!(
        served,
        vec!["alpha".to_owned(), "loam".to_owned()],
        "both repositories must be served (a repo named loam is not the layout)"
    );
    // `default` is not a repository here and must not resolve to the root.
    assert!(
        set.existing_repo_dir_for("default").is_err(),
        "default must not resolve to a multi-repo root"
    );
}

#[test]
fn exporting_an_unknown_repository_is_refused_not_created() {
    // A mistyped `--repo` used to initialize an empty repository and export
    // 0 commits with exit 0.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("server");
    let repo_root = root.join("alpha");
    std::fs::create_dir_all(&repo_root).unwrap();
    let repo = Repo::open(&repo_root).expect("repository opens");
    repo.put_blob(b"x").unwrap();
    let head = commit(
        &repo,
        &changeset(
            vec![],
            vec![change("f", Op::Add, Some(b"x"), Mode::Regular, None)],
            "seed",
        ),
    );
    repo.cas_ref("main", None, head).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
        .args(["export", "--root"])
        .arg(&root)
        .args(["--repo", "nope", "--into"])
        .arg(dir.path().join("out.git"))
        .output()
        .expect("export runs");
    assert!(
        !out.status.success(),
        "an unknown repository must be refused"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not found") && stderr.contains("alpha"),
        "the refusal names the served repositories: {stderr}"
    );
    assert!(
        !root.join("nope").exists(),
        "export must not create the unknown repository"
    );
}

#[test]
fn a_bare_loam_objects_root_is_refused_not_shadowed() {
    // A root holding only `loam/objects` is not the pre-rename layout (the
    // database file is the discriminator). It has foreign content, so opening
    // it must refuse rather than create an empty current-layout store beside
    // it — the same shadowing class as `resolve_layout`'s loam clause.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("loam").join("objects")).unwrap();
    let error = Repo::open(dir.path()).expect_err("a bare loam/objects root must be refused");
    assert!(
        error
            .to_string()
            .contains("refusing to create an empty repository"),
        "unexpected error: {error}"
    );
    assert!(
        !dir.path().join("metadata.sqlite3").exists(),
        "opening created a shadow database"
    );
    assert!(
        !dir.path().join("objects").exists(),
        "opening created a shadow object store"
    );
}
