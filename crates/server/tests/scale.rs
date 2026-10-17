//! Scale-out verification (docs/scale-out-design.md, §Verification 2):
//! N independent handles over the SAME off-box stores (Postgres metadata
//! + S3 objects) behave as one logical repository — read-your-writes
//! across handles, exactly-one-winner CAS races, idempotent identical
//! creates, and cross-handle object fetches. The second half repeats the
//! create+publish+fetch cycle split across two real `gardenbed-server`
//! processes spawned with `--storage`, proving the server wiring (flag
//! parsing, `StorageConfig::open`, `Repo::open_stores`) end to end.
//!
//! Gated on live services (no developer without Docker pays for it):
//! set `GARDENBED_SCALE_TEST=1` plus `GARDENBED_TEST_PG_URL` and the
//! `GARDENBED_TEST_S3_*` family. Without them the tests print a note and
//! pass. Against a running `tools/scale/compose.yaml` stack:
//!
//! ```sh
//! GARDENBED_SCALE_TEST=1 \
//! GARDENBED_TEST_PG_URL=postgres://gardenbed:gardenbed@127.0.0.1:55432/gardenbed \
//! GARDENBED_TEST_S3_ENDPOINT=http://127.0.0.1:55900 \
//! GARDENBED_TEST_S3_BUCKET=gardenbed-test \
//! GARDENBED_TEST_S3_ACCESS_KEY=gardenbed \
//! GARDENBED_TEST_S3_SECRET_KEY=gardenbed-secret \
//! GARDENBED_TEST_S3_REGION=us-east-1 \
//! GARDENBED_TEST_S3_PATH_STYLE=1 \
//! cargo test -p gardenbed-server --test scale -- --nocapture
//! ```
//!
//! Each run creates a fresh Postgres database (the bucket is shared:
//! object puts are content-addressed and idempotent) so repeated runs
//! never see stale metadata.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_protocol::client::{CasOutcome, GardenbedClient};
use gardenbed_server::{Repo, ServerError};
use gardenbed_store::{MetadataStore, ObjectStore, RefCas, S3Config, StorageConfig};

// ------------------------------------------------------------- fixtures

/// A fixed identity (times differ per changeset so log ordering is stable).
fn identity(name: &str, secs: u64) -> Identity {
    Identity {
        name: name.into(),
        email: format!("{name}@garden.example"),
        time_secs: 1_700_000_000 + secs,
        tz_min: 0,
    }
}

/// A canonically sorted Add change.
fn add(path: &str, content: &[u8]) -> Change {
    Change {
        path: path.into(),
        op: Op::Add,
        blob: Some(blob_id(content)),
        src_path: None,
        mode: Mode::Regular,
    }
}

/// One changeset fixture: message + fixed author/committer identity.
fn cs(parents: Vec<Id>, changes: Vec<Change>, message: &str, secs: u64) -> Changeset {
    Changeset {
        parents,
        changes,
        message: message.into(),
        author: identity("amy", secs),
        committer: identity("amy", secs),
        signature: None,
    }
}

/// Commits `cs` with its inline blobs, returning the assigned id.
fn commit(repo: &Repo, cs: &Changeset, blobs: &[&[u8]]) -> Result<Id, ServerError> {
    let inline: BTreeMap<String, Vec<u8>> = blobs
        .iter()
        .map(|b| (blob_id(b).to_hex(), b.to_vec()))
        .collect();
    repo.create_changeset(cs, &inline)
}

// --------------------------------------------------------- env plumbing

/// A unique name for this test run's storage (bucket prefix / database
/// name) so repeated runs never see stale state.
fn run_tag(test: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("gbdtest-{test}-{nanos:x}")
}

