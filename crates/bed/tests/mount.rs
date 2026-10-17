//! The virtualized checkout (FUSE), end to end against a real server:
//! the whole repository tree is listable and readable through the mount
//! while `.bed/objects` gains exactly what was read (lazy
//! materialization through the existing cache-first fetch path), byte
//! fidelity for files/symlinks/exec bits, `EROFS` for writes,
//! concurrency without deadlock, explicit and external unmount, and a
//! seeded stability loop.
//!
//! Every test here GATES on FUSE availability: machines without
//! `/dev/fuse` or `fusermount3` (CI runners) skip with the message
//! below — where FUSE *is* available the tests run and must pass. The
//! module-level API-shape tests in `src/mount.rs` always run, so the
//! module stays compiled everywhere.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gardenbed::mount::{fuse_available, mount};
use gardenbed::workspace::WorkingCopy;
use gardenbed_harness::generator::{GenOptions, GeneratedHistory, files_at, generate};
use gardenbed_model::{Id, Mode};
use tempfile::TempDir;

use common::Server;

/// The message printed when the host cannot mount FUSE filesystems.
const SKIP: &str = "mount test skipped: /dev/fuse unavailable";

macro_rules! require_fuse {
    () => {
        if !fuse_available() {
            eprintln!("{SKIP}");
            return;
        }
    };
}

/// One running server with an ingested synthetic history and a working
/// copy (created via the real `bed clone`, so the mount runs against
/// exactly the state a user would have).
struct Garden {
    /// Keeps the server root, the git oracle and the garden alive.
    _dir: TempDir,
    /// The working-copy root: `bed` commands and the mount live here.
    garden: PathBuf,
    server: Server,
    history: GeneratedHistory,
}

impl Garden {
    fn state_dir(&self) -> PathBuf {
        self.garden.join(".bed")
    }

    fn head_index(&self) -> usize {
        self.history.changesets.len() - 1
    }

    /// The working-copy files at head, as `path → (blob content, mode)`
    /// — ground truth from the generator, independent of the mount.
    fn expected_files(&self) -> BTreeMap<String, (Vec<u8>, Mode)> {
        files_at(&self.history, self.head_index())
            .into_iter()
            .map(|(path, (id, mode))| {
                (
                    path,
                    (
                        self.history
                            .blob(&id)
                            .expect("history carries its blobs")
                            .to_vec(),
                        mode,
                    ),
                )
            })
            .collect()
    }
}

/// A history with at least one exec file and one symlink, so the mode
/// fidelity checks always have material to work with. Deterministic:
/// the first seed in `1..` that qualifies.
fn qualifying_history() -> GeneratedHistory {
    let opts = GenOptions {
        commits: 30,
        ..GenOptions::default()
    };
    for seed in 1..100u64 {
        let history = generate(seed, &opts);
        let files = files_at(&history, history.changesets.len() - 1);
        let has = |want: Mode| files.values().any(|(_, m)| *m == want);
        if has(Mode::Exec) && has(Mode::Symlink) {
            return history;
        }
    }
    panic!("no seed in 1..100 produced an exec + symlink history");
}

/// Boots the server, ingests the history, clones a working copy, and
/// switches it to a **fully virtual** state: no sparse paths, nothing
/// materialized — the mount is the only way to see the tree.
async fn setup() -> Garden {
    let dir = tempfile::tempdir().expect("tempdir");
    let server_root = dir.path().join("server");
    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir created");
    let history = qualifying_history();
    common::ingest(&history, &server_root);
    let server = Server::spawn(&server_root).await;
    let (code, out, stderr) = common::bed(&garden, &["clone", &server.http()]);
    assert_eq!(code, 0, "clone failed: {stderr}\n{out}");
    // The clone selected no sparse paths and materialized nothing: the
    // mount must show EVERYTHING even though the working copy selected
    // nothing.
    assert!(
        common::bed(&garden, &["status"])
            .1
            .contains("no sparse paths selected"),
        "clone should start with no sparse selection"
    );
    Garden {
        _dir: dir,
        garden,
        server,
        history,
    }
}

