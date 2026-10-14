//! Export roundtrip tests (v0.1.md §M5): changesets → git via one
//! `git fast-import` stream.
//!
//! The oracles are git itself:
//!
//! * **SHA exactness** — an unsigned history materialized by the harness
//!   (`fixture.mapping`: changeset id ↔ git sha), imported into gardenbed
//!   stores and exported back must reproduce the *exact* same git commit
//!   SHAs. Both sides are content-determined; any byte drift (identity,
//!   dates, message, parents, tree) breaks equality, so that one
//!   comparison covers every commit field at once.
//! * `git fsck --strict` on every exported repository.
//! * Per-commit trees (`git ls-tree -r`) compared against independently
//!   computed expectations for gardenbed-native (never-materialized)
//!   histories, with blob shas derived by `git hash-object`; messages,
//!   identity lines and parent structure read back byte-exactly through
//!   `git cat-file --batch`.
//! * Ref filtering (`ExportOptions::refs`), idempotent re-export into an
//!   existing repository, and the empty-store no-op.
//!
//! The one deliberate divergence — signed commits are exported unsigned —
//! gets its own test: the signature is dropped, the SHA differs, the tree
//! matches, and the report counts it (see `docs/divergences.md` §1).

mod common;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use gardenbed_harness::oracle::{git_fsck, git_hash_object, git_ls_tree_r};
use gardenbed_harness::{GenOptions, files_at, generate};
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_rootstock::{ExportOptions, ImportOptions, export_to_path, import_from_path};
use gardenbed_store::{MetadataStore, ObjectStore, RefCas};
use tempfile::tempdir;

use common::{CatFile, Fixture, open_stores};

// ---------------------------------------------------------------- helpers

/// Runs git in `repo` (hermetic environment), feeding `stdin`, returning
/// stdout; any failure is a test error with git's stderr.
fn git_out(repo: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.args(args);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(stdin)
        .expect("stdin written");
    let out = child.wait_with_output().expect("git finishes");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// `git rev-parse --verify <ref>` → lowercase 40-hex, or None when the ref
/// does not exist (unborn). Tolerates git's non-zero exit by design.
fn rev_ref(repo: &Path, name: &str) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.args(["rev-parse", "--verify", "--quiet", name]);
    let out = cmd.stdin(Stdio::null()).output().expect("git spawns");
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    assert_eq!(s.len(), 40, "unexpected rev-parse output {s:?}");
    Some(s)
}

/// Every ref name in the target repo (`git for-each-ref`, sorted by
/// refname — symbolic refs like HEAD are not listed).
fn ref_names(repo: &Path) -> Vec<String> {
    String::from_utf8_lossy(&git_out(
        repo,
        &["for-each-ref", "--format=%(refname)"],
        b"",
    ))
    .lines()
    .filter(|l| !l.is_empty())
    .map(|l| l.to_owned())
    .collect()
}

/// The message bytes of a commit, parsed from its raw object (no `%B`
/// normalization involved). Thin re-export for readability.
fn commit_message(cat: &mut CatFile, sha: &str) -> Vec<u8> {
    common::git_commit_message(cat, sha)
}

/// Decodes 40 hex chars into 20 raw bytes.
fn hex_bytes(s: &str) -> Vec<u8> {
    assert_eq!(s.len() % 2, 0, "odd hex {s:?}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex digit"))
        .collect()
}

/// The model's `Mode` → git's octal mode string.
fn git_mode_str(mode: Mode) -> &'static str {
    match mode {
        Mode::Regular => "100644",
        Mode::Exec => "100755",
        Mode::Symlink => "120000",
        Mode::Gitlink => "160000",
    }
}

/// Formats `tz_min` as git's raw `±hhmm` form (mirror of the export and
/// the harness generator: -330 → "-0530", 345 → "+0545").
fn format_tz(tz_min: i32) -> String {
    let sign = if tz_min < 0 { '-' } else { '+' };
    let a = tz_min.unsigned_abs();
    format!("{}{:02}{:02}", sign, a / 60, a % 60)
}

