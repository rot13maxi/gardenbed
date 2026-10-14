//! End-to-end tests of the `bed` binary: the CLI voice, exit codes, and
//! the offline behaviors M4 promises (`status`/`plot` work with no server;
//! server-backed commands fail cleanly, never half-states).
//!
//! Full server-backed round-trips live in `read_path.rs` (they spawn a
//! real arboretum); here the arboretum is unreachable on purpose.

use std::fs;
use std::process::Command;

use gardenbed::bed::Bed;
use gardenbed::status::mtime_ns;
use gardenbed::treestate::TreestateEntry;
use gardenbed::treestate::HINT_VERIFIED;
use tempfile::TempDir;

/// Runs the real binary in `dir`, returning (exit code, stdout, stderr).
fn bed(dir: &TempDir, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(dir.path())
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
    assert!(stderr.contains("cannot reach the arboretum"), "stderr: {stderr}");

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
    assert!(out.contains("modified:") && out.contains("src/a.rs"), "out: {out}");
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
        assert_eq!(code, 1, "args {args:?}: stdout {stdout:?} stderr {stderr:?}");
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
    assert!(stderr.contains("cannot reach the arboretum"), "stderr: {stderr}");
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
        assert_eq!(code, 1, "args {args:?}: stdout {stdout:?} stderr {stderr:?}");
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
    assert!(stderr.contains("invalid changeset id or ref"), "stderr: {stderr}");

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
    let (code, out, stderr) = bed(
        &dir,
        &["--bed-dir", bed_dir.to_str().unwrap(), "status"],
    );
    assert_eq!(code, 0, "{stderr}");
    assert!(out.contains("the plot is empty"), "out: {out}");
}