/// Relative paths of everything under `root` (files, symlinks, dirs).
fn tree_under(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                out.push(path.clone());
                walk(&entry.path(), &path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, "", &mut out);
    out.sort();
    out
}

/// Walks the mount listing every directory (readdir proof) and returns
/// the sorted relative paths of all non-directory entries.
fn mount_listing(mountpoint: &Path) -> Vec<String> {
    tree_under(mountpoint)
        .into_iter()
        .filter(|p| {
            let full = mountpoint.join(p);
            !full
                .symlink_metadata()
                .expect("stat works")
                .file_type()
                .is_dir()
        })
        .collect()
}

// ---------------------------------------------------------------- mount

/// The full repository tree is listable through the mount — not just
/// sparse paths (this working copy selected NONE).
#[tokio::test]
async fn mount_lists_the_whole_tree() {
    require_fuse!();
    let mut g = setup().await;
    let mountpoint = g.garden.join("mnt");
    fs::create_dir(&mountpoint).unwrap();
    let bed = WorkingCopy::open(&g.state_dir()).expect("working copy opens");
    let handle = mount(&bed, &mountpoint).await.expect("mount succeeds");
    assert_eq!(handle.mountpoint(), mountpoint);

    // Every path appears in the listing — gitlinks surface as empty
    // read-only files rather than being hidden.
    let mut expected: Vec<String> = g.expected_files().keys().cloned().collect();
    expected.sort();
    let listing = mount_listing(&mountpoint);
    assert_eq!(listing, expected, "readdir must list every path");

    handle.unmount().expect("unmount succeeds");
    // The mountpoint is an ordinary directory again and removable.
    fs::remove_dir(&mountpoint).expect("mountpoint removable after unmount");
    g.server.shutdown().await;
}

/// getattr / get file contents of never-fetched paths are byte-identical
/// to `bed file`; symlink targets and exec bits surface correctly.
#[tokio::test]
async fn mount_reads_bytes_symlinks_and_exec_bits() {
    require_fuse!();
    let mut g = setup().await;
    let mountpoint = g.garden.join("mnt");
    fs::create_dir(&mountpoint).unwrap();
    let bed = WorkingCopy::open(&g.state_dir()).expect("working copy opens");
    let handle = mount(&bed, &mountpoint).await.expect("mount succeeds");

    let expected = g.expected_files();
    for (path, (content, mode)) in &expected {
        let full = mountpoint.join(path);
        let meta = full.symlink_metadata().expect("getattr works");
        match mode {
            Mode::Symlink => {
                assert!(meta.file_type().is_symlink(), "{path} must be a symlink");
                let target = fs::read_link(&full).expect("readlink works");
                assert_eq!(
                    &target.to_string_lossy().as_bytes(),
                    content,
                    "{path} target"
                );
            }
            Mode::Exec => {
                assert!(meta.file_type().is_file(), "{path} must be a file");
                assert!(
                    meta.permissions().mode() & 0o111 != 0,
                    "{path} must carry an exec bit"
                );
                assert_eq!(&fs::read(&full).unwrap(), content, "{path} bytes");
            }
            Mode::Regular => {
                assert!(meta.file_type().is_file(), "{path} must be a file");
                assert!(
                    meta.permissions().mode() & 0o111 == 0,
                    "{path} must not be executable"
                );
                assert_eq!(&fs::read(&full).unwrap(), content, "{path} bytes");
            }
            Mode::Gitlink => {
                // An opaque upstream pointer: surfaces as a file, never
                // a stored object. Listing already covered it.
            }
        }
        // Byte-identity with `bed file` (the command surface's own read
        // path) for every non-gitlink file.
        if *mode != Mode::Gitlink {
            let (code, out, stderr) = common::bed_raw(&g.garden, &["file", path]);
            assert_eq!(
                code,
                0,
                "bed file {path}: {}",
                String::from_utf8_lossy(&stderr)
            );
            assert_eq!(&out, content, "{path} must match bed file byte for byte");
        }
    }

    handle.unmount().expect("unmount succeeds");
    g.server.shutdown().await;
}