/// The distinct non-gitlink blob ids referenced by `changesets` — the
/// independent expectation for `ExportReport::blobs_exported`.
fn distinct_blobs(changesets: &[Changeset]) -> HashSet<Id> {
    changesets
        .iter()
        .flat_map(|cs| cs.changes.iter())
        .filter(|c| c.blob.is_some() && c.mode != Mode::Gitlink)
        .map(|c| c.blob.expect("filtered to Some"))
        .collect()
}

/// Imports a fixture's repository into its garden stores.
fn import_fixture(fx: &Fixture) {
    let (objects, metadata, engine) = open_stores(&fx.garden);
    let report = import_from_path(
        &fx.repo,
        objects,
        metadata,
        engine,
        &ImportOptions::default(),
    )
    .expect("import succeeds");
    assert_eq!(
        report.commits_new,
        fx.mapping.len() as u64,
        "the whole fixture imports"
    );
}

/// `(changeset id → git sha)` from an export report.
fn cs_sha_map(report: &gardenbed_rootstock::ExportReport) -> HashMap<Id, String> {
    report.commits.iter().cloned().collect()
}

/// Replays changesets `0..=upto` into a working set (the same semantics as
/// the harness's `files_at`, which generated histories use directly).
fn replay_native(changesets: &[Changeset], upto: usize) -> BTreeMap<String, (Id, Mode)> {
    use gardenbed_model::Op;
    let mut out: BTreeMap<String, (Id, Mode)> = BTreeMap::new();
    for cs in &changesets[..=upto] {
        for c in &cs.changes {
            match c.op {
                Op::Delete => {
                    out.remove(&c.path);
                }
                Op::Rename => {
                    if let Some(src) = &c.src_path {
                        out.remove(src);
                    }
                    if let Some(b) = c.blob {
                        out.insert(c.path.clone(), (b, c.mode));
                    }
                }
                Op::Add | Op::Modify => {
                    if let Some(b) = c.blob {
                        out.insert(c.path.clone(), (b, c.mode));
                    }
                }
            }
        }
    }
    out
}

