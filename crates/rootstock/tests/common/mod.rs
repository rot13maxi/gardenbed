//! Shared fixtures and oracles for the rootstock integration tests.
//!
//! Everything differential goes through two independent channels:
//!
//! * **git side**: `git ls-tree -r -z` (structure) and a persistent
//!   `git cat-file --batch` process (byte-exact content) — the harness
//!   oracle wrappers where possible.
//! * **gardenbed side**: the derive engine over the imported stores
//!   (`derive_tree` → `flatten_tree` → `get_blob`).
//!
//! Generated expectations (`files_at`, blob contents) close the triangle.
//!
//! Each test binary compiles this module separately and uses a different
//! subset of the helpers, so unused-code warnings are expected here.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_harness::{generate, materialize, GenOptions, GeneratedHistory};
use gardenbed_model::{Id, Mode};
use gardenbed_rootstock::{import_from_path, ImportOptions, ImportReport};
use gardenbed_store::{FsObjectStore, MetadataStore, ObjectStore, SqliteMetadataStore};
use tempfile::TempDir;

/// A generated + materialized history with a fresh gardenbed root next to
/// it. Both live in one temp dir that is cleaned up on drop.
pub struct Fixture {
    /// Holds the temp dir alive for the fixture's lifetime (also `dir.path()`
    /// for anything not covered by `repo`/`garden`).
    #[allow(dead_code)]
    pub dir: TempDir,
    /// The git repository (non-bare, sha1).
    pub repo: PathBuf,
    /// A gardenbed repository root (not yet populated).
    pub garden: PathBuf,
    pub history: GeneratedHistory,
    /// `(changeset id, git sha)` from materialization, index-aligned with
    /// `history.changesets`.
    pub mapping: Vec<(Id, String)>,
}

impl Fixture {
    pub fn generate(seed: u64, opts: &GenOptions) -> Fixture {
        let history = generate(seed, opts);
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let garden = dir.path().join("garden");
        let m = materialize(&history, &repo).expect("materialize");
        Fixture {
            dir,
            repo,
            garden,
            history,
            mapping: m.mapping,
        }
    }
}

/// Opens (creating if needed) the object and metadata stores under a
/// garden root, plus an engine over them.
pub fn open_stores(garden: &Path) -> (Arc<FsObjectStore>, Arc<SqliteMetadataStore>, Engine) {
    let objects = Arc::new(FsObjectStore::open(&garden.join("loam")).expect("object store"));
    let metadata =
        Arc::new(SqliteMetadataStore::open(&garden.join("seedbed.sqlite3")).expect("metadata"));
    let engine = Engine::new(objects.clone(), metadata.clone());
    (objects, metadata, engine)
}

/// Runs a full import of `repo` into the (fresh or existing) garden root.
pub fn import(repo: &Path, garden: &Path, opts: &ImportOptions) -> ImportReport {
    let (objects, metadata, engine) = open_stores(garden);
    import_from_path(repo, objects, metadata, engine, opts).expect("import succeeds")
}

/// Like [`import`], but returns the error instead of panicking.
pub fn try_import(repo: &Path, garden: &Path, opts: &ImportOptions) -> ImportReportResult {
    let (objects, metadata, engine) = open_stores(garden);
    import_from_path(repo, objects, metadata, engine, opts)
}

pub type ImportReportResult = Result<ImportReport, gardenbed_rootstock::ImportError>;

/// A scalar (usually a COUNT) from the seedbed sqlite.
pub fn sql_scalar(garden: &Path, sql: &str) -> u64 {
    let conn = rusqlite::Connection::open(garden.join("seedbed.sqlite3")).expect("sqlite opens");
    conn.query_row(sql, [], |r| r.get::<_, u64>(0)).expect("query")
}

/// All `(git_sha, cs_hex)` mapping rows, sorted by sha.
pub fn git_map_rows(garden: &Path) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(garden.join("seedbed.sqlite3")).expect("sqlite opens");
    let mut stmt = conn
        .prepare("SELECT git_sha, hex(cs) FROM git_map ORDER BY git_sha")
        .expect("prepare");
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .expect("query");
    rows.map(|r| r.expect("row")).collect()
}

