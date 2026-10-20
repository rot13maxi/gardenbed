//! Staleness-guard regression proof for `daemon_bin()` — the same class as
//! dogfood #12 but for `gardenbed-daemon`: a daemon binary built from
//! DIFFERENT tree state than the working copy must be refused even when
//! its mtime looks fresh, with an actionable panic naming the rebuild
//! command. A stale daemon produced one spurious 4 MiB test failure.
//!
//! The fingerprint MUST be read from the located binary's own bytes: the
//! crate's `SRC_DIGEST` const is linked into this test binary (from the
//! working copy), so comparing against it would compare the tree with
//! itself and accept any binary. Each binary therefore carries
//! `GARDENBED_<CRATE>_SRC_DIGEST:<hex>` in its rodata (built from
//! [`gardenbed_daemon::SRC_DIGEST_MARKER`], referenced from `main`), and
//! the guard greps the file for it.
//!
//! These tests live in their own binary (not `common/mod.rs`) on purpose:
//! the staleness tests mutate `crates/daemon/src` content/mtime, and every
//! test binary compiles `common` — embedding it there would run the
//! mutation concurrently across all integration binaries, poisoning their
//! staleness checks mid-run (identical reasoning to `server_bin_guard.rs`).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use common::{DIGEST_MARKER_PREFIX_DAEMON, check_binary_against_sources, daemon_bin};

/// The remedy the DIFFERENT refusal must name for the located profile: a
/// plain `cargo build` is a no-op there (the sources are older than the
/// binary), so the sources are touched first. Debug and release binaries
/// need their own profile, so the suffix follows the binary actually
/// found.
fn force_rebuild(profile: &str) -> String {
    let suffix = if profile == "release" {
        " --release"
    } else {
        ""
    };
    format!("touch crates/daemon/src/lib.rs && cargo build -p gardenbed-daemon{suffix}")
}
fn profile_of(bin: &std::path::Path) -> &'static str {
    if bin.components().any(|c| c.as_os_str() == "release") {
        "release"
    } else {
        "debug"
    }
}

fn daemon_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("daemon")
        .join("src")
        .canonicalize()
        .expect("crates/daemon/src exists")
}

/// Every phase runs inside ONE `#[test]` on purpose: cargo runs sibling
/// tests on parallel threads, and the mutation phases touch
/// `crates/daemon/src` — a concurrent `daemon_bin()` would observe the
/// window and fail confusingly.
#[test]
fn daemon_bin_refuses_tree_state_stale_and_trusts_override() {
    // The oracles under test are INACTIVE when the override is set
    // (`locate_guarded_bin` trusts `BED_DAEMON_BIN` as-is by design), so the
    // mutation phases cannot pass in that configuration — and would instead
    // fail after mutating sources. A canary that points the harness at the
    // deployed pair sets exactly that variable. Skip with an actionable
    // message rather than turning a correct harness configuration into a red.
    if let Ok(path) = std::env::var("BED_DAEMON_BIN") {
        eprintln!(
            "skipping the {path} oracle proof: BED_DAEMON_BIN is set, and an \
             override is trusted as-is (the oracles are deliberately inactive). \
             Unset BED_DAEMON_BIN to exercise the staleness guard."
        );
        return;
    }
    hermetic_oracles();
    real_source_mutations();
    override_passthrough();
}

