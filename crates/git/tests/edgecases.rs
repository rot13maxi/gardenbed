//! Structural edge cases the generator cannot produce on demand, built
//! with git plumbing only: dir→file and file→dir replacements (the
//! applier repair), empty-tree commits, submodule pointers (gitlinks),
//! type changes, and rename detection toggled off.

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::{CatFile, open_stores};
use gardenbed_git::{ImportOptions, import_from_path};
use gardenbed_model::{Changeset, Id, Mode, Op, blob_id};
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
        cmd.arg("init")
            .arg("-q")
            .arg("--initial-branch=main")
            .arg(&repo);
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
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
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
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
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
    metadata
        .get_changeset(cs_id)
        .expect("get")
        .expect("exists")
        .0
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
fn assert_tree_matches_git(garden: &Path, repo: &Path, sha: &str, cat: &mut CatFile) {
    let derived = derived_tree(garden, sha);
    let git_entries = git_tree(repo, sha);
    assert_eq!(
        derived
            .iter()
            .map(|(p, _, _)| p.as_str())
            .collect::<Vec<_>>(),
        git_entries
            .iter()
            .map(|(p, _, _)| p.as_str())
            .collect::<Vec<_>>(),
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
            objects
                .get_blob(*blob)
                .expect("get_blob")
                .unwrap_or_else(|| panic!("blob {blob} missing"))
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
    import_from_path(
        &b.repo,
        objects,
        metadata.clone(),
        engine,
        &ImportOptions::default(),
    )
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
        cmd.arg("mktree")
            .stdin(Stdio::null())
            .output()
            .expect("mktree runs")
    };
    assert!(empty.status.success());
    let empty = String::from_utf8(empty.stdout)
        .expect("utf8")
        .trim()
        .to_owned();
    let c1 = b.git_stdin(&["commit-tree", &empty], b"empty root\n");

    b.add("100644", &b.blob(b"content\n"), "a.txt");
    let c2 = b.commit(&[&c1], "add a.txt\n");

    let c3 = b.git_stdin(&["commit-tree", &empty, "-p", &c2], b"empty again\n");
    b.git(&["update-ref", "refs/heads/main", &c3]);

    let garden = b._dir.path().join("garden");
    let (objects, metadata, engine) = open_stores(&garden);
    import_from_path(
        &b.repo,
        objects,
        metadata,
        engine,
        &ImportOptions::default(),
    )
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
    import_from_path(
        &b.repo,
        objects.clone(),
        metadata,
        engine,
        &ImportOptions::default(),
    )
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
    import_from_path(
        &b.repo,
        objects,
        metadata,
        engine,
        &ImportOptions::default(),
    )
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
        import_from_path(
            &b.repo,
            objects,
            metadata,
            engine,
            &ImportOptions::default(),
        )
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

// ------------------------------------------ pre-canonical git modes (100664)

impl Builder {
    /// Hand-crafts a raw tree object carrying **literal** entry modes.
    ///
    /// Modern git fsck-checks fresh tree objects: plain
    /// `git hash-object -t tree -w --stdin` refuses a `100664` entry
    /// ("refusing to create malformed object", `badFilemode`), so the
    /// bytes are written with `--literally`, which bypasses the check.
    /// The resulting object is accepted by plain `git fsck` (rc 0 —
    /// `badFilemode` is only a warning; git 2.43 keeps it one even under
    /// `--strict`), exactly like the pre-canonical
    /// objects preserved in old histories (the linux corpus's "new git
    /// world order" commits normalize exactly such entries). `entries`
    /// are `(mode, hex sha, name)` and are written in git's tree order
    /// (directory names compare as if suffixed with `/`).
    fn raw_tree(&self, entries: &[(&str, &str, &str)]) -> String {
        let key = |e: &(&str, &str, &str)| {
            let is_dir = e.0 == "40000" || e.0 == "040000" || e.0 == "400";
            if is_dir {
                format!("{}/", e.2)
            } else {
                e.2.to_owned()
            }
        };
        let mut sorted: Vec<&(&str, &str, &str)> = entries.iter().collect();
        sorted.sort_by(|a, b| key(a).as_bytes().cmp(key(b).as_bytes()));
        let mut bytes = Vec::new();
        for (mode, sha, name) in sorted {
            bytes.extend_from_slice(mode.as_bytes());
            bytes.push(b' ');
            bytes.extend_from_slice(name.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&hex_bytes(sha));
        }
        self.git_stdin(
            &["hash-object", "-t", "tree", "-w", "--stdin", "--literally"],
            &bytes,
        )
    }

    /// Commits a hand-crafted tree via `commit-tree` plumbing (fixed raw
    /// dates for determinism) and moves `refs/heads/main` onto it.
    fn commit_tree(&self, tree: &str, parents: &[&str], message: &str) -> String {
        let mut args: Vec<String> = vec!["commit-tree".into(), tree.into()];
        for p in parents {
            args.push("-p".into());
            args.push((*p).to_string());
        }
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let sha = {
            let mut cmd = Command::new("git");
            cmd.current_dir(&self.repo);
            hermetic(&mut cmd);
            cmd.env("GIT_AUTHOR_DATE", "1113769846 -0700");
            cmd.env("GIT_COMMITTER_DATE", "1113769846 -0700");
            cmd.args(&arg_refs)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().expect("spawn commit-tree");
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(message.as_bytes())
                .expect("write message");
            let out = child.wait_with_output().expect("wait");
            assert!(
                out.status.success(),
                "commit-tree failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        };
        self.git(&["update-ref", "refs/heads/main", &sha]);
        sha
    }

    /// `git diff-tree -r --name-status` for a commit — the oracle for
    /// "git's diff machinery cannot see this change".
    fn diff_tree(&self, sha: &str) -> String {
        self.git(&["diff-tree", "-r", "--name-status", "--no-commit-id", sha])
    }
}

/// Pre-canonical git modes (early-2005 git wrote regular files as
/// `100664`; the linux corpus's "new git world order" commits normalize
/// them to `100644` with identical blob ids). git's diff machinery
/// canonicalizes the modes before comparing (`canon_mode`), so a commit
/// that only fixes modes produces **no diff-tree output at all** although
/// the raw trees differ. The import must classify it from the raw trees:
/// the fixup becomes a zero-change changeset (both sides are
/// `Mode::Regular` in Gardenbed's model) inheriting the parent tree,
/// while content changes under those modes and plain commits keep
/// classifying through diff-tree.
#[test]
fn pre_canonical_100664_mode_fixups_import_via_raw_fallback() {
    let b = Builder::new();

    let old = b.blob(b"old content\n");
    let keep = b.blob(b"keep\n");
    let new = b.blob(b"new content\n");
    let newer = b.blob(b"newer content\n");
    let keep2 = b.blob(b"kept, changed\n");
    let nest = b.blob(b"nested\n");

    // c1 (old world): file.txt and sub/nested.txt are group-writable.
    let sub664 = b.raw_tree(&[("100664", &nest, "nested.txt")]);
    let t1 = b.raw_tree(&[
        ("100664", &old, "file.txt"),
        ("100644", &keep, "other.txt"),
        ("40000", &sub664, "sub"),
    ]);
    let c1 = b.commit_tree(&t1, &[], "old world: group-writable modes\n");

    // c2: the mode-only fixup — identical blob ids, modes 100664→100644.
    let sub644 = b.raw_tree(&[("100644", &nest, "nested.txt")]);
    let t2 = b.raw_tree(&[
        ("100644", &old, "file.txt"),
        ("100644", &keep, "other.txt"),
        ("40000", &sub644, "sub"),
    ]);
    let c2 = b.commit_tree(
        &t2,
        &[&c1],
        "Fix up some file mode differences due to the new git world order.\n",
    );
    assert!(
        b.diff_tree(&c2).is_empty(),
        "fixture precondition: git reports nothing for the mode-only fixup"
    );

    // c3: content change while still 100664 — git sees the new blob and
    // reports a plain modify (the mixed content+mode variant).
    let t3 = b.raw_tree(&[
        ("100664", &new, "file.txt"),
        ("100644", &keep, "other.txt"),
        ("40000", &sub644, "sub"),
    ]);
    let c3 = b.commit_tree(&t3, &[&c2], "edit the group-writable file\n");
    assert_eq!(
        b.diff_tree(&c3),
        "M\tfile.txt",
        "fixture precondition: the content change is reported"
    );

    // c4: a plain commit through the index — the normal path.
    b.add("100644", &newer, "file.txt");
    b.add("100644", &keep2, "other.txt");
    b.add("100644", &nest, "sub/nested.txt");
    let c4 = b.commit(&[&c3], "plain commit via the index\n");
    assert_eq!(
        b.diff_tree(&c4),
        "M\tfile.txt\nM\tother.txt",
        "fixture precondition: the plain commit is reported"
    );

    // The hand-crafted objects are well-formed (fsck accepts 100664 as a
    // warning) — same acceptance the real pre-canonical corpora rely on.
    gardenbed_harness::git_fsck(&b.repo).expect("fsck accepts the hand-crafted 664 trees");

    let garden = b._dir.path().join("garden");
    let report = {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(
            &b.repo,
            objects,
            metadata,
            engine,
            &ImportOptions::default(),
        )
        .expect("the mode-only fixup must not hard-error")
    };
    assert_eq!(report.commits_new, 4, "every commit ingested");
    assert_eq!(report.commits_skipped, 0);

    let (_, metadata, engine) = open_stores(&garden);
    let cs_of = |sha: &str| -> (Id, Changeset) {
        let id = metadata
            .cs_for_git(sha)
            .expect("cs_for_git")
            .expect("mapped");
        let cs = metadata.get_changeset(id).expect("get").expect("exists").0;
        (id, cs)
    };
    let (cs1_id, cs1) = cs_of(&c1);
    let (cs2_id, cs2) = cs_of(&c2);
    let (cs3_id, cs3) = cs_of(&c3);
    let (_cs4_id, cs4) = cs_of(&c4);

    // c1: full-tree adds; 100664 imports as the canonical Regular mode.
    assert_eq!(cs1.parents, Vec::new());
    assert_eq!(
        cs1.changes,
        vec![
            gardenbed_model::Change {
                path: "file.txt".into(),
                op: Op::Add,
                blob: Some(blob_id(b"old content\n")),
                src_path: None,
                mode: Mode::Regular,
            },
            gardenbed_model::Change {
                path: "other.txt".into(),
                op: Op::Add,
                blob: Some(blob_id(b"keep\n")),
                src_path: None,
                mode: Mode::Regular,
            },
            gardenbed_model::Change {
                path: "sub/nested.txt".into(),
                op: Op::Add,
                blob: Some(blob_id(b"nested\n")),
                src_path: None,
                mode: Mode::Regular,
            },
        ],
        "c1 adds the full tree with canonicalized modes"
    );

    // c2: the mode-only fixup is a zero-change changeset linked to its
    // parent — no invented rename, no mode churn.
    assert_eq!(cs2.parents, vec![cs1_id]);
    assert!(
        cs2.changes.is_empty(),
        "664→644 with identical blobs is a no-op after canonicalization"
    );

    // c3: the content change is an ordinary Modify with the new blob.
    assert_eq!(cs3.parents, vec![cs2_id]);
    assert_eq!(
        cs3.changes,
        vec![gardenbed_model::Change {
            path: "file.txt".into(),
            op: Op::Modify,
            blob: Some(blob_id(b"new content\n")),
            src_path: None,
            mode: Mode::Regular,
        }],
        "content change under the old mode classifies via diff-tree"
    );

    // c4: the plain commit is untouched by the fallback.
    assert_eq!(cs4.parents, vec![cs3_id]);
    assert_eq!(
        cs4.changes,
        vec![
            gardenbed_model::Change {
                path: "file.txt".into(),
                op: Op::Modify,
                blob: Some(blob_id(b"newer content\n")),
                src_path: None,
                mode: Mode::Regular,
            },
            gardenbed_model::Change {
                path: "other.txt".into(),
                op: Op::Modify,
                blob: Some(blob_id(b"kept, changed\n")),
                src_path: None,
                mode: Mode::Regular,
            },
        ],
        "plain commits classify via diff-tree as before"
    );

    // The zero-change fixup inherits the parent's derived tree exactly,
    // and every derived mode is the canonical Regular.
    let t1 = engine
        .derive_tree(cs1_id)
        .expect("derive")
        .expect("non-empty");
    let t2 = engine
        .derive_tree(cs2_id)
        .expect("derive")
        .expect("non-empty");
    assert_eq!(t1, t2, "the zero-change fixup inherits the parent tree");
    assert_eq!(
        engine.flatten_tree(Some(t2)).expect("flatten"),
        vec![
            ("file.txt".into(), Mode::Regular, blob_id(b"old content\n")),
            ("other.txt".into(), Mode::Regular, blob_id(b"keep\n")),
            ("sub/nested.txt".into(), Mode::Regular, blob_id(b"nested\n")),
        ],
        "flatten modes are canonical Regular"
    );
    let entry = engine
        .tree_entry(Some(t2), "file.txt")
        .expect("tree_entry")
        .expect("exists");
    assert_eq!(entry.mode, Mode::Regular, "tree_entry mode is Regular");

    // Byte-exact content and path verification against git for all four
    // commits (git's ls-tree displays 100664 canonicalized as 100644).
    let mut cat = CatFile::spawn(&b.repo);
    for sha in [&c1, &c2, &c3, &c4] {
        assert_tree_matches_git(&garden, &b.repo, sha, &mut cat);
    }

    // Re-import: the walk prunes at the mapped tip; nothing is written.
    drop(cat);
    let second = {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(
            &b.repo,
            objects,
            metadata,
            engine,
            &ImportOptions::default(),
        )
        .expect("re-import succeeds")
    };
    assert_eq!(second.commits_new, 0, "nothing re-ingested");
    assert_eq!(second.commits_skipped, 1, "the walk prunes at the tip");
    assert_eq!(second.objects_written, 0, "a re-import writes zero objects");
    assert!(second.refs_updated.is_empty(), "refs must not move");
    let (objects, metadata, _) = open_stores(&garden);
    let exported = b._dir.path().join("raw-mode-export.git");
    let roundtrip = gardenbed_git::export_to_path(
        &exported,
        objects,
        metadata,
        &gardenbed_git::ExportOptions::default(),
    )
    .expect("raw non-canonical tree modes must export SHA-exact");
    for sha in [&c1, &c2, &c3, &c4] {
        assert!(roundtrip.commits.iter().any(|(_, git_sha)| git_sha == sha));
    }
    gardenbed_harness::git_fsck(&exported).expect("exported raw-mode history is valid");
}

/// Non-UTF-8 path bytes (latin-1 `café` with `\xe9`): the model keeps a
/// lossy name (`caf\u{FFFD}`) for querying, the raw tree preserves the
/// original bytes, and the export must reproduce the original SHA —
/// byte-exact history without silent name mangling.
#[test]
fn non_utf8_path_bytes_capture_and_roundtrip() {
    let b = Builder::new();

    let blob = b.blob(b"latin-1 named\n");
    // A tree entry name with the raw byte \xe9 (not valid UTF-8).
    let name: &[u8] = b"caf\xe9.txt";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"100644 ");
    bytes.extend_from_slice(name);
    bytes.push(0);
    bytes.extend_from_slice(&hex_bytes(&blob));
    let tree = b.git_stdin(
        &["hash-object", "-t", "tree", "-w", "--stdin", "--literally"],
        &bytes,
    );
    let c1 = b.commit_tree(&tree, &[], "latin-1 filename\n");

    gardenbed_harness::git_fsck(&b.repo).expect("fsck accepts the raw-name tree");

    let garden = b._dir.path().join("garden");
    let report = {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(&b.repo, objects, metadata, engine, &Default::default())
            .expect("non-UTF-8 names import via raw-tree capture")
    };
    assert_eq!(report.commits_new, 1);
    assert!(
        report.raw_trees_captured >= 1,
        "the non-UTF-8 tree is captured: {}",
        report.raw_trees_captured
    );

    // The model path is the lossy decode (queryable), never the raw bytes.
    let (objects, metadata, engine) = open_stores(&garden);
    let root = engine
        .derive_tree(metadata.cs_for_git(&c1).unwrap().unwrap())
        .expect("derive")
        .expect("root");
    let flat = engine.flatten_tree(Some(root)).expect("flatten");
    assert_eq!(flat.len(), 1);
    assert_eq!(
        flat[0].0, "caf\u{FFFD}.txt",
        "the model holds the lossy name for queries"
    );

    // Export: SHA-exact, because the raw tree row carries the bytes.
    let exported = b._dir.path().join("non-utf8-export.git");
    let roundtrip = gardenbed_git::export_to_path(
        &exported,
        objects,
        metadata,
        &gardenbed_git::ExportOptions::default(),
    )
    .expect("non-UTF-8 history exports SHA-exact");
    assert!(
        roundtrip.commits.iter().any(|(_, git_sha)| git_sha == &c1),
        "the commit SHA roundtrips"
    );
    assert!(
        roundtrip.raw_trees_emitted >= 1,
        "the raw tree is emitted verbatim: {}",
        roundtrip.raw_trees_emitted
    );
    gardenbed_harness::git_fsck(&exported).expect("exported raw-name history is valid");
}

/// Two sibling names whose lossy UTF-8 decodings collide (`a\xff` and
/// `a\xfe` both become `a\u{FFFD}`) cannot live in one model tree: the
/// import must refuse loudly, never silently conflate two files. The
/// refusal comes from whichever guard fires first — the model-building
/// duplicate-path check (diff classification) or the raw-tree capture
/// walk's collision guard (divergent trees) — both loud, both before
/// anything is ingested.
#[test]
fn lossy_sibling_name_collision_refuses_loudly() {
    let b = Builder::new();

    let blob = b.blob(b"content\n");
    let mut bytes = Vec::new();
    for name in [&b"a\xff"[..], &b"a\xfe"[..]] {
        bytes.extend_from_slice(b"100644 ");
        bytes.extend_from_slice(name);
        bytes.push(0);
        bytes.extend_from_slice(&hex_bytes(&blob));
    }
    let tree = b.git_stdin(
        &["hash-object", "-t", "tree", "-w", "--stdin", "--literally"],
        &bytes,
    );
    let _c1 = b.commit_tree(&tree, &[], "colliding siblings\n");

    let garden = b._dir.path().join("garden");
    let (objects, metadata, engine) = open_stores(&garden);
    let err = import_from_path(
        &b.repo,
        objects.clone(),
        metadata.clone(),
        engine,
        &Default::default(),
    )
    .expect_err("the colliding-siblings tree must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("duplicate path") || msg.contains("collide after lossy UTF-8 decoding"),
        "the refusal names the collision: {msg}"
    );

    // Nothing was ingested: the store stays empty of changesets.
    let rows = common::sql_scalar(&garden, "SELECT count(*) FROM changesets");
    assert_eq!(rows, 0, "a refused import stores no changesets");
}

/// A tampered raw-tree row (direct DB edit — the only path that can
/// produce one, since `put_raw_tree` checks the hash): the export's
/// per-tree SHA assertion must refuse loudly, never emit wrong bytes.
#[test]
fn tampered_raw_tree_row_refuses_at_export() {
    let b = Builder::new();

    let old = b.blob(b"old content\n");
    let t1 = b.raw_tree(&[("100664", &old, "file.txt")]);
    let c1 = b.commit_tree(&t1, &[], "old world\n");
    let _ = c1;

    let garden = b._dir.path().join("garden");
    {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(&b.repo, objects, metadata, engine, &Default::default())
            .expect("import with raw-tree capture succeeds");
    }

    // Tamper: replace the captured row's bytes with a decompressible
    // frame whose bytes hash elsewhere (same shape as the row's frame).
    {
        let conn =
            rusqlite::Connection::open(garden.join("metadata.sqlite3")).expect("open metadata");
        let frame: Vec<u8> = conn
            .query_row("SELECT data FROM git_raw_tree", [], |r| r.get(0))
            .expect("the captured row exists");
        let mut decoded = zstd::bulk::decompress(&frame, 256 * 1024 * 1024).expect("decode");
        decoded.extend_from_slice(b"tampered");
        let tampered = zstd::bulk::compress(&decoded, 3).expect("re-encode");
        conn.execute(
            "UPDATE git_raw_tree SET data = ?1",
            rusqlite::params![tampered],
        )
        .expect("tamper");
    }

    let (objects, metadata, _) = open_stores(&garden);
    let exported = b._dir.path().join("tampered-export.git");
    let err = gardenbed_git::export_to_path(
        &exported,
        objects,
        metadata,
        &gardenbed_git::ExportOptions::default(),
    )
    .expect_err("a tampered raw-tree row must refuse the export");
    let msg = err.to_string();
    assert!(
        msg.contains("a tampered or corrupt row") || msg.contains("tampered"),
        "the refusal names the tampered row: {msg}"
    );
}

/// A native changeset committed on top of imported pre-canonical history
/// (the bed flow over raw-captured commits): the export must still
/// reproduce the imported SHAs for the old commits and emit the native
/// child — raw capture must not break the incremental path.
#[test]
fn native_changeset_on_raw_captured_history_exports() {
    let b = Builder::new();

    let old = b.blob(b"old content\n");
    let t1 = b.raw_tree(&[("100664", &old, "file.txt")]);
    let c1 = b.commit_tree(&t1, &[], "old world\n");

    let garden = b._dir.path().join("garden");
    {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(&b.repo, objects, metadata, engine, &Default::default())
            .expect("import succeeds");
    }
    {
        let (objects, metadata, engine) = open_stores(&garden);
        let cs1 = metadata.cs_for_git(&c1).unwrap().expect("mapped");

        // A native child: the bed flow — plain Modify through the model
        // API, validated and stored like the server does.
        let child = Changeset {
            parents: vec![cs1],
            changes: vec![gardenbed_model::Change {
                path: "file.txt".into(),
                op: Op::Modify,
                blob: Some(blob_id(b"native edit\n")),
                src_path: None,
                mode: Mode::Regular,
            }],
            message: "native child\n".into(),
            author: gardenbed_model::Identity {
                name: "Bed".into(),
                email: "bed@example.com".into(),
                time_secs: 1_800_000_000,
                tz_min: 0,
            },
            committer: gardenbed_model::Identity {
                name: "Bed".into(),
                email: "bed@example.com".into(),
                time_secs: 1_800_000_000,
                tz_min: 0,
            },
            signature: None,
        };
        engine.certify_history(cs1).expect("certify parent");
        objects.put_blob(b"native edit\n").expect("blob");
        let cs_child = child.id().expect("canonical");
        let root = engine.validated_tree_for_new(&child).expect("derive tree");
        let rows = engine
            .derived_rows_for_validated_tree(cs_child, &child, root)
            .expect("derived rows");
        metadata
            .put_validated_changeset(&child, root, &rows, None, None, None)
            .expect("put changeset");
        // Export walks refs: move main onto the native child (the bed
        // publish flow) so both commits are reachable.
        let won = metadata
            .cas_ref("refs/heads/main", Some(cs1), cs_child)
            .expect("cas");
        assert!(
            matches!(won, gardenbed_store::RefCas::Won { .. }),
            "the ref must move onto the native child"
        );
    }

    // Export: the imported commit keeps its SHA (raw tree), the child
    // exports after it (parents before children).
    let (objects, metadata, _) = open_stores(&garden);
    let exported = b._dir.path().join("native-child-export.git");
    let roundtrip = gardenbed_git::export_to_path(
        &exported,
        objects,
        metadata,
        &gardenbed_git::ExportOptions::default(),
    )
    .expect("native-on-raw history exports");
    assert!(
        roundtrip.commits.iter().any(|(_, git_sha)| git_sha == &c1),
        "the imported pre-canonical commit keeps its SHA"
    );
    assert_eq!(roundtrip.commits.len(), 2, "import + native child");
    gardenbed_harness::git_fsck(&exported).expect("exported history is valid");
}

/// Regression (found in review): two ordinary, header-reconstructable
/// commits sharing the SAME non-canonical root tree in one import. The
/// second commit inserts no new raw-tree row (the walk's memo decides
/// the shared root once), but the root still DIVERGES — raw-commit
/// capture must be forced from the divergence flag, never from the row
/// count, or the second commit exports with a canonical tree SHA and
/// breaks SHA-exactness.
#[test]
fn two_commits_sharing_noncanonical_root_export_exact() {
    let b = Builder::new();

    // One 100664 root tree; two commits point at it (git allows
    // metadata-only commits with the same tree).
    let old = b.blob(b"shared content\n");
    let t1 = b.raw_tree(&[("100664", &old, "file.txt")]);
    let c1 = b.commit_tree(&t1, &[], "first commit on the shared tree\n");
    let c2 = b.commit_tree(&t1, &[&c1], "second commit, same tree, headers fine\n");

    gardenbed_harness::git_fsck(&b.repo).expect("fsck accepts the hand-crafted trees");

    let garden = b._dir.path().join("garden");
    let report = {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(&b.repo, objects, metadata, engine, &Default::default())
            .expect("import succeeds")
    };
    assert_eq!(report.commits_new, 2);
    // ONE row (the shared root decided once), but BOTH commits diverged.
    assert_eq!(
        report.raw_trees_captured, 1,
        "the shared root is captured exactly once"
    );

    // Both commits export SHA-exact — the memo hit must not demote the
    // second commit to canonical reconstruction.
    let (objects, metadata, _) = open_stores(&garden);
    let exported = b._dir.path().join("shared-root-export.git");
    let roundtrip = gardenbed_git::export_to_path(
        &exported,
        objects,
        metadata,
        &gardenbed_git::ExportOptions::default(),
    )
    .expect("both commits export");
    for sha in [&c1, &c2] {
        assert!(
            roundtrip.commits.iter().any(|(_, git_sha)| git_sha == sha),
            "commit {sha} must export SHA-exact"
        );
    }
    assert_eq!(roundtrip.commits.len(), 2);
    gardenbed_harness::git_fsck(&exported).expect("exported history is valid");
}

/// Nested non-UTF-8 directory names: a non-UTF-8 dir entry whose subtree
/// contains a divergent grandchild (100664 file inside it). The capture
/// walk and the export both pair raw dir entries with model subtrees by
/// the LOSSY name (the model holds `d\u{FFFD}/…`), and the association
/// must resolve after the collision guard — otherwise the divergent
/// grandchild loses its model association and export cannot render the
/// canonical siblings.
#[test]
fn nested_non_utf8_dir_with_divergent_grandchild_roundtrips() {
    let b = Builder::new();

    let deep = b.blob(b"deep content\n");
    let plain = b.blob(b"plain sibling\n");

    // Inner tree: 100664 file `inner.txt` (divergent grandchild).
    let mut inner = Vec::new();
    inner.extend_from_slice(b"100664 inner.txt\0");
    inner.extend_from_slice(&hex_bytes(&deep));
    let inner_tree = b.git_stdin(
        &["hash-object", "-t", "tree", "-w", "--stdin", "--literally"],
        &inner,
    );

    // Outer tree: non-UTF-8 dir name `d\xe9` containing the inner tree,
    // plus a canonical sibling file.
    let dir_name: &[u8] = b"d\xe9";
    let mut outer = Vec::new();
    // git tree order: `d\xe9/` sorts before `plain.txt`.
    outer.extend_from_slice(b"40000 ");
    outer.extend_from_slice(dir_name);
    outer.push(0);
    outer.extend_from_slice(&hex_bytes(&inner_tree));
    outer.extend_from_slice(b"100644 plain.txt\0");
    outer.extend_from_slice(&hex_bytes(&plain));
    let outer_tree = b.git_stdin(
        &["hash-object", "-t", "tree", "-w", "--stdin", "--literally"],
        &outer,
    );
    let c1 = b.commit_tree(&outer_tree, &[], "nested non-utf8 dir\n");

    gardenbed_harness::git_fsck(&b.repo).expect("fsck accepts the nested trees");

    let garden = b._dir.path().join("garden");
    let report = {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(&b.repo, objects, metadata, engine, &Default::default())
            .expect("nested non-UTF-8 dirs import via raw capture")
    };
    assert_eq!(report.commits_new, 1);
    assert!(
        report.raw_trees_captured >= 2,
        "the non-UTF-8 outer tree AND the 100664 inner tree are captured: {}",
        report.raw_trees_captured
    );

    // Export: SHA-exact through the nested raw rows.
    let (objects, metadata, _) = open_stores(&garden);
    let exported = b._dir.path().join("nested-export.git");
    let roundtrip = gardenbed_git::export_to_path(
        &exported,
        objects,
        metadata,
        &gardenbed_git::ExportOptions::default(),
    )
    .expect("nested non-UTF-8 history exports SHA-exact");
    assert!(roundtrip.commits.iter().any(|(_, git_sha)| git_sha == &c1));
    gardenbed_harness::git_fsck(&exported).expect("exported nested history is valid");
}

/// Owner-bit mode rule (review finding 2): git normalizes pre-canonical
/// blob modes by the OWNER exec bit only — 100654 and 100645 become
/// 100644, only 100744 becomes 100755 (oracle:
/// /tmp/gardenbed-todo/mode-oracle/results.json). The model must agree:
/// group/other exec bits alone do NOT make a file Exec. A native child
/// of such a commit must export 100644, not 100755 — the model mode is
/// what leaks into natively-created history.
#[test]
fn group_exec_bit_modes_map_to_regular_and_native_children_export_644() {
    let b = Builder::new();

    let blob = b.blob(b"mode probe\n");
    let t1 = b.raw_tree(&[("100654", &blob, "f")]);
    let c1 = b.commit_tree(&t1, &[], "group-exec pre-canonical mode\n");
    // 100745: other-exec — also Regular by the owner-bit rule, and by
    // git's own normalization.
    let t2 = b.raw_tree(&[("100645", &blob, "g")]);
    let c2 = b.commit_tree(&t2, &[&c1], "other-exec pre-canonical mode\n");

    gardenbed_harness::git_fsck(&b.repo).expect("fsck accepts the hand-crafted trees");

    let garden = b._dir.path().join("garden");
    let report = {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(&b.repo, objects, metadata, engine, &Default::default())
            .expect("import succeeds")
    };
    assert_eq!(report.commits_new, 2);

    // The MODEL trees must classify both as Regular (flat paths f, g).
    // The model drops no entries here, so the derived tree contains
    // both files; the model mode is observable through the derived
    // rows' blob modes.
    let (_objects, metadata, _engine) = open_stores(&garden);
    let cs2 = metadata.cs_for_git(&c2).unwrap().expect("c2 mapped");
    let cs = metadata.get_changeset(cs2).unwrap().expect("present").0;
    let f_mode = cs
        .changes
        .iter()
        .find(|c| c.path == "g")
        .expect("path g in the model")
        .mode;
    assert_eq!(f_mode, Mode::Regular, "100645 (other-exec) maps to Regular");
    let cs1 = metadata.cs_for_git(&c1).unwrap().expect("c1 mapped");
    let cs1d = metadata.get_changeset(cs1).unwrap().expect("present").0;
    let f1_mode = cs1d
        .changes
        .iter()
        .find(|c| c.path == "f")
        .expect("path f in the model")
        .mode;
    assert_eq!(
        f1_mode,
        Mode::Regular,
        "100654 (group-exec) maps to Regular"
    );

    // A native descendant of c2 re-modifies f: its exported mode must
    // be 100644 (the model says Regular), never 100755.
    {
        let (objects, metadata, engine) = open_stores(&garden);
        let child = Changeset {
            parents: vec![cs2],
            changes: vec![gardenbed_model::Change {
                path: "f".into(),
                op: Op::Modify,
                blob: Some(blob_id(b"native edit\n")),
                src_path: None,
                mode: Mode::Regular,
            }],
            message: "native child of other-exec history\n".into(),
            author: gardenbed_model::Identity {
                name: "Bed".into(),
                email: "bed@example.com".into(),
                time_secs: 1_800_000_000,
                tz_min: 0,
            },
            committer: gardenbed_model::Identity {
                name: "Bed".into(),
                email: "bed@example.com".into(),
                time_secs: 1_800_000_000,
                tz_min: 0,
            },
            signature: None,
        };
        engine.certify_history(cs2).expect("certify parent");
        objects.put_blob(b"native edit\n").expect("blob");
        let cs_child = child.id().expect("canonical");
        let root = engine.validated_tree_for_new(&child).expect("derive tree");
        let rows = engine
            .derived_rows_for_validated_tree(cs_child, &child, root)
            .expect("derived rows");
        metadata
            .put_validated_changeset(&child, root, &rows, None, None, None)
            .expect("put changeset");
        let won = metadata
            .cas_ref("refs/heads/main", Some(cs2), cs_child)
            .expect("cas");
        assert!(matches!(won, gardenbed_store::RefCas::Won { .. }));
    }

    let (objects, metadata, _) = open_stores(&garden);
    let exported = b._dir.path().join("mode-native-child-export.git");
    let roundtrip = gardenbed_git::export_to_path(
        &exported,
        objects,
        metadata,
        &gardenbed_git::ExportOptions::default(),
    )
    .expect("native-on-group-exec history exports");
    assert_eq!(roundtrip.commits.len(), 3, "two imports + native child");
    for sha in [&c1, &c2] {
        assert!(
            roundtrip.commits.iter().any(|(_, git_sha)| git_sha == sha),
            "commit {sha} keeps its SHA through raw capture"
        );
    }
    gardenbed_harness::git_fsck(&exported).expect("exported history is valid");

    // The exported native child carries f as 100644 — the observable
    // mode in any downstream clone (run against the EXPORT, not the
    // source repo).
    let mut cmd = Command::new("git");
    cmd.current_dir(&exported)
        .arg("ls-tree")
        .arg("refs/heads/main")
        .arg("--")
        .arg("f");
    hermetic(&mut cmd);
    let out = cmd.output().expect("ls-tree runs");
    assert!(out.status.success(), "ls-tree failed");
    let modes = String::from_utf8_lossy(&out.stdout);
    assert!(
        modes.starts_with("100644"),
        "the native child exports f as 100644, not 100755: {modes}"
    );
}

/// Empty directories (review finding 3): the model drops them, the raw
/// tree keeps them. A commit whose tree carries an empty dir DIVERGES
/// from the canonical render of what the model derives, so the capture
/// predicate must fire (root diverged ⇒ raw commit captured ⇒ SHA-exact
/// roundtrip). The old predicate compared against render_canonical of
/// the RAW entries — which preserves the empty-dir entry — so it said
/// "no divergence," nothing was captured, and export refused loudly.
#[test]
fn empty_dir_commit_roundtrips_sha_exact() {
    let b = Builder::new();

    let blob = b.blob(b"file next to the empty dir\n");
    let empty = b.raw_tree(&[]); // the empty dir's own tree object
    let root = b.raw_tree(&[("40000", &empty, "d"), ("100644", &blob, "f.txt")]);
    let c1 = b.commit_tree(&root, &[], "commit with an empty directory\n");

    gardenbed_harness::git_fsck(&b.repo).expect("fsck accepts the hand-crafted trees");

    let garden = b._dir.path().join("garden");
    let report = {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(&b.repo, objects, metadata, engine, &Default::default())
            .expect("import succeeds")
    };
    assert_eq!(report.commits_new, 1);
    assert!(
        report.raw_trees_captured >= 2,
        "the root AND the empty dir's tree are captured: {}",
        report.raw_trees_captured
    );

    // Export: SHA-exact through the raw rows (the empty dir survives).
    let (objects, metadata, _) = open_stores(&garden);
    let exported = b._dir.path().join("empty-dir-export.git");
    let roundtrip = gardenbed_git::export_to_path(
        &exported,
        objects,
        metadata,
        &gardenbed_git::ExportOptions::default(),
    )
    .expect("empty-dir history exports SHA-exact");
    assert!(roundtrip.commits.iter().any(|(_, git_sha)| git_sha == &c1));
    gardenbed_harness::git_fsck(&exported).expect("exported history is valid");
}

/// Memo-upgrade regression (review finding 3, follow-up): in ONE import
/// batch, c0 has the EMPTY root tree, c1 adds only an empty directory
/// `d` (whose child is that same empty-tree SHA), and c2 returns to the
/// empty root. The empty-tree SHA is first memoized as a canonical ROOT
/// (c0: byte-exact, no row); c1's walk then needs it as a ROW — the
/// captured parent names `d` and the export walk resolves dir entries
/// through raw-tree rows. A forced-child capture must UPGRADE the stale
/// no-row memo decision, not be suppressed by it; c2's memo hit (now
/// "captured") must force c2's own raw-commit capture so it too exports
/// SHA-exact.
#[test]
fn empty_tree_sha_memo_upgrade_from_canonical_root_to_dropped_child() {
    let b = Builder::new();

    let empty = b.raw_tree(&[]);
    let root_with_d = b.raw_tree(&[("40000", &empty, "d")]);
    let c0 = b.commit_tree(&empty, &[], "c0: empty root\n");
    let c1 = b.commit_tree(&root_with_d, &[&c0], "c1: add empty dir d\n");
    let c2 = b.commit_tree(&empty, &[&c1], "c2: back to empty root\n");

    gardenbed_harness::git_fsck(&b.repo).expect("fsck accepts the hand-crafted trees");

    let garden = b._dir.path().join("garden");
    let report = {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(&b.repo, objects, metadata, engine, &Default::default())
            .expect("import succeeds")
    };
    assert_eq!(report.commits_new, 3);
    // The empty-tree SHA: one row (upgraded when c1's `d` needs it),
    // plus c1's root carrying `d` — the exact count is the row upgrade
    // (empty tree) + the parent (root with d); c0 and c2 add nothing.
    assert_eq!(
        report.raw_trees_captured, 2,
        "empty-tree row upgraded from the no-row root decision + the root with d"
    );

    // ALL THREE commits export SHA-exact.
    let (objects, metadata, _) = open_stores(&garden);
    let exported = b._dir.path().join("empty-tree-upgrade-export.git");
    let roundtrip = gardenbed_git::export_to_path(
        &exported,
        objects,
        metadata,
        &gardenbed_git::ExportOptions::default(),
    )
    .expect("the empty-tree memo-upgrade history exports SHA-exact");
    assert_eq!(roundtrip.commits.len(), 3);
    for sha in [&c0, &c1, &c2] {
        assert!(
            roundtrip.commits.iter().any(|(_, git_sha)| git_sha == sha),
            "commit {sha} must export SHA-exact"
        );
    }
    gardenbed_harness::git_fsck(&exported).expect("exported history is valid");
}

/// Empty-dir-only ROOT commit (review finding 3): a root whose tree
/// contains only empty directories — git's diff sees no blobs, so the
/// classifier produces an empty diff. The model legitimately derives
/// the empty tree (it drops empty dirs); the raw-tree capture walk
/// preserves the directories so the commit exports SHA-exact. The old
/// guard refused this shape ("produced no diff although its tree is not
/// empty") because it counted RAW tree entries; it now refuses only
/// when the root's FILE closure is non-empty — a genuinely unexpected
/// empty diff (real files git somehow did not report) still refuses.
#[test]
fn empty_dir_only_root_commit_roundtrips_sha_exact() {
    let b = Builder::new();

    // Nested empty dirs: d/ contains only an empty dir e/.
    let empty = b.raw_tree(&[]);
    let d_tree = b.raw_tree(&[("40000", &empty, "e")]);
    let root = b.raw_tree(&[("40000", &d_tree, "d")]);
    let c0 = b.commit_tree(&root, &[], "root with only nested empty dirs\n");
    // Metadata reuse on the same root: a second commit pointing at the
    // same tree must also export SHA-exact (memo decisions are reused,
    // no double rows).
    let c1 = b.commit_tree(&root, &[&c0], "same tree again\n");

    gardenbed_harness::git_fsck(&b.repo).expect("fsck accepts the hand-crafted trees");

    let garden = b._dir.path().join("garden");
    let report = {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(&b.repo, objects, metadata, engine, &Default::default())
            .expect("an empty-dir-only root imports — its file closure is empty")
    };
    assert_eq!(report.commits_new, 2);
    // Rows: the empty tree `e`, the dir tree `d` (contains e), and the
    // root (contains d) — all captured; the second commit adds none.
    assert_eq!(
        report.raw_trees_captured, 3,
        "e + d + root captured once: {}",
        report.raw_trees_captured
    );

    // Both commits export SHA-exact through the raw rows.
    let (objects, metadata, _) = open_stores(&garden);
    let exported = b._dir.path().join("empty-root-export.git");
    let roundtrip = gardenbed_git::export_to_path(
        &exported,
        objects,
        metadata,
        &gardenbed_git::ExportOptions::default(),
    )
    .expect("the empty-dir-only-root history exports SHA-exact");
    assert_eq!(roundtrip.commits.len(), 2);
    for sha in [&c0, &c1] {
        assert!(
            roundtrip.commits.iter().any(|(_, git_sha)| git_sha == sha),
            "commit {sha} must export SHA-exact"
        );
    }
    gardenbed_harness::git_fsck(&exported).expect("exported history is valid");
}

/// Legacy-upgrade association regression (root compatibility blocker,
/// R2 hard blocker): a store imported by an OLD build (pre-owner-bit
/// rule) holds a legacy `Exec` model for a `100654` git entry — the
/// legacy seed is EXACTLY what the old binary wrote: canonical Exec
/// changeset + git mapping + ref, and NOTHING else (verified against
/// the real control-binary probe: no raw commit row, no raw-tree row
/// for the legacy root c0). The upgraded build then imports NEW git
/// history: c1 (metadata-only descendant on `main`, inheriting the
/// legacy Exec frontier — its capture walk writes the raw-tree row
/// with the legacy-Exec association) and c2 (independent fresh root on
/// `refs/heads/other` reusing the SAME git tree SHA, deriving Regular
/// under the owner-bit rule). The import must NOT fail on the
/// association clash: the raw-tree association is first-seen ADVISORY
/// (one git tree legitimately normalizes to different model trees
/// across a legacy frontier and a fresh import), while the BYTES stay
/// content-addressed and immutable.
///
/// Honest export boundary (matches the root probe's actual export log,
/// export-correct-root.log): the OLD `main` history still contains the
/// legacy c0 whose canonical reconstruction diverges (Exec model vs
/// 100654 bytes) and holds NO raw rows — exporting `main` keeps
/// refusing loudly (the known pre-canonical divergence, retired only
/// by fresh full re-import; see docs/divergences.md). The freshly
/// imported `other` branch — every commit of which the CURRENT build
/// captured verbatim — exports SHA-exact for its own ref.
#[test]
fn legacy_exec_frontier_and_fresh_regular_import_coexist() {
    let b = Builder::new();

    let blob = b.blob(b"content\n");
    // One 100654 entry: owner bit clear => Regular under the owner-bit
    // rule; the legacy rule mapped it Exec.
    let shared_tree = b.raw_tree(&[("100654", &blob, "f")]);
    let c0 = b.commit_tree(&shared_tree, &[], "legacy root\n");
    let c1 = b.commit_tree(&shared_tree, &[&c0], "incremental metadata only\n");
    // Independent fresh root, same git tree, on its OWN branch.
    let c2 = b.commit_tree(&shared_tree, &[], "fresh independent root\n");
    // commit_tree left main on c2; restore the probe's ref topology:
    // main at c1 (legacy + descendant), other at c2 (fresh).
    b.git(&["update-ref", "refs/heads/main", &c1]);
    b.git(&["update-ref", "refs/heads/other", &c2]);

    let garden = b._dir.path().join("garden");
    {
        // --- The LEGACY seed: exactly what the old binary wrote ---
        let (objects, metadata, engine) = open_stores(&garden);
        let legacy_cs = Changeset {
            parents: vec![],
            changes: vec![gardenbed_model::Change {
                path: "f".into(),
                op: Op::Add,
                blob: Some(blob_id(b"content\n")),
                src_path: None,
                mode: Mode::Exec,
            }],
            message: "legacy root\n".into(),
            author: gardenbed_model::Identity {
                name: "Legacy".into(),
                email: "legacy@example.test".into(),
                time_secs: 1,
                tz_min: 0,
            },
            committer: gardenbed_model::Identity {
                name: "Legacy".into(),
                email: "legacy@example.test".into(),
                time_secs: 1,
                tz_min: 0,
            },
            signature: None,
        };
        objects.put_blob(b"content\n").expect("blob");
        let cs_id = legacy_cs.id().expect("canonical");
        let root = engine
            .validated_tree_for_new(&legacy_cs)
            .expect("derive legacy tree");
        let rows = engine
            .derived_rows_for_validated_tree(cs_id, &legacy_cs, root)
            .expect("derived rows");
        metadata
            .put_validated_changeset(&legacy_cs, root, &rows, Some(&c0), None, None)
            .expect("legacy put: canonical Exec CS + git mapping only");
        metadata
            .cas_ref("refs/heads/main", None, cs_id)
            .expect("legacy ref");
        // NO raw commit row, NO raw-tree row: the old binary captured
        // none for c0 (verified against the real control probe store).
    }

    // --- Upgraded build imports BOTH branches (single real import,
    // main first: c0 skipped via mapping, c1 inherits the legacy Exec
    // frontier and captures the raw-tree row with the Exec
    // association; then other: c2 derives Regular for the SAME git
    // tree bytes — the old clash point, must now succeed). ---
    let report = {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(
            &b.repo,
            objects,
            metadata,
            engine,
            &ImportOptions::default(),
        )
        .expect("the upgraded import must not fail on the legacy frontier")
    };
    assert_eq!(report.commits_new, 2, "c1 and c2; c0 is mapped-skipped");
    assert_eq!(report.raw_trees_captured, 1, "the shared divergent tree");

    // The association column keeps the FIRST-seen (legacy Exec) value
    // while the fresh Regular branch imported fine: advisory, observable.
    {
        let (_objects, metadata, _engine) = open_stores(&garden);
        let key: [u8; 20] = hex_bytes(&shared_tree)
            .as_slice()
            .try_into()
            .expect("20-byte sha");
        let assoc = metadata.raw_tree_model_id(key).unwrap().expect("row");
        // The first-seen association is a real model tree id (c1's
        // capture on main landed before c2's on other): advisory but
        // observable.
        assert!(assoc.is_some(), "association stays a real model tree");
    }

    // c1 carries the ONLY raw commit row (probe-verified); the fresh
    // branch c2 was captured verbatim by the current build.
    {
        let (_objects, metadata, _engine) = open_stores(&garden);
        let cs_c1 = metadata.cs_for_git(&c1).unwrap().expect("c1 mapped");
        assert!(
            metadata.raw_commit(cs_c1).unwrap().is_some(),
            "c1 has a raw commit row"
        );
        let cs_c2 = metadata.cs_for_git(&c2).unwrap().expect("c2 mapped");
        assert!(
            metadata.raw_commit(cs_c2).unwrap().is_some(),
            "c2 has a raw commit row"
        );
    }

    // --- Export boundary, honestly ---
    // The freshly imported `other` branch exports SHA-exact for its
    // own ref (its commits were captured verbatim by this build).
    {
        let (objects, metadata, _) = open_stores(&garden);
        let exported = b._dir.path().join("legacy-upgrade-fresh-export.git");
        let opts = gardenbed_git::ExportOptions {
            refs: vec!["refs/heads/other".into()],
        };
        let roundtrip = gardenbed_git::export_to_path(&exported, objects, metadata, &opts)
            .expect("the fresh branch exports SHA-exact");
        assert_eq!(roundtrip.commits.len(), 1);
        assert_eq!(
            roundtrip.refs,
            vec![("refs/heads/other".into(), c2.clone())]
        );
        gardenbed_harness::git_fsck(&exported).expect("exported fresh branch is valid");
    }
    // The OLD `main` history still refuses loudly: c0's canonical
    // reconstruction diverges (legacy Exec model vs its 100654 bytes)
    // and it holds no raw rows — the known pre-canonical divergence,
    // retired only by a fresh full re-import (root probe's actual
    // export-correct-root.log: exit 1, SHA divergence on c0).
    {
        let (objects, metadata, _) = open_stores(&garden);
        let exported = b._dir.path().join("legacy-upgrade-main-export.git");
        let opts = gardenbed_git::ExportOptions {
            refs: vec!["refs/heads/main".into()],
        };
        let err = gardenbed_git::export_to_path(&exported, objects, metadata, &opts)
            .expect_err("the legacy main history must refuse loudly, not silently diverge");
        let msg = format!("{err}");
        assert!(
            msg.contains("reconstruction diverges") || msg.contains("computed git SHA"),
            "loud SHA-divergence refusal, got: {msg}"
        );
    }
}

/// Fresh-first order: the upgraded build imports the fresh Regular
/// branch BEFORE the legacy state exists, then the legacy Exec
/// frontier arrives (an old binary's incremental write into the
/// now-upgraded store) for the SAME git tree bytes. Both orders must
/// coexist — the association is advisory in either direction.
#[test]
fn fresh_regular_import_then_legacy_exec_frontier_coexist() {
    let b = Builder::new();

    let blob = b.blob(b"content\n");
    let shared_tree = b.raw_tree(&[("100654", &blob, "f")]);
    let c0 = b.commit_tree(&shared_tree, &[], "legacy root\n");
    let c1 = b.commit_tree(&shared_tree, &[&c0], "incremental metadata only\n");
    let c2 = b.commit_tree(&shared_tree, &[], "fresh independent root\n");
    b.git(&["update-ref", "refs/heads/main", &c1]);
    b.git(&["update-ref", "refs/heads/other", &c2]);

    let garden = b._dir.path().join("garden");
    {
        // Fresh import FIRST (only `other`: Regular model for the 100654
        // entry — c2's raw rows land now).
        let (objects, metadata, engine) = open_stores(&garden);
        let opts = ImportOptions {
            refs: vec!["refs/heads/other".into()],
            ..Default::default()
        };
        let report =
            import_from_path(&b.repo, objects, metadata, engine, &opts).expect("fresh import");
        assert_eq!(report.commits_new, 1);
        assert_eq!(report.raw_trees_captured, 1, "the 100654 tree diverges");
    }
    {
        // Then the legacy frontier: an old binary's incremental import
        // of `main` — canonical Exec CS + mapping + raw capture for the
        // same git bytes with the OLD Exec model — through public APIs
        // (exactly the state the old control binary leaves behind).
        let (objects, metadata, engine) = open_stores(&garden);
        let legacy_cs = Changeset {
            parents: vec![],
            changes: vec![gardenbed_model::Change {
                path: "f".into(),
                op: Op::Add,
                blob: Some(blob_id(b"content\n")),
                src_path: None,
                mode: Mode::Exec,
            }],
            message: "legacy root\n".into(),
            author: gardenbed_model::Identity {
                name: "Legacy".into(),
                email: "legacy@example.test".into(),
                time_secs: 1,
                tz_min: 0,
            },
            committer: gardenbed_model::Identity {
                name: "Legacy".into(),
                email: "legacy@example.test".into(),
                time_secs: 1,
                tz_min: 0,
            },
            signature: None,
        };
        objects.put_blob(b"content\n").expect("blob");
        let cs_id = legacy_cs.id().expect("canonical");
        let root = engine
            .validated_tree_for_new(&legacy_cs)
            .expect("derive legacy tree");
        let rows = engine
            .derived_rows_for_validated_tree(cs_id, &legacy_cs, root)
            .expect("derived rows");
        metadata
            .put_validated_changeset(&legacy_cs, root, &rows, Some(&c0), None, None)
            .expect("legacy mapping into the upgraded store");
        metadata
            .cas_ref("refs/heads/main", None, cs_id)
            .expect("legacy ref");
        // The legacy capture walk's raw-tree put for the shared tree
        // with the Exec association: same bytes, different association
        // — idempotent Ok under the advisory rule (this is the exact
        // clash that used to fail the import in the other order).
        let tree_bytes = {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(b"100654 f\0");
            bytes.extend_from_slice(&hex_bytes(&blob));
            bytes
        };
        metadata
            .put_raw_tree(
                hex_bytes(&shared_tree)
                    .as_slice()
                    .try_into()
                    .expect("20-byte sha"),
                root,
                &tree_bytes,
            )
            .expect("legacy same-bytes put is an idempotent no-op");
    }
    {
        // Now the upgraded build imports the REST of main (c1, the
        // metadata-only descendant): the memo hits the shared tree
        // (already captured with the fresh Regular association) with
        // the LEGACY Exec context — must succeed.
        let (objects, metadata, engine) = open_stores(&garden);
        let report = import_from_path(
            &b.repo,
            objects,
            metadata,
            engine,
            &ImportOptions::default(),
        )
        .expect("import after the legacy frontier must not fail");
        assert_eq!(report.commits_new, 1, "only c1 remains");
    }

    // The fresh branch still exports SHA-exact after the legacy
    // frontier landed in the same store.
    {
        let (objects, metadata, _) = open_stores(&garden);
        let exported = b._dir.path().join("fresh-then-legacy-export.git");
        let opts = gardenbed_git::ExportOptions {
            refs: vec!["refs/heads/other".into()],
        };
        let roundtrip = gardenbed_git::export_to_path(&exported, objects, metadata, &opts)
            .expect("the fresh branch still exports SHA-exact");
        assert_eq!(roundtrip.commits.len(), 1);
        assert_eq!(
            roundtrip.refs,
            vec![("refs/heads/other".into(), c2.clone())]
        );
        gardenbed_harness::git_fsck(&exported).expect("exported fresh branch is valid");
    }
}

/// Single-batch variant: the fresh branch and the legacy descendant
/// arrive in ONE import batch after the legacy seed — the capture
/// walk's memo sees the same git tree SHA under two model contexts
/// inside one walk instance. Plus: a native child on the fresh branch
/// and a reopen/re-export.
#[test]
fn single_batch_import_of_legacy_and_fresh_frontiers() {
    let b = Builder::new();

    let blob = b.blob(b"content\n");
    let shared_tree = b.raw_tree(&[("100654", &blob, "f")]);
    let c0 = b.commit_tree(&shared_tree, &[], "legacy root\n");
    let c1 = b.commit_tree(&shared_tree, &[&c0], "incremental metadata only\n");
    let c2 = b.commit_tree(&shared_tree, &[], "fresh independent root\n");
    b.git(&["update-ref", "refs/heads/main", &c1]);
    b.git(&["update-ref", "refs/heads/other", &c2]);

    let garden = b._dir.path().join("garden");
    // Legacy seed (CS + mapping + ref, NO raw rows).
    let _cs_legacy = {
        let (objects, metadata, engine) = open_stores(&garden);
        let legacy_cs = Changeset {
            parents: vec![],
            changes: vec![gardenbed_model::Change {
                path: "f".into(),
                op: Op::Add,
                blob: Some(blob_id(b"content\n")),
                src_path: None,
                mode: Mode::Exec,
            }],
            message: "legacy root\n".into(),
            author: gardenbed_model::Identity {
                name: "Legacy".into(),
                email: "legacy@example.test".into(),
                time_secs: 1,
                tz_min: 0,
            },
            committer: gardenbed_model::Identity {
                name: "Legacy".into(),
                email: "legacy@example.test".into(),
                time_secs: 1,
                tz_min: 0,
            },
            signature: None,
        };
        objects.put_blob(b"content\n").expect("blob");
        let cs_id = legacy_cs.id().expect("canonical");
        let root = engine
            .validated_tree_for_new(&legacy_cs)
            .expect("derive legacy tree");
        let rows = engine
            .derived_rows_for_validated_tree(cs_id, &legacy_cs, root)
            .expect("derived rows");
        metadata
            .put_validated_changeset(&legacy_cs, root, &rows, Some(&c0), None, None)
            .expect("legacy put");
        metadata
            .cas_ref("refs/heads/main", None, cs_id)
            .expect("legacy ref");
        cs_id
    };

    // Single batch: c1 (legacy Exec context) and c2 (fresh Regular
    // context) in ONE import — the memo handles the same tree SHA
    // under both contexts.
    let report = {
        let (objects, metadata, engine) = open_stores(&garden);
        import_from_path(
            &b.repo,
            objects,
            metadata,
            engine,
            &ImportOptions::default(),
        )
        .expect("single batch import")
    };
    assert_eq!(report.commits_new, 2);
    assert_eq!(
        report.raw_trees_captured, 1,
        "the shared tree captured once"
    );

    // Fresh branch export stays SHA-exact.
    {
        let (objects, metadata, _) = open_stores(&garden);
        let exported = b._dir.path().join("single-batch-export.git");
        let opts = gardenbed_git::ExportOptions {
            refs: vec!["refs/heads/other".into()],
        };
        let roundtrip = gardenbed_git::export_to_path(&exported, objects, metadata, &opts)
            .expect("the fresh branch exports SHA-exact");
        assert_eq!(roundtrip.commits.len(), 1);
        assert_eq!(
            roundtrip.refs,
            vec![("refs/heads/other".into(), c2.clone())]
        );
        gardenbed_harness::git_fsck(&exported).expect("exported history is valid");
    }

    // Native child on the fresh branch (the model now says Regular).
    let cs_c2 = {
        let (_objects, metadata, _engine) = open_stores(&garden);
        metadata.cs_for_git(&c2).unwrap().expect("c2 mapped")
    };
    {
        let (objects, metadata, engine) = open_stores(&garden);
        let child = Changeset {
            parents: vec![cs_c2],
            changes: vec![gardenbed_model::Change {
                path: "f".into(),
                op: Op::Modify,
                blob: Some(blob_id(b"native edit\n")),
                src_path: None,
                mode: Mode::Regular,
            }],
            message: "native child\n".into(),
            author: gardenbed_model::Identity {
                name: "Bed".into(),
                email: "bed@example.com".into(),
                time_secs: 1_800_000_000,
                tz_min: 0,
            },
            committer: gardenbed_model::Identity {
                name: "Bed".into(),
                email: "bed@example.com".into(),
                time_secs: 1_800_000_000,
                tz_min: 0,
            },
            signature: None,
        };
        engine.certify_history(cs_c2).expect("certify parent");
        objects.put_blob(b"native edit\n").expect("blob");
        let cs_child = child.id().expect("canonical");
        let root = engine.validated_tree_for_new(&child).expect("derive tree");
        let rows = engine
            .derived_rows_for_validated_tree(cs_child, &child, root)
            .expect("derived rows");
        metadata
            .put_validated_changeset(&child, root, &rows, None, None, None)
            .expect("put changeset");
        let won = metadata
            .cas_ref("refs/heads/other", Some(cs_c2), cs_child)
            .expect("cas");
        assert!(matches!(won, gardenbed_store::RefCas::Won { .. }));
    }

    // Reopen and re-export the fresh branch WITH the native child:
    // c2 keeps its SHA (captured verbatim) and the native child
    // exports after it.
    {
        let (objects, metadata, _) = open_stores(&garden);
        let exported2 = b._dir.path().join("single-batch-native-export.git");
        let opts = gardenbed_git::ExportOptions {
            refs: vec!["refs/heads/other".into()],
        };
        let roundtrip2 = gardenbed_git::export_to_path(&exported2, objects, metadata, &opts)
            .expect("native child on the fresh branch exports");
        assert_eq!(roundtrip2.commits.len(), 2, "c2 + native child");
        assert_eq!(roundtrip2.refs.len(), 1);
        assert_eq!(roundtrip2.refs[0].0, "refs/heads/other");
        gardenbed_harness::git_fsck(&exported2).expect("exported history is valid");
    }
}
