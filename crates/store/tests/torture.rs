//! M1 verification: torn-write torture under SIGKILL.
//!
//! The parent test spawns ITSELF as a child process (same test binary,
//! filtered to `torture_child_entry` via `--exact`). The child puts N blobs
//! (and periodic changesets) into a fresh store, reporting each success on
//! stdout. The parent SIGKILLs the child mid-loop, then reopens the store
//! and asserts that every object the child successfully reported reads back
//! byte-exact (a truncated object must never be observable). After the
//! loop, `PRAGMA integrity_check` must be ok and all committed changesets
//! must have survived.
//!
//! Tuning:
//! - `GARDENBED_TORTURE_ITERS`      — kill iterations (default 25)
//! - `GARDENBED_TORTURE_N`          — blobs per child run (default 40)
//! - `GARDENBED_TORTURE_CHILD_DELAY_US` — child pause between ops (default 1500)

mod common;

use common::random_bytes;
use common::sample_changeset;
use common::xorshift64;
use gardenbed_model::{Changeset, Id};
use gardenbed_store::{FsObjectStore, MetadataStore, ObjectStore, SqliteMetadataStore};
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::str::FromStr;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// Deterministic blob content for (iteration, blob index); parent and child
/// compute identical bytes from these seeds alone.
fn torture_blob_content(iter: u64, idx: usize) -> Vec<u8> {
    let seed = iter
        .wrapping_mul(1_000_003)
        .wrapping_add(idx as u64)
        .wrapping_add(0x0709_17B0);
    let len = 1024 + (xorshift64(seed) % 4096) as usize;
    random_bytes(seed, len)
}

/// Deterministic changeset for (iteration, changeset index).
fn torture_changeset(iter: u64, idx: usize) -> Changeset {
    let seed = iter
        .wrapping_mul(1_000_003)
        .wrapping_add(idx as u64)
        .wrapping_add(500_000);
    sample_changeset(seed, 2)
}

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ------------------------------------------------------------------ parent

/// What a child reported before it was killed.
struct ChildReport {
    /// `(blob index in the child loop, blob id)` per reported object.
    objs: Vec<(usize, Id)>,
    /// `(changeset index, changeset id)` per reported changeset.
    css: Vec<(usize, Id)>,
}