/// Oracle-level proof, fully hermetic: a synthetic binary whose bytes
/// carry a fingerprint taken from a *marked* copy of the daemon sources,
/// with the copy then restored and stamped OLDER than the binary — the
/// exact "built from tree state X, sources now look older" shape. The
/// mtime oracle must pass and only the binary-side digest can refuse.
fn hermetic_oracles() {
    let tmp = std::env::temp_dir().join(format!("gb-daemon-guard-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    // Layout: <tmp>/daemon/src/** (a copy, so the digest's crate root is
    // <tmp>/daemon) and <tmp>/bin/gardenbed-daemon.
    copy_tree(&daemon_src_dir(), &tmp.join("daemon").join("src"));
    let src = tmp.join("daemon").join("src");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&bin_dir).expect("bin dir");

    // The digest the "build" saw: the copy with one extra comment.
    let marker_file = src.join("lib.rs");
    let pristine = fs::read(&marker_file).expect("copied lib.rs");
    let mut marked = pristine.clone();
    marked.extend_from_slice(b"\n// daemon tree-state probe\n");
    fs::write(&marker_file, &marked).expect("marked copy written");
    let built_from = common::current_src_digest(&src);
    fs::write(&marker_file, &pristine).expect("copy restored");

    // The copy is now the CURRENT tree; stamp it OLDER than the binary so
    // only the digest oracle can fire (the `bed switch` class).
    let binary = bin_dir.join("gardenbed-daemon");
    let profile = profile_of(&binary);
    write_marker_binary(&binary, DIGEST_MARKER_PREFIX_DAEMON, &built_from);
    stamp_tree(&src, SystemTime::now() - Duration::from_secs(3600));

    let err = check_binary_against_sources(
        "gardenbed-daemon",
        "daemon",
        "cargo build -p gardenbed-daemon",
        DIGEST_MARKER_PREFIX_DAEMON,
        &binary,
        &src,
        profile,
    )
    .expect_err("a binary built from other tree state must be refused");
    assert!(
        err.contains("DIFFERENT")
            && err.contains("crates/daemon")
            && err.contains(&force_rebuild(profile)),
        "the digest refusal names the mismatch and a remedy that actually \
         rebuilds: {err}"
    );

    // Rebuild (so to speak): the binary now carries the CURRENT digest →
    // accepted, with sources still older than the binary.
    let current = common::current_src_digest(&src);
    write_marker_binary(&binary, DIGEST_MARKER_PREFIX_DAEMON, &current);
    check_binary_against_sources(
        "gardenbed-daemon",
        "daemon",
        "cargo build -p gardenbed-daemon",
        DIGEST_MARKER_PREFIX_DAEMON,
        &binary,
        &src,
        profile,
    )
    .expect("a rebuilt binary passes");

    // mtime oracle: correct marker, but a source newer than the binary.
    let main_rs = src.join("main.rs");
    let main_orig = main_rs
        .metadata()
        .and_then(|m| m.modified())
        .expect("main.rs stat");
    let future = SystemTime::now() + Duration::from_secs(3600);
    set_mtime(&main_rs, future);
    let err = check_binary_against_sources(
        "gardenbed-daemon",
        "daemon",
        "cargo build -p gardenbed-daemon",
        DIGEST_MARKER_PREFIX_DAEMON,
        &binary,
        &src,
        profile,
    )
    .expect_err("a binary older than its sources must be refused");
    assert!(
        err.contains("STALE") && err.contains("cargo build -p gardenbed-daemon"),
        "the mtime refusal names the fix: {err}"
    );
    set_mtime(&main_rs, main_orig);

    // A binary with no marker at all is not silently trusted.
    fs::write(&binary, b"no fingerprint here").expect("markerless binary");
    let err = check_binary_against_sources(
        "gardenbed-daemon",
        "daemon",
        "cargo build -p gardenbed-daemon",
        DIGEST_MARKER_PREFIX_DAEMON,
        &binary,
        &src,
        profile,
    )
    .expect_err("a markerless binary cannot be checked and must be refused");
    assert!(err.contains("carries no"), "the refusal says why: {err}");

    let _ = fs::remove_dir_all(&tmp);
}

/// The real workspace binary must carry the daemon marker, and the
/// mutation of the REAL sources (content changed under a held older mtime
/// — fresh-looking mtime, changed tree) must be refused by `daemon_bin()`
/// through that marker. Content and mtime are restored exactly; the
/// restore mtime comes from the untouched sibling `main.rs`, never from
/// the marker file (a crashed earlier run must not poison itself).
fn real_source_mutations() {
    let src = daemon_src_dir();
    let marker = src.join("lib.rs");
    // Locate the fresh binary ONCE, before any mutation: the remedy test
    // needs a PATH (calling daemon_bin() on the stale state it builds
    // would panic by design).
    let located = daemon_bin();
    assert!(
        common::embedded_digest_of(&located, DIGEST_MARKER_PREFIX_DAEMON).is_some(),
        "the built daemon carries its fingerprint marker"
    );

    // The RAII guard restores content and mtimes on BOTH paths: an assert
    // below, or any panic (including a killed-run unwind), must never leave
    // the probe in the tree — one aborted attempt used to deposit a probe
    // that poisoned every later digest check in the checkout.
    let restore = Restore::capture(&src);
    let restore_mtime = src
        .join("main.rs")
        .metadata()
        .and_then(|m| m.modified())
        .expect("main.rs stat");

    let mut probe = fs::read(&marker).expect("lib.rs readable");
    probe.extend_from_slice(b"\n// tree-state staleness probe\n");
    fs::write(&marker, &probe).expect("probe written");
    filetime::set_file_mtime(&marker, filetime::FileTime::from_system_time(restore_mtime))
        .expect("mtime held");
    let panic = std::panic::catch_unwind(daemon_bin)
        .expect_err("tree-state-stale daemon binary must be refused");
    drop(restore);

    let msg = panic_message(&panic);
    assert!(
        msg.contains("DIFFERENT")
            && msg.contains("crates/daemon")
            && msg.contains("cargo build -p gardenbed-daemon"),
        "the refusal names the tree-state mismatch and the fix: {msg}"
    );

    // The DIFFERENT refusal's prescribed remedy must actually rebuild.
    // The phase CONSTRUCTS the stale-by-tree-state workspace itself (a
    // marked build, then pristine sources stamped older), runs the exact
    // command the panic names, and requires the rebuild to happen; a
    // regression to a no-op `cargo build` fails it.
    remedy_rebuilds(&located);

    // The restored tree passes again — no poisoning of later runs.
    let bin = daemon_bin();
    assert!(
        bin.is_file(),
        "daemon_bin returns a file: {}",
        bin.display()
    );
    // mtime oracle on the real binary: a source stamped into the future.
    let original_marker_mtime = marker
        .metadata()
        .and_then(|m| m.modified())
        .expect("lib.rs stat");
    let future = SystemTime::now() + Duration::from_secs(3600);
    filetime::set_file_mtime(&marker, filetime::FileTime::from_system_time(future))
        .expect("touch lib.rs");
    let result = std::panic::catch_unwind(daemon_bin);
    filetime::set_file_mtime(
        &marker,
        filetime::FileTime::from_system_time(original_marker_mtime),
    )
    .expect("restore lib.rs mtime");
    let panic = result.expect_err("the stale binary must be refused");
    let msg = panic_message(&panic);
    assert!(
        msg.contains("STALE") && msg.contains("cargo build -p gardenbed-daemon"),
        "the refusal names the fix: {msg}"
    );
}

/// Constructs the stale-by-tree-state workspace, then runs the EXACT
/// remedy the DIFFERENT panic names (parsed out of the panic text) and
/// proves the rebuild happened.
///
/// The state is built, not assumed: write a marked `lib.rs`, build the
/// daemon in the located binary's profile (the new binary embeds the
/// marked digest), restore the pristine content, and stamp every daemon
/// source older than that binary. A plain `cargo build` is a NO-OP there
/// — which is the regression this test exists to catch — while the named
/// `touch … && cargo build …` remedy forces the recompile.
///
/// `binary` is the path located BEFORE the state was built (calling
/// `daemon_bin()` on the stale state would panic by design). Content and
/// mtimes are restored on every path, including a panic, by [`Restore`].
fn remedy_rebuilds(binary: &Path) {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("daemon")
        .canonicalize()
        .expect("crates/daemon exists");
    let src = crate_root.join("src");
    let marker = src.join("lib.rs");
    let profile = profile_of(binary);
    let mut restore = Restore::capture(&src);
    // Self-healing: if anything panics between the marked build and the
    // remedy (or the remedy itself fails), Drop restores content/mtimes
    // AND rebuilds the daemon so a marked binary cannot poison later runs.
    restore.watch_binary(binary.to_path_buf(), crate_root.clone(), profile);

    // 1. Marked build: the binary now embeds the digest of the marked
    //    tree. Same profile as the located file.
    let pristine = fs::read(&marker).expect("lib.rs readable");
    let mut marked = pristine.clone();
    marked.extend_from_slice(b"\n// remedy-proof marked build\n");
    fs::write(&marker, &marked).expect("marked lib.rs written");
    let mut build = std::process::Command::new(cargo_bin());
    build
        .current_dir(&crate_root)
        .args(["build", "-p", "gardenbed-daemon"]);
    if profile == "release" {
        build.arg("--release");
    }
    let out = build.output().expect("marked cargo build runs");
    assert!(
        out.status.success(),
        "the marked build must succeed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // 2. Restore pristine sources and make them OLDER than the binary:
    //    exactly the state in which Cargo considers the crate fresh.
    restore.restore_content();
    let binary_age = binary
        .metadata()
        .and_then(|m| m.modified())
        .expect("binary mtime");
    let older = binary_age - Duration::from_secs(60);
    restore.stamp_all(older);
    let stale_marker = common::embedded_digest_of(binary, DIGEST_MARKER_PREFIX_DAEMON)
        .expect("the marked binary keeps its marker");
    assert_ne!(
        stale_marker,
        common::current_src_digest(&src),
        "precondition: the located binary is stale by tree state"
    );

    // 3. The guard refuses, and its message names the remedy.
    let msg = check_binary_against_sources(
        "gardenbed-daemon",
        "daemon",
        "cargo build -p gardenbed-daemon",
        DIGEST_MARKER_PREFIX_DAEMON,
        binary,
        &src,
        profile,
    )
    .expect_err("the constructed stale state must be refused");
    assert!(
        msg.contains("DIFFERENT") && msg.contains("touch crates/daemon/src/lib.rs"),
        "the refusal names a rebuild-forcing remedy: {msg}"
    );
    let command = prescribed_command(&msg);

    // 4. Run the panic's own command, from the workspace root.
    let workspace_root = crate_root
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");
    for part in command.split(" && ") {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(part)
            .current_dir(workspace_root)
            .env("PATH", cargo_path_env())
            .output()
            .unwrap_or_else(|e| panic!("running {part:?}: {e}"));
        assert!(
            out.status.success(),
            "the prescribed step {part:?} must succeed: {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // 5. The binary was really rebuilt from the current sources, and the
    //    guard accepts it. (Calling daemon_bin() only AFTER the remedy is
    //    what makes this an end-to-end proof.)
    let rebuilt = common::embedded_digest_of(binary, DIGEST_MARKER_PREFIX_DAEMON)
        .expect("the rebuilt binary keeps its marker");
    assert_eq!(
        rebuilt,
        common::current_src_digest(&src),
        "the prescribed remedy rebuilt the binary from the current sources \
         (a no-op `cargo build` leaves the old marker)"
    );
    assert_ne!(
        rebuilt, stale_marker,
        "the marker actually changed (the remedy is not a no-op)"
    );
    let accepted = daemon_bin();
    assert!(accepted.is_file(), "the guard accepts the rebuilt binary");
}

/// The command inside the backticks of the `Rebuild first ...:\n` clause.
fn prescribed_command(msg: &str) -> String {
    let anchor = "Rebuild first";
    let after = &msg[msg.find(anchor).expect("the refusal names a remedy")..];
    let open = after.find('`').expect("the remedy is backticked") + 1;
    let close = after[open..].find('`').expect("the remedy is backticked") + open;
    after[open..close].to_owned()
}

/// `$CARGO` when the harness set it, else `cargo`.
fn cargo_bin() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

/// A PATH that includes the directory of `$CARGO` (if set), so the literal
/// `cargo` in the prescribed command resolves to the same toolchain.
fn cargo_path_env() -> String {
    let mut path = std::env::var("PATH").unwrap_or_default();
    if let Ok(cargo) = std::env::var("CARGO")
        && let Some(dir) = Path::new(&cargo).parent()
    {
        path = format!("{}:{}", dir.display(), path);
    }
    path
}

/// Captures `src` content and mtimes and restores them on drop — including
/// while unwinding — so a failed remedy proof cannot poison later runs.
/// When armed with [`Restore::watch_binary`], it also rebuilds the binary
/// from the restored sources on an unwind path — and after the phase's
/// final verification asserts, because it stays armed for the whole phase
/// — so a marked binary left behind by a failure cannot make every later
/// run refuse.
struct Restore {
    files: Vec<(PathBuf, Vec<u8>, SystemTime)>,
    /// `Some` when `watch_binary` armed the repair build. There is no
    /// disarm: Drop's digest check already returns early for a healthy
    /// binary, and keeping the guard armed through the VERIFICATION
    /// asserts is what heals the workspace when one of them fails.
    repair: Option<(PathBuf, PathBuf, &'static str)>,
}

impl Restore {
    fn capture(src: &Path) -> Self {
        let mut files = Vec::new();
        for path in walk(src) {
            let content = fs::read(&path).expect("source readable");
            let mtime = path
                .metadata()
                .and_then(|m| m.modified())
                .expect("source mtime");
            files.push((path, content, mtime));
        }
        Self {
            files,
            repair: None,
        }
    }

    fn watch_binary(&mut self, binary: PathBuf, crate_root: PathBuf, profile: &'static str) {
        self.repair = Some((binary, crate_root, profile));
    }

    fn restore_content(&self) {
        for (path, content, _) in &self.files {
            fs::write(path, content).expect("source restored");
        }
    }

    fn stamp_all(&self, when: SystemTime) {
        for (path, _, _) in &self.files {
            filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when))
                .expect("source stamped");
        }
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        for (path, content, mtime) in &self.files {
            let _ = fs::write(path, content);
            let _ = filetime::set_file_mtime(path, filetime::FileTime::from_system_time(*mtime));
        }
        let Some((binary, crate_root, profile)) = &self.repair else {
            return;
        };
        // Unwind path: make sure the binary matches the restored sources.
        let src = crate_root.join("src");
        let digest = common::current_src_digest(&src);
        if common::embedded_digest_of(binary, DIGEST_MARKER_PREFIX_DAEMON).as_deref()
            == Some(digest.as_str())
        {
            return;
        }
        // Touch a source into the FUTURE so Cargo cannot consider the
        // crate fresh even when the binary's mtime is the same second.
        let stamp = src.join("lib.rs");
        let _ = filetime::set_file_mtime(
            &stamp,
            filetime::FileTime::from_system_time(SystemTime::now() + Duration::from_secs(5)),
        );
        let mut build = std::process::Command::new(cargo_bin());
        build
            .current_dir(crate_root)
            .args(["build", "-p", "gardenbed-daemon"]);
        if *profile == "release" {
            build.arg("--release");
        }
        let _ = build.output();
        // Restore the captured mtimes last, so the tree looks untouched.
        for (path, _, mtime) in &self.files {
            let _ = filetime::set_file_mtime(path, filetime::FileTime::from_system_time(*mtime));
        }
    }
}

/// Every regular file under `dir`, recursively.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).expect("dir readable") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

/// The override trusts the caller as-is: no source walk at all.
fn override_passthrough() {
    let bin = daemon_bin();
    // SAFETY: this is the only test in this binary setting the variable,
    // and the refusal phases have returned (no concurrent walker).
    unsafe {
        std::env::set_var("BED_DAEMON_BIN", &bin);
    }
    let overridden = daemon_bin();
    // SAFETY: see above.
    unsafe {
        std::env::remove_var("BED_DAEMON_BIN");
    }
    assert_eq!(overridden, bin, "BED_DAEMON_BIN passes through");

    // A nonexistent override is refused loudly rather than silently
    // falling back to the workspace binary.
    // SAFETY: see above.
    unsafe {
        std::env::set_var("BED_DAEMON_BIN", "/nonexistent/gardenbed-daemon");
    }
    let result = std::panic::catch_unwind(daemon_bin);
    // SAFETY: see above.
    unsafe {
        std::env::remove_var("BED_DAEMON_BIN");
    }
    let panic = result.expect_err("a nonexistent BED_DAEMON_BIN must be refused");
    let msg = panic_message(&panic);
    assert!(
        msg.contains("BED_DAEMON_BIN"),
        "the refusal names the override: {msg}"
    );
}

/// Writes a fake binary whose bytes carry `prefix + hex` exactly as the
/// real build leaves it in rodata, with a fresh mtime.
fn write_marker_binary(path: &Path, prefix: &str, digest: &str) {
    let mut bytes = b"\x7fELF synthetic test binary\n\0".to_vec();
    bytes.extend_from_slice(prefix.as_bytes());
    bytes.extend_from_slice(digest.as_bytes());
    bytes.extend_from_slice(b"\0trailing\0");
    fs::write(path, bytes).expect("synthetic binary written");
    let now = SystemTime::now();
    set_mtime(path, now);
}

/// Recursively copies `from` to `to` (test-fixture scale).
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("dst dir");
    for entry in fs::read_dir(from).expect("src readable") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let dst = to.join(entry.file_name());
        if path.is_dir() {
            copy_tree(&path, &dst);
        } else {
            fs::copy(&path, &dst).expect("file copied");
        }
    }
}

/// Sets `path`'s mtime, recursing into directories.
fn stamp_tree(path: &Path, when: SystemTime) {
    set_mtime(path, when);
    if path.is_dir() {
        for entry in fs::read_dir(path).expect("dir readable") {
            stamp_tree(&entry.expect("dir entry").path(), when);
        }
    }
}

fn set_mtime(path: &Path, when: SystemTime) {
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when)).expect("set mtime");
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .as_ref()
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "<non-string panic>".to_owned())
}