/// Full differential check of an export of a **gardenbed-native** history
/// (never materialized by git, so there are no ground-truth shas): for
/// every changeset, the exported commit must carry the changeset's message
/// byte-exactly, its identity lines verbatim (raw `±hhmm` dates included),
/// its exact parent structure, and a `git ls-tree -r` tree equal to the
/// independently replayed working set — with blob shas derived by
/// `git hash-object` and gitlink pointers stripped of their zero padding.
fn check_native_export(
    target: &Path,
    report: &gardenbed_rootstock::ExportReport,
    changesets: &[Changeset],
    blobs: &HashMap<Id, Vec<u8>>,
    state_at: &dyn Fn(usize) -> BTreeMap<String, (Id, Mode)>,
) {
    let cs_sha: HashMap<Id, String> = report.commits.iter().cloned().collect();
    assert_eq!(
        cs_sha.len(),
        changesets.len(),
        "every changeset was exported exactly once"
    );

    // gardenbed blob id → git blob sha (computed once, on demand).
    let mut git_shas: HashMap<Id, String> = HashMap::new();
    let mut cat = CatFile::spawn(target);

    for (i, cs) in changesets.iter().enumerate() {
        let id = cs.id().expect("changeset encodes");
        let sha = cs_sha
            .get(&id)
            .unwrap_or_else(|| panic!("changeset {i} ({}) was not exported", id.to_hex()));

        // -- message byte-exact
        assert_eq!(
            commit_message(&mut cat, sha),
            cs.message.as_bytes(),
            "message bytes of changeset {i}"
        );

        // -- identity lines verbatim, raw dates (this is what makes
        //    unsigned roundtrips sha-exact)
        let raw = cat.commit_raw(sha);
        let text = String::from_utf8_lossy(&raw).to_string();
        let want_author = format!(
            "author {} <{}> {} {}",
            cs.author.name,
            cs.author.email,
            cs.author.time_secs,
            format_tz(cs.author.tz_min)
        );
        let want_committer = format!(
            "committer {} <{}> {} {}",
            cs.committer.name,
            cs.committer.email,
            cs.committer.time_secs,
            format_tz(cs.committer.tz_min)
        );
        assert!(
            text.contains(&want_author),
            "author line of changeset {i}:\n{text}"
        );
        assert!(
            text.contains(&want_committer),
            "committer line of changeset {i}:\n{text}"
        );

        // -- parent structure
        let got_parents =
            String::from_utf8_lossy(&git_out(target, &["show", "-s", "--format=%P", sha], b""))
                .trim()
                .to_owned();
        let want_parents: Vec<&str> = cs.parents.iter().map(|p| cs_sha[p].as_str()).collect();
        let got: Vec<&str> = got_parents.split_whitespace().collect();
        assert_eq!(got, want_parents, "parents of changeset {i}");

        // -- tree: expected from the replay, actual from git ls-tree -r
        let mut expected: BTreeMap<String, (String, Vec<u8>)> = BTreeMap::new();
        for (path, (blob, mode)) in state_at(i) {
            match mode {
                Mode::Gitlink => {
                    // The pointer is stored zero-padded to 32 bytes; git
                    // wants the bare 20-byte upstream sha.
                    assert!(blob.as_bytes()[..12].iter().all(|b| *b == 0));
                    expected.insert(path, ("160000".into(), blob.as_bytes()[12..].to_vec()));
                }
                _ => {
                    let content = blobs
                        .get(&blob)
                        .unwrap_or_else(|| panic!("blob {blob} has no content"));
                    let sha = git_shas
                        .entry(blob)
                        .or_insert_with(|| git_hash_object(target, content).expect("hash-object"))
                        .clone();
                    expected.insert(path, (git_mode_str(mode).to_owned(), hex_bytes(&sha)));
                }
            }
        }
        let actual: BTreeMap<String, (String, Vec<u8>)> = git_ls_tree_r(target, sha)
            .expect("ls-tree")
            .into_iter()
            .map(|(p, m, s)| (p, (m, s)))
            .collect();
        assert_eq!(actual, expected, "tree of changeset index {i} ({sha})");
    }
}

// ------------------------------------------------------------------ tests

