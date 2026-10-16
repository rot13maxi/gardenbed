//! End-to-end tests of the `bed` binary: the CLI voice, exit codes, and
//! the offline behaviors M4 promises (`status`/`plot` work with no server;
//! server-backed commands fail cleanly, never half-states).
//!
//! The arboretum is unreachable on purpose in the offline tests. The
//! serve-backed regressions at the bottom (clone ref resolution, the
//! FailedPrecondition voice, the plot-add note) spawn a real arboretum
//! over a hand-built repo, exactly like `read_path.rs`/`write_path.rs`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use gardenbed::bed::Bed;
use gardenbed::status::mtime_ns;
use gardenbed::treestate::HINT_VERIFIED;
use gardenbed::treestate::TreestateEntry;
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;
use tempfile::TempDir;

/// Runs the real binary in `dir`, returning (exit code, stdout, stderr).
fn bed(dir: &TempDir, args: &[&str]) -> (i32, String, String) {
    bed_in(dir.path(), args)
}

/// [`bed`] for a plain directory — the serve-backed tests run each bed in
/// its own garden subdirectory.
fn bed_in(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("bed binary should run");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A tempdir with a freshly planted bed (as `bed clone` would leave it,
/// minus the server round-trip).
fn planted_bed() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    Bed::init(&dir.path().join(".bed"), "http://127.0.0.1:1", "garden").unwrap();
    dir
}

/// Records `prefixes` in the bed's config, as `bed plot add` would once
/// its fetch phase succeeded (the tests here run offline, so the plot
/// list is set through the library).
fn plot(dir: &TempDir, prefixes: &[&str]) {
    let mut bed = Bed::open(&dir.path().join(".bed")).unwrap();
    bed.config_mut().current_cs = Some("0".repeat(64));
    bed.config_mut().plot = prefixes.iter().map(|s| s.to_string()).collect();
    bed.save_config().unwrap();
}

/// Seeds the treestate as if `src/<name>` had been materialized from the
/// server with exactly `content`.
fn materialize(dir: &TempDir, name: &str, content: &[u8]) {
    fs::create_dir_all(dir.path().join("src")).unwrap();
    let path = dir.path().join("src").join(name);
    fs::write(&path, content).unwrap();
    let meta = fs::metadata(&path).unwrap();

    let bed = gardenbed::bed::Bed::open(&dir.path().join(".bed")).unwrap();
    let ts = bed.treestate().unwrap();
    ts.upsert(&TreestateEntry {
        path: format!("src/{name}"),
        blob_id: gardenbed_model::blob_id(content),
        mode: 0,
        size: content.len() as i64,
        mtime_ns: mtime_ns(&meta),
        hint: HINT_VERIFIED,
    })
    .unwrap();
}

#[test]
fn status_without_a_bed_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) = bed(&dir, &["status"]);
    assert_eq!(code, 1);
    assert!(stdout.is_empty());
    assert!(stderr.contains("not a bed"), "stderr: {stderr}");
}

#[test]
fn plot_list_and_remove_roundtrip_offline() {
    let dir = planted_bed();
    plot(&dir, &["src", "docs"]);

    let (code, out, _) = bed(&dir, &["plot", "list"]);
    assert_eq!(code, 0);
    assert_eq!(out, "src\ndocs\n", "out: {out}");

    let (code, out, _) = bed(&dir, &["plot", "remove", "src", "tests"]);
    assert_eq!(code, 0);
    assert!(out.contains("unplotted src"), "out: {out}");
    assert!(out.contains("not plotted: tests"), "out: {out}");

    let (code, out, _) = bed(&dir, &["plot", "list"]);
    assert_eq!(code, 0);
    assert_eq!(out, "docs\n", "out: {out}");
}