/// Virtualization proof: after reading N arbitrary files, the local
/// object cache gained exactly their blobs (server GetStats counters
/// corroborate; unread blobs were never fetched).
#[tokio::test]
async fn mount_fetches_only_what_was_read() {
    require_fuse!();
    let mut g = setup().await;
    let mountpoint = g.garden.join("mnt");
    fs::create_dir(&mountpoint).unwrap();
    let bed = WorkingCopy::open(&g.state_dir()).expect("working copy opens");
    let handle = mount(&bed, &mountpoint).await.expect("mount succeeds");

    let expected = g.expected_files();
    let readable: Vec<(&String, &(Vec<u8>, Mode))> = expected
        .iter()
        .filter(|(_, (_, mode))| *mode == Mode::Regular || *mode == Mode::Exec)
        .collect();
    assert!(readable.len() >= 6, "fixture must have several plain files");

    // Read exactly these four (deterministic: first, middle-ish, and
    // two others spread across the path space).
    let chosen: Vec<(&String, &(Vec<u8>, Mode))> = vec![
        readable[0],
        readable[readable.len() / 2],
        readable[readable.len() - 1],
        readable[readable.len() / 3],
    ];
    let chosen_ids: std::collections::HashSet<Id> = chosen
        .iter()
        .map(|(path, _)| {
            files_at(&g.history, g.head_index())
                .get(*path)
                .expect("path maps to a blob")
                .0
        })
        .collect();

    // The clone materialized nothing and no sparse paths are selected,
    // so the cache starts empty: whatever is cached after the reads is
    // exactly what the mount fetched (checked below against the read
    // set via the store's `has`).

    for (path, (content, _)) in &chosen {
        assert_eq!(&fs::read(mountpoint.join(path)).unwrap(), content);
    }

    // Everything the mount could have needed under the read set — and
    // nothing more. `want_list` mirrors the fetch path's own traversal.
    let (want_trees, want_blobs) = common::want_list(&g.server.root, g.history.head, &["."]);
    let _ = want_trees; // tree fetches depend on walk shape, not on file choice

    // The blobs actually in the cache now: probe each expected blob id.
    let cached_now: std::collections::HashSet<Id> = {
        use gardenbed_store::ObjectStore as _;
        let store = gardenbed_store::FsObjectStore::open(&g.state_dir()).unwrap();
        want_blobs
            .iter()
            .filter(|id| store.has(**id).unwrap())
            .copied()
            .collect()
    };

    // Every read blob must be cached...
    for id in &chosen_ids {
        assert!(
            cached_now.contains(id),
            "read blob {} must be cached",
            id.short()
        );
    }
    // ...and the cached set must be a subset of (chosen ids + blobs the
    // mount never fetched but earlier commands may have pulled). The
    // mount itself fetched nothing outside the read set — proven by the
    // server's get_blob counter delta matching |chosen ids not already
    // cached before the reads|.
    let client = g.server.client().await;
    let (trees, blobs) = g.server.counted(&client).await;
    // The clone materialized nothing and no sparse paths are selected,
    // so before the mount the only traffic was clone's ref lookups. The
    // mount's tree walk + chosen reads are all that can have bumped
    // these. Sanity bounds: at least the chosen blobs, at most every
    // blob in the tree.
    assert!(
        blobs >= chosen_ids.len() as u64 && blobs <= want_blobs.len() as u64,
        "server get_blob counter {blobs} must cover the {} reads and stay within the tree's {} blobs",
        chosen_ids.len(),
        want_blobs.len()
    );
    assert!(trees <= want_trees.len() as u64, "tree fetches bounded");

    // The strongest statement: blobs NOT in the read set were not
    // fetched from the server. Compute how many of the tree's blobs the
    // cache now holds; every cached blob must be in the read set (the
    // mount is the only reader, and it read only the chosen files).
    // Symlink targets count as reads only if their paths were read —
    // they were not (only Regular/Exec paths above were chosen), but
    // `stat` on symlinks fetches targets (documented metadata policy),
    // so exclude symlink blobs from the "never fetched" expectation.
    let symlink_blob_ids: std::collections::HashSet<Id> = expected
        .iter()
        .filter(|(_, (_, mode))| *mode == Mode::Symlink)
        .map(|(path, _)| files_at(&g.history, g.head_index()).get(path).unwrap().0)
        .collect();
    for id in &cached_now {
        assert!(
            chosen_ids.contains(id) || symlink_blob_ids.contains(id),
            "blob {} was fetched but never read",
            id.short()
        );
    }

    handle.unmount().expect("unmount succeeds");
    g.server.shutdown().await;
}

