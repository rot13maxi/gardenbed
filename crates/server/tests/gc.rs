//! The M13 gc suite (v0.2.md §7): mark correctness against a brute-force
//! reachability walk, collection semantics under `--min-age`, and the
//! `repo.lock` flock contract between `serve` and `gc`.
//!
//! * **V1 mark correctness**: a generated history (plus force-pushed ref
//!   moves and planted orphans) is ingested; the mark's live set must equal
//!   an independent brute-force walk (refs → parent closure → blob refs →
//!   tree closure), and gc must delete zero live objects.
//! * **V1b batch boundaries**: a synthetic history wider than the mark's
//!   write batches (>4096 live ids, one >4096-entry tree) is checked
//!   against the same oracle, so a dropped id at a `gc_live` /
//!   `gc_expanded` flush boundary cannot pass.
//! * **V2 collection semantics**: one unreferenced blob and one stale
//!   `.tmp-*` file per shard are collected exactly; young orphans survive
//!   the default min-age; a second gc deletes zero (convergence).
//! * **Locking**: `gc` (offline) fails against a running server with the
//!   actionable message and exit code 1; `--force` runs regardless.
//!
//! V3 (SIGKILL × 100) and V4 (`--force` under concurrent load) live in
//! `gc_chaos.rs`; V5 (cross-check on real imports) in `gc_repos.rs`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr as _;
use std::time::Duration;

use gardenbed_harness::{GenOptions, generate};
use gardenbed_model::{Changeset, Id, Mode, Tree, TreeEntry, TreeKind, blob_id};
use gardenbed_protocol::client::GardenbedClient;
use gardenbed_server::Repo;
use gardenbed_store::ObjectStore as _;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::timeout;
use tokio_stream::StreamExt as _;

/// Backdate an object this far below "now" so it clears any min-age gate.
const OLD_SECS: u64 = 100_000;

// ---------------------------------------------------------------- helpers

/// One running server process (fresh ephemeral port per spawn).
struct ServerProc {
    child: tokio::process::Child,
    addr: String,
}

impl ServerProc {
    async fn spawn(root: &Path) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
            .arg("--addr")
            .arg("127.0.0.1:0")
            .arg("--root")
            .arg(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("gardenbed-server binary spawns");
        let stdout = child.stdout.take().expect("server stdout piped");
        let line = timeout(Duration::from_secs(30), async {
            BufReader::new(stdout).lines().next_line().await
        })
        .await
        .expect("server announces itself within 30s")
        .expect("server stdout open")
        .expect("server prints a line");
        let addr = line
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("expected a listening line, got {line:?}"))
            .to_owned();
        ServerProc { child, addr }
    }

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// Runs `gardenbed-server gc` as a subprocess; returns (exit code, stdout,
/// stderr). The binary is the test-time build (`CARGO_BIN_EXE_`).
#[allow(dead_code)]
fn gc_cli(root: &Path, args: &[&str]) -> (i32, String, String) {
    gc_cli_env(root, args, &[])
}

