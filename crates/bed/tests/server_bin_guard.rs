//! Dogfood finding #12 regression proof: `server_bin()` must refuse a
//! `gardenbed-server` binary built from DIFFERENT tree state than the
//! working copy — even when its mtime looks fresh — with a panic naming
//! the fix command. mtime alone is blind to that (`bed switch`
//! materializes sources with commit-time mtimes), so the second oracle
//! reads the fingerprint embedded in the LOCATED BINARY's own bytes
//! (`GARDENBED_SERVER_SRC_DIGEST:<hex>`, from
//! `gardenbed_server::SRC_DIGEST_MARKER` referenced from the bin's
//! `main`) and compares it with the digest of the current sources.
//!
//! Reading the file matters: the crate's `SRC_DIGEST` const is linked into
//! this **test** binary from the working copy, so comparing against it
//! would always compare the tree with itself and accept any binary — the
//! hole this guard had before. `hermetic_oracles` pins the fixed shape
//! with a synthetic binary/source pair, `real_source_mutations` proves the
//! real workspace binary carries a marker and is refused when the sources
//! change under it.
//!
//! These tests live in their own binary (not `common/mod.rs`) on purpose:
//! the staleness test touches `crates/server/src`, and every test binary
//! compiles `common` — embedding it there would run the touch
//! concurrently across all integration binaries, poisoning their
//! staleness checks mid-run.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{DIGEST_MARKER_PREFIX_SERVER, check_binary_against_sources, server_bin};

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
    format!("touch crates/server/src/lib.rs && cargo build -p gardenbed-server{suffix}")
}
fn profile_of(bin: &std::path::Path) -> &'static str {
    if bin.components().any(|c| c.as_os_str() == "release") {
        "release"
    } else {
        "debug"
    }
}

fn server_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("server")
        .join("src")
        .canonicalize()
        .expect("crates/server/src exists")
}

/// All phases run inside ONE `#[test]` on purpose: cargo runs sibling
/// tests on parallel threads, and the refusal phases mutate lib.rs's
/// content/mtime — a concurrent `server_bin()` would observe the window
/// and fail confusingly (this exact race was caught by the full-suite
/// run). Order: hermetic oracle proofs → real mtime refusal → real
/// tree-state refusal → fresh binary → override pass-through.
#[test]
fn server_bin_refuses_stale_and_trusts_override() {
    // An override is trusted as-is (the oracles are deliberately inactive),
    // so the mutation phases cannot pass while BED_SERVER_BIN is set — the
    // canary form that points the harness at the deployed pair. Skip with an
    // actionable message instead of failing after mutating sources.
    if let Ok(path) = std::env::var("BED_SERVER_BIN") {
        eprintln!(
            "skipping the oracle proof: BED_SERVER_BIN is set ({path}), and an \
             override is trusted as-is (the oracles are deliberately inactive). \
             The override contract itself is still checked below; unset \
             BED_SERVER_BIN to exercise the staleness guard."
        );
        // The override contract is the one property that IS reachable in
        // this configuration — never skip it.
        override_passthrough();
        return;
    }
    hermetic_oracles();
    stale_binary_is_refused_with_fix_command();
    tree_state_stale_binary_is_refused_even_with_fresh_mtime();
    // The workspace binary was rebuilt before this suite ran; the fresh
    // path must return it without panicking.
    let bin = server_bin();
    assert!(
        bin.is_file(),
        "server_bin returns a file: {}",
        bin.display()
    );
    override_passthrough();
}

/// The override trusts the caller as-is: no source walk at all. Runs in both
/// the normal and the canary (override-set) configuration.
fn override_passthrough() {
    let bin = server_bin();
    // The override trusts the caller: point it at the same binary and the
    // guard must not walk sources at all.
    //
    // SAFETY: this is the only test setting BED_SERVER_BIN, and the
    // refusal phases have already returned (no concurrent walker).
    unsafe {
        std::env::set_var("BED_SERVER_BIN", &bin);
    }
    let overridden = server_bin();
    // SAFETY: see above.
    unsafe {
        std::env::remove_var("BED_SERVER_BIN");
    }
    assert_eq!(overridden, bin, "BED_SERVER_BIN passes through");
}