/// Parallel reads of distinct files while another thread walks a
/// different subtree: no deadlock, all bytes correct.
#[tokio::test]
async fn mount_survives_concurrent_reads() {
    require_fuse!();
    let mut g = setup().await;
    let mountpoint = g.garden.join("mnt");
    fs::create_dir(&mountpoint).unwrap();
    let bed = WorkingCopy::open(&g.state_dir()).expect("working copy opens");
    let handle = mount(&bed, &mountpoint).await.expect("mount succeeds");

    let expected = g.expected_files();
    let readable: Vec<(String, Vec<u8>)> = expected
        .iter()
        .filter(|(_, (_, mode))| *mode == Mode::Regular || *mode == Mode::Exec)
        .map(|(p, (c, _))| (p.clone(), c.clone()))
        .collect();
    assert!(readable.len() >= 4, "fixture must have several plain files");

    // Reader threads: two file readers over interleaved halves, plus a
    // directory walker over the whole tree (a different subtree's
    // worth of traffic through the same single-threaded runtime).
    let half = readable.len() / 2;
    let reader = |files: Vec<(String, Vec<u8>)>, root: PathBuf| {
        move || {
            for (path, content) in files {
                assert_eq!(
                    fs::read(root.join(&path)).unwrap_or_else(|e| panic!("read {path}: {e}")),
                    content,
                    "bytes must stay correct under concurrency"
                );
            }
        }
    };
    let walker_root = mountpoint.clone();
    let walker = move || {
        for path in tree_under(&walker_root) {
            let full = walker_root.join(&path);
            if full.symlink_metadata().unwrap().file_type().is_dir() {
                fs::read_dir(&full).unwrap_or_else(|e| panic!("readdir {path}: {e}"));
            }
        }
    };

    let t1 = std::thread::spawn(reader(readable[..half].to_vec(), mountpoint.clone()));
    let t2 = std::thread::spawn(reader(readable[half..].to_vec(), mountpoint.clone()));
    let t3 = std::thread::spawn(walker);
    for t in [t1, t2, t3] {
        t.join().expect("no reader thread panicked");
    }

    handle.unmount().expect("unmount succeeds");
    g.server.shutdown().await;
}