/// `gc_cli` with extra `(KEY, VALUE)` environment entries.
fn gc_cli_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    use std::process::Command;
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gardenbed-server"));
    cmd.arg("gc").arg("--root").arg(root).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("gc subprocess runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Backdates a path's mtime so it clears any min-age gate used here.
fn backdate(path: &Path) {
    let old = std::time::SystemTime::now()
        .checked_sub(Duration::from_secs(OLD_SECS))
        .expect("clock is past 1970 + OLD_SECS");
    let f = std::fs::File::open(path)
        .unwrap_or_else(|e| panic!("backdate open {}: {e}", path.display()));
    f.set_modified(old)
        .unwrap_or_else(|e| panic!("backdate {}: {e}", path.display()));
    // Verify: some filesystems reject set_modified on files we cannot open.
    let got = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .expect("mtime reads back");
    assert!(
        got < std::time::SystemTime::now() - Duration::from_secs(OLD_SECS / 2),
        "backdate did not stick on {}: {got:?}",
        path.display()
    );
}

/// Plant an orphan: a well-named object file with tree-shaped junk bytes.
/// Returns the id and path.
fn plant_tree_orphan(root: &Path, seed: &str) -> (Id, PathBuf) {
    let id = blob_id(seed.as_bytes());
    let hex = id.to_hex();
    let path = root.join("objects").join(&hex[..2]).join(&hex[2..]);
    std::fs::create_dir_all(path.parent().expect("shard dir")).expect("shard created");
    std::fs::write(
        &path,
        [0x02u8]
            .iter()
            .chain(b"orphan-junk".iter())
            .copied()
            .collect::<Vec<u8>>(),
    )
    .expect("orphan written");
    (id, path)
}

/// Plant a `.tmp-*` staging file in the shard of `seed`'s hash.
fn plant_tmp(root: &Path, shard_seed: &str) -> PathBuf {
    let hex = blob_id(shard_seed.as_bytes()).to_hex();
    let dir = root.join("objects").join(&hex[..2]);
    std::fs::create_dir_all(&dir).expect("shard created");
    let path = dir.join(".tmp-424242-7");
    std::fs::write(&path, b"stale staging bytes").expect("tmp written");
    path
}

/// The on-disk object id set (all shards), excluding dotfiles.
fn disk_ids(root: &Path) -> HashSet<Id> {
    let mut out = HashSet::new();
    let objects = root.join("objects");
    for entry in std::fs::read_dir(&objects).expect("objects dir reads") {
        let shard = entry.expect("shard entry").path();
        if !shard.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&shard).expect("shard reads") {
            let file = file.expect("file entry").path();
            let name = file
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            if name.starts_with('.') {
                continue;
            }
            let shard_hex = shard.file_name().expect("shard name").to_string_lossy();
            out.insert(Id::from_str(&format!("{shard_hex}{name}")).expect("64-hex name"));
        }
    }
    out
}

/// The mark's verdict over the disk universe (the only universe the
/// sweep can act on): on-disk ids the mark proves live.
fn mark_live(root: &Path) -> HashSet<Id> {
    let mark = gardenbed_server::gc::Mark::materialize(root, true).expect("mark materializes");
    disk_ids(root)
        .into_iter()
        .filter(|id| mark.is_live(*id).expect("is_live answers"))
        .collect()
}

/// The brute-force live set over the same universe: the oracle walk's
/// verdict for exactly the on-disk ids.
fn brute_force_live_on_disk(root: &Path) -> HashSet<Id> {
    let oracle = brute_force_live(root);
    let disk = disk_ids(root);
    oracle.intersection(&disk).copied().collect()
}

/// Brute-force independent reachability walk, straight from SQLite + the
/// object store (no gc code): refs → all-parents closure → non-gitlink
/// blob refs of every stored changeset → tree closure over certificate
/// roots and derived tree rows.
fn brute_force_live(root: &Path) -> HashSet<Id> {
    let repo = Repo::open(root).expect("repo opens for the oracle");
    let conn = rusqlite::Connection::open(root.join("metadata.sqlite3")).expect("metadata opens");

    // 1. Every stored changeset row is a root (D14: rows are never
    //    collected, so their references stay live).
    let mut live: HashSet<Id> = HashSet::new();
    let mut stmt = conn
        .prepare("SELECT id, canonical FROM changesets")
        .expect("changesets select prepares");
    let mut rows = stmt.query([]).expect("changesets query runs");
    let mut changesets: Vec<Changeset> = Vec::new();
    while let Some(row) = rows.next().expect("changeset row reads") {
        let id_bytes: Vec<u8> = row.get(0).expect("id");
        let canonical: Vec<u8> = row.get(1).expect("canonical");
        let id: [u8; 32] = id_bytes.try_into().expect("32-byte id");
        live.insert(Id::from_bytes(id));
        changesets.push(Changeset::decode(&canonical).expect("stored canonical decodes"));
    }

    // 2. Ref tips (already rows, hence live; kept explicit for the shape).
    let mut stmt = conn
        .prepare("SELECT cs FROM refs")
        .expect("refs select prepares");
    let mut rows = stmt.query([]).expect("refs query runs");
    while let Some(row) = rows.next().expect("ref row reads") {
        let cs: Vec<u8> = row.get(0).expect("cs");
        let id: [u8; 32] = cs.try_into().expect("32-byte ref tip");
        live.insert(Id::from_bytes(id));
    }

    // 3. Blob refs (non-gitlink) and parents of every changeset.
    for cs in &changesets {
        for p in &cs.parents {
            live.insert(*p);
        }
        for c in &cs.changes {
            if c.mode != Mode::Gitlink
                && let Some(b) = c.blob
            {
                live.insert(b);
            }
        }
    }

    // 4. Tree closure: seeds from validated certificates and derived rows,
    //    expanded over real tree payloads. Blob ids never resolve as trees
    //    (a blob file's envelope bytes fail Tree::decode), so the walk is
    //    naturally partitioned.
    let mut stmt = conn
        .prepare(
            "SELECT root_tree FROM validated_changesets_v1 WHERE length(root_tree) = 32 \
             UNION ALL \
             SELECT bytes FROM derived WHERE kind = 'tree' AND length(bytes) = 32",
        )
        .expect("tree seeds prepare");
    let mut rows = stmt.query([]).expect("tree seeds query");
    let mut queue: Vec<Id> = Vec::new();
    while let Some(row) = rows.next().expect("seed row reads") {
        let bytes: Vec<u8> = row.get(0).expect("tree seed");
        let arr: [u8; 32] = bytes.try_into().expect("32-byte tree seed");
        queue.push(Id::from_bytes(arr));
    }
    let mut seen_trees: HashSet<Id> = HashSet::new();
    while let Some(id) = queue.pop() {
        if !seen_trees.insert(id) {
            continue;
        }
        live.insert(id);
        let Some(tree) = repo.get_tree(id).expect("tree read works") else {
            continue; // cache-miss seed: contributes no children
        };
        for entry in &tree.entries {
            match entry.kind {
                TreeKind::Dir => {
                    if let Some(child) = entry.tree {
                        queue.push(child);
                    }
                }
                TreeKind::File => {
                    if let Some(blob) = entry.blob {
                        live.insert(blob);
                    }
                }
            }
        }
    }
    live
}