#[test]
fn plot_add_needs_the_arboretum_and_writes_nothing_on_failure() {
    let dir = planted_bed();
    let config_before = fs::read_to_string(dir.path().join(".bed/config.toml")).unwrap();

    // The arboretum is unreachable: `plot add` fails cleanly and the plot
    // list stays as it was (validate + connect before any local write).
    let (code, stdout, stderr) = bed(&dir, &["plot", "add", "src"]);
    assert_eq!(code, 1, "stdout: {stdout} stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout:?}");
    assert!(
        stderr.contains("cannot reach the arboretum"),
        "stderr: {stderr}"
    );

    let config_after = fs::read_to_string(dir.path().join(".bed/config.toml")).unwrap();
    assert_eq!(config_before, config_after, "config must be untouched");

    let (code, out, _) = bed(&dir, &["plot", "list"]);
    assert_eq!(code, 0);
    assert!(out.contains("the plot is empty"), "out: {out}");

    // A prefix that fails normalization is rejected even earlier.
    let (code, _, stderr) = bed(&dir, &["plot", "add", "../escape"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("invalid plot prefix"), "stderr: {stderr}");
}

#[test]
fn status_is_clean_right_after_planting() {
    let dir = planted_bed();
    plot(&dir, &["src"]);
    materialize(&dir, "a.rs", b"hello");

    let (code, out, _) = bed(&dir, &["status"]);
    assert_eq!(code, 0);
    assert!(out.contains("the plot is clean"), "out: {out}");

    let (code, out, _) = bed(&dir, &["status", "--full-scan"]);
    assert_eq!(code, 0);
    assert!(out.contains("the plot is clean"), "out: {out}");
}

#[test]
fn status_reports_modified_deleted_and_untracked() {
    let dir = planted_bed();
    plot(&dir, &["src"]);
    materialize(&dir, "a.rs", b"hello");
    materialize(&dir, "b.rs", b"byebye");

    // Same-size different content: caught despite unchanged size.
    fs::write(dir.path().join("src/a.rs"), b"hell0").unwrap();
    // Deleted.
    fs::remove_file(dir.path().join("src/b.rs")).unwrap();
    // Untracked.
    fs::write(dir.path().join("src/new.rs"), b"fresh").unwrap();

    let (code, out, _) = bed(&dir, &["status"]);
    assert_eq!(code, 0);
    assert!(out.contains("modified:"), "out: {out}");
    assert!(out.contains("src/a.rs"), "out: {out}");
    assert!(out.contains("deleted:"), "out: {out}");
    assert!(out.contains("src/b.rs"), "out: {out}");
    assert!(out.contains("untracked:"), "out: {out}");
    assert!(out.contains("src/new.rs"), "out: {out}");

    // --full-scan agrees (and hashes everything).
    let (_, out, _) = bed(&dir, &["status", "--full-scan"]);
    assert!(
        out.contains("modified:") && out.contains("src/a.rs"),
        "out: {out}"
    );
}

#[test]
fn status_touch_without_content_change_is_not_modified() {
    let dir = planted_bed();
    plot(&dir, &["src"]);
    materialize(&dir, "a.rs", b"hello");

    // Rewrite identical bytes: mtime moves, size and content don't.
    fs::write(dir.path().join("src/a.rs"), b"hello").unwrap();
    let (_, out, _) = bed(&dir, &["status"]);
    assert!(out.contains("the plot is clean"), "out: {out}");
}

#[test]
fn read_path_commands_fail_cleanly_without_an_arboretum() {
    let dir = planted_bed();
    plot(&dir, &["src"]);
    // The bed "has" a current changeset id that no arboretum knows; the
    // connection itself is the first thing to fail.
    let cases: &[&[&str]] = &[
        &["goto", "main"],
        &["file", "src/a.rs", "--cs", "main"],
        &["log", "--limit", "3"],
        &["diff"],
        &["blame", "src/a.rs", "--cs", "main"],
    ];
    for args in cases {
        let (code, stdout, stderr) = bed(&dir, args);
        assert_eq!(
            code, 1,
            "args {args:?}: stdout {stdout:?} stderr {stderr:?}"
        );
        assert!(
            stderr.contains("cannot reach the arboretum"),
            "args {args:?}: stderr {stderr:?}"
        );
        assert!(stdout.is_empty(), "args {args:?}: stdout {stdout:?}");
    }

    // A bed with no current changeset names the missing state instead.
    let dir = planted_bed();
    let (code, _, stderr) = bed(&dir, &["log"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no current changeset"), "stderr: {stderr}");
}

#[test]
fn clone_fails_cleanly_without_an_arboretum_and_leaves_no_bed() {
    let dir = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) = bed(&dir, &["clone", "http://127.0.0.1:1"]);
    assert_eq!(code, 1, "stdout: {stdout} stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout:?}");
    assert!(
        stderr.contains("cannot reach the arboretum"),
        "stderr: {stderr}"
    );
    assert!(!dir.path().join(".bed").exists(), "no bed may be planted");
}

#[test]
fn write_path_and_blame_fail_cleanly_without_an_arboretum() {
    // -- sow never contacts the arboretum for a clean plot: the local
    //    NothingToSow error comes first (exit 1, no stub code involved).
    let dir = planted_bed();
    let (code, stdout, stderr) = bed(&dir, &["sow", "-m", "tend the rows"]);
    assert_eq!(code, 1, "stdout {stdout:?} stderr {stderr:?}");
    assert!(stderr.contains("the plot is clean"), "stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout:?}");

    // -- with something to sow, the (deliberately unreachable) arboretum
    //    connection is what fails — nothing was uploaded, no half-state.
    let dir = planted_bed();
    plot(&dir, &["src"]);
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/new.txt"), b"fresh\n").unwrap();
    let (code, stdout, stderr) = bed(&dir, &["sow", "-m", "tend the rows"]);
    assert_eq!(code, 1, "stdout {stdout:?} stderr {stderr:?}");
    assert!(
        stderr.contains("cannot reach the arboretum"),
        "stderr: {stderr}"
    );
    assert!(stdout.is_empty(), "stdout: {stdout:?}");

    // -- graft and harvest are wired too: the connection fails before any
    //    local state is touched.
    for args in [&["graft", "main"][..], &["harvest"][..]] {
        let dir = planted_bed();
        let (code, stdout, stderr) = bed(&dir, args);
        assert_eq!(
            code, 1,
            "args {args:?}: stdout {stdout:?} stderr {stderr:?}"
        );
        assert!(
            stderr.contains("cannot reach the arboretum"),
            "args {args:?}: stderr {stderr:?}"
        );
        assert!(stdout.is_empty(), "args {args:?}: stdout {stdout:?}");
    }

    // -- blame is wired as well (M7): this bed has no current changeset,
    //    which is diagnosed locally, before any connection attempt — the
    //    same validate-then-connect ordering every read command follows.
    let dir = planted_bed();
    let (code, stdout, stderr) = bed(&dir, &["blame", "src/a.rs"]);
    assert_eq!(code, 1, "stdout {stdout:?} stderr {stderr:?}");
    assert!(stderr.contains("no current changeset"), "stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout:?}");
}

#[test]
fn commands_still_validate_arguments_locally() {
    let dir = planted_bed();

    // A bad plot prefix on clone is rejected before any connection.
    let (code, _, stderr) = bed(&dir, &["clone", "http://s", "--plot", "../escape"]);
    assert_eq!(code, 1, "stderr: {stderr}");
    assert!(stderr.contains("invalid plot prefix"), "stderr: {stderr}");

    // A bad revspec on goto likewise.
    let (code, _, stderr) = bed(&dir, &["goto", ""]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("invalid changeset id or ref"),
        "stderr: {stderr}"
    );

    // A bad path on blame and on file.
    for args in [&["blame", "/absolute"][..], &["file", "/absolute"][..]] {
        let (code, _, stderr) = bed(&dir, args);
        assert_eq!(code, 1, "args {args:?}");
        assert!(stderr.contains("invalid path"), "args {args:?}: {stderr}");
    }

    // An empty message on sow.
    let (code, _, stderr) = bed(&dir, &["sow", "-m", "   "]);
    assert_eq!(code, 1);
    assert!(stderr.contains("message is empty"), "stderr: {stderr}");
}

#[test]
fn clone_refuses_to_overwrite_an_existing_bed_before_connecting() {
    let dir = planted_bed();
    let (code, _, stderr) = bed(&dir, &["clone", "http://127.0.0.1:1"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("already a bed"), "stderr: {stderr}");
}

#[test]
fn status_and_plot_need_a_bed_too() {
    let dir = tempfile::tempdir().unwrap();
    for args in [&["plot", "list"][..], &["goto", "main"][..]] {
        let (code, _, stderr) = bed(&dir, args);
        assert_eq!(code, 1, "args {args:?}");
        assert!(stderr.contains("not a bed"), "args {args:?}: {stderr}");
    }
}

#[test]
fn custom_bed_dir_override_is_honored() {
    let dir = tempfile::tempdir().unwrap();
    let bed_dir = dir.path().join("state").join("mybed");
    Bed::init(&bed_dir, "http://127.0.0.1:1", "garden").unwrap();

    // No ./.bed in cwd — the override must carry the day.
    let (code, out, stderr) = bed(&dir, &["--bed-dir", bed_dir.to_str().unwrap(), "status"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(out.contains("the plot is empty"), "out: {out}");
}

// ---------------------------------------------------- FIX-F: empty sow message
// An empty `-m ""` is its own EmptyMessage error with the fix spelled out —
// not the generic config-invalid voice — and it leaves zero state behind.

#[test]
fn sow_with_an_empty_message_is_a_clean_empty_message_error() {
    let dir = planted_bed();
    let config_before = fs::read_to_string(dir.path().join(".bed/config.toml")).unwrap();

    let (code, stdout, stderr) = bed(&dir, &["sow", "-m", ""]);
    assert_eq!(code, 1, "stdout: {stdout:?} stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout:?}");
    assert_eq!(
        stderr, "bed: the message is empty — sow with -m <message>\n",
        "the EmptyMessage voice, not the config-invalid one"
    );

    // No partial state: the config is untouched, no heads were appended.
    let config_after = fs::read_to_string(dir.path().join(".bed/config.toml")).unwrap();
    assert_eq!(config_before, config_after);
    assert!(
        fs::read_to_string(dir.path().join(".bed/heads"))
            .unwrap()
            .is_empty(),
        "no heads may be appended"
    );
}

// =======================================================================
// Serve-backed regressions (QA hardening). A real arboretum runs over a
// hand-built repo, so each test controls exactly which refs the arboretum
// offers and which changeset each points at — the fixture shape follows
// write_path.rs.

/// One `add` change plus its content, the shape the fixture repo is built
/// from (as in write_path.rs).
fn seed_add(path: &str, content: &[u8]) -> (Change, Vec<u8>) {
    (
        Change {
            path: path.into(),
            op: Op::Add,
            blob: Some(blob_id(content)),
            src_path: None,
            mode: Mode::Regular,
        },
        content.to_vec(),
    )
}

/// Seeds a two-changeset repo at `root` and returns `(cs1, cs2)`: cs1
/// adds docs/readme.md and src/lib.rs, cs2 (its child) modifies
/// docs/readme.md. Refs are pointed afterwards, so each test chooses
/// exactly which refs the arboretum offers.
fn seed_repo(root: &Path) -> (Id, Id) {
    let repo = Repo::open(root).expect("repo opens");
    let ident = |secs: u64| Identity {
        name: "Seeder".into(),
        email: "seed@bed.example".into(),
        time_secs: secs,
        tz_min: 0,
    };
    let (c1, b1) = seed_add("docs/readme.md", b"# hello\n");
    let (c2, b2) = seed_add("src/lib.rs", b"pub fn bed() {}\n");
    let cs1 = Changeset {
        parents: Vec::new(),
        changes: vec![c1.clone(), c2.clone()],
        message: "seed the garden".into(),
        author: ident(1_700_000_000),
        committer: ident(1_700_000_000),
        signature: None,
    };
    let mut inline = std::collections::BTreeMap::new();
    inline.insert(c1.blob.unwrap().to_hex(), b1);
    inline.insert(c2.blob.unwrap().to_hex(), b2);
    let id1 = repo.create_changeset(&cs1, &inline).expect("cs1 ingests");

    let (c3, b3) = seed_add("docs/readme.md", b"# hello\n\n tended\n");
    let cs2 = Changeset {
        parents: vec![id1],
        changes: vec![Change {
            path: "docs/readme.md".into(),
            op: Op::Modify,
            blob: c3.blob,
            src_path: None,
            mode: Mode::Regular,
        }],
        message: "tend the readme".into(),
        author: ident(1_700_000_100),
        committer: ident(1_700_000_100),
        signature: None,
    };
    let mut inline2 = inline;
    inline2.insert(c3.blob.unwrap().to_hex(), b3);
    let id2 = repo.create_changeset(&cs2, &inline2).expect("cs2 ingests");
    (id1, id2)
}

/// The bed's current changeset, read from its config (the source of truth
/// the CLI itself writes).
fn current_cs(garden: &Path) -> Id {
    let text = fs::read_to_string(garden.join(".bed/config.toml")).expect("config.toml");
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("current_cs") {
            let (_, value) = rest.split_once('=').expect("current_cs = <hex>");
            let hex = value.trim().trim_matches('"');
            return Id::from_str(hex).expect("current_cs is 64 hex chars");
        }
    }
    panic!("config.toml has no current_cs:\n{text}");
}

/// One running arboretum over a freshly seeded repo whose refs the test
/// chose, plus an empty garden ready for `bed clone`.
struct CloneWorld {
    _dir: TempDir,
    garden: PathBuf,
    server: common::Arboretum,
    /// The root changeset.
    cs1: Id,
    /// cs1's child (modifies docs/readme.md).
    cs2: Id,
}

/// Spawns the world; `index` 0 points the ref at cs1, 1 at cs2.
async fn clone_world(refs: &[(&str, usize)]) -> CloneWorld {
    let dir = tempfile::tempdir().expect("tempdir");
    let arb_root = dir.path().join("arboretum");
    let (cs1, cs2) = seed_repo(&arb_root);
    {
        let repo = Repo::open(&arb_root).expect("repo opens");
        for (name, index) in refs {
            let id = if *index == 0 { cs1 } else { cs2 };
            repo.cas_ref(name, None, id)
                .expect("ref created (it must not exist yet)");
        }
    }
    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");
    let server = common::Arboretum::spawn(&arb_root).await;
    CloneWorld {
        _dir: dir,
        garden,
        server,
        cs1,
        cs2,
    }
}

// ------------------------------------------------- ISSUE-1.1: clone ref resolution
// `bed clone` no longer hardcodes `main`: `--ref` names the starting ref
// exactly; without it, main → master → trunk resolve in order (main wins
// deterministically when several exist), and the all-miss error says what
// was tried, what the arboretum has, and the flag that fixes it.

#[tokio::test]
async fn clone_without_a_ref_falls_back_to_the_sole_ref() {
    // The arboretum grows only `master`: no flag must still plant a bed.
    let w = clone_world(&[("master", 0)]).await;
    let (code, out, stderr) = bed_in(&w.garden, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains("the garden grows from "), "out: {out}");
    assert!(out.contains("(master)"), "out: {out}");
    assert_eq!(current_cs(&w.garden), w.cs1, "planted from master's tip");
}

#[tokio::test]
async fn clone_without_a_ref_prefers_main_when_both_common_refs_exist() {
    // main → cs2, master → cs1: with no flag, main must win — not
    // whichever name happens to be answered first.
    let w = clone_world(&[("master", 0), ("main", 1)]).await;
    let (code, out, stderr) = bed_in(&w.garden, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains("(main)"), "out: {out}");
    assert_eq!(
        current_cs(&w.garden),
        w.cs2,
        "main's tip wins, deterministically"
    );
}

#[tokio::test]
async fn clone_ref_flag_plants_from_the_named_ref() {
    let w = clone_world(&[("master", 0)]).await;
    let (code, out, stderr) = bed_in(&w.garden, &["clone", &w.server.http(), "--ref", "master"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains("(master)"), "out: {out}");
    assert_eq!(current_cs(&w.garden), w.cs1);
}

#[tokio::test]
async fn clone_ref_flag_errors_cleanly_on_an_unknown_ref() {
    let w = clone_world(&[("master", 0)]).await;
    let (code, stdout, stderr) = bed_in(&w.garden, &["clone", &w.server.http(), "--ref", "nope"]);
    assert_eq!(code, 1, "stdout: {stdout:?} stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout:?}");
    assert!(
        stderr.contains("the arboretum has no ref \"nope\""),
        "stderr: {stderr}"
    );
    assert!(
        !w.garden.join(".bed").exists(),
        "no bed may be planted on a failed clone"
    );
}

#[tokio::test]
async fn clone_without_a_ref_errors_actionably_when_no_common_ref_exists() {
    // `develop` only: every probe misses, and the error says what was
    // tried, what exists (the arboretum's own full ref spelling), and the
    // flag that fixes it.
    let w = clone_world(&[("develop", 0)]).await;
    let (code, stdout, stderr) = bed_in(&w.garden, &["clone", &w.server.http()]);
    assert_eq!(code, 1, "stdout: {stdout:?} stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout:?}");
    assert!(
        stderr.contains("no ref main (tried main, master, trunk); pass --ref"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("the arboretum has refs: refs/heads/develop"),
        "stderr: {stderr}"
    );
    assert!(
        !w.garden.join(".bed").exists(),
        "no bed may be planted on a failed clone"
    );

    // A garden with no refs at all: the same plain verdict, nothing to list.
    let w = clone_world(&[]).await;
    let (code, _, stderr) = bed_in(&w.garden, &["clone", &w.server.http()]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("no ref main (tried main, master, trunk); pass --ref\n"),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("the arboretum has refs"),
        "nothing to list: {stderr}"
    );
}

// --------------------------------------------- FIX-E: plot add names its empties
// `bed plot add <prefix>` prints a note when the newly added prefix matches
// nothing at the current changeset: planting ahead of history is allowed,
// but silent emptiness reads like success-by-accident.

#[tokio::test]
async fn plot_add_notes_when_the_prefix_matches_nothing() {
    let w = clone_world(&[("main", 0)]).await;
    let (code, _, stderr) = bed_in(&w.garden, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");

    // A prefix that matches at the current changeset: no note.
    let (code, out, stderr) = bed_in(&w.garden, &["plot", "add", "docs"]);
    assert_eq!(code, 0, "stderr: {stderr}\n{out}");
    assert!(out.contains("plotted docs\n"), "out: {out}");
    assert!(!out.contains("note:"), "out: {out}");

    // A prefix that names nothing: still exit 0 and recorded — but told.
    let (code, out, stderr) = bed_in(&w.garden, &["plot", "add", "nowhere"]);
    assert_eq!(code, 0, "stderr: {stderr}\n{out}");
    assert!(out.contains("plotted nowhere"), "out: {out}");
    assert!(out.contains("nothing exists under it"), "out: {out}");

    // Both prefixes are recorded in the plot.
    let (code, out, _) = bed_in(&w.garden, &["plot", "list"]);
    assert_eq!(code, 0);
    assert_eq!(out, "docs\nnowhere\n", "out: {out}");
}

// ---------------------------------------- FIX-B: short gRPC code in Rpc errors
// `bed <cmd>: the arboretum refused (FailedPrecondition): ...` renders the
// tonic Code VARIANT name, not the long description. A conflicting graft is
// the cleanest way to drive a FailedPrecondition through the CLI (harvest's
// one is translated into a dedicated error before it renders).

#[tokio::test]
async fn rpc_errors_render_the_short_grpc_code_variant() {
    let w = clone_world(&[("main", 1)]).await;

    // Two beds, both starting at main (cs2), both tending docs/readme.md.
    let garden_a = w.garden.clone();
    let garden_b = w._dir.path().join("garden-b");
    fs::create_dir_all(&garden_b).expect("second garden dir");

    // Bed A sows its version of the file.
    let (code, _, stderr) = bed_in(&garden_a, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let (code, _, stderr) = bed_in(&garden_a, &["plot", "add", "docs"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    fs::write(garden_a.join("docs/readme.md"), b"bed a's version\n").expect("write");
    let (code, _, stderr) = bed_in(&garden_a, &["sow", "-m", "bed a tends"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let a_cs = current_cs(&garden_a);

    // Bed B sows a diverging version of the same path and harvests:
    // main moves onto B's changeset.
    let (code, _, stderr) = bed_in(&garden_b, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let (code, _, stderr) = bed_in(&garden_b, &["plot", "add", "docs"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    fs::write(garden_b.join("docs/readme.md"), b"bed b's version\n").expect("write");
    let (code, _, stderr) = bed_in(&garden_b, &["sow", "-m", "bed b tends"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let (code, _, stderr) = bed_in(&garden_b, &["harvest"]);
    assert_eq!(code, 0, "stderr: {stderr}");

    // Bed A grafts onto the new main: the same path was written on both
    // sides, so the arboretum refuses with FAILED_PRECONDITION — reported
    // as the short variant name, never the long description.
    let (code, stdout, stderr) = bed_in(&garden_a, &["graft", "main"]);
    assert_eq!(code, 1, "stdout: {stdout:?} stderr: {stderr}");
    assert!(stdout.is_empty(), "stdout: {stdout:?}");
    assert!(
        stderr.contains("(FailedPrecondition)"),
        "the short variant name must appear: {stderr}"
    );
    // The long Display description is exactly what the fix removed.
    assert!(
        !stderr.contains("not in a state required"),
        "not the long description: {stderr}"
    );
    assert!(
        stderr.contains("docs/readme.md"),
        "the refusal names the conflict: {stderr}"
    );
    // And the refusal moved nothing: the bed stays on its own changeset.
    assert_eq!(
        current_cs(&garden_a),
        a_cs,
        "a failed graft leaves the bed where it stood"
    );
}
