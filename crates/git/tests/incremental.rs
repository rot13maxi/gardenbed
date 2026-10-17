//! Incrementality, force pushes, and the exact object-write accounting.

mod common;

use std::io::Write as _;

use common::{
    Fixture, git_map_rows, import, object_set, open_stores, ref_rows, sql_scalar, try_import,
};
use gardenbed_git::{ImportError, ImportOptions};
use gardenbed_harness::GenOptions;
use gardenbed_store::{FsObjectStore, MetadataStore};

/// Appends `n` new commits on top of `refs/heads/main` using git plumbing
/// (deterministic content), returning the new tip sha.
fn append_commits(repo: &std::path::Path, n: usize) -> String {
    let hermetic = |cmd: &mut std::process::Command| {
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(&k);
            }
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    };
    let git = |args: &[&str]| -> String {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(repo);
        hermetic(&mut cmd);
        let out = cmd.args(args).output().expect("git runs");
        assert!(out.status.success(), "git {:?} failed", args);
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    };

    let mut parent = git(&["rev-parse", "refs/heads/main"]);
    for i in 0..n {
        let name = format!("added-later-{i}.txt");
        let content = format!("content appended after the first import ({i})\n");
        let blob = {
            let mut cmd = std::process::Command::new("git");
            cmd.current_dir(repo);
            hermetic(&mut cmd);
            cmd.args(["hash-object", "-w", "--stdin"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
            let mut child = cmd.spawn().expect("hash-object");
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(content.as_bytes())
                .expect("write blob");
            let out = child.wait_with_output().expect("hash-object output");
            String::from_utf8(out.stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        };
        git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},{name}"),
        ]);
        let tree = git(&["write-tree"]);
        // commit-tree with the message on stdin (verbatim, like the
        // harness materializer) and a fixed identity — the hermetic env
        // strips any ambient git config, so identity must come from env.
        parent = {
            let mut cmd = std::process::Command::new("git");
            cmd.current_dir(repo);
            hermetic(&mut cmd);
            cmd.args(["commit-tree", &tree, "-p", &parent])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .env("GIT_AUTHOR_NAME", "Later Author")
                .env("GIT_AUTHOR_EMAIL", "later@example.com")
                .env("GIT_COMMITTER_NAME", "Later Committer")
                .env("GIT_COMMITTER_EMAIL", "later@example.com");
            let mut child = cmd.spawn().expect("commit-tree");
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(format!("appended commit {i}\n").as_bytes())
                .expect("write message");
            let out = child.wait_with_output().expect("commit-tree output");
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
        git(&["update-ref", "refs/heads/main", &parent]);
    }
    parent
}

/// Importing an unchanged repository twice writes zero new objects and
/// moves no refs.
#[test]
fn unchanged_reimport_is_free() {
    let fixture = Fixture::generate(2024, &GenOptions::default());
    let first = import(&fixture.repo, &fixture.garden, &Default::default());
    let objects_after_first = object_set(&fixture.garden);

    let second = import(&fixture.repo, &fixture.garden, &Default::default());
    // The walk prunes at the mapped frontier: it reaches only the tip.
    assert_eq!(second.commits_total, 1);
    assert_eq!(second.commits_new, 0, "no commit should be re-ingested");
    assert_eq!(
        second.commits_skipped, 1,
        "the tip is skipped at the frontier"
    );
    assert_eq!(
        second.commits_total,
        second.commits_new + second.commits_skipped
    );
    assert_eq!(second.objects_written, 0, "no object should be rewritten");
    assert!(second.refs_updated.is_empty(), "refs must not move");
    assert_eq!(object_set(&fixture.garden), objects_after_first);
    let _ = first;
}