/// Ingests a generated history the way the chaos suite does (inline blobs,
/// gitlink pointers skipped). Returns (repo, head).
fn ingest_history(root: &Path, seed: u64, commits: u32) -> (Repo, Id) {
    let repo = Repo::open(root).expect("repo opens");
    let history = generate(
        seed,
        &GenOptions {
            commits,
            merge_ratio: 0.15,
            ..GenOptions::default()
        },
    );
    let blobs = history.blob_contents.clone();
    let is_gitlink = |id: &Id| {
        id.as_bytes()[..12].iter().all(|b| *b == 0)
            && blobs
                .binary_search_by(|(k, _)| k.as_bytes().cmp(id.as_bytes()))
                .is_ok_and(|i| blobs[i].1 == id.as_bytes())
    };
    for cs in &history.changesets {
        let mut inline = BTreeMap::new();
        for change in &cs.changes {
            let Some(blob) = change.blob else { continue };
            if change.mode == Mode::Gitlink || is_gitlink(&blob) {
                continue;
            }
            inline.insert(
                blob.to_hex(),
                history.blob(&blob).expect("blob content").to_vec(),
            );
        }
        repo.create_changeset(cs, &inline)
            .expect("generated changeset ingests");
    }
    repo.cas_ref("main", None, history.head)
        .expect("main ref set");
    (repo, history.head)
}

// ------------------------------------------------------------------- V1