/// The parsed `GARDENBED_TEST_S3_*` family, or `None` when any is unset.
fn s3_env() -> Option<S3Config> {
    let endpoint = std::env::var("GARDENBED_TEST_S3_ENDPOINT").ok()?;
    let bucket = std::env::var("GARDENBED_TEST_S3_BUCKET").ok()?;
    let access_key = std::env::var("GARDENBED_TEST_S3_ACCESS_KEY").ok()?;
    let secret_key = std::env::var("GARDENBED_TEST_S3_SECRET_KEY").ok()?;
    Some(S3Config {
        endpoint,
        bucket,
        region: std::env::var("GARDENBED_TEST_S3_REGION")
            .unwrap_or_else(|_| "us-east-1".into()),
        access_key,
        secret_key,
        path_style: std::env::var("GARDENBED_TEST_S3_PATH_STYLE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(true),
    })
}

/// Skips (passes with a note) unless `GARDENBED_SCALE_TEST=1` and the
/// live-service env vars are present. Returns the live `StorageConfig`
/// plus a per-run tag for unique ref names (parity-test style: shared
/// database + bucket, per-run names, idempotent content-addressed puts).
fn live_config(test: &str) -> Option<(StorageConfig, String)> {
    if std::env::var("GARDENBED_SCALE_TEST").ok().as_deref() != Some("1") {
        println!("note: scale tests skipped (set GARDENBED_SCALE_TEST=1 to run)");
        return None;
    }
    let url = std::env::var("GARDENBED_TEST_PG_URL").ok();
    let s3 = s3_env();
    if url.is_none() || s3.is_none() {
        println!(
            "note: scale tests skipped — live services not configured. Export:\n  \
             GARDENBED_SCALE_TEST=1\n  \
             GARDENBED_TEST_PG_URL=postgres://gardenbed:gardenbed@127.0.0.1:55432/gardenbed\n  \
             GARDENBED_TEST_S3_ENDPOINT=http://127.0.0.1:55900\n  \
             GARDENBED_TEST_S3_BUCKET=gardenbed-test\n  \
             GARDENBED_TEST_S3_ACCESS_KEY=gardenbed\n  \
             GARDENBED_TEST_S3_SECRET_KEY=gardenbed-secret\n  \
             GARDENBED_TEST_S3_REGION=us-east-1\n  \
             GARDENBED_TEST_S3_PATH_STYLE=1"
        );
        return None;
    }
    let tag = run_tag(test);
    let config = StorageConfig {
        metadata: gardenbed_store::MetadataConfig::Postgres {
            url: url.expect("checked above"),
        },
        objects: gardenbed_store::ObjectsConfig::S3(s3.expect("checked above")),
    };
    Some((config, tag))
}

/// Writes the config to a temp file and opens it through the public
/// `StorageConfig::from_path` + `open` API — the exact server wiring.
fn open_stores(
    dir: &tempfile::TempDir,
    config: &StorageConfig,
) -> (Arc<dyn ObjectStore>, Arc<dyn MetadataStore>) {
    let path: PathBuf = dir.path().join("storage.toml");
    let toml = render_config_toml(config);
    std::fs::write(&path, toml).expect("write storage.toml");
    StorageConfig::from_path(&path)
        .and_then(|c| c.open())
        .expect("StorageConfig::from_path + open")
}

/// Renders a `StorageConfig` back to the TOML the server consumes.
fn render_config_toml(config: &StorageConfig) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    match &config.metadata {
        gardenbed_store::MetadataConfig::Sqlite { root } => {
            let _ = writeln!(out, "[metadata]");
            let _ = writeln!(out, "backend = \"sqlite\"");
            let _ = writeln!(out, "root = \"{}\"", root.display());
        }
        gardenbed_store::MetadataConfig::Postgres { url } => {
            let _ = writeln!(out, "[metadata]");
            let _ = writeln!(out, "backend = \"postgres\"");
            let _ = writeln!(out, "url = \"{url}\"");
        }
    }
    match &config.objects {
        gardenbed_store::ObjectsConfig::Fs { root } => {
            let _ = writeln!(out, "\n[objects]");
            let _ = writeln!(out, "backend = \"fs\"");
            let _ = writeln!(out, "root = \"{}\"", root.display());
        }
        gardenbed_store::ObjectsConfig::S3(s3) => {
            let _ = writeln!(out, "\n[objects]");
            let _ = writeln!(out, "backend = \"s3\"");
            let _ = writeln!(out, "endpoint = \"{}\"", s3.endpoint);
            let _ = writeln!(out, "bucket = \"{}\"", s3.bucket);
            let _ = writeln!(out, "region = \"{}\"", s3.region);
            let _ = writeln!(out, "access_key = \"{}\"", s3.access_key);
            let _ = writeln!(out, "secret_key = \"{}\"", s3.secret_key);
            let _ = writeln!(out, "path_style = {}", s3.path_style);
        }
    }
    out
}

// ------------------------------------------------- in-process scenarios

