//! Dogfood finding #12 regression proof: `server_bin()` must refuse a
//! `gardenbed-server` binary older than the `crates/server` sources with
//! a panic naming the fix command — a stale binary fails protocol
//! dependent tests in ways that look like code regressions, because
//! `cargo test -p gardenbed` never rebuilds the server package.
//!
//! These tests live in their own binary (not `common/mod.rs`) on purpose:
//! the staleness test touches `crates/server/src` mtimes, and every test
//! binary compiles `common` — embedding it there would run the touch
//! concurrently across all integration binaries, poisoning their
//! staleness checks mid-run.

mod common;

use std::fs;
use std::path::PathBuf;

use common::server_bin;

fn server_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("server")
        .join("src")
        .canonicalize()
        .expect("crates/server/src exists")
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
    let panic = std::panic::catch_unwind(server_bin).expect_err("the stale binary must be refused");
    filetime::set_file_mtime(
        &marker,
        filetime::FileTime::from_system_time(original_mtime),
    )
    .expect("restore lib.rs mtime");
    let msg = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "<non-string panic>".to_owned());
    assert!(
        msg.contains("STALE") && msg.contains("cargo build -p gardenbed-server"),
        "the refusal names the fix: {msg}"
    );
}

/// All phases run inside ONE `#[test]` on purpose: cargo runs sibling
/// tests on parallel threads, and the refusal phases mutate lib.rs's
/// content/mtime — a concurrent `server_bin()` would observe the window
/// and fail confusingly (this exact race was caught by the full-suite
/// run). Order: mtime refusal → tree-state refusal → fresh binary →
/// override pass-through.
#[test]
fn server_bin_refuses_stale_and_trusts_override() {
    stale_binary_is_refused_with_fix_command();
    tree_state_stale_binary_is_refused_even_with_fresh_mtime();
    // The workspace binary was rebuilt before this suite ran (both
    // refusal phases just proved the oracles are live); the fresh path
    // must return it without panicking.
    let bin = server_bin();
    assert!(
        bin.is_file(),
        "server_bin returns a file: {}",
        bin.display()
    );
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
    let panic =
        std::panic::catch_unwind(server_bin).expect_err("tree-state-stale binary must be refused");
    // Restore content AND mtime — net-zero, and self-healing for a
    // poisoned mtime (restore_mtime comes from main.rs, not the marker).
    fs::write(&marker, &original).expect("lib.rs restored");
    filetime::set_file_mtime(&marker, filetime::FileTime::from_system_time(restore_mtime))
        .expect("mtime restored");
    let msg = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "<non-string panic>".to_owned());
    assert!(
        msg.contains("DIFFERENT") && msg.contains("cargo build -p gardenbed-server"),
        "the refusal names the tree-state mismatch and the fix: {msg}"
    );
    // The restored tree passes again — no poisoning of later runs.
    let bin = server_bin();
    assert!(bin.is_file());
}