/// V1: the mark equals a brute-force walk over a merge-dense generated
/// history, with force-pushed ref moves and planted orphans mixed in; gc
/// deletes zero live objects and leaves sampled reads unchanged.
#[tokio::test]
async fn v1_mark_matches_brute_force_walk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    let (repo, head) = ingest_history(root, 0xC0FFEE, 120);
    // Force-push shape: move `main` back to an older changeset. The
    // abandoned tip stays a stored row (D14) — its blobs stay live.
    let log = repo.log(head, 0).expect("log works");
    let older = log[40].0;
    repo.cas_ref("main", Some(head), older)
        .expect("force-push ref move");
    // A branch ref on the abandoned tip too (multi-root shape).
    repo.cas_ref("rescue", None, head).expect("rescue ref set");

    // Planted orphans: objects with no metadata reference at all.
    let orphans: Vec<Id> = (0..5)
        .map(|i| plant_tree_orphan(root, &format!("orphan-{i}")).0)
        .collect();
    for (id, path) in orphans.iter().zip(0..5) {
        let _ = path;
        backdate(
            &root
                .join("objects")
                .join(&id.to_hex()[..2])
                .join(&id.to_hex()[2..]),
        );
    }

    // Sampled pre-GC reads (byte-exact answers the gc must not disturb).
    let pre_blob: Vec<(Id, Vec<u8>)> = {
        let conn = rusqlite::Connection::open(root.join("metadata.sqlite3")).unwrap();
        let mut stmt = conn.prepare("SELECT id FROM changesets LIMIT 5").unwrap();
        let ids: Vec<Vec<u8>> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        ids.iter()
            .map(|b| {
                // first non-gitlink blob of that changeset
                let canonical: Vec<u8> = conn
                    .query_row("SELECT canonical FROM changesets WHERE id = ?1", [b], |r| {
                        r.get(0)
                    })
                    .unwrap();
                let decoded = Changeset::decode(&canonical).unwrap();
                let blob = decoded
                    .changes
                    .iter()
                    .find(|c| c.mode != Mode::Gitlink && c.blob.is_some())
                    .and_then(|c| c.blob)
                    .expect("sample changeset has a blob");
                let content = repo
                    .get_blob(blob)
                    .expect("blob reads")
                    .expect("blob exists");
                (blob, content)
            })
            .collect()
    };

    // The mark vs the oracle.
    let mark = mark_live(root);
    let oracle = brute_force_live_on_disk(root);
    assert_eq!(
        mark, oracle,
        "mark live set must equal the brute-force walk"
    );
    // Orphans are absent from both.
    for id in &orphans {
        assert!(!mark.contains(id), "planted orphan {id} must not be live");
    }

    // gc (min-age 0 collects everything old): deletes exactly the orphans.
    let (code, out, err) = gc_cli(root, &["--min-age", "0"]);
    assert_eq!(code, 0, "gc exits 0; stderr: {err}");
    assert!(
        out.contains("5 tree(s) deleted"),
        "report says 5 trees: {out}"
    );

    // Zero live deleted: disk now holds exactly the oracle set.
    let after = disk_ids(root);
    assert_eq!(after, oracle, "gc deleted only the planted orphans");

    // Sampled reads byte-exact after gc.
    for (id, want) in &pre_blob {
        let got = repo
            .get_blob(*id)
            .expect("blob still reads")
            .expect("blob present");
        assert_eq!(&got, want, "sampled blob {id} byte-exact after gc");
    }
}

// ------------------------------------------------------------------ V1b