/// One kill iteration: spawn child, let it store a few objects, SIGKILL it,
/// and return everything it reported.
fn kill_iteration(root: &Path, iter: u64, n_blobs: usize) -> ChildReport {
    let exe = std::env::current_exe().expect("current_exe");
    let mut child = Command::new(&exe)
        .arg("torture_child_entry")
        .arg("--exact")
        .arg("--nocapture")
        .env("GARDENBED_TORTURE_CHILD", "1")
        .env("GARDENBED_TORTURE_ROOT", root)
        .env("GARDENBED_TORTURE_ITER", iter.to_string())
        .env("GARDENBED_TORTURE_N", n_blobs.to_string())
        .env(
            "GARDENBED_TORTURE_CHILD_DELAY_US",
            env_or("GARDENBED_TORTURE_CHILD_DELAY_US", 1500).to_string(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn child");

    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);

    let mut objs: Vec<(usize, Id)> = Vec::new();
    let mut css: Vec<(usize, Id)> = Vec::new();
    let mut saw_done = false;

    // Wait for the child's first report — proof it is up and storing — then
    // kill after a small randomized delay to land the SIGKILL mid-loop.
    let first_line;
    loop {
        let mut buf = String::new();
        let n = reader.read_line(&mut buf).expect("read child stdout");
        if n == 0 {
            panic!(
                "child produced no objects before exiting — torture harness broken \
                 (check the --exact filter name and env plumbing)"
            );
        }
        let line = buf.trim_end();
        if line.starts_with("OBJ ") || line.starts_with("CS ") {
            first_line = line.to_owned();
            break;
        }
    }
    record_line(first_line, &mut objs, &mut css, &mut saw_done);

    // Deterministic kill delay, 1..=15 ms, so the kill point varies by
    // iteration.
    let delay_ms = 1 + (xorshift64(iter ^ 0x5163_1C75) % 15);
    thread::sleep(Duration::from_millis(delay_ms));

    child.kill().expect("SIGKILL child");
    // Drain whatever the child reported before it died.
    for line in reader.lines() {
        match line {
            Ok(l) => record_line(l, &mut objs, &mut css, &mut saw_done),
            Err(_) => break,
        }
    }
    let status = child.wait().expect("wait on child");
    if saw_done {
        assert!(
            status.success(),
            "child printed DONE but exited with {status}"
        );
    } else if status.code().is_some() {
        panic!(
            "child exited on its own with code {:?} — crash, not a kill",
            status.code()
        );
    }

    ChildReport { objs, css }
}

fn record_line(
    line: String,
    objs: &mut Vec<(usize, Id)>,
    css: &mut Vec<(usize, Id)>,
    saw_done: &mut bool,
) {
    if let Some(rest) = line.strip_prefix("OBJ ") {
        let mut parts = rest.split_whitespace();
        let idx: usize = parts.next().expect("obj index").parse().expect("obj idx");
        let id = Id::from_str(parts.next().expect("obj id")).expect("obj id hex");
        objs.push((idx, id));
    } else if let Some(rest) = line.strip_prefix("CS ") {
        let mut parts = rest.split_whitespace();
        let idx: usize = parts.next().expect("cs index").parse().expect("cs idx");
        let id = Id::from_str(parts.next().expect("cs id")).expect("cs id hex");
        css.push((idx, id));
    } else if line.trim() == "DONE" {
        *saw_done = true;
    }
    // Anything else is libtest scaffolding; ignore.
}

fn verify_everything(
    root: &Path,
    all_objs: &[(u64, usize, Id)],
    all_css: &[(u64, usize, Id)],
) -> usize {
    let store = FsObjectStore::open(root).expect("reopen objects");
    let mut absent = 0;
    for (iter, idx, id) in all_objs {
        let expected = torture_blob_content(*iter, *idx);
        let got = store.get_blob(*id).expect("get after kill");
        let present_expected = got.is_some();
        match got {
            // Byte-exact or absent; a truncated object must never be
            // observable. Absent can only mean the rename had not been made
            // durable yet — it is never wrong bytes.
            Some(bytes) => assert_eq!(
                bytes, expected,
                "blob iter={iter} idx={idx} id={id} must read back byte-exact"
            ),
            None => absent += 1,
        }
        match store.has(*id) {
            Ok(present) => assert_eq!(
                present, present_expected,
                "has() must agree with get_blob()"
            ),
            Err(e) => panic!("has() after reopen failed: {e:?}"),
        }
    }

    let metadata =
        SqliteMetadataStore::open(&root.join("metadata.sqlite3")).expect("reopen metadata");
    for (iter, idx, id) in all_css {
        let expected = torture_changeset(*iter, *idx);
        let got = metadata
            .get_changeset(*id)
            .expect("get_changeset after kill");
        let (decoded, canonical) = got.expect("committed changeset must survive");
        assert_eq!(decoded, expected, "changeset iter={iter} idx={idx} decoded");
        assert_eq!(
            canonical,
            expected.canonical_bytes().expect("expected canonical"),
            "changeset canonical bytes exact"
        );
    }
    absent
}

#[test]
fn torn_write_torture_sigkill_mid_put() {
    let iters = env_or("GARDENBED_TORTURE_ITERS", 25) as usize;
    let n_blobs = env_or("GARDENBED_TORTURE_N", 40) as usize;

    let root = tempfile::tempdir().expect("tempdir");
    // Pre-create both stores so the first child finds them initialized.
    FsObjectStore::open(root.path()).expect("open objects");
    SqliteMetadataStore::open(&root.path().join("metadata.sqlite3")).expect("open metadata");

    let mut all_objs: Vec<(u64, usize, Id)> = Vec::new();
    let mut all_css: Vec<(u64, usize, Id)> = Vec::new();
    let started = Instant::now();

    for iter in 0..iters as u64 {
        let report = kill_iteration(root.path(), iter, n_blobs);
        assert!(
            !report.objs.is_empty(),
            "every kill must follow at least one stored object"
        );
        for (idx, id) in report.objs {
            all_objs.push((iter, idx, id));
        }
        for (idx, id) in report.css {
            all_css.push((iter, idx, id));
        }

        // After each kill: every reported object reads back byte-exact (or
        // was never durably published).
        verify_everything(root.path(), &all_objs, &all_css);
    }

    // After the loop: sqlite integrity + full survival sweep.
    let conn = rusqlite::Connection::open(root.path().join("metadata.sqlite3"))
        .expect("open db for integrity check");
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .expect("integrity_check runs");
    assert_eq!(verdict, "ok", "PRAGMA integrity_check after torture");
    let absent = verify_everything(root.path(), &all_objs, &all_css);

    assert!(
        !all_css.is_empty(),
        "torture must have committed at least one changeset"
    );
    eprintln!(
        "torture: {iters} kills, {} blobs verified ({} absent), {} changesets survived, {:?}",
        all_objs.len() - absent,
        absent,
        all_css.len(),
        started.elapsed()
    );
}

// ------------------------------------------------------------------- child

/// Child entry point. When run WITHOUT the torture env var (i.e. during a
/// normal `cargo test`), this test does nothing.
#[test]
fn torture_child_entry() {
    if std::env::var("GARDENBED_TORTURE_ROOT").is_err()
        || std::env::var("GARDENBED_TORTURE_CHILD").ok().as_deref() != Some("1")
    {
        return;
    }
    let iter: u64 = std::env::var("GARDENBED_TORTURE_ITER")
        .expect("child iter")
        .parse()
        .expect("iter number");
    let n_blobs: usize = std::env::var("GARDENBED_TORTURE_N")
        .expect("child n")
        .parse()
        .expect("n number");
    let delay_us = env_or("GARDENBED_TORTURE_CHILD_DELAY_US", 1500);

    let root = std::env::var("GARDENBED_TORTURE_ROOT").expect("child root");
    let root = Path::new(&root);
    let store = FsObjectStore::open(root).expect("child opens objects");
    let metadata =
        SqliteMetadataStore::open(&root.join("metadata.sqlite3")).expect("child opens metadata");

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for i in 0..n_blobs {
        let content = torture_blob_content(iter, i);
        let id = store.put_blob(&content).expect("child put_blob");
        writeln!(out, "OBJ {i} {}", id.to_hex()).expect("write OBJ");
        out.flush().expect("flush OBJ");

        if i % common::CHANGESET_EVERY == 0 {
            let cs = torture_changeset(iter, i / common::CHANGESET_EVERY);
            let cs_id = metadata.put_changeset(&cs).expect("child put_changeset");
            writeln!(out, "CS {} {}", i / common::CHANGESET_EVERY, cs_id.to_hex())
                .expect("write CS");
            out.flush().expect("flush CS");
        }
        thread::sleep(Duration::from_micros(delay_us));
    }
    writeln!(out, "DONE").expect("write DONE");
    out.flush().expect("flush DONE");
}