/// A 200-iteration seeded read loop over random paths: zero errors, all
/// bytes verified.
#[tokio::test]
async fn mount_stable_under_read_loop() {
    require_fuse!();
    let mut g = setup().await;
    let mountpoint = g.garden.join("mnt");
    fs::create_dir(&mountpoint).unwrap();
    let bed = WorkingCopy::open(&g.state_dir()).expect("working copy opens");
    let handle = mount(&bed, &mountpoint).await.expect("mount succeeds");

    let expected = g.expected_files();
    let readable: Vec<(String, Vec<u8>)> = expected
        .iter()
        .filter(|(_, (_, mode))| *mode == Mode::Regular || *mode == Mode::Exec)
        .map(|(p, (c, _))| (p.clone(), c.clone()))
        .collect();
    assert!(!readable.is_empty());

    // Deterministic LCG — seeded, no external rng dependency.
    let mut state: u64 = 0x5EED_2026;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 33) as usize) % readable.len()
    };
    for i in 0..200 {
        let (path, content) = &readable[next()];
        let got = fs::read(mountpoint.join(path))
            .unwrap_or_else(|e| panic!("iteration {i}, read {path}: {e}"));
        assert_eq!(&got, content, "iteration {i}, bytes of {path}");
    }

    handle.unmount().expect("unmount succeeds");
    g.server.shutdown().await;
}

/// Writes fail with EROFS through every mutation surface.
#[tokio::test]
async fn mount_writes_fail_erofs() {
    require_fuse!();
    let mut g = setup().await;
    let mountpoint = g.garden.join("mnt");
    fs::create_dir(&mountpoint).unwrap();
    let bed = WorkingCopy::open(&g.state_dir()).expect("working copy opens");
    let handle = mount(&bed, &mountpoint).await.expect("mount succeeds");

    let some_file = mount_listing(&mountpoint)
        .into_iter()
        .find(|p| mountpoint.join(p).symlink_metadata().unwrap().is_file())
        .expect("a file to write to");
    let full = mountpoint.join(&some_file);

    // write(2) on a read-only open
    let err = fs::write(&full, b"nope").expect_err("writes must fail");
    assert_eq!(err.raw_os_error(), Some(30), "EROFS, got {err}");

    // create(2) of a new file
    let err = fs::write(mountpoint.join("definitely-new"), b"nope").expect_err("creates must fail");
    assert_eq!(err.raw_os_error(), Some(30), "EROFS, got {err}");

    // mkdir(2)
    let err = fs::create_dir(mountpoint.join("definitely-dir")).expect_err("mkdir must fail");
    assert_eq!(err.raw_os_error(), Some(30), "EROFS, got {err}");

    // unlink(2)
    let err = fs::remove_file(&full).expect_err("unlink must fail");
    assert_eq!(err.raw_os_error(), Some(30), "EROFS, got {err}");

    // rmdir(2)
    let some_dir = tree_under(&mountpoint)
        .into_iter()
        .find(|p| {
            mountpoint
                .join(p)
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_dir()
        })
        .expect("a directory to remove");
    let err = fs::remove_dir(mountpoint.join(&some_dir)).expect_err("rmdir must fail");
    assert_eq!(err.raw_os_error(), Some(30), "EROFS, got {err}");

    // setattr / chmod
    let err =
        fs::set_permissions(&full, fs::Permissions::from_mode(0o600)).expect_err("chmod must fail");
    assert_eq!(err.raw_os_error(), Some(30), "EROFS, got {err}");

    handle.unmount().expect("unmount succeeds");
    g.server.shutdown().await;
}