/// V1b: the mark's write-batch boundaries are exercised, and dropping the
/// id that triggers a flush is caught.
///
/// The mark batches both its `gc_live` inserts and its `gc_expanded`
/// claims at [`gardenbed_server::gc::WRITE_BATCH`]. The V1 fixture reaches
/// only ~3.5k live ids with a widest tree of ~50 entries, so neither flush
/// path executes there: mutating the buffer to drop the boundary id (the
/// id pushed when the buffer fills) left the whole suite green (gc 6/6,
/// gc_repos 1/1, gc_chaos 4/4) while `gc --dry-run` reported one live
/// object short — a reachable object marked dead.
///
/// The fixture is built directly from the metadata and object stores, and
/// that is deliberate. The mark is documented as a pure function of SQLite
/// state, so a test may construct that state; doing so buys the property
/// that makes this guard total: **every id the walk pushes has
/// multiplicity one**. In an ingested history each blob is pushed twice —
/// once by the changeset's blob refs and again by the tree entry that
/// names it — so a dropped id fails the oracle only when the boundary
/// happens to land on a tree id. That is a parity property of the fixture
/// which one extra seed push in the product, or any change to the batch
/// bound, silently destroys (measured: with a copied 4096 test constant
/// and the real bound raised to 8192 or 16384, the drop mutation stayed
/// green). Here the changeset side pushes nothing: one
/// `derived(kind='tree')` row seeds the closure, and each tree/blob is
/// named exactly once, so a dropped id is always observable.
///
/// * The root tree is `DIRS = 2 * WRITE_BATCH + 1` Dir entries wide —
///   sized from the real constant, never a copied literal — so the claim
///   buffer fills and flushes *inside* a single tree's entry loop,
///   `2 * WRITE_BATCH + 1` claims against a `WRITE_BATCH` bound. At the
///   current bound that is 8193 entries.
/// * The live buffer sees `2 * DIRS + 1` pushes (the root tree id, then
///   each child's blob followed by its own tree id), so it flushes
///   repeatedly: `4 * WRITE_BATCH + 3` ids at any bound. At the current
///   bound that is 16387 on-disk ids.
///
/// The assertion is the V1 oracle (mark == independent brute-force walk)
/// plus the gc report's live count, so a lost boundary id — which marks a
/// reachable object dead and would let the sweep unlink it — fails here.
#[tokio::test]
async fn v1b_batch_boundaries_mark_exactly() {
    // Sized from the real bound, never a copy of it.
    const DIRS: usize = 2 * gardenbed_server::gc::WRITE_BATCH + 1;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    // Opens the repository: schema tables, object store, fresh metadata.
    let repo = Repo::open(root).expect("repo opens");
    let objects = repo.objects_handle();

    // One unique blob per directory, each named by exactly one tree entry.
    let mut blobs = Vec::with_capacity(DIRS);
    for d in 0..DIRS {
        let content = format!("boundary blob {d}").into_bytes();
        blobs.push(objects.put_blob(&content).expect("blob stored"));
    }
    // Child trees: one file entry each.
    let mut children = Vec::with_capacity(DIRS);
    for blob in &blobs {
        let tree = Tree {
            entries: vec![TreeEntry {
                name: "f".into(),
                kind: TreeKind::File,
                mode: Mode::Regular,
                blob: Some(*blob),
                tree: None,
            }],
        };
        let bytes = tree.canonical_bytes().expect("child tree encodes");
        let id = tree.id().expect("child tree id");
        objects.put_tree(id, &bytes).expect("child tree stored");
        children.push(id);
    }
    // Root tree: one Dir entry per child (the codec sorts canonically).
    let root_tree = Tree {
        entries: children
            .iter()
            .enumerate()
            .map(|(d, child)| TreeEntry {
                name: format!("d{d:04}"),
                kind: TreeKind::Dir,
                mode: Mode::Regular,
                blob: None,
                tree: Some(*child),
            })
            .collect(),
    };
    let root_bytes = root_tree.canonical_bytes().expect("root tree encodes");
    let root_id = root_tree.id().expect("root tree id");
    objects
        .put_tree(root_id, &root_bytes)
        .expect("root tree stored");

    // The mark's seed: one `derived(kind='tree')` row. The `cs` key is
    // opaque to the mark (it reads only `bytes`); a synthetic key keeps
    // the row from posing as any changeset's cache entry.
    {
        let conn =
            rusqlite::Connection::open(root.join("metadata.sqlite3")).expect("metadata opens");
        conn.execute(
            "INSERT INTO derived(kind, cs, bytes) VALUES('tree', ?1, ?2)",
            rusqlite::params![blob_id(b"v1b seed key").as_bytes(), root_id.as_bytes()],
        )
        .expect("derived seed row");
    }

    // Fixture shape guard: both flush conditions must be reachable. If a
    // future edit shrinks this fixture, the test must say so rather than
    // silently stop covering the boundaries. (DIRS derives from the real
    // bound, so these check the shape that derivation produces.)
    let top = objects
        .get_tree(root_id)
        .expect("root tree reads")
        .expect("tree exists");
    let top = Tree::decode(&top).expect("root tree decodes");
    assert_eq!(
        top.entries.len(),
        DIRS,
        "the root tree must hold one Dir entry per fixture directory"
    );
    assert!(
        top.entries.len() > 2 * gardenbed_server::gc::WRITE_BATCH,
        "fixture must be wider than two claim batches: {} entries",
        top.entries.len()
    );
    let on_disk = disk_ids(root);
    assert_eq!(
        on_disk.len(),
        2 * DIRS + 1,
        "one root tree + one tree and one blob per directory"
    );

    // The oracle: mark == brute-force reachability over the on-disk set.
    let mark = mark_live(root);
    let oracle = brute_force_live_on_disk(root);
    assert_eq!(
        mark, oracle,
        "mark live set must equal the brute-force walk"
    );

    // And the sweep agrees: every on-disk object is live, so a min-age-0
    // dry run reports all of them live and deletes nothing. A dropped
    // boundary id shows up here as a live count one short plus a
    // (would-be) deletion.
    let (code, out, err) = gc_cli(root, &["--dry-run", "--min-age", "0"]);
    assert_eq!(code, 0, "dry-run exits 0; stderr: {err}");
    assert!(
        out.contains(&format!("{} live", on_disk.len())),
        "every on-disk object is live ({} expected): {out}",
        on_disk.len()
    );
    assert!(
        out.contains("0 blob(s) and 0 tree(s) deleted"),
        "no live object may be a deletion candidate: {out}"
    );
}

