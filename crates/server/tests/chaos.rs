//! The M8 chaos suite (v0.1.md §M8.2): for every RPC class, start a client
//! call, SIGKILL the server mid-call, restart, and prove the repository
//! came through intact:
//!
//! 1. **Acknowledged results read back byte-exact.** Every call that
//!    returned before the kill is re-fetched from the restarted server and
//!    must match exactly; refs touched by an interrupted write must sit at
//!    one of the two legal values (old or new) — a torn ref is impossible
//!    by construction (one-row CAS in one immediate transaction).
//! 2. **Loam checksum sweep.** Every object file under `<root>/loam/` is
//!    walked and verified against its content address: blob envelopes are
//!    zstd-decoded and SHA-256-checked (`GBLO1` envelope, canonical
//!    encoding §6); trees are decoded from canonical bytes and hashed.
//! 3. **`PRAGMA integrity_check` ok** on the seedbed after WAL recovery.
//! 4. **Derived-data rebuild determinism.** All derived kinds (`tree`,
//!    `skiplist`, `pathsummary`) are wiped, recomputed from the changeset
//!    log through a fresh in-process engine, and the answers (root trees of
//!    a sample, blame ranges, diff, merge-base, ancestry) must be identical
//!    to the pre-chaos baseline.
//!
//! Classes: GetTree, GetBlob, CreateChangeset, PutBlob, Log, Diff, Blame,
//! CasRef, Graft, Harvest. Default 10 kill/restart cycles per class (100
//! total); `GARDENBED_CHAOS_ITERS` raises it (the M8 gate runs 100/class
//! once per release). The default run stays within a few minutes.
//!
//! Durability premise: this suite exercises the *durable* store, so
//! `GARDENBED_FSYNC` is removed from the environment before any store is
//! opened (the demo/bench bulk-import toggle must never leak in).

use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr as _;

use gardenbed_derive::BlameRange;
use gardenbed_harness::{GenOptions, generate};
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_protocol::client::{CasOutcome, GardenbedClient};
use gardenbed_protocol::convert::id_from_bytes;
use gardenbed_server::Repo;
use sha2::Digest;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::{Duration, sleep, timeout};

const REPO: &str = "garden";
/// Blame oracle path: modified by every changeset of the appended chain.
const BLAME_PATH: &str = "log/changes.txt";
/// The streamed big blob's size (GetBlob/PutBlob ride 64 KiB chunks).
const BIG_BLOB_LEN: usize = 4 << 20;
/// PutBlob body size (multi-chunk, killed mid-stream).
const PUT_BLOB_LEN: usize = 8 << 20;
/// Harvest-chain depth (≥ default iterations; cycles wrap).
const HCHAIN_LEN: usize = 12;

// ---------------------------------------------------------------- server

/// One running arboretum process (fresh ephemeral port per spawn).
struct ServerProc {
    child: tokio::process::Child,
    addr: String,
}

impl ServerProc {
    /// Spawns the built `gardenbed-server` over `root` and waits for its
    /// `listening on <addr>` stdout line.
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