/// Adding commits upstream and re-importing writes exactly the new
/// commits' objects — and a fresh full import of the extended repository
/// produces a byte-identical store (same objects, same mappings, same
/// refs) as the incremental one.
#[test]
fn incremental_extension_matches_full_import() {
    let fixture = Fixture::generate(2077, &GenOptions::default());
    let base_commits = fixture.history.changesets.len();
    let first = import(&fixture.repo, &fixture.garden, &Default::default());
    assert_eq!(first.commits_new as usize, base_commits);

    let objects_before = object_set(&fixture.garden);
    append_commits(&fixture.repo, 5);
    let second = import(&fixture.repo, &fixture.garden, &Default::default());
    assert_eq!(second.commits_new, 5, "exactly the new commits");
    assert_eq!(
        second.commits_skipped, 1,
        "the mapped parent is the frontier"
    );
    assert_eq!(second.commits_total, 6, "new commits plus the frontier");
    assert_eq!(
        second.commits_total,
        second.commits_new + second.commits_skipped
    );
    // Exact accounting: the report equals the on-disk objects delta.
    let objects_after = object_set(&fixture.garden);
    let delta = (objects_after.len() - objects_before.len()) as u64;
    assert_eq!(
        delta, second.objects_written,
        "objects_written == objects delta"
    );
    // Something must have been written (blobs + trees), and it must be
    // far fewer objects than a full re-import would touch.
    assert!(delta > 0);

    // A fresh store, importing the FULL extended history from scratch,
    // must converge on exactly the same store contents.
    let fresh_garden = fixture.dir.path().join("garden-fresh");
    import(&fixture.repo, &fresh_garden, &Default::default());
    assert_eq!(
        object_set(&fresh_garden),
        objects_after,
        "object sets identical"
    );
    assert_eq!(git_map_rows(&fresh_garden), git_map_rows(&fixture.garden));
    assert_eq!(ref_rows(&fresh_garden), ref_rows(&fixture.garden));

    // And re-importing the extended repo into the incremental store is
    // still free.
    let third = import(&fixture.repo, &fixture.garden, &Default::default());
    assert_eq!(third.commits_new, 0);
    assert_eq!(third.objects_written, 0);
}

/// A force push (rewritten tip) re-imports cleanly: refs move, old
/// objects and mapping rows remain valid, the mapping stays a bijection.
#[test]
fn force_push_reimports_cleanly() {
    let fixture = Fixture::generate(1999, &GenOptions::default());
    import(&fixture.repo, &fixture.garden, &Default::default());
    let old_tip = fixture.mapping.last().expect("has commits").1.clone();
    let old_rows = git_map_rows(&fixture.garden);

    // Rewrite the tip: same tree, new message, new parent → new sha.
    let hermetic = |cmd: &mut std::process::Command| {
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(&k);
            }
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    };
    let git_out = |args: &[&str]| -> String {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(&fixture.repo);
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
    };
    let tree = git_out(&["rev-parse", &format!("{old_tip}^{{tree}}")]);
    let parent = git_out(&["rev-parse", &format!("{old_tip}^")]);
    let new_tip = {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(&fixture.repo);
        hermetic(&mut cmd);
        cmd.args(["commit-tree", &tree, "-p", &parent])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .env("GIT_AUTHOR_NAME", "Force Pusher")
            .env("GIT_AUTHOR_EMAIL", "force@example.com")
            .env("GIT_COMMITTER_NAME", "Force Pusher")
            .env("GIT_COMMITTER_EMAIL", "force@example.com");
        let mut child = cmd.spawn().expect("commit-tree");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(b"rewritten by the force-push test\n")
            .expect("message");
        let out = child.wait_with_output().expect("output");
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
    assert_ne!(old_tip, new_tip);
    git_out(&["update-ref", "refs/heads/main", &new_tip, &old_tip]);

    // The rewritten tip makes main move sideways off the old tip — a
    // rewind that loses the old tip's changeset, so it needs the flag.
    let report = import(
        &fixture.repo,
        &fixture.garden,
        &ImportOptions {
            allow_rewind: true,
            ..Default::default()
        },
    );
    assert_eq!(report.commits_new, 1, "only the rewritten tip is new");
    assert_eq!(
        report.commits_skipped, 1,
        "the walk stops at the rewritten tip's mapped parent"
    );
    assert_eq!(
        report.commits_total,
        report.commits_new + report.commits_skipped
    );
    assert_eq!(report.refs_updated.len(), 1, "refs/heads/main moved");
    assert_eq!(report.refs_updated[0].0, "refs/heads/main");

    // The ref now points at the new tip's changeset.
    let (_, metadata, engine) = open_stores(&fixture.garden);
    let new_cs = metadata
        .cs_for_git(&new_tip)
        .expect("query")
        .expect("new tip mapped");
    assert_eq!(
        report.refs_updated[0].1, new_cs,
        "ref moved onto the new tip"
    );

    // Old objects remain (harmless), and the mapping keeps its bijection:
    // distinct shas ↔ distinct changesets, every changeset present.
    let rows = git_map_rows(&fixture.garden);
    assert_eq!(rows.len(), old_rows.len() + 1, "one new mapping row");
    let mut shas: Vec<&String> = rows.iter().map(|(s, _)| s).collect();
    shas.sort();
    shas.dedup();
    assert_eq!(shas.len(), rows.len(), "no duplicate git shas");
    let mut css: Vec<&String> = rows.iter().map(|(_, c)| c).collect();
    css.sort();
    css.dedup();
    assert_eq!(css.len(), rows.len(), "no duplicate changesets");

    // The new tip's derived tree matches git's.
    let tree_id = engine
        .derive_tree(new_cs)
        .expect("derive")
        .expect("non-empty tree");
    let flat = engine.flatten_tree(Some(tree_id)).expect("flatten");
    let ls = gardenbed_harness::git_ls_tree_r(&fixture.repo, &new_tip).expect("ls-tree");
    assert_eq!(flat.len(), ls.len(), "rewritten tip tree matches git");

    // The git repo itself is still well-formed (old objects are dangling
    // but valid).
    gardenbed_harness::git_fsck(&fixture.repo).expect("fsck");
}

