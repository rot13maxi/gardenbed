//! CLI-level coverage for `gardenbed-server import`'s backward-ref-move
//! (rewind) reporting: the silent-rewind regression — importing a git
//! repository whose `main` is behind the store's published `main` moved
//! the ref backward with exit 0 and no output naming the loss.
//!
//! Drives the built binary (`CARGO_BIN_EXE_gardenbed-server`) like the
//! smoke suite, against a throwaway git repo and root, mirroring the
//! dogfood scenario: import, publish work that is ahead of git, re-import
//! the moved-back tip.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Runs `git` in `dir` with a hermetic environment, returning stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "CLI Test")
        .env("GIT_AUTHOR_EMAIL", "cli@example.com")
        .env("GIT_COMMITTER_NAME", "CLI Test")
        .env("GIT_COMMITTER_EMAIL", "cli@example.com")
        .args(args);
    let out = cmd.output().expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

/// Runs the built `gardenbed-server import` binary, returning
/// (exit code, stdout, stderr).
fn server_import(from: &Path, root: &Path, extra: &[&str]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
        .arg("import")
        .arg("--from")
        .arg(from)
        .arg("--root")
        .arg(root)
        .args(extra)
        .output()
        .expect("server binary runs");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Creates a one-commit git repository (`main` at c1) and a `bed`-side
/// clone whose main is one dogfood commit (c2) AHEAD. Re-importing the
/// repo afterwards is the "git main behind bed main" rewind case.
fn dogfood_fixture(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("rewind-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    // The git repo: c1 on main (the moved-back tip).
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-q", "-b", "main", "--object-format=sha1"]);
    std::fs::write(repo.join("f.txt"), b"one\n").expect("write");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "c1"]);

    // The dogfood clone: one commit on top of c1 (published work).
    let clone = dir.join("clone");
    git(
        &dir,
        &[
            "clone",
            "-q",
            "--no-local",
            &repo.to_string_lossy(),
            &clone.to_string_lossy(),
        ],
    );
    std::fs::write(clone.join("f.txt"), b"one\ndogfood\n").expect("write");
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "dogfood work"]);

    (dir, repo, clone)
}

/// The dogfood bug: importing a repo whose git main is BEHIND the store's
/// published main must fail non-zero naming the ref and both tips (not
/// silently rewind), and `--allow-rewind` must permit the move while
/// printing a loud warning. A forward move stays silent.
#[test]
fn import_rewind_is_loud_and_off_by_default() {
    let (dir, repo, clone) = dogfood_fixture("cli");
    let root = dir.join("garden");

    // First import from the clone: main = dogfood tip.
    let (code, out, err) = server_import(&clone, &root, &[]);
    assert_eq!(code, Some(0), "first import succeeds: {out} {err}");
    assert!(
        out.contains("1 ref(s) moved"),
        "first import moved main: {out}"
    );

    // Re-import the repo whose git main is BEHIND (moved back to c1):
    // default must refuse, non-zero, naming the ref.
    let (code, out, err) = server_import(&repo, &root, &[]);
    assert_ne!(code, Some(0), "rewind must fail by default: {out} {err}");
    assert!(
        err.contains("refs/heads/main"),
        "stderr names the ref: {err}"
    );
    assert!(
        err.contains("--allow-rewind"),
        "stderr names the escape hatch: {err}"
    );
    assert!(
        err.contains("backward"),
        "stderr says the ref moved backward: {err}"
    );

    // --allow-rewind: exit 0, a loud warning on stderr naming the ref
    // and the fell-off count, and the report line mentions the rewind.
    let (code, out, err) = server_import(&repo, &root, &["--allow-rewind"]);
    assert_eq!(code, Some(0), "allow-rewind import succeeds: {out} {err}");
    assert!(
        err.contains("warning: import moved refs/heads/main backward"),
        "loud warning on stderr: {err}"
    );
    assert!(
        err.contains("changeset(s)"),
        "warning reports what fell off: {err}"
    );
    assert!(out.contains("1 ref(s) rewound"), "report counts it: {out}");

    // Forward move stays silent: extend the clone by one commit and
    // re-import it — no warning, exit 0, no rewind in the report.
    std::fs::write(clone.join("g.txt"), b"forward\n").expect("write");
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "forward work"]);
    let (code, out, err) = server_import(&clone, &root, &[]);
    assert_eq!(code, Some(0), "forward import succeeds: {out} {err}");
    assert!(
        !err.contains("backward"),
        "forward move is silent on stderr: {err}"
    );
    assert!(
        !out.contains("rewound"),
        "forward move reports no rewind: {out}"
    );
}