// ------------------------------------------------------------------- V2

/// V2: one unreferenced blob + one stale `.tmp-*` per shard are collected
/// exactly; young orphans survive the default min-age; a second gc deletes
/// zero (convergence).
#[tokio::test]
async fn v2_collection_semantics() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let (_repo, _head) = ingest_history(root, 0xBEEF, 40);

    // Old orphan blob: a real GBLO1 envelope for content no one references.
    let orphan_content = b"unreferenced blob content".to_vec();
    let orphan_id = {
        let repo = Repo::open(root).expect("repo opens");
        repo.put_blob(&orphan_content).expect("orphan uploads")
    };
    backdate(
        &root
            .join("objects")
            .join(&orphan_id.to_hex()[..2])
            .join(&orphan_id.to_hex()[2..]),
    );

    // Old tmp file in a different shard.
    let tmp = plant_tmp(root, "tmp-shard-seed");
    backdate(&tmp);

    // Young orphan (fresh mtime): must survive the default min-age.
    let young = plant_tree_orphan(root, "young-orphan").0;
    let young_path = root
        .join("objects")
        .join(&young.to_hex()[..2])
        .join(&young.to_hex()[2..]);

    let pre = disk_ids(root);
    let (code, out, err) = gc_cli(root, &[]); // default 24h min-age
    assert_eq!(code, 0, "gc exits 0; stderr: {err}");
    assert!(
        out.contains("1 blob(s) and 0 tree(s) deleted"),
        "default-age gc collects only the old orphan blob: {out}"
    );
    assert!(out.contains("1 tmp file(s) reaped"), "tmp reaped: {out}");
    let post = disk_ids(root);
    assert_eq!(post.len(), pre.len() - 1, "exactly the old orphan went");
    assert!(!post.contains(&orphan_id), "old orphan blob deleted");
    assert!(
        post.contains(&young),
        "young orphan survived default min-age"
    );
    assert!(!tmp.exists(), "stale tmp reaped");

    // Second gc with min-age 0 (no grace): the young orphan goes now.
    let (code, out, err) = gc_cli(root, &["--min-age", "0"]);
    assert_eq!(code, 0, "second gc exits 0; stderr: {err}");
    assert!(
        out.contains("0 blob(s) and 1 tree(s) deleted"),
        "second gc collects the young tree orphan: {out}"
    );
    let settled = disk_ids(root);
    assert!(
        !settled.contains(&young),
        "min-age 0 collects the young orphan"
    );
    assert!(!young_path.exists(), "young orphan file unlinked");

    // Third gc over a settled store: zero, zero (convergence).
    let (code, out, err) = gc_cli(root, &["--min-age", "0"]);
    assert_eq!(code, 0, "third gc exits 0; stderr: {err}");
    assert!(
        out.contains("0 blob(s) and 0 tree(s) deleted"),
        "third gc deletes zero: {out}"
    );
}

// ------------------------------------------------------------ locking

/// `gc` without `--force` fails against a running server (exit 1, the
/// actionable message); `--force --dry-run` runs fine alongside it.
#[tokio::test]
async fn offline_gc_fails_against_running_server() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let (_repo, _head) = ingest_history(root, 0x10C0, 10);

    let mut server = ServerProc::spawn(root).await;
    let (code, _out, err) = gc_cli(root, &[]);
    assert_eq!(code, 1, "contended gc exits 1; stderr: {err}");
    assert!(
        err.contains("repository is in use") && err.contains("--force"),
        "error names the lock and the escape hatch: {err}"
    );
    let (code, out, err) = gc_cli(root, &["--force", "--dry-run"]);
    assert_eq!(code, 0, "force dry-run works alongside the server: {err}");
    assert!(out.starts_with("gc:"), "report printed: {out}");

    server.kill().await;
    drop(server);
    // The lock dies with the process; offline gc works again.
    let (code, out, err) = gc_cli(root, &[]);
    assert_eq!(code, 0, "gc after server shutdown exits 0; stderr: {err}");
    assert!(out.starts_with("gc:"), "report printed: {out}");
}

