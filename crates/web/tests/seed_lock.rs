//! `gardenbed-web seed` is a writer: it must hold the SHARED `repo.lock`
//! for its whole run, exactly like `gardenbed-server import`, so an
//! offline `gardenbed-server gc` (exclusive, non-blocking) refuses to
//! interleave.
//!
//! The oracle is CAUSAL, not timing-based:
//!
//! 1. With the exclusive lock held, the seed is blocked *before*
//!    `Repo::open`, so at ~300 ms the repository root must contain ONLY
//!    `repo.lock` — no `metadata.sqlite3`, no `objects/`. A lockless seed
//!    creates those within tens of milliseconds, so their presence proves
//!    the lock is gone even though the whole seed outlasts the window.
//! 2. After release, the same child must exit 0 and report `seeded tip`
//!    with a published `main`.
//! 3. Oracle 3 samples once, immediately after `<root>/metadata.sqlite3`
//!    appears while the child is still running: a seed that holds the
//!    shared flock across `Repo::open` until exit is guaranteed to be
//!    observed as `Held` there; a guard that acquires and immediately
//!    drops has already released by then. (Blind sampling is a race and
//!    was observed to miss the momentary hold; this probe is deliberately
//!    ordered after the file's appearance for that reason.)
//!
//! The test does not depend on `GARDENBED_FSYNC` (it asserts nothing about
//! durability ordering) and runs entirely inside a tempdir.

use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::tempdir;

/// How long the seed is observed while the exclusive lock is held.
const BLOCKED_WINDOW: Duration = Duration::from_millis(300);

#[test]
fn seed_waits_for_the_exclusive_gc_lock_then_completes() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path().join("repo");

    // The gc-side lock: exclusive, taken before the seed starts. This
    // also creates `repo.lock`, the only entry allowed to exist while the
    // seed is blocked.
    let guard = gardenbed_server::gc::lock_exclusive(&root).expect("exclusive lock");

    let mut child = Command::new(env!("CARGO_BIN_EXE_gardenbed-web"))
        .arg("seed")
        .arg("--root")
        .arg(&root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("gardenbed-web seed spawns");

    // Causal assertion: a seed that took the shared lock cannot have
    // opened the repo yet, so nothing but the lock file exists. A
    // lockless seed would already have created the store and metadata.
    std::thread::sleep(BLOCKED_WINDOW);
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "seed must still be waiting for the shared repo.lock"
    );
    let mut names: Vec<String> = std::fs::read_dir(&root)
        .expect("root readable")
        .map(|e| {
            e.expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["repo.lock".to_owned()],
        "while blocked on the shared lock the seed must not have opened \
         the repository (no metadata.sqlite3, no objects/) — seeing them \
         means the seed ran without the lock"
    );

    // Release the exclusive lock: the same child proceeds and completes.
    drop(guard);

    // Third oracle: is the shared lock still held once the seed is inside
    // its write phase? Sampling an exclusive probe blindly is a race — a
    // momentary shared hold can be missed between 1 ms polls. So probe
    // only AFTER `<root>/metadata.sqlite3` exists (`Repo::open` created
    // it, i.e. the shared acquisition already succeeded) and only while
    // the child is verifiably still running. A seed that holds the guard
    // until completion is guaranteed to be observed there; one that
    // drops immediately after acquiring has already released by then.
    let mut saw_shared_holder = false;
    let mut opened = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if !opened && root.join("metadata.sqlite3").exists() {
            // Past `Repo::open` — the shared acquisition already
            // succeeded. Probe immediately (before consulting try_wait)
            // so the observation cannot be lost to the child exiting
            // between the two calls; a whole seed stays in this state
            // for hundreds of milliseconds, far longer than one poll.
            opened = true;
            match gardenbed_server::gc::lock_exclusive(&root) {
                Ok(probe) => drop(probe), // released early — not caught
                Err(gardenbed_server::gc::LockError::Held) => saw_shared_holder = true,
                Err(e) => panic!("probing the gc lock: {e}"),
            }
        }
        if child.try_wait().expect("try_wait").is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the seed did not finish within 30 s"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let out = child.wait_with_output().expect("seed finishes");
    assert!(opened, "the seed exited before creating metadata.sqlite3");
    assert!(
        saw_shared_holder,
        "the shared repo.lock must still be held after Repo::open while \
         the seed writes: an exclusive probe taken once metadata.sqlite3 \
         exists and the child is alive must observe it as Held"
    );
    assert!(
        out.status.success(),
        "seed exits 0 after the lock is released: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("seeded tip"),
        "seed reports the tip: {stdout}"
    );

    // The history really landed, and `main` is published.
    let repo = gardenbed_server::Repo::open(&root).expect("seeded repo opens");
    let tip = repo.get_ref("main").expect("get_ref").expect("main exists");
    assert!(
        repo.log(tip, 0).expect("walk").len() > 1,
        "the demo history is reachable from main"
    );
}