/// Read-your-writes, CAS races, idempotent creates, cross-handle object
/// fetch — all through `Repo` handles over separately-opened stores
/// (the seam, without port management).
#[test]
fn replica_correctness_in_process() {
    let Some((config, tag)) = live_config("inproc") else { return };
    let dir = tempfile::tempdir().expect("tempdir");

    // Two handles, two independent open() calls over the same backing
    // services — the in-process stand-in for two replicas.
    let (objects_a, metadata_a) = open_stores(&dir, &config);
    let (objects_b, metadata_b) = open_stores(&dir, &config);
    let a = Repo::open_stores(objects_a, metadata_a);
    let b = Repo::open_stores(objects_b, metadata_b);

    // (a) create via A is visible via B (read-your-writes).
    let blob = b"scale-out blob\n".to_vec();
    let cs_root = cs(vec![], vec![add("f.txt", &blob)], "root", 1);
    let id_a = commit(&a, &cs_root, &[&blob]).expect("A creates the changeset");
    let got_b = b.get_changeset(id_a).expect("B reads metadata");
    assert_eq!(got_b, cs_root, "B sees A's changeset");

    // Object written via A, fetched via B (cross-handle object fetch).
    let got_blob = b
        .get_blob(blob_id(&blob))
        .expect("B fetches A's object")
        .expect("object present");
    assert_eq!(got_blob, blob, "byte-identical object across handles");

    // A second changeset off the first, so the CAS race has two distinct
    // target values.
    let blob2 = b"second blob\n".to_vec();
    let cs_two = cs(vec![id_a], vec![add("g.txt", &blob2)], "two", 2);
    let id_two = commit(&a, &cs_two, &[&blob2]).expect("A creates the child");

    // (b) concurrent cas_ref on one ref from both handles → exactly one
    // Won. Spawned threads so the two CASes genuinely overlap; both
    // create the same ref (`expected = None`) with different values.
    let a = Arc::new(a);
    let b = Arc::new(b);
    let a_cas = Arc::clone(&a);
    let b_cas = Arc::clone(&b);
    let tag_b = tag.clone();
    let handle_a = std::thread::spawn(move || {
        a_cas
            .cas_ref(&format!("{tag}-feature"), None, id_a)
            .expect("A CAS")
    });
    let handle_b = std::thread::spawn(move || {
        b_cas
            .cas_ref(&format!("{tag_b}-feature"), None, id_two)
            .expect("B CAS")
    });
    let out_a = handle_a.join().expect("A thread");
    let out_b = handle_b.join().expect("B thread");
    let wins = matches!(out_a, RefCas::Won { .. }) as u32
        + matches!(out_b, RefCas::Won { .. }) as u32;
    assert_eq!(wins, 1, "exactly one CAS won (a={out_a:?}, b={out_b:?})");
    // Whoever lost reported the winner's value as the blocking current.
    let current = match (&out_a, &out_b) {
        (_, RefCas::Won { current }) | (RefCas::Won { current }, _) => *current,
        (RefCas::Conflict { current }, _) => *current,
    };
    assert!(
        current == id_a || current == id_two,
        "the ref holds one of the raced values"
    );

    // (c) concurrent identical create_changeset from both handles →
    // the same id, no error: content-addressed puts are idempotent.
    // Fixed identities make the changeset (and its id) identical from
    // both sides; the winner stores it, the loser's put is a no-op.
    let blob3 = b"raced blob\n".to_vec();
    let cs_race = cs(vec![id_a], vec![add("r.txt", &blob3)], "race", 3);
    let expected = cs_race.id().expect("id derives");
    let inline: BTreeMap<String, Vec<u8>> =
        [(blob_id(&blob3).to_hex(), blob3.clone())].into();
    let a_create = Arc::clone(&a);
    let cs_a = cs_race.clone();
    let inline_a = inline.clone();
    let handle_a = std::thread::spawn(move || {
        a_create
            .create_changeset(&cs_a, &inline_a)
            .expect("A races create")
    });
    let id_b = b
        .create_changeset(&cs_race, &inline)
        .expect("B races create");
    let id_a_race = handle_a.join().expect("A create thread");
    assert_eq!(id_a_race, expected, "A's raced create got the shared id");
    assert_eq!(id_b, expected, "B's raced create got the shared id");
    // One changeset, not two: the metadata resolves the same row.
    assert_eq!(
        a.get_changeset(expected).expect("raced cs readable"),
        cs_race,
        "the raced changeset is the one both sides computed"
    );

    println!("scale: in-process replica correctness ok (tag={})", run_tag("inproc"));
}

/// One running server: the spawned child plus its bound address.
struct ServerProc {
    child: tokio::process::Child,
    addr: String,
}