/// Importing a repository whose git `main` is BEHIND the store's `main`
/// (published work ahead of git) is the silent-rewind bug: the default
/// must refuse the move (non-zero via `ImportError::Ref`) naming the ref
/// and both tips, and `--allow-rewind` must permit it while reporting it
/// with the fell-off count. A forward move stays silent.
#[test]
fn backward_ref_move_is_refused_then_reported() {
    let fixture = Fixture::generate(2111, &GenOptions::default());
    import(&fixture.repo, &fixture.garden, &Default::default());
    // The store's tip (NOT `fixture.mapping`'s changeset id: rename
    // detection reshapes changes, so stored ids differ from generated
    // ones; the git shas still map 1:1).
    let old_tip = {
        let (_, metadata, _) = open_stores(&fixture.garden);
        metadata
            .resolve_ref("refs/heads/main")
            .expect("resolve")
            .expect("main exists after import")
    };

    // Simulate published-but-not-in-git work: a changeset committed
    // directly into the gardenbed store and published onto main via CAS.
    let (objects, metadata, engine) = open_stores(&fixture.garden);
    let published = {
        use gardenbed_model::{Change, Identity, Op};
        let cs = gardenbed_model::Changeset {
            parents: vec![old_tip],
            changes: vec![Change {
                path: "published.txt".into(),
                op: Op::Add,
                blob: Some(gardenbed_model::blob_id(b"published work\n")),
                src_path: None,
                mode: gardenbed_model::Mode::Regular,
            }],
            message: "published from bed\n".into(),
            author: Identity {
                name: "Bed".into(),
                email: "bed@example.com".into(),
                time_secs: 1_800_000_000,
                tz_min: 0,
            },
            committer: Identity {
                name: "Bed".into(),
                email: "bed@example.com".into(),
                time_secs: 1_800_000_000,
                tz_min: 0,
            },
            signature: None,
        };
        // The server's own create path (crates/server/src/lib.rs
        // `create_changeset`): blob in, derived rows, validated put.
        use gardenbed_store::ObjectStore as _;
        // The server validates parent history before deriving (its
        // `validate_history`); imported tips are only certified on
        // re-import, so do it here explicitly.
        engine.certify_history(old_tip).expect("certify parent");
        objects.put_blob(b"published work\n").expect("blob");
        let cs_id = cs.id().expect("canonical");
        let root = engine.validated_tree_for_new(&cs).expect("derive tree");
        let rows = engine
            .derived_rows_for_validated_tree(cs_id, &cs, root)
            .expect("derived rows");
        metadata
            .put_validated_changeset(&cs, root, &rows, None)
            .expect("put changeset");
        let id = cs_id;
        let won = metadata
            .cas_ref("refs/heads/main", Some(old_tip), id)
            .expect("cas");
        assert!(matches!(won, gardenbed_store::RefCas::Won { .. }));
        id
    };
    drop((objects, metadata, engine));

    // Default: re-importing the unchanged git repo (whose main is now
    // BEHIND the published tip) must fail loudly, before moving the ref.
    let err = try_import(&fixture.repo, &fixture.garden, &Default::default())
        .expect_err("rewind must be refused by default");
    match err {
        ImportError::Ref { name, detail } => {
            assert_eq!(name, "refs/heads/main");
            assert!(
                detail.contains("--allow-rewind"),
                "error must name the escape hatch: {detail}"
            );
            assert!(
                detail.contains("losing 1 changeset"),
                "counts the loss: {detail}"
            );
        }
        other => panic!("expected ImportError::Ref, got {other:?}"),
    }
    // The refusal happened before any ref was touched.
    let (_, metadata, _) = open_stores(&fixture.garden);
    assert_eq!(
        metadata.resolve_ref("refs/heads/main").expect("resolve"),
        Some(published),
        "the ref was not moved by the refused import"
    );
    drop(metadata);

    // --allow-rewind: the move is permitted and reported with the count.
    let report = import(
        &fixture.repo,
        &fixture.garden,
        &ImportOptions {
            allow_rewind: true,
            ..Default::default()
        },
    );
    assert_eq!(report.refs_rewound.len(), 1, "the rewind is reported");
    let rewind = &report.refs_rewound[0];
    assert_eq!(rewind.name, "refs/heads/main");
    assert_eq!(rewind.old_tip, published);
    assert_eq!(
        rewind.fell_off,
        Some(1),
        "only the published changeset fell off (the shared imported \
         history stays reachable from the git tip)"
    );
    assert!(report.to_string().contains("1 ref(s) rewound"));
    let (_, metadata, _) = open_stores(&fixture.garden);
    assert_eq!(
        metadata.resolve_ref("refs/heads/main").expect("resolve"),
        Some(old_tip),
        "main rewound to the git tip"
    );

    // Forward moves stay silent: extend git history and re-import.
    append_commits(&fixture.repo, 1);
    let report = import(&fixture.repo, &fixture.garden, &Default::default());
    assert!(report.refs_rewound.is_empty(), "forward move is silent");
    assert_eq!(report.refs_updated.len(), 1, "the ref moved forward");
}