/// The dry-run never mutates: same disk before and after.
#[tokio::test]
async fn dry_run_never_mutates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let (_repo, _head) = ingest_history(root, 0xD0D0, 30);
    let (oid, opath) = plant_tree_orphan(root, "dry-run-orphan");
    backdate(&opath);
    let tmp = plant_tmp(root, "dry-run-tmp");
    backdate(&tmp);

    let before = disk_ids(root);
    let (code, out, err) = gc_cli(root, &["--dry-run", "--min-age", "0"]);
    assert_eq!(code, 0, "dry-run exits 0; stderr: {err}");
    assert!(
        out.contains("1 tree(s) deleted") && out.contains("1 tmp file(s) reaped"),
        "dry-run reports what it would do: {out}"
    );
    assert_eq!(disk_ids(root), before, "dry-run deleted nothing");
    assert!(tmp.exists(), "dry-run reaped nothing");
    assert!(opath.exists(), "orphan still present");
    let _ = oid;
}

/// A client against a server that holds the shared lock keeps working
/// (the shared hold is advisory, held for the server's lifetime only).
#[tokio::test]
async fn server_serves_while_holding_shared_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let (_repo, head) = ingest_history(root, 0x5EED, 10);

    let mut server = ServerProc::spawn(root).await;
    let client = GardenbedClient::connect(&format!("http://{}", server.addr))
        .await
        .expect("client connects");
    let mut stream = client
        .log("default", head, 10)
        .await
        .expect("log RPC works");
    let first = stream.next().await.expect("log yields entries");
    assert!(first.is_ok(), "log entry decodes: {:?}", first.err());
    server.kill().await;
}

// ---------------------------------------------------------------- P2-1

/// An object store that deletes one designated tree object immediately
/// after a chosen `put_tree` fires — a gc unlink landing in the
/// derivation→commit window, with none of the race's timing luck. The
/// deleted id is an INTERMEDIATE tree (not the root), which is exactly
/// the shape a root-only verify would miss (round-2 review, P2-1).
struct FaultyStore {
    inner: gardenbed_store::FsObjectStore,
    /// Delete this id right after this put_tree id is written.
    after_writing: Id,
    delete: Id,
}

impl gardenbed_store::ObjectStore for FaultyStore {
    fn put_blob(&self, content: &[u8]) -> Result<Id, gardenbed_store::StoreError> {
        self.inner.put_blob(content)
    }
    fn get_blob(&self, id: Id) -> Result<Option<Vec<u8>>, gardenbed_store::StoreError> {
        self.inner.get_blob(id)
    }
    fn put_tree(&self, id: Id, canonical_bytes: &[u8]) -> Result<(), gardenbed_store::StoreError> {
        self.inner.put_tree(id, canonical_bytes)?;
        if id == self.after_writing {
            // A concurrent gc's unlink: plain std::fs (the sweep's own
            // deletion is a plain unlink too).
            let hex = self.delete.to_hex();
            std::fs::remove_file(self.inner.objects_dir().join(&hex[..2]).join(&hex[2..]))
                .expect("fault injection deletes the intermediate tree");
        }
        Ok(())
    }
    fn get_tree(&self, id: Id) -> Result<Option<Vec<u8>>, gardenbed_store::StoreError> {
        self.inner.get_tree(id)
    }
    fn has(&self, id: Id) -> Result<bool, gardenbed_store::StoreError> {
        self.inner.has(id)
    }
}