impl ServerProc {
    /// Spawns the built binary on an ephemeral port with `--storage`
    /// and waits for its `listening on <addr>` stdout line.
    async fn spawn(storage: &std::path::Path) -> Self {
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::time::{Duration, timeout};

        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
            .arg("--addr")
            .arg("127.0.0.1:0")
            .arg("--storage")
            .arg(storage)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("server binary spawns");
        let stdout = child.stdout.take().expect("stdout piped");
        let mut lines = BufReader::new(stdout).lines();
        let line = timeout(Duration::from_secs(60), lines.next_line())
            .await
            .expect("server announces itself within 60s")
            .expect("server stdout open")
            .expect("server prints a line");
        let addr = line
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("expected a listening line, got {line:?}"))
            .to_owned();
        ServerProc { child, addr }
    }

    /// A connected client.
    async fn client(&self) -> GardenbedClient {
        GardenbedClient::connect(&format!("http://{}", self.addr))
            .await
            .expect("client connects")
    }

    /// Terminates the server and waits for it to exit.
    async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// A blob's content streamed back through GetBlob, reassembled.
async fn read_blob(client: &GardenbedClient, repo: &str, id: Id) -> Vec<u8> {
    use tokio_stream::StreamExt;
    let mut stream = client.get_blob(repo, id).await.expect("GetBlob ok");
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("chunk ok").data);
    }
    out
}

/// Full gRPC cycle split across two real server processes spawned with
/// `--storage`: create via replica 1, publish via replica 2, fetch via
/// replica 1 again (a round trip that must cross the off-box stores).
#[tokio::test]
async fn replica_correctness_grpc() {
    let Some((config, tag)) = live_config("grpc") else { return };
    let dir = tempfile::tempdir().expect("tempdir");
    let path: PathBuf = dir.path().join("storage.toml");
    std::fs::write(&path, render_config_toml(&config)).expect("write storage.toml");

    let one = ServerProc::spawn(&path).await;
    let two = ServerProc::spawn(&path).await;
    // The repo name is per-run: refs live in a shared database, so a
    // per-run repo name keeps publish/"main" unique without port math.
    // (The server is single-repo in v0.1; the name tags the run.)
    let repo = format!("garden-{tag}");
    let repo = repo.as_str();

    let c1 = one.client().await;
    let c2 = two.client().await;

    // Create through replica 1.
    let blob = b"grpc scale blob\n".to_vec();
    let cs_root = cs(vec![], vec![add("f.txt", &blob)], "root", 1);
    let id = c1
        .create_changeset(
            repo,
            &cs_root,
            [(blob_id(&blob), blob.clone())],
        )
        .await
        .expect("replica 1 creates");
    // Visible through replica 2 (read-your-writes across processes).
    assert_eq!(
        c2.get_changeset(repo, id).await.expect("replica 2 reads"),
        cs_root
    );

    // Publish through replica 2, fetch the ref + blob through replica 1.
    let published = c2.publish(repo, id).await.expect("replica 2 publishes");
    assert_eq!(published, id);
    assert_eq!(
        c1.get_ref(repo, "main").await.expect("replica 1 ref"),
        Some(id)
    );
    let got = read_blob(&c1, repo, blob_id(&blob)).await;
    assert_eq!(got, blob, "replica 1 fetches replica 1's object");

    // A CAS race across the two processes on one fresh ref: exactly one
    // Won (the other must Conflict, never error).
    let blob2 = b"grpc race blob\n".to_vec();
    let cs_two = cs(vec![id], vec![add("g.txt", &blob2)], "two", 2);
    let id_two = c1
        .create_changeset(repo, &cs_two, [(blob_id(&blob2), blob2.clone())])
        .await
        .expect("replica 1 creates the child");
    let race_ref = format!("{tag}-race");
    let (r1, r2) = tokio::join!(
        c1.cas_ref(repo, &race_ref, None, id),
        c2.cas_ref(repo, &race_ref, None, id_two)
    );
    let out1 = r1.expect("replica 1 CAS ok");
    let out2 = r2.expect("replica 2 CAS ok");
    let wins = matches!(out1, CasOutcome::Won) as u32 + matches!(out2, CasOutcome::Won) as u32;
    assert_eq!(wins, 1, "exactly one process won (1={out1:?}, 2={out2:?})");

    one.shutdown().await;
    two.shutdown().await;
    println!("scale: grpc replica correctness ok");
}