    fn http(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn client(&self) -> GardenbedClient {
        GardenbedClient::connect(&self.http())
            .await
            .expect("client connects to the arboretum")
    }

    /// SIGKILL (the chaos primitive) and reap.
    async fn kill(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// The kill delay for iteration `i` (ms): a sweep from "before the call is
/// sent" to "well into the response stream", so every phase of every RPC
/// class gets killed at least once — and the long delays let short calls
/// finish, exercising the acknowledged-then-killed path too.
fn kill_delay(i: usize) -> Duration {
    Duration::from_millis([0, 1, 2, 4, 8, 16, 32, 128][i % 8])
}

// ------------------------------------------------------------- fixture

/// The one-line ingest used by every fixture (mirrors the harness's
/// contract: gitlink pointers are opaque, everything else rides inline).
fn is_gitlink_pointer(id: &Id, blobs: &[(Id, Vec<u8>)]) -> bool {
    id.as_bytes()[..12].iter().all(|b| *b == 0)
        && blobs
            .binary_search_by(|(k, _)| k.as_bytes().cmp(id.as_bytes()))
            .is_ok_and(|i| blobs[i].1 == id.as_bytes())
}

/// The whole fixture: an ingested history plus the extra shapes the chaos
/// classes need, all in one lineage rooted at the generated history.
struct Fixture {
    root: PathBuf,
    /// `main` before the chaos loop (also every mutator's fixed parent).
    main: Id,
    /// An older changeset on the mainline (the diff/graft anchor).
    cs40: Id,
    tree: Option<Id>,
    /// The root tree of `main` as served before any chaos (the GetTree
    /// baseline, kept as a model value so verification never reopens the
    /// store the suite is trying to incriminate).
    tree_model: Option<gardenbed_model::Tree>,
    log_ids: Vec<Id>,
    diff: Vec<Change>,
    blame: Vec<BlameRange>,
    big_blob: Id,
    big_content: Vec<u8>,
    graftable: Vec<(Id, Changeset)>,
    hchain: Vec<Id>,
    /// Root-tree answers per sampled changeset (rebuild determinism).
    /// `Id` hashes by content; no ordering is needed.
    sample_trees: HashMap<Id, Option<Id>>,
}

/// Builds the fixture in-process (fsync on — the durability premise).
fn build_fixture(root: &Path) -> Fixture {
    let repo = Repo::open(root).expect("fixture repo opens");

    // A merge-dense generated history (the M-fixture shape), then three
    // deterministic extensions appended on its first-parent chain:
    // 60 walk changesets (blame oracle), one 4 MiB blob, graft/harvest
    // material. Identity times increase so log ordering is stable.
    let history = generate(
        7,
        &GenOptions {
            commits: 150,
            max_files: 240,
            merge_ratio: 0.08,
            crlf_content: true,
            empty_files: true,
            ..GenOptions::default()
        },
    );
    let blobs = history.blob_contents.clone();
    for cs in &history.changesets {
        let mut inline = BTreeMap::new();
        for change in &cs.changes {
            let Some(blob) = change.blob else { continue };
            if change.mode == Mode::Gitlink || is_gitlink_pointer(&blob, &blobs) {
                continue; // opaque upstream pointer, never a loam object
            }
            let content = history
                .blob(&blob)
                .expect("generated history carries every blob it references")
                .to_vec();
            debug_assert_eq!(blob_id(&content), blob);
            inline.insert(blob.to_hex(), content);
        }
        repo.create_changeset(cs, &inline)
            .expect("generated changeset ingests cleanly");
    }
    let cs40 = history.changesets[40]
        .id()
        .expect("generated changeset ids always encode");
    let base_head = history.head;

    let ident = |n: u64| Identity {
        name: "Chaos".into(),
        email: "chaos@bed.example".into(),
        time_secs: 1_800_000_000 + n,
        tz_min: 0,
    };
    let mut parent = base_head;

    // 60 walk changesets: `log/changes.txt` rewritten with one appended
    // line per commit (first-parent blame ⇒ ~60 ranges), plus one additive
    // file each.
    let mut story = String::new();
    for i in 0..60u64 {
        story.push_str(&format!("chaos walk line {i}: the plot thickens\n"));
        let mut changes = Vec::new();
        let mut inline = BTreeMap::new();
        let blob = blob_id(story.as_bytes());
        changes.push(Change {
            path: BLAME_PATH.into(),
            op: Op::Modify,
            blob: Some(blob),
            src_path: None,
            mode: Mode::Regular,
        });
        inline.insert(blob.to_hex(), story.clone().into_bytes());
        let walk = format!("walk/step-{i:03}.txt");
        let content = format!("walk step {i}\n");
        let blob = blob_id(content.as_bytes());
        changes.push(Change {
            path: walk,
            op: Op::Add,
            blob: Some(blob),
            src_path: None,
            mode: Mode::Regular,
        });
        inline.insert(blob.to_hex(), content.into_bytes());
        let cs = Changeset {
            parents: vec![parent],
            changes,
            message: format!("chaos walk {i}"),
            author: ident(i),
            committer: ident(i),
            signature: None,
        };
        parent = repo
            .create_changeset(&cs, &inline)
            .expect("walk cs ingests");
    }

    // The 4 MiB blob the streaming classes stream. It exceeds the inline
    // cap on CreateChangeset (1 MiB, v0.1.md §5), so it takes the real
    // write path a heavy client would use: PutBlob first, then a
    // changeset referencing it with nothing inline.
    let big_content: Vec<u8> = (0..BIG_BLOB_LEN).map(|i| (i % 251) as u8).collect();
    let blob = repo.put_blob(&big_content).expect("big blob uploads");
    debug_assert_eq!(blob, blob_id(&big_content), "put_blob is content-addressed");
    let big_cs = Changeset {
        parents: vec![parent],
        changes: vec![Change {
            path: "blobs/huge.bin".into(),
            op: Op::Add,
            blob: Some(blob),
            src_path: None,
            mode: Mode::Regular,
        }],
        message: "chaos big blob".into(),
        author: ident(60),
        committer: ident(60),
        signature: None,
    };
    parent = repo
        .create_changeset(&big_cs, &BTreeMap::new())
        .expect("big-blob cs ingests");
    let main = parent;

    // Three additive changesets graftable onto any parent (new paths only).
    let mut graftable = Vec::new();
    for n in 0..3u64 {
        let mut changes = Vec::new();
        let mut inline = BTreeMap::new();
        for k in 0..30u64 {
            let content = format!("graft payload {n}/{k}\n");
            let blob = blob_id(content.as_bytes());
            changes.push(Change {
                path: format!("graft/{n}/f{k:02}.txt"),
                op: Op::Add,
                blob: Some(blob),
                src_path: None,
                mode: Mode::Regular,
            });
            inline.insert(blob.to_hex(), content.into_bytes());
        }
        let cs = Changeset {
            parents: vec![main],
            changes,
            message: format!("graftable {n}"),
            author: ident(70 + n),
            committer: ident(70 + n),
            signature: None,
        };
        let id = repo
            .create_changeset(&cs, &inline)
            .expect("graftable ingests");
        graftable.push((id, cs));
    }

    // The harvest chain: hchain[0] is a child of main, each next a child of
    // the previous — harvesting them in order is always a fast-forward.
    let mut hchain = Vec::new();
    let mut hp = main;
    for k in 0..HCHAIN_LEN {
        let content = format!("harvest step {k}\n");
        let blob = blob_id(content.as_bytes());
        let cs = Changeset {
            parents: vec![hp],
            changes: vec![Change {
                path: format!("harvest/step-{k:02}.txt"),
                op: Op::Add,
                blob: Some(blob),
                src_path: None,
                mode: Mode::Regular,
            }],
            message: format!("harvestable {k}"),
            author: ident(80 + k as u64),
            committer: ident(80 + k as u64),
            signature: None,
        };
        let mut inline = BTreeMap::new();
        inline.insert(blob.to_hex(), content.into_bytes());
        let id = repo.create_changeset(&cs, &inline).expect("hchain ingests");
        hchain.push(id);
        hp = id;
    }

    repo.cas_ref("main", None, main).expect("main ref created");

    // Baseline answers, computed before any chaos (the truth every
    // restarted server must reproduce).
    let mut sample_trees = HashMap::new();
    for cs in [cs40, base_head, main] {
        sample_trees.insert(cs, repo.root_tree(cs).expect("root_tree works"));
    }
    let tree = sample_trees[&main];
    let tree_model = tree.map(|id| {
        repo.get_tree(id)
            .expect("baseline tree fetches")
            .expect("root tree exists")
    });
    let log = repo.log(main, 0).expect("log works");
    let log_ids: Vec<Id> = log.iter().map(|(id, _)| *id).collect();
    let diff = repo.diff(cs40, main).expect("diff works");
    let blame = repo.blame(main, BLAME_PATH).expect("blame works");
    assert!(
        blame.len() >= 50,
        "the walk chain must give real blame work"
    );

    Fixture {
        root: root.to_path_buf(),
        main,
        cs40,
        tree,
        tree_model,
        log_ids,
        diff,
        blame,
        big_blob: blob,
        big_content,
        graftable,
        hchain,
        sample_trees,
    }
}

// -------------------------------------------------------------- outcomes

/// What one chaos call produced before the kill.
enum Outcome {
    /// A completed read/derive RPC whose result must read back identically.
    Read { name: &'static str },
    /// PutBlob acknowledged: `(blob id, content)`.
    PutBlob(Id, Vec<u8>),
    /// CreateChangeset acknowledged: `(id, the changeset as sent)`.
    Created(Id, Changeset),
    /// CasRef answered: `(ref value before, new value, outcome)`.
    Cas(Option<Id>, Id, Option<CasOutcome>),
    /// Graft acknowledged: `(grafted id, new parent, changes)`.
    Grafted(Id, Id, Vec<Change>),
    /// Harvest acknowledged or rejected: the target (and main before).
    Harvested(Id, Option<Id>),
    /// The call failed cleanly (transport error) — nothing acknowledged.
    Failed(&'static str),
}

/// The RPC classes under chaos, in the M8 list's order.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Class {
    GetTree,
    GetBlob,
    CreateChangeset,
    PutBlob,
    Log,
    Diff,
    Blame,
    CasRef,
    Graft,
    Harvest,
}

const CLASSES: [Class; 10] = [
    Class::GetTree,
    Class::GetBlob,
    Class::CreateChangeset,
    Class::PutBlob,
    Class::Log,
    Class::Diff,
    Class::Blame,
    Class::CasRef,
    Class::Graft,
    Class::Harvest,
];

impl Class {
    fn label(self) -> &'static str {
        match self {
            Class::GetTree => "GetTree",
            Class::GetBlob => "GetBlob",
            Class::CreateChangeset => "CreateChangeset",
            Class::PutBlob => "PutBlob",
            Class::Log => "Log",
            Class::Diff => "Diff",
            Class::Blame => "Blame",
            Class::CasRef => "CasRef",
            Class::Graft => "Graft",
            Class::Harvest => "Harvest",
        }
    }
}

/// Everything one spawned call needs, owned (so `drive` is `Spawn`-able
/// and the kill can land anywhere in the call's lifetime).
struct CallCtx {
    client: GardenbedClient,
    main: Id,
    cs40: Id,
    tree: Option<Id>,
    big_blob: Id,
    log_ids: Vec<Id>,
    diff: Vec<Change>,
    blame: Vec<BlameRange>,
    graftable: (Id, Changeset),
    harvest_target: Id,
}

/// Starts one call of `class` (iteration `i`) against the ctx's client.
///
/// Runs on its own task so the test can SIGKILL the server mid-call; the
/// transport error the kill produces resolves every await below. A result
/// that arrives *before* the kill is asserted against the fixture baseline
/// right here (an acknowledged wrong answer is a bug, kill or no kill).
async fn drive(ctx: CallCtx, class: Class, i: usize) -> Outcome {
    let client = &ctx.client;
    match class {
        Class::GetTree => {
            let Some(tree_id) = ctx.tree else {
                return Outcome::Failed("GetTree (empty fixture tree)");
            };
            match client.get_tree(REPO, tree_id).await {
                Ok(_) => Outcome::Read { name: "GetTree" },
                Err(_) => Outcome::Failed("GetTree"),
            }
        }
        Class::GetBlob => match client.get_blob(REPO, ctx.big_blob).await {
            Ok(mut stream) => {
                let mut content = Vec::with_capacity(BIG_BLOB_LEN);
                let mut ok = true;
                loop {
                    match stream.message().await {
                        Ok(Some(chunk)) => content.extend_from_slice(&chunk.data),
                        Ok(None) => break,
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok && content.len() == BIG_BLOB_LEN {
                    assert_eq!(
                        blob_id(&content),
                        ctx.big_blob,
                        "GetBlob acknowledged a wrong answer pre-kill"
                    );
                    Outcome::Read { name: "GetBlob" }
                } else {
                    Outcome::Failed("GetBlob (partial stream)")
                }
            }
            Err(_) => Outcome::Failed("GetBlob"),
        },
        Class::CreateChangeset => {
            let (cs, inline) = chaos_changeset(&ctx.main, i, 40);
            match client.create_changeset(REPO, &cs, inline).await {
                Ok(id) => Outcome::Created(id, cs),
                Err(_) => Outcome::Failed("CreateChangeset"),
            }
        }
        Class::PutBlob => {
            let content: Vec<u8> = (0..PUT_BLOB_LEN).map(|i| (i % 249) as u8).collect();
            let chunks: Vec<Vec<u8>> = content.chunks(64 << 10).map(<[u8]>::to_vec).collect();
            match client.put_blob(REPO, tokio_stream::iter(chunks)).await {
                Ok(id) => Outcome::PutBlob(id, content),
                Err(_) => Outcome::Failed("PutBlob"),
            }
        }
        Class::Log => match client.log(REPO, ctx.main, 0).await {
            Ok(mut stream) => {
                let mut ids = Vec::new();
                let mut ok = true;
                loop {
                    match stream.message().await {
                        Ok(Some(s)) => ids.push(id_from_bytes(&s.cs_id).expect("wire id")),
                        Ok(None) => break,
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    assert_eq!(ids, ctx.log_ids, "Log acknowledged a wrong answer pre-kill");
                    Outcome::Read { name: "Log" }
                } else {
                    Outcome::Failed("Log (partial stream)")
                }
            }
            Err(_) => Outcome::Failed("Log"),
        },
        Class::Diff => match client.diff(REPO, ctx.cs40, ctx.main).await {
            Ok(mut stream) => {
                let mut changes = Vec::new();
                let mut ok = true;
                loop {
                    match stream.message().await {
                        Ok(Some(m)) => match Change::try_from(m) {
                            Ok(c) => changes.push(c),
                            Err(_) => {
                                ok = false;
                                break;
                            }
                        },
                        Ok(None) => break,
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    assert_eq!(
                        changes, ctx.diff,
                        "Diff acknowledged a wrong answer pre-kill"
                    );
                    Outcome::Read { name: "Diff" }
                } else {
                    Outcome::Failed("Diff (partial stream)")
                }
            }
            Err(_) => Outcome::Failed("Diff"),
        },
        Class::Blame => match client.blame(REPO, ctx.main, BLAME_PATH).await {
            Ok(mut stream) => {
                let mut ranges = Vec::new();
                let mut ok = true;
                loop {
                    match stream.message().await {
                        Ok(Some(r)) => {
                            ranges.push((r.lo, r.hi, id_from_bytes(&r.cs_id).expect("wire id")))
                        }
                        Ok(None) => break,
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    assert_eq!(
                        ranges, ctx.blame,
                        "Blame acknowledged a wrong answer pre-kill"
                    );
                    Outcome::Read { name: "Blame" }
                } else {
                    Outcome::Failed("Blame (partial stream)")
                }
            }
            Err(_) => Outcome::Failed("Blame"),
        },
        Class::CasRef => {
            let before = match client.get_ref(REPO, "chaos-rpc").await {
                Ok(value) => value,
                Err(_) => return Outcome::Failed("CasRef (pre-read)"),
            };
            // Alternate a winning CAS with a provably losing one (expected
            // = an unrelated changeset) so both outcomes occur.
            let new = ctx.main;
            let expected = if i.is_multiple_of(2) {
                before
            } else {
                Some(ctx.cs40)
            };
            match client.cas_ref(REPO, "chaos-rpc", expected, new).await {
                Ok(outcome) => Outcome::Cas(before, new, Some(outcome)),
                Err(_) => Outcome::Cas(before, new, None),
            }
        }
        Class::Graft => {
            let (gid, cs) = &ctx.graftable;
            match client.graft(REPO, *gid, ctx.cs40).await {
                Ok(id) => Outcome::Grafted(id, ctx.cs40, cs.changes.clone()),
                Err(_) => Outcome::Failed("Graft"),
            }
        }
        Class::Harvest => {
            let before = match client.get_ref(REPO, "main").await {
                Ok(value) => value,
                Err(_) => return Outcome::Failed("Harvest (pre-read)"),
            };
            let target = ctx.harvest_target;
            match client.harvest(REPO, target).await {
                Ok(id) => {
                    assert_eq!(id, target, "Harvest echoes the harvested changeset");
                    Outcome::Harvested(target, before)
                }
                // A rejected harvest (target behind main once the chain
                // wraps) is a clean refusal, not an error outcome.
                Err(_) => Outcome::Harvested(target, before),
            }
        }
    }
}

/// A deterministic additive changeset for the CreateChangeset class:
/// `count` new files under `chaos/<iteration>/`, all inline.
fn chaos_changeset(parent: &Id, i: usize, count: usize) -> (Changeset, Vec<(Id, Vec<u8>)>) {
    let mut changes = Vec::with_capacity(count);
    let mut inline = Vec::with_capacity(count);
    for k in 0..count {
        let content = format!("chaos write {i}/{k}\n");
        let blob = blob_id(content.as_bytes());
        changes.push(Change {
            path: format!("chaos/{i:03}/f{k:02}.txt"),
            op: Op::Add,
            blob: Some(blob),
            src_path: None,
            mode: Mode::Regular,
        });
        inline.push((blob, content.into_bytes()));
    }
    let ident = Identity {
        name: "Chaos".into(),
        email: "chaos@bed.example".into(),
        time_secs: 1_900_000_000 + i as u64,
        tz_min: 0,
    };
    (
        Changeset {
            parents: vec![*parent],
            changes,
            message: format!("chaos iteration {i}"),
            author: ident.clone(),
            committer: ident,
            signature: None,
        },
        inline,
    )
}

/// Re-verifies one cycle's acknowledged results against the restarted
/// server, plus the loose post-kill invariants for interrupted writes.
async fn verify(client: &GardenbedClient, fx: &Fixture, outcome: &Outcome) {
    match outcome {
        Outcome::Read { name } => match *name {
            "GetTree" => {
                let tree_id = fx.tree.expect("fixture has a non-empty root tree");
                let tree = client
                    .get_tree(REPO, tree_id)
                    .await
                    .expect("tree reads back");
                assert_eq!(
                    Some(tree),
                    fx.tree_model,
                    "GetTree result changed across the kill"
                );
            }
            "GetBlob" => {
                let mut stream = client
                    .get_blob(REPO, fx.big_blob)
                    .await
                    .expect("acknowledged blob reads back");
                let mut got = Vec::with_capacity(BIG_BLOB_LEN);
                loop {
                    match stream.message().await {
                        Ok(Some(chunk)) => got.extend_from_slice(&chunk.data),
                        Ok(None) => break,
                        Err(status) => panic!("blob stream completes after restart: {status}"),
                    }
                }
                assert_eq!(
                    &got, &fx.big_content,
                    "GetBlob result changed across the kill"
                );
            }
            "Log" => {
                let mut stream = client
                    .log(REPO, fx.main, 0)
                    .await
                    .expect("acknowledged log reads back");
                let mut ids = Vec::new();
                loop {
                    match stream.message().await {
                        Ok(Some(s)) => ids.push(id_from_bytes(&s.cs_id).expect("wire id")),
                        Ok(None) => break,
                        Err(status) => panic!("log stream completes after restart: {status}"),
                    }
                }
                assert_eq!(ids, fx.log_ids, "Log result changed across the kill");
            }
            "Diff" => {
                let mut stream = client
                    .diff(REPO, fx.cs40, fx.main)
                    .await
                    .expect("acknowledged diff reads back");
                let mut changes = Vec::new();
                loop {
                    match stream.message().await {
                        Ok(Some(m)) => changes.push(Change::try_from(m).expect("wire change")),
                        Ok(None) => break,
                        Err(status) => panic!("diff stream completes after restart: {status}"),
                    }
                }
                assert_eq!(changes, fx.diff, "Diff result changed across the kill");
            }
            "Blame" => {
                let mut stream = client
                    .blame(REPO, fx.main, BLAME_PATH)
                    .await
                    .expect("acknowledged blame reads back");
                let mut ranges = Vec::new();
                loop {
                    match stream.message().await {
                        Ok(Some(r)) => {
                            ranges.push((r.lo, r.hi, id_from_bytes(&r.cs_id).expect("wire id")))
                        }
                        Ok(None) => break,
                        Err(status) => panic!("blame stream completes after restart: {status}"),
                    }
                }
                assert_eq!(ranges, fx.blame, "Blame result changed across the kill");
            }
            other => panic!("untracked read class {other}"),
        },
        Outcome::PutBlob(id, content) => {
            let mut stream = client
                .get_blob(REPO, *id)
                .await
                .expect("acknowledged blob reads back");
            let mut got = Vec::with_capacity(content.len());
            loop {
                match stream.message().await {
                    Ok(Some(chunk)) => got.extend_from_slice(&chunk.data),
                    Ok(None) => break,
                    Err(status) => panic!("blob stream completes after restart: {status}"),
                }
            }
            assert_eq!(
                &got, content,
                "acknowledged PutBlob did not read back byte-exact"
            );
        }
        Outcome::Created(id, sent) => {
            let got = client
                .get_changeset(REPO, *id)
                .await
                .expect("acknowledged changeset reads back");
            assert_eq!(&got, sent, "acknowledged CreateChangeset did not read back");
            let tree = client
                .root_tree(REPO, *id)
                .await
                .expect("acknowledged changeset derives");
            assert!(tree.is_some(), "an additive changeset has a non-empty tree");
        }
        Outcome::Cas(before, new, result) => {
            let now = client.get_ref(REPO, "chaos-rpc").await.expect("ref reads");
            match result {
                Some(CasOutcome::Won) => assert_eq!(now, Some(*new), "won CAS must read back"),
                Some(CasOutcome::Conflict { current }) => {
                    assert_eq!(
                        now,
                        Some(*current),
                        "conflict CAS leaves the ref at current"
                    )
                }
                // Killed before the answer: the ref sits at one of the two
                // legal values — never torn, never a third value.
                None => assert!(
                    now == *before || now == Some(*new),
                    "ref moved to a third value across the kill: {now:?}"
                ),
            }
        }
        Outcome::Grafted(id, parent, changes) => {
            let got = client
                .get_changeset(REPO, *id)
                .await
                .expect("acknowledged graft reads back");
            assert_eq!(got.parents, vec![*parent], "graft parents must read back");
            assert_eq!(got.changes, *changes, "graft changes must read back");
        }
        Outcome::Harvested(target, before) => {
            let now = client.get_ref(REPO, "main").await.expect("main reads");
            if now.is_some_and(|id| id == *target) {
                // Acknowledged or committed-before-the-kill: both fine.
            } else {
                // Not (visibly) harvested: main must be untouched or exactly
                // the target — never a third value.
                assert_eq!(
                    now, *before,
                    "main moved to a third value across a Harvest kill"
                );
            }
        }
        Outcome::Failed(_) => {
            // Nothing was acknowledged. State-changing calls may still have
            // committed before the kill (the response, not the commit, was
            // lost) — that is exactly the clean-failure contract; the
            // checksum sweep + rebuild determinism below cover the state.
        }
    }
}

// ---------------------------------------------------- offline verification

/// Walks every object under `<root>/loam/objects` and verifies it against
/// its content address: blob envelopes are zstd-decoded and SHA-256
/// checked; trees are decoded from canonical bytes and hashed. Returns the
/// number of objects verified.
fn sweep_loam(root: &Path) -> u64 {
    let objects = root.join("loam").join("objects");
    let mut verified = 0u64;
    let mut stack = vec![objects];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        {
            let entry = entry.expect("dir entry");
            // A SIGKILL mid-write leaves the atomic-write tmp file behind
            // (`.tmp-<pid>-<n>`, never published, never read back). That is
            // the designed crash behavior, not corruption — skip dot
            // entries; everything else must still be a 64-hex object.
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if entry.file_type().expect("file type").is_dir() {
                stack.push(entry.path());
                continue;
            }
            let shard = entry
                .path()
                .parent()
                .expect("shard dir")
                .file_name()
                .expect("shard name")
                .to_string_lossy()
                .to_string();
            let name = entry.file_name().to_string_lossy().to_string();
            let id = Id::from_str(&format!("{shard}{name}")).expect("object names are 64-hex ids");
            let bytes = std::fs::read(entry.path()).expect("object reads");
            if bytes.starts_with(b"GBLO1") {
                let raw_len = u64::from_le_bytes(bytes[5..13].try_into().expect("envelope header"));
                let content = zstd::stream::decode_all(Cursor::new(&bytes[13..]))
                    .unwrap_or_else(|e| panic!("blob {id} frame does not decode: {e}"));
                assert_eq!(
                    content.len() as u64,
                    raw_len,
                    "blob {id} envelope length lies (torn write observed)"
                );
                assert_eq!(blob_id(&content), id, "blob {id} fails its checksum");
            } else {
                assert_eq!(
                    bytes.first().copied(),
                    Some(0x02),
                    "object {id} is neither a blob envelope nor a tree payload"
                );
                gardenbed_model::Tree::decode(&bytes)
                    .unwrap_or_else(|e| panic!("object {id} is not a decodable tree: {e}"));
                let digest = sha2::Sha256::digest(&bytes);
                assert_eq!(
                    digest.as_slice(),
                    id.as_bytes(),
                    "tree {id} fails its checksum"
                );
            }
            verified += 1;
        }
    }
    verified
}

/// `PRAGMA integrity_check` on the seedbed (after WAL recovery).
fn integrity_check(root: &Path) {
    let conn = rusqlite::Connection::open(root.join("seedbed.sqlite3")).expect("seedbed opens");
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .expect("integrity_check runs");
    assert_eq!(verdict, "ok", "seedbed failed its integrity check");
}

/// Wipes every derived-data kind and recomputes the sampled answers from
/// the changeset log through a fresh in-process engine; they must be
/// identical to the pre-chaos baseline.
fn rebuild_derived_deterministically(fx: &Fixture) {
    let conn = rusqlite::Connection::open(fx.root.join("seedbed.sqlite3")).expect("seedbed opens");
    let wiped = conn
        .execute("DELETE FROM derived", [])
        .expect("derived wiped");
    assert!(wiped > 0, "the fixture has derived data to wipe");

    let repo = Repo::open(&fx.root).expect("repo reopens");
    for (cs, want) in &fx.sample_trees {
        let got = repo.root_tree(*cs).expect("tree recomputes from the log");
        assert_eq!(&got, want, "recomputed tree for {cs} differs from baseline");
    }
    let blame = repo.blame(fx.main, BLAME_PATH).expect("blame recomputes");
    assert_eq!(blame, fx.blame, "recomputed blame differs from baseline");
    let diff = repo.diff(fx.cs40, fx.main).expect("diff recomputes");
    assert_eq!(diff, fx.diff, "recomputed diff differs from baseline");
    let base = repo
        .merge_base(fx.cs40, fx.main)
        .expect("merge_base recomputes");
    assert_eq!(base, Some(fx.cs40), "recomputed merge_base differs");
    assert!(
        repo.is_ancestor(fx.cs40, fx.main)
            .expect("is_ancestor recomputes"),
        "recomputed ancestry differs"
    );
}

// ------------------------------------------------------------------ the test

/// The whole chaos matrix, serially: fixture → per-class kill/restart
/// cycles → per-cycle full verification. One test function so the
/// `GARDENBED_FSYNC` guard and the iteration counts stay race-free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rpc_classes_survive_sigkill_mid_call() {
    // Durability premise: crash-durability is what is under test. The env
    // is process-global; this is the first statement, before any store is
    // opened and before any other thread of this test touches the env.
    // SAFETY: no concurrent env readers exist yet (see above).
    unsafe {
        std::env::remove_var("GARDENBED_FSYNC");
    }

    let iters: usize = std::env::var("GARDENBED_CHAOS_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    assert!(iters > 0, "GARDENBED_CHAOS_ITERS must be positive");

    // Fixtures live on tmpfs when one exists: the fsync path is identical,
    // but the disk (and any concurrent bulk import) is not the subject.
    let shm = Path::new("/dev/shm");
    let dir = if shm.is_dir() {
        tempfile::TempDir::new_in(shm)
    } else {
        tempfile::tempdir()
    }
    .expect("chaos tempdir");
    let root = dir.path().join("arboretum");

    let t0 = std::time::Instant::now();
    let fx = build_fixture(&root);
    println!(
        "chaos: fixture built in {:.1?} ({} first-parent log entries, {} blame ranges)",
        t0.elapsed(),
        fx.log_ids.len(),
        fx.blame.len()
    );

    let mut tally: HashMap<&'static str, (usize, usize)> = HashMap::new(); // class → (ok, failed)
    let mut cycles = 0usize;
    let mut verified_objects = 0u64;
    for class in CLASSES {
        for i in 0..iters {
            // 1. spawn, start the call, kill mid-flight.
            let mut server = ServerProc::spawn(&root).await;
            let client = server.client().await;
            let ctx = CallCtx {
                client: client.clone(),
                main: fx.main,
                cs40: fx.cs40,
                tree: fx.tree,
                big_blob: fx.big_blob,
                log_ids: fx.log_ids.clone(),
                diff: fx.diff.clone(),
                blame: fx.blame.clone(),
                graftable: fx.graftable[i % fx.graftable.len()].clone(),
                harvest_target: fx.hchain[i % fx.hchain.len()],
            };
            let task = tokio::spawn(drive(ctx, class, i));
            sleep(kill_delay(i)).await;
            server.kill().await;
            let outcome = timeout(Duration::from_secs(60), task)
                .await
                .expect("the interrupted call resolves within 60s of the kill")
                .expect("the call task does not panic");
            drop(client);

            // 2. restart and verify the acknowledged results over RPC.
            let mut restarted = ServerProc::spawn(&root).await;
            let client = restarted.client().await;
            verify(&client, &fx, &outcome).await;
            restarted.kill().await;

            // 3. offline: loam checksum sweep, seedbed integrity, derived
            //    rebuild determinism.
            verified_objects = sweep_loam(&root);
            integrity_check(&root);
            rebuild_derived_deterministically(&fx);
            let entry = tally.entry(class.label()).or_insert((0, 0));
            match &outcome {
                Outcome::Failed(why) => {
                    entry.1 += 1;
                    println!(
                        "chaos: {} iteration {i}: killed before any answer ({why})",
                        class.label()
                    );
                }
                _ => entry.0 += 1,
            }
            cycles += 1;
        }
        let (ok, failed) = tally[class.label()];
        println!(
            "chaos: {:>16} × {iters} — {ok} completed, {failed} killed clean",
            class.label()
        );
    }

    println!(
        "chaos: {cycles} kill/restart cycles, {verified_objects} loam objects verified, \
         seedbed ok, derived rebuild deterministic"
    );
    assert_eq!(cycles, CLASSES.len() * iters);
}
