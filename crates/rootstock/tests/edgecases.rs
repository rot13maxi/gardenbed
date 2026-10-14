//! Structural edge cases the generator cannot produce on demand, built
//! with git plumbing only: dir→file and file→dir replacements (the
//! applier repair), empty-tree commits, submodule pointers (gitlinks),
//! type changes, and rename detection toggled off.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::{open_stores, CatFile};
use gardenbed_model::{blob_id, Changeset, Id, Mode, Op};
use gardenbed_rootstock::{import_from_path, ImportOptions};
use gardenbed_store::{MetadataStore, ObjectStore};
use tempfile::TempDir;

/// A tiny plumbing-only git repository builder (hermetic, deterministic).
struct Builder {
    _dir: TempDir,
    repo: PathBuf,
}

impl Builder {
    fn new() -> Builder {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        let mut cmd = Command::new("git");
        cmd.arg("init").arg("-q").arg("--initial-branch=main").arg(&repo);
        let out = cmd.output().expect("git init runs");
        assert!(out.status.success(), "git init failed");
        Builder { _dir: dir, repo }
    }

    fn git(&self, args: &[&str]) -> String {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.repo);
        hermetic(&mut cmd);
        let out = cmd.args(args).output().expect("git runs");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8").trim().to_owned()
    }

    fn git_stdin(&self, args: &[&str], input: &[u8]) -> String {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.repo);
        hermetic(&mut cmd);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(input)
            .expect("write stdin");
        let out = child.wait_with_output().expect("wait");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8").trim().to_owned()
    }

    /// Writes a blob and returns its git sha.
    fn blob(&self, content: &[u8]) -> String {
        self.git_stdin(&["hash-object", "-w", "--stdin"], content)
    }

    /// Adds/overwrites an index entry (`mode` is git's octal string).
    fn add(&self, mode: &str, sha: &str, path: &str) {
        self.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("{mode},{sha},{path}"),
        ]);
    }

    fn remove(&self, path: &str) {
        self.git(&["update-index", "--force-remove", path]);
    }

    fn tree(&self) -> String {
        self.git(&["write-tree"])
    }

    /// Commits the current index (message verbatim via stdin, fixed
    /// identity), moves `refs/heads/main` to it, and returns the sha.
    fn commit(&self, parents: &[&str], message: &str) -> String {
        let tree = self.tree();
        let mut args: Vec<String> = vec!["commit-tree".into(), tree];
        for p in parents {
            args.push("-p".into());
            args.push((*p).to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let sha = self.git_stdin(&arg_refs, message.as_bytes());
        self.git(&["update-ref", "refs/heads/main", &sha]);
        sha
    }
}

fn hermetic(cmd: &mut Command) {
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_AUTHOR_NAME", "Edge Author");
    cmd.env("GIT_AUTHOR_EMAIL", "edge@example.com");
    cmd.env("GIT_COMMITTER_NAME", "Edge Committer");
    cmd.env("GIT_COMMITTER_EMAIL", "edge@example.com");
}

/// The imported changeset of a commit sha.
fn imported_cs(garden: &Path, sha: &str) -> Changeset {
    let (_, metadata, _) = open_stores(garden);
    let cs_id = metadata
        .cs_for_git(sha)
        .expect("cs_for_git")
        .unwrap_or_else(|| panic!("commit {sha} not mapped"));
    metadata.get_changeset(cs_id).expect("get").expect("exists").0
}

/// Decodes 40 hex chars into 20 bytes.
fn hex_bytes(s: &str) -> Vec<u8> {
    assert_eq!(s.len() % 2, 0, "odd hex");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex digit"))
        .collect()
}

/// The derived tree (path → mode, blob id) of an imported commit; an
/// empty tree (engine `None`) flattens to an empty vec.
fn derived_tree(garden: &Path, sha: &str) -> Vec<(String, Mode, Id)> {
    let (_, metadata, engine) = open_stores(garden);
    let cs_id = metadata.cs_for_git(sha).expect("q").expect("mapped");
    match engine.derive_tree(cs_id).expect("derive") {
        None => Vec::new(),
        Some(tree) => {
            let mut flat = engine.flatten_tree(Some(tree)).expect("flatten");
            flat.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            flat
        }
    }
}

/// Git's ls-tree -r as (path, mode, sha), sorted by path bytes.
fn git_tree(repo: &Path, sha: &str) -> Vec<(String, String, Vec<u8>)> {
    let out = {
        let mut cmd = Command::new("git");
        cmd.current_dir(repo);
        hermetic(&mut cmd);
        let out = cmd
            .args(["ls-tree", "-r", "-z", sha])
            .output()
            .expect("ls-tree runs");
        assert!(out.status.success(), "ls-tree failed");
        out.stdout
    };
    let mut entries = Vec::new();
    for tok in out.split(|b| *b == 0) {
        if tok.is_empty() {
            continue;
        }
        // One entry per NUL: "mode SP type SP sha TAB path".
        let tab = tok
            .iter()
            .position(|b| *b == b'\t')
            .expect("ls-tree entry has a TAB");
        let meta = String::from_utf8_lossy(&tok[..tab]).to_string();
        let path = String::from_utf8_lossy(&tok[tab + 1..]).to_string();
        let mut it = meta.splitn(3, ' ');
        let mode = it.next().expect("mode");
        let _kind = it.next().expect("kind");
        let oid = it.next().expect("oid");
        entries.push((path, mode.to_string(), hex_bytes(oid)));
    }
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    entries
}

/// Compares a derived tree against git's, byte-exactly for content.
fn assert_tree_matches_git(
    garden: &Path,
    repo: &Path,
    sha: &str,
    cat: &mut CatFile,
) {
    let derived = derived_tree(garden, sha);
    let git_entries = git_tree(repo, sha);
    assert_eq!(
        derived.iter().map(|(p, _, _)| p.as_str()).collect::<Vec<_>>(),
        git_entries.iter().map(|(p, _, _)| p.as_str()).collect::<Vec<_>>(),
        "commit {sha}: derived paths differ from git"
    );
    for ((p, mode, blob), (gp, gmode, gsha)) in derived.iter().zip(git_entries.iter()) {
        assert_eq!(p, gp);
        assert_eq!(
            *mode,
            common::mode_from_git(gmode),
            "commit {sha}: mode of {p:?}"
        );
        if *mode == Mode::Gitlink {
            assert_eq!(&blob.as_bytes()[12..], gsha.as_slice());
            assert!(blob.as_bytes()[..12].iter().all(|b| *b == 0));
            continue;
        }
        let ours = {
            let (objects, _, _) = open_stores(garden);
            objects.get_blob(*blob).expect("get_blob").unwrap_or_else(|| panic!("blob {blob} missing"))
        };
        assert_eq!(ours, cat.read(&format!("{sha}:{gp}")), "content of {p:?}");
    }
}

/// A directory replaced by a file (`A P` + `D P/f1` + `D P/f2`) imports as
/// a repaired `Rename P ← P/f1` plus the remaining `Delete P/f2`, and the
/// derived trees still match git exactly.
#[test]
fn dir_replaced_by_file_imports_as_rename() {
    let b = Builder::new();
    b.add("100644", &b.blob(b"one\n"), "P/f1");
    b.add("100644", &b.blob(b"two\n"), "P/f2");
    b.add("100644", &b.blob(b"base\n"), "root.txt");
    let c1 = b.commit(&[], "add dir P\n");

    b.remove("P/f1");
    b.remove("P/f2");
    b.add("100644", &b.blob(b"now a file\n"), "P");
    let c2 = b.commit(&[&c1], "P becomes a file\n");

    let garden = b._dir.path().join("garden");
    let opts = ImportOptions::default();
    let (objects, metadata, engine) = open_stores(&garden);
    let report = import_from_path(&b.repo, objects.clone(), metadata.clone(), engine, &opts)
        .expect("import succeeds");
    assert_eq!(report.commits_new, 2);

    let cs2 = imported_cs(&garden, &c2);
    let want = vec![
        gardenbed_model::Change {
            path: "P".into(),
            op: Op::Rename,
            blob: Some(blob_id(b"now a file\n")),
            src_path: Some("P/f1".into()),
            mode: Mode::Regular,
        },
        gardenbed_model::Change {
            path: "P/f2".into(),
            op: Op::Delete,
            blob: None,
            src_path: None,
            mode: Mode::Regular,
        },
    ];
    assert_eq!(cs2.changes, want, "dir→file must be repaired into a rename");

    let mut cat = CatFile::spawn(&b.repo);
    assert_tree_matches_git(&garden, &b.repo, &c1, &mut cat);
    assert_tree_matches_git(&garden, &b.repo, &c2, &mut cat);
}

/// A file replaced by a directory (`D P` + `A P/a` + `A P/b`) needs no
/// repair and imports literally.
#[test]
fn file_replaced_by_dir_needs_no_repair() {
    let b = Builder::new();
    b.add("100644", &b.blob(b"plain file\n"), "P");
    let c1 = b.commit(&[], "file P\n");

    b.remove("P");
    b.add("100644", &b.blob(b"aaa\n"), "P/a");
    b.add("100644", &b.blob(b"bbb\n"), "P/b");
    let c2 = b.commit(&[&c1], "P becomes a dir\n");

    let garden = b._dir.path().join("garden");
    let (objects, metadata, engine) = open_stores(&garden);
    import_from_path(&b.repo, objects, metadata.clone(), engine, &ImportOptions::default())
        .expect("import succeeds");

    let cs2 = imported_cs(&garden, &c2);
    let want = vec![
        gardenbed_model::Change {
            path: "P".into(),
            op: Op::Delete,
            blob: None,
            src_path: None,
            mode: Mode::Regular,
        },
        gardenbed_model::Change {
            path: "P/a".into(),
            op: Op::Add,
            blob: Some(blob_id(b"aaa\n")),
            src_path: None,
            mode: Mode::Regular,
        },
        gardenbed_model::Change {
            path: "P/b".into(),
            op: Op::Add,
            blob: Some(blob_id(b"bbb\n")),
            src_path: None,
            mode: Mode::Regular,
        },
    ];
    assert_eq!(cs2.changes, want, "file→dir must import verbatim");

    let mut cat = CatFile::spawn(&b.repo);
    assert_tree_matches_git(&garden, &b.repo, &c1, &mut cat);
    assert_tree_matches_git(&garden, &b.repo, &c2, &mut cat);
}

/// Empty trees: an empty root commit (no changes at all), a normal commit
/// on top, and an empty tree again (one delete). All derived trees match
/// git's.
#[test]
fn empty_tree_commits() {
    let b = Builder::new();
    // c1: the empty tree, no parents (mktree with empty stdin).
    let empty = {
        let mut cmd = Command::new("git");
        cmd.current_dir(&b.repo);
        hermetic(&mut cmd);
        cmd.arg("mktree").stdin(Stdio::null()).output().expect("mktree runs")
    };
    assert!(empty.status.success());
    let empty = String::from_utf8(empty.stdout).expect("utf8").trim().to_owned();
    let c1 = b.git_stdin(&["commit-tree", &empty], b"empty root\n");

    b.add("100644", &b.blob(b"content\n"), "a.txt");
    let c2 = b.commit(&[&c1], "add a.txt\n");

    let c3 = b.git_stdin(&["commit-tree", &empty, "-p", &c2], b"empty again\n");
    b.git(&["update-ref", "refs/heads/main", &c3]);

    let garden = b._dir.path().join("garden");
    let (objects, metadata, engine) = open_stores(&garden);
    import_from_path(&b.repo, objects, metadata, engine, &ImportOptions::default())
        .expect("import succeeds");

    let cs1 = imported_cs(&garden, &c1);
    assert!(cs1.changes.is_empty(), "empty root commit has no changes");
    assert!(cs1.parents.is_empty());

    let cs2 = imported_cs(&garden, &c2);
    assert_eq!(cs2.changes.len(), 1);
    assert_eq!(cs2.changes[0].op, Op::Add);
    assert_eq!(cs2.changes[0].path, "a.txt");

    let cs3 = imported_cs(&garden, &c3);
    assert_eq!(cs3.changes.len(), 1);
    assert_eq!(cs3.changes[0].op, Op::Delete);
    assert_eq!(cs3.changes[0].path, "a.txt");

    let mut cat = CatFile::spawn(&b.repo);
    for sha in [&c1, &c2, &c3] {
        assert_tree_matches_git(&garden, &b.repo, sha, &mut cat);
    }
    // The empty derived tree: the engine represents it as `None`
    // (documented contract of `derive_tree`).
    let (_, metadata, engine) = open_stores(&garden);
    let cs_id = metadata.cs_for_git(&c1).expect("q").expect("mapped");
    assert!(
        engine.derive_tree(cs_id).expect("derive").is_none(),
        "an empty tree is represented as None"
    );
}

/// Submodule pointers (mode 160000): the blob field is the git sha
/// left-padded to 32 bytes, no blob is written, and the derived tree
/// matches git's ls-tree.
#[test]
fn gitlinks_are_padded_and_never_dereferenced() {
    let b = Builder::new();
    b.add("100644", &b.blob(b"base\n"), "base.txt");
    let fake_sub = "1234567890abcdef1234567890abcdef12345678";
    b.add("160000", fake_sub, "sub/module");
    let c1 = b.commit(&[], "with a submodule\n");

    let garden = b._dir.path().join("garden");
    let (objects, metadata, engine) = open_stores(&garden);
    import_from_path(&b.repo, objects.clone(), metadata, engine, &ImportOptions::default())
        .expect("import succeeds");

    let mut padded = [0u8; 32];
    padded[12..].copy_from_slice(&hex_bytes(fake_sub));
    let padded = Id::from_bytes(padded);

    let cs1 = imported_cs(&garden, &c1);
    assert_eq!(cs1.changes.len(), 2);
    let gl = cs1
        .changes
        .iter()
        .find(|c| c.path == "sub/module")
        .expect("gitlink change present");
    assert_eq!(gl.op, Op::Add);
    assert_eq!(gl.mode, Mode::Gitlink);
    assert_eq!(gl.blob, Some(padded), "gitlink blob is 12 zeros + git sha");

    // The pointer was never dereferenced: no blob object for it.
    assert!(
        objects.get_blob(padded).expect("get_blob").is_none(),
        "gitlink pointers must not be written as blobs"
    );

    let mut cat = CatFile::spawn(&b.repo);
    assert_tree_matches_git(&garden, &b.repo, &c1, &mut cat);
}

/// A type change (regular file → symlink on the same path) imports as a
/// Modify carrying the new mode.
#[test]
fn typechange_imports_as_modify() {
    let b = Builder::new();
    b.add("100644", &b.blob(b"was a file\n"), "f");
    let c1 = b.commit(&[], "regular file\n");

    b.remove("f");
    b.add("120000", &b.blob(b"now-a-link"), "f");
    let c2 = b.commit(&[&c1], "f becomes a symlink\n");

    let garden = b._dir.path().join("garden");
    let (objects, metadata, engine) = open_stores(&garden);
    import_from_path(&b.repo, objects, metadata, engine, &ImportOptions::default())
        .expect("import succeeds");

    let cs2 = imported_cs(&garden, &c2);
    assert_eq!(cs2.changes.len(), 1, "git reports one typechange");
    assert_eq!(cs2.changes[0].op, Op::Modify, "T imports as Modify");
    assert_eq!(cs2.changes[0].path, "f");
    assert_eq!(cs2.changes[0].mode, Mode::Symlink);
    assert_eq!(cs2.changes[0].blob, Some(blob_id(b"now-a-link")));

    let mut cat = CatFile::spawn(&b.repo);
    assert_tree_matches_git(&garden, &b.repo, &c1, &mut cat);
    assert_tree_matches_git(&garden, &b.repo, &c2, &mut cat);
}

/// With `detect_renames = false`, git runs with `--no-renames` and a
/// rename imports as the add+delete pair — same tree, different shape.
#[test]
fn renames_off_yields_add_delete_pair() {
    let b = Builder::new();
    b.add("100644", &b.blob(b"same content\n"), "old.txt");
    let c1 = b.commit(&[], "before rename\n");

    b.remove("old.txt");
    b.add("100644", &b.blob(b"same content\n"), "new.txt");
    let c2 = b.commit(&[&c1], "renamed\n");

    let garden_on = b._dir.path().join("garden-on");
    {
        let (objects, metadata, engine) = open_stores(&garden_on);
        import_from_path(&b.repo, objects, metadata, engine, &ImportOptions::default())
            .expect("import with renames");
        let cs = imported_cs(&garden_on, &c2);
        assert_eq!(cs.changes.len(), 1);
        assert_eq!(cs.changes[0].op, Op::Rename);
        assert_eq!(cs.changes[0].path, "new.txt");
        assert_eq!(cs.changes[0].src_path.as_deref(), Some("old.txt"));
    }

    let garden_off = b._dir.path().join("garden-off");
    {
        let (objects, metadata, engine) = open_stores(&garden_off);
        import_from_path(
            &b.repo,
            objects,
            metadata,
            engine,
            &ImportOptions {
                detect_renames: false,
                ..Default::default()
            },
        )
        .expect("import without renames");
        let cs = imported_cs(&garden_off, &c2);
        assert_eq!(cs.changes.len(), 2, "no rename detection: add+delete");
        assert_eq!(cs.changes[0].op, Op::Add);
        assert_eq!(cs.changes[0].path, "new.txt");
        assert_eq!(cs.changes[1].op, Op::Delete);
        assert_eq!(cs.changes[1].path, "old.txt");
    }

    // Identical trees either way.
    let mut cat = CatFile::spawn(&b.repo);
    assert_tree_matches_git(&garden_on, &b.repo, &c2, &mut cat);
    assert_tree_matches_git(&garden_off, &b.repo, &c2, &mut cat);
}