/// The headline roundtrip: a materialized fixture (unsigned) is imported
/// into gardenbed stores and exported back — every git SHA must be
/// reproduced **exactly**, and the exported repository must be fsck-clean
/// with byte-exact messages.
#[test]
fn export_roundtrips_unsigned_history_sha_exact() {
    let fx = Fixture::generate(4242, &GenOptions::default());
    import_fixture(&fx);

    let (objects, metadata, _engine) = open_stores(&fx.garden);
    let target = fx.dir.path().join("export.git");
    let report = export_to_path(
        &target,
        objects,
        metadata.clone(),
        &ExportOptions::default(),
    )
    .expect("export succeeds");

    assert_eq!(
        report.commits_exported,
        fx.mapping.len() as u64,
        "one commit per imported changeset"
    );
    assert_eq!(report.commits.len() as u64, report.commits_exported);
    assert_eq!(
        report.signed_dropped, 0,
        "the generated history is unsigned"
    );
    assert_eq!(
        report.blobs_exported,
        distinct_blobs(&fx.history.changesets).len() as u64,
        "one blob record per distinct non-gitlink blob id"
    );

    // -- the (changeset, git sha) pairs reproduce the fixture exactly.
    //    The git SHAs are the materialized ones — byte-for-byte (identity,
    //    dates, message, parents and tree are all inside a git sha). The
    //    changeset ids are the ones the *import* derived (rootstock
    //    re-derives changesets from git's own diffs, so they are the
    //    store's ids, not the generator's). Order-insensitive: export order
    //    is topological DFS, the mapping is commit order.
    let mut exported_shas: Vec<&str> = report.commits.iter().map(|(_, s)| s.as_str()).collect();
    let mut fixture_shas: Vec<&str> = fx.mapping.iter().map(|(_, s)| s.as_str()).collect();
    exported_shas.sort_unstable();
    fixture_shas.sort_unstable();
    assert_eq!(
        exported_shas, fixture_shas,
        "every git sha reproduced exactly"
    );

    let mut imported: Vec<(Id, String)> = fx
        .mapping
        .iter()
        .map(|(_, sha)| {
            (
                metadata
                    .cs_for_git(sha)
                    .expect("cs_for_git")
                    .unwrap_or_else(|| panic!("commit {sha} not mapped")),
                sha.clone(),
            )
        })
        .collect();
    imported.sort_by(|a, b| a.1.cmp(&b.1));
    let mut exported = report.commits.clone();
    exported.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        exported, imported,
        "exported (changeset, git sha) pairs reproduce the store's mapping"
    );

    // -- refs: report == seedbed == git, pointwise.
    let refs = metadata.list_refs().expect("list_refs");
    let cs_sha = cs_sha_map(&report);
    assert_eq!(report.refs.len(), refs.len());
    for ((name, cs), (rname, rsha)) in refs.iter().zip(report.refs.iter()) {
        assert_eq!(name, rname, "report refs follow the seedbed's order");
        assert_eq!(
            cs_sha.get(cs).map(String::as_str),
            Some(rsha.as_str()),
            "ref {name}: tip sha matches its changeset's exported sha"
        );
        assert_eq!(
            rev_ref(&target, name).as_deref(),
            Some(rsha.as_str()),
            "ref {name} as git sees it"
        );
    }
    let got: Vec<String> = report.refs.iter().map(|(n, _)| n.clone()).collect();
    assert_eq!(
        ref_names(&target),
        got,
        "git holds exactly the exported refs"
    );

    git_fsck(&target).expect("fsck --strict clean");

    // -- messages byte-exact against the exported repository.
    let mut cat = CatFile::spawn(&target);
    for (cs_id, sha) in &report.commits {
        let (cs, _) = metadata
            .get_changeset(*cs_id)
            .expect("get_changeset")
            .expect("exists");
        assert_eq!(commit_message(&mut cat, sha), cs.message.as_bytes());
    }

    // -- HEAD points at the first exported branch so the bare repository
    //    is immediately usable (`git log`, clones) — the M8 demo relies on
    //    this.
    let head = git_out(&target, &["symbolic-ref", "HEAD"], b"");
    assert_eq!(String::from_utf8_lossy(&head).trim(), "refs/heads/main");
    let log = git_out(&target, &["log", "--format=%H", "-n", "1"], b"");
    assert_eq!(String::from_utf8_lossy(&log).trim(), report.refs[0].1);
}

/// Exporting twice into the same target repository yields identical
/// results (objects are content-addressed; the second run recreates the
/// same shas) and leaves the repository fsck-clean.
#[test]
fn export_into_an_existing_repo_is_idempotent() {
    let fx = Fixture::generate(
        777,
        &GenOptions {
            commits: 15,
            ..Default::default()
        },
    );
    import_fixture(&fx);

    let (objects, metadata, _engine) = open_stores(&fx.garden);
    let target = fx.dir.path().join("export.git");

    let first = export_to_path(
        &target,
        objects.clone(),
        metadata.clone(),
        &ExportOptions::default(),
    )
    .expect("first export");
    let second = export_to_path(&target, objects, metadata, &ExportOptions::default())
        .expect("second export into the same repository");

    assert_eq!(first.commits_exported, second.commits_exported);
    assert_eq!(first.blobs_exported, second.blobs_exported);
    assert_eq!(first.commits, second.commits, "same changesets → same shas");
    assert_eq!(first.refs, second.refs);
    git_fsck(&target).expect("fsck clean after the re-export");
}

