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
            String::from_utf8(out.stdout).expect("utf8").trim().to_owned()
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
/// `Mode::Regular` in gardenbed's model) inheriting the parent tree,
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
        import_from_path(&b.repo, objects, metadata, engine, &ImportOptions::default())
            .expect("the mode-only fixup must not hard-error")
    };
    assert_eq!(report.commits_new, 4, "every commit ingested");
    assert_eq!(report.commits_skipped, 0);

    let (_, metadata, engine) = open_stores(&garden);
    let cs_of = |sha: &str| -> (Id, Changeset) {
        let id = metadata.cs_for_git(sha).expect("cs_for_git").expect("mapped");
        let cs = metadata.get_changeset(id).expect("get").expect("exists").0;
        (id, cs)
    };
    let (cs1_id, cs1) = cs_of(&c1);
    let (cs2_id, cs2) = cs_of(&c2);
    let (cs3_id, cs3) = cs_of(&c3);
    let (cs4_id, cs4) = cs_of(&c4);

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
    let t1 = engine.derive_tree(cs1_id).expect("derive").expect("non-empty");
    let t2 = engine.derive_tree(cs2_id).expect("derive").expect("non-empty");
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
        import_from_path(&b.repo, objects, metadata, engine, &ImportOptions::default())
            .expect("re-import succeeds")
    };
    assert_eq!(second.commits_new, 0, "nothing re-ingested");
    assert_eq!(second.commits_skipped, 1, "the walk prunes at the tip");
    assert_eq!(second.objects_written, 0, "a re-import writes zero objects");
    assert!(second.refs_updated.is_empty(), "refs must not move");
}