/// P2-1 deterministically: a commit whose derivation re-links an
/// existing subtree collection must refuse when a concurrent collection
/// removed an INTERMEDIATE tree between the derivation and the commit
/// row. The root itself is present, so a root-only check passes — the
/// closure walk is what catches it. The commit fails naming the tree
/// and no row lands.
#[tokio::test]
async fn p2_1_missing_intermediate_tree_fails_the_commit() {
    use gardenbed_model::{Change, Identity, Op};
    use gardenbed_store::MetadataStore;

    let dir = tempfile::tempdir().expect("tempdir");
    let objects = std::sync::Arc::new(gardenbed_store::FsObjectStore::open(dir.path()).unwrap());
    let metadata = std::sync::Arc::new(
        gardenbed_store::SqliteMetadataStore::open(&dir.path().join("meta.sqlite3")).unwrap(),
    );

    let identity = || Identity {
        name: "p2".into(),
        email: "p2@bed.example".into(),
        time_secs: 1_800_000_000,
        tz_min: 0,
    };
    let make_cs = |parents: Vec<Id>, changes: Vec<Change>, message: &str| Changeset {
        parents,
        changes,
        message: message.into(),
        author: identity(),
        committer: identity(),
        signature: None,
    };

    // Parent: a committed history with nesting.
    let repo = Repo::new(objects.clone(), metadata.clone());
    let mut parent_changes: Vec<Change> = (0..6u32)
        .map(|i| Change {
            path: format!("p/dir{}/sub{}/f-{:02}.txt", i % 3, i % 2, i),
            op: Op::Add,
            blob: Some(blob_id(format!("p blob {i}\n").as_bytes())),
            src_path: None,
            mode: Mode::Regular,
        })
        .collect();
    parent_changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let mut parent_inline = BTreeMap::new();
    for i in 0..6u32 {
        let content = format!("p blob {i}\n").into_bytes();
        parent_inline.insert(blob_id(&content).to_hex(), content);
    }
    let parent = repo
        .create_changeset(&make_cs(vec![], parent_changes, "parent"), &parent_inline)
        .expect("parent commits");

    // The replayed changeset: adds deeper nesting, so its derivation
    // re-links the parent's subtree ids AND writes new intermediates.
    let mut changes: Vec<Change> = (0..12u32)
        .map(|i| Change {
            path: format!("p/dir{}/sub{}/g-{:02}.txt", i % 3, i % 2, i),
            op: Op::Add,
            blob: Some(blob_id(format!("g blob {i}\n").as_bytes())),
            src_path: None,
            mode: Mode::Regular,
        })
        .collect();
    changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let mut inline = BTreeMap::new();
    for i in 0..12u32 {
        let content = format!("g blob {i}\n").into_bytes();
        inline.insert(blob_id(&content).to_hex(), content);
    }
    let cs = make_cs(vec![parent], changes, "replay");

    // First derivation: materialize the trees and enumerate the closure.
    // Blobs first — validated_tree_for_new requires them present.
    for (_, content) in inline.iter() {
        repo.put_blob(content).expect("blob pre-put");
    }
    let engine = repo.engine_handle();
    let root = engine
        .validated_tree_for_new(&cs)
        .expect("first derivation works");
    let root = root.expect("non-empty root");
    // Walk to find an intermediate tree (a dir entry's id, not the root).
    let mut intermediate: Option<Id> = None;
    {
        let mut queue = vec![root];
        while let Some(id) = queue.pop()
            && intermediate.is_none()
        {
            let bytes = objects.get_tree(id).unwrap().expect("tree present");
            let tree = gardenbed_model::Tree::decode(&bytes).expect("tree decodes");
            for entry in &tree.entries {
                if let Some(child) = entry.tree {
                    intermediate = Some(child);
                }
            }
        }
    }
    let intermediate = intermediate.expect("the closure has intermediate trees");
    assert_ne!(intermediate, root, "the chosen id is not the root");

    // The faulty store: when the derivation re-writes the root (the last
    // put_tree of finish()), delete the intermediate — a gc sweep landing
    // between derivation and commit. Same metadata, so the parent row
    // stays visible.
    let faulty = std::sync::Arc::new(FaultyStore {
        inner: gardenbed_store::FsObjectStore::open(dir.path()).unwrap(),
        after_writing: root,
        delete: intermediate,
    });
    let repo2 = Repo::new(faulty, metadata.clone());

    let result = repo2.create_changeset(&cs, &inline);
    let text = match result {
        Ok(id) => panic!("commit must refuse: row {id} landed without its closure"),
        Err(e) => e.to_string(),
    };
    assert!(
        text.contains("was collected"),
        "error names the collected tree: {text}"
    );
    assert!(
        text.contains(&intermediate.to_hex()),
        "error names the intermediate {intermediate}: {text}"
    );
    // No row landed.
    let cs_id = cs.id().unwrap();
    assert!(
        metadata.get_changeset(cs_id).unwrap().is_none(),
        "the failed commit left no row"
    );
}