/// `ExportOptions::refs` filters which refs are created: with a second
/// branch present, a full export materializes both, a pattern naming only
/// `refs/heads/main` materializes exactly one (while still exporting the
/// whole history `main` reaches).
#[test]
fn export_honors_the_ref_glob() {
    let fx = Fixture::generate(
        99,
        &GenOptions {
            commits: 16,
            merge_ratio: 0.0,
            ..Default::default()
        },
    );
    import_fixture(&fx);

    let (objects, metadata, _engine) = open_stores(&fx.garden);
    // A second branch pointing at an earlier mainline commit — resolved
    // through the store's mapping (changeset ids are the import's).
    let mid = fx.mapping.len() / 2;
    let mid_cs = metadata
        .cs_for_git(&fx.mapping[mid].1)
        .expect("cs_for_git")
        .unwrap_or_else(|| panic!("commit {} not mapped", fx.mapping[mid].1));
    assert!(matches!(
        metadata
            .cas_ref("refs/heads/wip", None, mid_cs)
            .expect("cas_ref"),
        RefCas::Won { .. }
    ));

    // -- everything: both refs, sorted by name, each at its changeset's sha.
    let all_target = fx.dir.path().join("all.git");
    let all = export_to_path(
        &all_target,
        objects.clone(),
        metadata.clone(),
        &ExportOptions::default(),
    )
    .expect("export of all refs");
    let names: Vec<String> = all.refs.iter().map(|(n, _)| n.clone()).collect();
    assert_eq!(names, vec!["refs/heads/main", "refs/heads/wip"]);
    assert_eq!(
        ref_names(&all_target),
        names,
        "git holds exactly the exported refs"
    );
    assert_eq!(
        all.refs[1].1, fx.mapping[mid].1,
        "wip points at its changeset's original (materialized) sha"
    );
    assert_eq!(all.commits_exported, fx.mapping.len() as u64);

    // -- only main: the ref list is filtered; main still reaches the whole
    //    (first-parent-linear) history.
    let main_target = fx.dir.path().join("main.git");
    let only_main = export_to_path(
        &main_target,
        objects,
        metadata,
        &ExportOptions {
            refs: vec!["refs/heads/main".into()],
        },
    )
    .expect("export of main only");
    assert_eq!(
        only_main.refs,
        vec![("refs/heads/main".to_owned(), all.refs[0].1.clone())]
    );
    assert_eq!(
        only_main.commits_exported,
        fx.mapping.len() as u64,
        "main reaches every commit; only the ref list is filtered"
    );
    assert!(!ref_names(&main_target).iter().any(|r| r.ends_with("/wip")));
    git_fsck(&main_target).expect("fsck clean");
}