/// Oracle-level proof, fully hermetic and without touching the real
/// crate: a synthetic binary carrying the fingerprint of a MARKED copy of
/// the sources, with the copy restored and stamped older than the binary
/// — the "built from tree state X" shape. Only the binary-side digest can
/// refuse it, and a "rebuilt" marker (current digest) must pass.
fn hermetic_oracles() {
    let tmp = std::env::temp_dir().join(format!("gb-server-guard-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    copy_tree(&server_src_dir(), &tmp.join("server").join("src"));
    let src = tmp.join("server").join("src");
    let bin_dir = tmp.join("bin");
    fs::create_dir_all(&bin_dir).expect("bin dir");

    let marker_file = src.join("lib.rs");
    let pristine = fs::read(&marker_file).expect("copied lib.rs");
    let mut marked = pristine.clone();
    marked.extend_from_slice(b"\n// server tree-state probe\n");
    fs::write(&marker_file, &marked).expect("marked copy written");
    let built_from = common::current_src_digest(&src);
    fs::write(&marker_file, &pristine).expect("copy restored");

    let binary = bin_dir.join("gardenbed-server");
    let profile = profile_of(&binary);
    write_marker_binary(&binary, DIGEST_MARKER_PREFIX_SERVER, &built_from);
    // Sources older than the binary: the mtime oracle passes.
    for entry in walk(&src) {
        filetime::set_file_mtime(
            &entry,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - Duration::from_secs(3600),
            ),
        )
        .expect("stamp older");
    }

    let err = check_binary_against_sources(
        "gardenbed-server",
        "server",
        "cargo build -p gardenbed-server",
        DIGEST_MARKER_PREFIX_SERVER,
        &binary,
        &src,
        profile,
    )
    .expect_err("a binary built from other tree state must be refused");
    assert!(
        err.contains("DIFFERENT")
            && err.contains("crates/server")
            && err.contains(&force_rebuild(profile)),
        "the digest refusal names the mismatch and a remedy that actually \
         rebuilds: {err}"
    );

    // Rebuild: the marker now matches the current sources → accepted.
    let current = common::current_src_digest(&src);
    write_marker_binary(&binary, DIGEST_MARKER_PREFIX_SERVER, &current);
    check_binary_against_sources(
        "gardenbed-server",
        "server",
        "cargo build -p gardenbed-server",
        DIGEST_MARKER_PREFIX_SERVER,
        &binary,
        &src,
        profile,
    )
    .expect("a rebuilt binary passes");
    let _ = fs::remove_dir_all(&tmp);
}

/// `server_bin()` refuses a binary older than `crates/server` sources
/// with a panic naming the fix command. Touching one source file into
/// the future is the oracle: the guard must refuse while it is newer
/// than any build artifact. The touch restores the original mtime —
/// net-zero filesystem change (content is never modified; mtime is put
/// back exactly), or the test would poison every later run.
fn stale_binary_is_refused_with_fix_command() {
    let src = server_src_dir();
    let marker = src.join("lib.rs");
    let original_mtime = marker
        .metadata()
        .and_then(|m| m.modified())
        .expect("crates/server/src/lib.rs stat");
    // mtime into the future: newer than any build artifact. This is the
    // only test binary that touches server mtimes, and the mtime is put
    // back before the assert returns.
    let future = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
    filetime::set_file_mtime(&marker, filetime::FileTime::from_system_time(future))
        .expect("touch lib.rs");
    let result = std::panic::catch_unwind(server_bin);
    filetime::set_file_mtime(
        &marker,
        filetime::FileTime::from_system_time(original_mtime),
    )
    .expect("restore lib.rs mtime");
    let panic = result.expect_err("the stale binary must be refused");
    let msg = panic_message(&panic);
    assert!(
        msg.contains("STALE") && msg.contains("cargo build -p gardenbed-server"),
        "the refusal names the fix: {msg}"
    );
}

/// The content-digest oracle: a binary built from DIFFERENT tree state
/// than the working copy must be refused even when its mtime looks fresh
/// — the dogfood #12 core (`bed switch` materializes sources with
/// commit-time mtimes, so mtime alone cannot see tree-state staleness).
/// Oracle: append a comment to one server source while holding its mtime
/// at the original value, so the mtime check passes and only the digest
/// can catch it.
fn tree_state_stale_binary_is_refused_even_with_fresh_mtime() {
    let src = server_src_dir();
    let marker = src.join("lib.rs");
    let original = fs::read(&marker).expect("lib.rs readable");
    // The restore target mtime comes from the UNTOUCHED sibling main.rs —
    // bed materializes all sources with commit-time mtimes, so this is
    // lib.rs's true stamp. Never capture from lib.rs itself: a crashed
    // earlier run may have left it stamped NOW, and re-capturing that
    // value would faithfully restore the poison (self-sustaining).
    let restore_mtime = src
        .join("main.rs")
        .metadata()
        .and_then(|m| m.modified())
        .expect("main.rs stat");
    // Different content, SAME old mtime — fs::write stamps NOW, which
    // would trip the mtime oracle before the digest one is reached, so
    // the old mtime is held immediately after the write; the point is
    // that ONLY the digest can catch this probe.
    let mut probe = original.clone();
    probe.extend_from_slice(b"\n// tree-state staleness probe\n");
    fs::write(&marker, &probe).expect("probe written");
    filetime::set_file_mtime(&marker, filetime::FileTime::from_system_time(restore_mtime))
        .expect("mtime held");
    let result = std::panic::catch_unwind(server_bin);
    // Restore content AND mtime BEFORE interpreting the result — net-zero,
    // and self-healing for a poisoned mtime (restore_mtime comes from
    // main.rs, not the marker). An aborted run must never leave the probe
    // behind: one deposit per attempt used to poison every later digest
    // check in the tree.
    fs::write(&marker, &original).expect("lib.rs restored");
    filetime::set_file_mtime(&marker, filetime::FileTime::from_system_time(restore_mtime))
        .expect("mtime restored");
    let panic = result.expect_err("tree-state-stale binary must be refused");
    let msg = panic_message(&panic);
    assert!(
        msg.contains("DIFFERENT") && msg.contains("cargo build -p gardenbed-server"),
        "the refusal names the tree-state mismatch and the fix: {msg}"
    );
    // The restored tree passes again — no poisoning of later runs.
    let bin = server_bin();
    assert!(bin.is_file());
}

/// Writes a fake binary carrying `prefix + hex` exactly as the real build
/// leaves it in rodata, with a fresh mtime.
fn write_marker_binary(path: &Path, prefix: &str, digest: &str) {
    let mut bytes = b"\x7fELF synthetic test binary\n\0".to_vec();
    bytes.extend_from_slice(prefix.as_bytes());
    bytes.extend_from_slice(digest.as_bytes());
    bytes.extend_from_slice(b"\0trailing\0");
    fs::write(path, bytes).expect("synthetic binary written");
    filetime::set_file_mtime(
        path,
        filetime::FileTime::from_system_time(std::time::SystemTime::now()),
    )
    .expect("fresh mtime");
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

/// Every regular file under `dir`, recursively.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).expect("listing readable") {
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

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    panic
        .as_ref()
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "<non-string panic>".to_owned())
}