/// Refs as `(name, cs hex)`, sorted by name.
pub fn ref_rows(garden: &Path) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(garden.join("seedbed.sqlite3")).expect("sqlite opens");
    let mut stmt = conn
        .prepare("SELECT name, hex(cs) FROM refs ORDER BY name")
        .expect("prepare");
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .expect("query");
    rows.map(|r| r.expect("row")).collect()
}

/// Relative paths of every object file under the loam.
pub fn object_set(garden: &Path) -> BTreeSet<String> {
    let objects_root = garden.join("loam").join("objects");
    let mut out = BTreeSet::new();
    let mut stack = vec![objects_root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir") {
            let entry = entry.expect("entry");
            if entry.file_type().expect("ft").is_dir() {
                stack.push(entry.path());
            } else {
                out.insert(entry.path().strip_prefix(&objects_root).expect("relative").to_string_lossy().to_string());
            }
        }
    }
    out
}

/// Maps git's mode string to a gardenbed mode.
pub fn mode_from_git(mode: &str) -> Mode {
    match mode {
        "100644" => Mode::Regular,
        "100755" => Mode::Exec,
        "120000" => Mode::Symlink,
        "160000" => Mode::Gitlink,
        other => panic!("unexpected git mode {other:?}"),
    }
}

/// A persistent `git cat-file --batch` process for byte-exact oracle
/// reads (`<commit-sha>:<path>` and raw object specs).
pub struct CatFile {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    pub fn spawn(repo: &Path) -> CatFile {
        let mut cmd = Command::new("git");
        cmd.current_dir(repo);
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(&k);
            }
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
        cmd.args(["cat-file", "--batch"]);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn cat-file");
        CatFile {
            stdin: Some(child.stdin.take().expect("stdin")),
            stdout: BufReader::new(child.stdout.take().expect("stdout")),
            child,
        }
    }

    /// Reads the bytes of a `<rev>` (typically `<sha>:<path>`); missing
    /// objects are hard errors (tests only query existing paths).
    pub fn read(&mut self, spec: &str) -> Vec<u8> {
        {
            let stdin = self.stdin.as_mut().expect("stdin alive");
            stdin.write_all(spec.as_bytes()).expect("write spec");
            stdin.write_all(b"\n").expect("write newline");
            stdin.flush().expect("flush");
        }
        let mut header = String::new();
        self.stdout.read_line(&mut header).expect("header line");
        let header = header.trim_end();
        assert!(
            !header.ends_with("missing"),
            "cat-file: {spec} is missing (header {header:?})"
        );
        // "<sha> <type> <len>"
        let len: usize = header
            .rsplit(' ')
            .next()
            .expect("len field")
            .parse()
            .unwrap_or_else(|_| panic!("bad cat-file header {header:?}"));
        let mut buf = vec![0u8; len];
        self.stdout.read_exact(&mut buf).expect("content bytes");
        let mut nl = [0u8; 1];
        self.stdout.read_exact(&mut nl).expect("trailing newline");
        assert_eq!(nl[0], b'\n', "cat-file framing broken");
        buf
    }

    /// The raw bytes of a commit object (for byte-exact message checks).
    pub fn commit_raw(&mut self, sha: &str) -> Vec<u8> {
        self.read(sha)
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

/// The message bytes of a commit, parsed from its raw object (no `%B`
/// normalization involved).
pub fn git_commit_message(cat: &mut CatFile, sha: &str) -> Vec<u8> {
    let raw = cat.commit_raw(sha);
    let pos = raw
        .windows(2)
        .position(|w| w == b"\n\n")
        .expect("commit has a message separator");
    raw[pos + 2..].to_vec()
}

/// Verifies that the imported changeset, applied to the first-parent
/// state, reproduces the child state — without assuming any particular
/// change representation. (Git legitimately classifies a rename whose
/// content changes below its 50% similarity threshold as add+delete, so
/// imported changesets may differ from the generator's representation
/// while describing the same tree transition.)
fn check_delta_semantics(
    parent: &BTreeMap<String, (Id, Mode)>,
    child: &BTreeMap<String, (Id, Mode)>,
    cs: &gardenbed_model::Changeset,
    label: &str,
) {
    use gardenbed_model::Op;
    let mut deletes: BTreeSet<&str> = BTreeSet::new();
    // rename source → (destination, blob, mode)
    let mut renames: BTreeMap<&str, (&str, Id, Mode)> = BTreeMap::new();
    // destination path → (blob, mode) for Add/Modify/Rename
    let mut upserts: BTreeMap<&str, (Id, Mode)> = BTreeMap::new();
    for c in &cs.changes {
        match c.op {
            Op::Add => {
                assert!(c.src_path.is_none(), "{label}: Add carries src_path");
                upserts.insert(&c.path, (c.blob.expect("Add has blob"), c.mode));
            }
            Op::Modify => {
                assert!(c.src_path.is_none(), "{label}: Modify carries src_path");
                upserts.insert(&c.path, (c.blob.expect("Modify has blob"), c.mode));
            }
            Op::Delete => {
                assert!(
                    c.blob.is_none() && c.src_path.is_none(),
                    "{label}: Delete carries payload"
                );
                assert_eq!(c.mode, Mode::Regular, "{label}: Delete mode");
                deletes.insert(&c.path);
            }
            Op::Rename => {
                let src = c.src_path.as_deref().expect("Rename carries src");
                assert_ne!(src, c.path, "{label}: Rename src == dst");
                let blob = c.blob.expect("Rename has blob");
                renames.insert(src, (&c.path, blob, c.mode));
                upserts.insert(&c.path, (blob, c.mode));
            }
        }
    }
    // 1. Every disappearing path is explained by a delete or a rename
    //    source. (An in-place change is covered by check 2 via upserts.)
    for (p, _old) in parent {
        if !child.contains_key(p) {
            assert!(
                deletes.contains(p.as_str()) || renames.contains_key(p.as_str()),
                "{label}: {p:?} vanished with no delete/rename source"
            );
        }
    }
    // 2. Every state change producing a new path state is explained by an
    //    add/modify/rename destination.
    for (p, new) in child {
        if parent.get(p) != Some(new) {
            assert!(
                upserts.contains_key(p.as_str()),
                "{label}: {p:?} changed with no add/modify/rename destination"
            );
        }
    }
    // 3. Each recorded change agrees with both states.
    for p in &deletes {
        assert!(parent.contains_key(*p), "{label}: Delete of absent {p:?}");
        assert!(!child.contains_key(*p), "{label}: Delete but {p:?} survives");
    }
    for (src, (dst, blob, mode)) in &renames {
        assert!(parent.contains_key(*src), "{label}: Rename src {src:?} absent");
        assert!(!child.contains_key(*src), "{label}: Rename src {src:?} survives");
        assert_eq!(
            child.get(*dst),
            Some(&(*blob, *mode)),
            "{label}: Rename dst {dst:?} state"
        );
    }
    for (p, (blob, mode)) in &upserts {
        assert_eq!(
            child.get(*p),
            Some(&(*blob, *mode)),
            "{label}: upsert {p:?} state"
        );
    }
}

/// The full differential sweep: for **every** changeset of the history,
/// compare gardenbed's derived state against git and the generator.
///
/// The import is verified against **git** (the oracle), not against the
/// generator's internal changeset bytes: git classifies a rename whose
/// content changed below its similarity threshold as add+delete, so an
/// imported changeset id can differ from the generated one while the tree
/// is identical. Accordingly this checks (a) the git↔changeset mapping is
/// a bijection covering every commit, (b) the imported changeset applied
/// to the first-parent state reproduces the child state, and (c) every
/// derived tree matches both git and the generator, byte-exactly.
pub fn sweep(fixture: &Fixture, garden: &Path) {
    let (objects, metadata, engine) = open_stores(garden);
    let mut cat = CatFile::spawn(&fixture.repo);

    assert_eq!(
        git_map_rows(garden).len(),
        fixture.mapping.len(),
        "one mapping row per ingested commit"
    );

    for (i, (_, sha)) in fixture.mapping.iter().enumerate() {
        // -- mapping bijection, pointwise (import-internal, not generator ids)
        let cs_id = metadata
            .cs_for_git(sha)
            .expect("cs_for_git")
            .unwrap_or_else(|| panic!("commit {sha}: mapping row missing"));
        assert_eq!(
            metadata.git_for_cs(cs_id).expect("git_for_cs").as_deref(),
            Some(sha.as_str()),
            "changeset {cs_id}: inverse mapping wrong"
        );

        // -- stored changeset round-trips through the codec
        let (cs, canonical) = metadata.get_changeset(cs_id).expect("get_changeset").expect("exists");
        let decoded = gardenbed_model::Changeset::decode(&canonical).expect("canonical decodes");
        assert_eq!(decoded, cs, "changeset {i}: stored bytes do not round-trip");

        // -- metadata vs git: message (byte-exact) and parents
        let message = git_commit_message(&mut cat, sha);
        assert_eq!(
            cs.message.as_bytes(),
            message.as_slice(),
            "commit {sha}: message mismatch"
        );
        let log = &gardenbed_harness::git_log(&fixture.repo, sha, 1).expect("git log")[0];
        assert_eq!(log.author_name, cs.author.name, "commit {sha}: author name");
        assert_eq!(log.author_email, cs.author.email, "commit {sha}: author email");
        let parent_css: Vec<Id> = log
            .parents
            .iter()
            .map(|p| {
                metadata.cs_for_git(p).expect("cs_for_git").unwrap_or_else(|| {
                    panic!("commit {sha}: parent {p} has no mapping")
                })
            })
            .collect();
        assert_eq!(
            cs.parents, parent_css,
            "commit {sha}: parent structure/order differs from git"
        );

        // -- delta semantics: changes applied to the first-parent state
        //    (the generator's view) must reproduce this changeset's state.
        let parent_state = if log.parents.is_empty() {
            BTreeMap::new()
        } else {
            let pi = fixture
                .mapping
                .iter()
                .position(|(_, s)| s == &log.parents[0])
                .unwrap_or_else(|| panic!("commit {sha}: first parent not in fixture mapping"));
            gardenbed_harness::files_at(&fixture.history, pi)
        };
        let child_state = gardenbed_harness::files_at(&fixture.history, i);
        check_delta_semantics(&parent_state, &child_state, &cs, &format!("commit {sha}"));

        // -- generated expectation for this changeset
        let expected = &child_state;

        // -- git tree: path → (mode, blob sha)
        let ls = gardenbed_harness::git_ls_tree_r(&fixture.repo, sha).expect("ls-tree");
        let mut git_files: Vec<(String, Mode, Vec<u8>)> = ls
            .into_iter()
            .map(|(p, m, sha)| (p, mode_from_git(&m), sha))
            .collect();
        git_files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

        // -- derived tree: path → (mode, blob id)
        let tree = engine.derive_tree(cs_id).expect("derive_tree");
        let mut derived = engine.flatten_tree(tree).expect("flatten_tree");
        derived.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

        assert_eq!(
            derived.iter().map(|(p, _, _)| p.as_str()).collect::<Vec<_>>(),
            git_files.iter().map(|(p, _, _)| p.as_str()).collect::<Vec<_>>(),
            "commit {sha}: derived tree paths differ from git"
        );
        assert_eq!(
            derived.len(),
            expected.len(),
            "commit {sha}: derived file count differs from the generated history"
        );
        for (path, mode, blob) in &derived {
            // generated expectation
            let (want_blob, want_mode) =
                expected.get(path).unwrap_or_else(|| panic!("commit {sha}: path {path:?} missing from files_at"));
            assert_eq!(mode, want_mode, "commit {sha}: mode of {path:?}");
            assert_eq!(blob, want_blob, "commit {sha}: blob id of {path:?}");
        }
        for ((dpath, dmode, dblob), (gpath, gmode, gsha)) in derived.iter().zip(git_files.iter()) {
            assert_eq!(dpath, gpath);
            assert_eq!(dmode, gmode, "commit {sha}: mode of {dpath:?}");
            if *dmode == Mode::Gitlink {
                // git show fails on gitlinks; the pointer bytes themselves
                // are the oracle: our blob field is 12 zeros + the sha.
                assert_eq!(&dblob.as_bytes()[12..], gsha.as_slice());
                assert!(dblob.as_bytes()[..12].iter().all(|b| *b == 0));
                continue;
            }
            let ours = objects.get_blob(*dblob).expect("get_blob").unwrap_or_else(|| panic!("blob {} missing", dblob));
            let theirs = cat.read(&format!("{sha}:{gpath}"));
            assert_eq!(
                ours, theirs,
                "commit {sha}: content of {dpath:?} differs from git show"
            );
        }
    }
}