/// The deliberate divergence (docs/divergences.md §1): a signed commit is
/// imported with its `gpgsig` captured, but fast-import cannot express a
/// signature, so the export writes it **unsigned** — the git SHA differs,
/// the tree matches byte-exactly, and the report counts the drop.
#[test]
fn signed_commits_drop_their_signature() {
    let dir = tempdir().expect("tempdir");
    let src = dir.path().join("signed.git");
    git_out(
        dir.path(),
        &["init", "-q", "--bare", "--object-format=sha1", "signed.git"],
        b"",
    );

    // Hand-build the signed commit with plumbing (no GPG available/needed):
    // one blob, one file, a structurally valid fake signature.
    let blob = git_out(
        &src,
        &["hash-object", "-w", "--stdin"],
        b"hello signed world\n",
    );
    let blob_sha = String::from_utf8_lossy(&blob).trim().to_owned();
    let tree = git_out(
        &src,
        &["mktree"],
        format!("100644 blob {blob_sha}\tfile.txt\n").as_bytes(),
    );
    let tree_sha = String::from_utf8_lossy(&tree).trim().to_owned();
    let raw = [
        format!("tree {tree_sha}"),
        "author Gopher <g@arden> 1700000000 +0000".to_owned(),
        "committer Gopher <g@arden> 1700000000 +0000".to_owned(),
        "gpgsig -----BEGIN PGP SIGNATURE-----".to_owned(),
        " iQEzBAABCgAdFiEEfake".to_owned(),
        " =fake".to_owned(),
        " -----END PGP SIGNATURE-----".to_owned(),
        String::new(),
        "signed!".to_owned(),
    ]
    .join("\n");
    let raw = format!("{raw}\n");
    let commit = git_out(
        &src,
        &["hash-object", "-t", "commit", "-w", "--stdin"],
        raw.as_bytes(),
    );
    let orig_sha = String::from_utf8_lossy(&commit).trim().to_owned();
    git_out(&src, &["update-ref", "refs/heads/main", &orig_sha], b"");
    git_fsck(&src).expect("the hand-built signed commit is valid git");

    // -- import: the signature is captured.
    let (objects, metadata, engine) = open_stores(&dir.path().join("garden"));
    let import = import_from_path(
        &src,
        objects.clone(),
        metadata.clone(),
        engine,
        &ImportOptions::default(),
    )
    .expect("import succeeds");
    assert_eq!(import.commits_new, 1);
    let cs_id = metadata
        .cs_for_git(&orig_sha)
        .expect("cs_for_git")
        .unwrap_or_else(|| panic!("commit {orig_sha} not mapped"));
    let (cs, _) = metadata
        .get_changeset(cs_id)
        .expect("get_changeset")
        .expect("exists");
    let sig = cs
        .signature
        .as_ref()
        .expect("the signature is captured at import");
    assert!(
        String::from_utf8_lossy(sig).contains("BEGIN PGP SIGNATURE"),
        "the captured signature is the gpgsig payload"
    );
    assert_eq!(cs.message, "signed!\n");

    // -- export: unsigned, counted, tree-preserving.
    let target = dir.path().join("export.git");
    let report = export_to_path(&target, objects, metadata, &ExportOptions::default())
        .expect("export succeeds");
    assert_eq!(report.commits_exported, 1);
    assert_eq!(report.signed_dropped, 1, "the dropped signature is counted");
    let exported_sha = &report.commits[0].1;
    assert_ne!(
        exported_sha, &orig_sha,
        "dropping the signature changes the sha"
    );

    // The tree survives byte-exactly; only the header is gone.
    assert_eq!(
        git_ls_tree_r(&src, &orig_sha).expect("ls-tree of the original"),
        git_ls_tree_r(&target, exported_sha).expect("ls-tree of the export"),
        "the exported commit's tree matches the signed original"
    );
    let mut cat = CatFile::spawn(&target);
    let raw_text = String::from_utf8_lossy(&cat.commit_raw(exported_sha)).to_string();
    assert!(
        !raw_text.contains("gpgsig"),
        "no signature header in:\n{raw_text}"
    );
    assert!(raw_text.contains("author Gopher <g@arden> 1700000000 +0000"));
    assert!(raw_text.contains("committer Gopher <g@arden> 1700000000 +0000"));
    assert_eq!(commit_message(&mut cat, exported_sha), b"signed!\n");
    git_fsck(&target).expect("fsck clean");
}