/// Unmount: the explicit handle path AND an external `fusermount -u`
/// both end the session cleanly; the mountpoint is removable afterward.
#[tokio::test]
async fn mount_unmount_external_and_removal() {
    require_fuse!();
    let mut g = setup().await;

    // External unmount first: the handle must observe it gracefully.
    let mountpoint = g.garden.join("mnt");
    fs::create_dir(&mountpoint).unwrap();
    let bed = WorkingCopy::open(&g.state_dir()).expect("working copy opens");
    let handle = mount(&bed, &mountpoint).await.expect("mount succeeds");
    // Read something through the mount to prove it was live.
    let some_file = mount_listing(&mountpoint)
        .into_iter()
        .find(|p| mountpoint.join(p).symlink_metadata().unwrap().is_file())
        .expect("a file to read");
    let _ = fs::read(mountpoint.join(&some_file)).expect("mount was live");

    let status = std::process::Command::new("fusermount3")
        .arg("-u")
        .arg(&mountpoint)
        .status()
        .expect("fusermount3 runs");
    assert!(status.success(), "external fusermount -u succeeds");
    // The handle's own teardown observes the external unmount (its
    // fusermount call reports "not mounted" — tolerated as a clean
    // end) and joins the serve thread without hanging. Give the kernel
    // a moment to detach first so the teardown's unmount is a no-op
    // rather than a race against the external one.
    std::thread::sleep(Duration::from_millis(200));
    handle
        .unmount()
        .expect("handle unmount tolerates the external unmount");

    // Explicit unmount second.
    let mountpoint2 = g.garden.join("mnt2");
    fs::create_dir(&mountpoint2).unwrap();
    let bed = WorkingCopy::open(&g.state_dir()).expect("working copy opens");
    let handle2 = mount(&bed, &mountpoint2)
        .await
        .expect("second mount succeeds");
    let _ = fs::read_dir(&mountpoint2).expect("second mount live");
    handle2.unmount().expect("explicit unmount succeeds");

    // Both mountpoints are ordinary directories again, and removable.
    fs::remove_dir(&mountpoint).expect("mountpoint removable after external unmount");
    fs::remove_dir(&mountpoint2).expect("mountpoint removable after explicit unmount");
    g.server.shutdown().await;
}

/// `bed mount <dir>` end to end: the real binary mounts the current
/// changeset, serves reads while it stays in the foreground, and exits
/// 0 after SIGINT — leaving an ordinary, removable directory behind.
#[tokio::test]
async fn bed_mount_command_serves_reads_and_exits_on_sigint() {
    require_fuse!();
    let mut g = setup().await;
    let mountpoint = g.garden.join("cli-mnt");
    fs::create_dir(&mountpoint).unwrap();

    let mut child = common::bed_async(&g.garden, &["mount", "cli-mnt"]);
    // The command announces itself on stderr once the mount is live.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "bed mount did not come up"
        );
        let mut expected: Vec<String> = g.expected_files().keys().cloned().collect();
        expected.sort();
        if mount_listing(&mountpoint) == expected {
            break;
        }
        assert!(
            child.try_wait().expect("child is pollable").is_none(),
            "bed mount exited early"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // Every file reads with exact bytes through the CLI mount — the
    // same fidelity the library-level tests assert, now through the
    // binary's foreground serving loop.
    let expected = g.expected_files();
    for (path, (bytes, mode)) in &expected {
        let got = match mode {
            // An opaque upstream pointer: surfaces as an empty
            // read-only file, never a stored object.
            Mode::Gitlink => continue,
            // The link's target text is its content; the target path
            // itself need not exist.
            Mode::Symlink => fs::read_link(mountpoint.join(path))
                .expect("readlink through CLI mount")
                .to_string_lossy()
                .as_bytes()
                .to_vec(),
            _ => fs::read(mountpoint.join(path)).expect("read through CLI mount"),
        };
        assert_eq!(&got, bytes, "byte fidelity through bed mount: {path}");
    }
    // SIGINT ends the command cleanly: exit 0, mount gone, directory
    // removable.
    kill(child.id().expect("pid"), libc::SIGINT);
    let out = child.wait_with_output().await.expect("child reaped");
    assert_eq!(out.status.code(), Some(0), "bed mount exits 0 on SIGINT");
    assert!(
        mount_listing(&mountpoint).is_empty(),
        "mountpoint is an ordinary empty directory again"
    );
    fs::remove_dir(&mountpoint).expect("mountpoint removable after bed mount");
    g.server.shutdown().await;
}

/// Sends `signal` to `pid` (test-only sugar over `libc::kill`).
fn kill(pid: u32, signal: i32) {
    let r = unsafe { libc::kill(pid as libc::pid_t, signal) };
    assert_eq!(r, 0, "kill({pid}, {signal}) failed");
}