/// SHA-256 repositories are rejected with a documented error, never a
/// panic.
#[test]
fn sha256_repos_are_unsupported() {
    let fixture = Fixture::generate(5, &GenOptions::default());
    // Build a sha256 clone next to it.
    let dir = fixture.dir.path().join("sha256.git");
    let out = std::process::Command::new("git")
        .arg("clone")
        .arg("--quiet")
        .arg("--bare")
        .arg("--object-format=sha256")
        .arg(&fixture.repo)
        .arg(&dir)
        .output()
        .expect("git clone runs");
    if !out.status.success() {
        // Ancient git without sha256 support: nothing to test.
        eprintln!("skipping: git lacks --object-format=sha256");
        return;
    }
    let err = try_import(
        &dir,
        &fixture.dir.path().join("g256"),
        &ImportOptions::default(),
    );
    assert!(
        matches!(err, Err(ImportError::Unsupported(_))),
        "expected Unsupported, got {err:?}"
    );
}

/// Ref-pattern filtering: only the selected branches are walked and
/// recorded.
#[test]
fn ref_glob_selects_branches() {
    let fixture = Fixture::generate(3001, &GenOptions::default());
    // A second branch pointing mid-history (its commits are all already
    // reachable from main, so the walk set is unchanged by it).
    let mid = &fixture.mapping[fixture.mapping.len() / 2].1;
    let out = std::process::Command::new("git")
        .current_dir(&fixture.repo)
        .args(["update-ref", "refs/heads/side", mid])
        .output()
        .expect("update-ref");
    assert!(out.status.success());

    let opts = ImportOptions {
        refs: vec!["refs/heads/main".into()],
        ..Default::default()
    };
    let report = import(&fixture.repo, &fixture.garden, &opts);
    assert_eq!(
        report.commits_new as usize,
        fixture.history.changesets.len()
    );
    assert_eq!(report.refs_updated.len(), 1, "only main is recorded");
    assert_eq!(report.refs_updated[0].0, "refs/heads/main");

    // Importing everything now walks nothing new (same reachability) but
    // records the second branch.
    let report2 = import(&fixture.repo, &fixture.garden, &Default::default());
    assert_eq!(report2.commits_new, 0);
    assert_eq!(report2.refs_updated.len(), 1);
    assert_eq!(report2.refs_updated[0].0, "refs/heads/side");
}

/// The mapping table has exactly one row per ingested commit and both
/// directions are unique (store-enforced bijection).
#[test]
fn mapping_is_a_bijection() {
    let fixture = Fixture::generate(777, &GenOptions::default());
    import(&fixture.repo, &fixture.garden, &Default::default());
    let commits = fixture.history.changesets.len() as u64;
    assert_eq!(
        sql_scalar(&fixture.garden, "SELECT COUNT(*) FROM git_map"),
        commits
    );
    assert_eq!(
        sql_scalar(
            &fixture.garden,
            "SELECT COUNT(DISTINCT git_sha) FROM git_map"
        ),
        commits
    );
    assert_eq!(
        sql_scalar(&fixture.garden, "SELECT COUNT(DISTINCT cs) FROM git_map"),
        commits
    );
    assert_eq!(
        sql_scalar(&fixture.garden, "SELECT COUNT(*) FROM changesets"),
        commits
    );
    let _ = FsObjectStore::open; // (import only to keep the dependency list honest)
}