/// A gardenbed-native history (never materialized as git) exports to
/// trees that match an independent replay of the changesets, with blob
/// shas derived by git itself and every message byte-exact.
#[test]
fn export_of_native_history_reproduces_trees_and_messages() {
    let opts = GenOptions {
        commits: 30,
        merge_ratio: 0.25,
        octopus_ratio: 0.15,
        rename_ratio: 0.25,
        delete_ratio: 0.15,
        max_files: 14,
        max_dirs: 4,
        unicode_paths: true,
        crlf_content: true,
        empty_files: true,
        ..Default::default()
    };
    let history = generate(31415, &opts);
    assert!(
        history.changesets.iter().any(|cs| cs.parents.len() >= 2),
        "the generated history exercises merges"
    );

    let dir = tempdir().expect("tempdir");
    let (objects, metadata, _engine) = open_stores(&dir.path().join("garden"));

    // Ingest the native history with plain store calls: blobs (gitlink
    // pointers are not blobs), changesets, one branch at the head.
    let blobs: HashMap<Id, Vec<u8>> = history.blob_contents.iter().cloned().collect();
    for cs in &history.changesets {
        for c in &cs.changes {
            if let Some(b) = c.blob
                && c.mode != Mode::Gitlink
            {
                let content = blobs
                    .get(&b)
                    .unwrap_or_else(|| panic!("blob {b} has no content"));
                objects.put_blob(content).expect("put_blob");
            }
        }
    }
    for cs in &history.changesets {
        metadata.put_changeset(cs).expect("put_changeset");
    }
    metadata
        .cas_ref("refs/heads/main", None, history.head)
        .expect("cas_ref");

    let target = dir.path().join("export.git");
    let report = export_to_path(&target, objects, metadata, &ExportOptions::default())
        .expect("export succeeds");

    assert_eq!(report.commits_exported, history.changesets.len() as u64);
    assert_eq!(report.signed_dropped, 0);
    assert_eq!(
        report.blobs_exported,
        distinct_blobs(&history.changesets).len() as u64
    );

    check_native_export(&target, &report, &history.changesets, &blobs, &|i| {
        files_at(&history, i)
    });
    git_fsck(&target).expect("fsck clean");

    let cs_sha = cs_sha_map(&report);
    assert_eq!(
        rev_ref(&target, "refs/heads/main").as_deref(),
        Some(cs_sha[&history.head].as_str()),
        "refs/heads/main is at the exported head"
    );
}

/// A tiny hand-built history covering every special shape deterministically
/// (no seed fishing): exec bit, symlink, a gitlink pointer, a space and a
/// non-ASCII component in paths, a rename, a delete and an empty file.
#[test]
fn export_of_a_hand_built_history_reproduces_every_special_shape() {
    let readme1 = b"# bed\nv0.1\n".to_vec();
    let readme2 = b"# bed\nv0.1 (grafted)\n".to_vec();
    let script = b"#!/bin/sh\necho planted\n".to_vec();
    let spaced = b"content with a space in its name\n".to_vec();
    let link_target = b"plot/target.txt".to_vec();
    let empty: Vec<u8> = Vec::new();
    // The upstream git SHA-1 of an imaginary submodule commit, zero-padded
    // to 32 bytes (canonical spec §3).
    let mut padded = [0u8; 32];
    padded[12..].copy_from_slice(&hex_bytes("0123456789abcdef0123456789abcdef01234567"));

    let readme1_id = blob_id(&readme1);
    let readme2_id = blob_id(&readme2);
    let script_id = blob_id(&script);
    let spaced_id = blob_id(&spaced);
    let link_id = blob_id(&link_target);
    let empty_id = blob_id(&empty);
    let gitlink_id = Id::from_bytes(padded);

    let blobs: HashMap<Id, Vec<u8>> = [
        (readme1_id, readme1),
        (readme2_id, readme2),
        (script_id, script),
        (spaced_id, spaced),
        (link_id, link_target),
        (empty_id, empty),
    ]
    .into_iter()
    .collect();

    let ident = |name: &str, secs: u64, tz: i32| Identity {
        name: name.to_owned(),
        email: "gopher@garden.example".to_owned(),
        time_secs: secs,
        tz_min: tz,
    };
    let add = |path: &str, blob: Id, mode: Mode| Change {
        path: path.to_owned(),
        op: Op::Add,
        blob: Some(blob),
        src_path: None,
        mode,
    };

    // Canonical order: strictly ascending by path bytes.
    let cs0 = Changeset {
        parents: vec![],
        changes: vec![
            add("README.md", readme1_id, Mode::Regular),
            add("run.sh", script_id, Mode::Exec),
            add("space name.txt", spaced_id, Mode::Regular),
            add("sub", gitlink_id, Mode::Gitlink),
            add("日本語/link", link_id, Mode::Symlink),
        ],
        message: "plant the bed\n".into(),
        author: ident("Ada Gardener", 1_700_000_000, 60),
        committer: ident("Fennel Seed", 1_700_000_100, -330),
        signature: None,
    };
    let cs0_id = cs0.id().expect("cs0 encodes");
    let cs1 = Changeset {
        parents: vec![cs0_id],
        changes: vec![
            Change {
                path: "README.md".into(),
                op: Op::Modify,
                blob: Some(readme2_id),
                src_path: None,
                mode: Mode::Regular,
            },
            Change {
                path: "moved link".into(),
                op: Op::Rename,
                blob: Some(link_id),
                src_path: Some("日本語/link".into()),
                mode: Mode::Symlink,
            },
        ],
        message: "move the link, edit the readme\n".into(),
        author: ident("Basil Root", 1_700_003_600, 345),
        committer: ident("Ginger Root", 1_700_003_700, 90),
        signature: None,
    };
    let cs1_id = cs1.id().expect("cs1 encodes");
    let cs2 = Changeset {
        parents: vec![cs1_id],
        changes: vec![
            add("empty.txt", empty_id, Mode::Regular),
            Change {
                path: "run.sh".into(),
                op: Op::Delete,
                blob: None,
                src_path: None,
                mode: Mode::Regular,
            },
        ],
        message: "drop the script, add an empty file\n".into(),
        author: ident("Chloe Bloom", 1_700_007_200, 780),
        committer: ident("Hazel Nut", 1_700_007_300, 0),
        signature: None,
    };
    let changesets = vec![cs0, cs1, cs2];

    let dir = tempdir().expect("tempdir");
    let (objects, metadata, _engine) = open_stores(&dir.path().join("garden"));
    for content in blobs.values() {
        objects.put_blob(content).expect("put_blob");
    }
    for cs in &changesets {
        metadata.put_changeset(cs).expect("put_changeset");
    }
    let head = changesets.last().unwrap().id().expect("head encodes");
    metadata
        .cas_ref("refs/heads/main", None, head)
        .expect("cas_ref");

    let target = dir.path().join("export.git");
    let report = export_to_path(&target, objects, metadata, &ExportOptions::default())
        .expect("export succeeds");

    assert_eq!(report.commits_exported, 3);
    assert_eq!(
        report.blobs_exported, 6,
        "readme1, readme2, script, spaced, link, empty"
    );
    assert_eq!(report.signed_dropped, 0);

    check_native_export(&target, &report, &changesets, &blobs, &|i| {
        replay_native(&changesets, i)
    });
    git_fsck(&target).expect("fsck clean");
}

/// An empty seedbed exports a clean no-op: zero counts, no refs in git, an
/// empty marks file that stays on disk, and a fsck-clean target.
#[test]
fn export_of_an_empty_store_is_a_clean_noop() {
    let dir = tempdir().expect("tempdir");
    let (objects, metadata, _engine) = open_stores(&dir.path().join("garden"));

    let target = dir.path().join("export.git");
    let report = export_to_path(&target, objects, metadata, &ExportOptions::default())
        .expect("export succeeds");

    assert_eq!(report.commits_exported, 0);
    assert_eq!(report.blobs_exported, 0);
    assert_eq!(report.signed_dropped, 0);
    assert!(report.refs.is_empty());
    assert!(report.commits.is_empty());

    // The marks file is created even for an empty stream and stays on disk.
    assert!(report.marks_path.exists(), "marks file left in place");
    assert!(
        std::fs::read(&report.marks_path)
            .expect("read marks")
            .is_empty(),
        "no marks for an empty stream"
    );

    git_fsck(&target).expect("fsck clean");
    assert_eq!(
        rev_ref(&target, "refs/heads/main"),
        None,
        "no ref was created (unborn branch)"
    );
}
