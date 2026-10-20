//! Token auth, end to end (real `gardenbed-server` binary, `serve
//! --auth <auth.toml>`): the role matrix, repo scoping, wildcards, and
//! the open-server back-compat default.

use std::collections::BTreeMap;
use std::path::Path;

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_protocol::client::GardenbedClient;
use tempfile::TempDir;
use tonic::Status;

/// A fixed identity.
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

/// One changeset fixture.
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
async fn commit(client: &GardenbedClient, repo: &str, cs: &Changeset, blobs: &[&[u8]]) -> Id {
    client
        .create_changeset(repo, cs, blobs.iter().map(|b| (blob_id(b), b.to_vec())))
        .await
        .expect("changeset created")
}

/// One running authed server: the spawned child plus its bound address.
struct ServerProc {
    child: tokio::process::Child,
    addr: String,
}

impl ServerProc {
    /// Spawns `serve --auth <auth_path>` on an ephemeral port and waits
    /// for its `listening on <addr>` stdout line.
    async fn spawn(root: &Path, auth_path: &Path) -> Self {
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::time::{Duration, timeout};

        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
            .arg("serve")
            .arg("--auth")
            .arg(auth_path)
            .arg("--addr")
            .arg("127.0.0.1:0")
            .arg("--root")
            .arg(root)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("server binary spawns");
        let stdout = child.stdout.take().expect("stdout piped");
        let mut lines = BufReader::new(stdout).lines();
        let line = timeout(Duration::from_secs(30), lines.next_line())
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

    /// A connected client WITHOUT any token (anonymous).
    async fn anonymous(&self) -> GardenbedClient {
        GardenbedClient::connect(&format!("http://{}", self.addr))
            .await
            .expect("client connects")
    }

    /// A connected client authenticated as `token`.
    async fn token(&self, token: &str) -> GardenbedClient {
        GardenbedClient::connect(&format!("http://{}", self.addr))
            .await
            .expect("client connects")
            .with_token(token)
    }

    /// Terminates the server and waits for it to exit.
    async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// The fixture repo root with a published two-changeset history.
fn fixture_root(dir: &TempDir) -> std::path::PathBuf {
    let root = dir.path().join("root");
    // The registry layout: the repository is served as `garden` (the
    // name the tests' tokens and RPCs address).
    let repo = gardenbed_server::Repo::open(&root.join("garden")).expect("repo opens");
    let (c1, b1) = (add("docs/readme.md", b"# hello\n"), b"# hello\n");
    let (c2, b2) = (add("src/main.rs", b"fn main() {}\n"), b"fn main() {}\n");
    let cs1 = cs(Vec::new(), vec![c1.clone(), c2.clone()], "seed", 0);
    let mut inline1 = BTreeMap::new();
    inline1.insert(c1.blob.expect("blob id").to_hex(), b1.to_vec());
    inline1.insert(c2.blob.expect("blob id").to_hex(), b2.to_vec());
    let id1 = repo.create_changeset(&cs1, &inline1).expect("cs1 ingests");
    let (c3, b3) = (add("docs/more.md", b"more\n"), b"more\n");
    let cs2 = cs(vec![id1], vec![c3.clone()], "grow", 1);
    let mut inline2 = BTreeMap::new();
    inline2.insert(c3.blob.expect("blob id").to_hex(), b3.to_vec());
    let id2 = repo.create_changeset(&cs2, &inline2).expect("cs2 ingests");
    repo.cas_ref("main", None, id2).expect("main created");
    // A second repository (`elsewhere`): the per-repo token's own
    // repo, and a name the wildcard token can reach that `garden`'s
    // tokens cannot. One commit is enough — it only needs to exist.
    let other = gardenbed_server::Repo::open(&root.join("elsewhere")).expect("repo opens");
    let (c4, b4) = (add("note.md", b"away\n"), b"away\n");
    let cs3 = cs(Vec::new(), vec![c4.clone()], "root", 0);
    let mut inline3 = BTreeMap::new();
    inline3.insert(c4.blob.expect("blob id").to_hex(), b4.to_vec());
    let id3 = other.create_changeset(&cs3, &inline3).expect("cs3 ingests");
    other.cas_ref("main", None, id3).expect("main created");
    root
}

/// Asserts the error is PERMISSION_DENIED naming `repo` and a next step.
fn assert_denied(err: &Status, repo: &str) {
    assert_eq!(err.code(), tonic::Code::PermissionDenied, "{err}");
    assert!(err.message().contains(repo), "message: {}", err.message());
    assert!(
        err.message().contains("token"),
        "message names the next step: {}",
        err.message()
    );
}

/// The auth matrix: anonymous denied on the first RPC, wrong token
/// denied, read-token reads-ok/writes-denied, write-token all-ok,
/// wildcard and per-repo scoping — and denied requests bump no counters.
#[tokio::test]
async fn auth_matrix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = fixture_root(&dir);
    let auth_path = dir.path().join("auth.toml");
    std::fs::write(
        &auth_path,
        "[tokens.\"reader\"]\nrepos = [\"garden\"]\nrole = \"read\"\n\n\
         [tokens.\"writer\"]\nrepos = [\"garden\"]\nrole = \"write\"\n\n\
         [tokens.\"anywhere\"]\nrepos = [\"*\"]\nrole = \"read\"\n\n\
         [tokens.\"other-repo\"]\nrepos = [\"elsewhere\"]\nrole = \"admin\"\n",
    )
    .expect("auth.toml written");

    let server = ServerProc::spawn(&root, &auth_path).await;
    const REPO: &str = "garden";

    // Anonymous: denied on the very first RPC, with the actionable hint.
    let anon = server.anonymous().await;
    let err = anon.get_ref(REPO, "main").await.unwrap_err();
    assert_denied(&err, REPO);
    assert!(
        err.message().contains("requires a token"),
        "anonymous names the next step: {}",
        err.message()
    );
    // Counters prove the denial happened before any store touch: a read
    // and a write are both attempted, both bump nothing.
    let before: BTreeMap<String, u64> = server
        .token("writer")
        .await
        .get_stats(REPO)
        .await
        .expect("GetStats works for a valid token");
    let err = anon
        .put_blob(REPO, tokio_stream::iter([b"x".to_vec()]))
        .await
        .unwrap_err();
    assert_denied(&err, REPO);
    let after: BTreeMap<String, u64> = server
        .token("writer")
        .await
        .get_stats(REPO)
        .await
        .expect("GetStats works");
    assert_eq!(before, after, "denied requests bump no counters");

    // Wrong token: denied, same actionable shape.
    let wrong = server.token("no-such-token").await;
    let err = wrong.get_ref(REPO, "main").await.unwrap_err();
    assert_denied(&err, REPO);
    assert!(
        err.message().contains("denied for this token"),
        "wrong token names the next step: {}",
        err.message()
    );

    // Read token: every read path works (ref → changeset → tree → blob,
    // plus the streamed ones), every write path is denied.
    let reader = server.token("reader").await;
    let main = reader
        .get_ref(REPO, "main")
        .await
        .expect("read: GetRef")
        .expect("main exists");
    let tip = reader
        .get_changeset(REPO, main)
        .await
        .expect("read: GetChangeset");
    let _tree_id = reader
        .root_tree(REPO, main)
        .await
        .expect("read: RootTree")
        .expect("non-empty root");
    use tokio_stream::StreamExt;
    let mut stream = reader
        .get_blob(REPO, tip.changes[0].blob.expect("blob id"))
        .await
        .expect("read: GetBlob");
    let _ = stream.next().await.expect("blob chunk").expect("chunk ok");
    let entries: Vec<_> = drain(reader.log(REPO, main, 0).await).await;
    assert_eq!(entries.len(), 2, "read: Log walks both changesets");
    let refs: Vec<_> = drain(reader.list_refs(REPO).await).await;
    assert_eq!(refs.len(), 1, "read: ListRefs sees main");

    // Read-token writes: put_blob, create_changeset, cas_ref, publish.
    let err = reader
        .put_blob(REPO, tokio_stream::iter([b"x".to_vec()]))
        .await
        .unwrap_err();
    assert_denied(&err, REPO);
    let err = reader.create_changeset(REPO, &tip, []).await.unwrap_err();
    assert_denied(&err, REPO);
    let err = reader
        .cas_ref(REPO, "feature", None, main)
        .await
        .unwrap_err();
    assert_denied(&err, REPO);
    let err = reader.publish(REPO, main, None, false).await.unwrap_err();
    assert_denied(&err, REPO);

    // Write token: reads succeed and so do writes (one real commit +
    // publish, proving the store accepted it).
    let writer = server.token("writer").await;
    let _ = writer
        .get_ref(REPO, "main")
        .await
        .expect("write token reads");
    let change = add("auth-note.md", b"token auth works\n");
    let cs_new = cs(vec![main], vec![change.clone()], "from a write token", 2);
    let id = commit(&writer, REPO, &cs_new, &[b"token auth works\n"]).await;
    let published = writer
        .publish(REPO, id, None, false)
        .await
        .expect("write token publishes");
    assert_eq!(published, id);
    let main_now = writer
        .get_ref(REPO, "main")
        .await
        .expect("GetRef")
        .expect("main");
    assert_eq!(main_now, id, "publish fast-forwarded main");

    // Wildcard: `*` matches any repo name — here the second real
    // repository under the same root.
    let anywhere = server.token("anywhere").await;
    anywhere
        .get_ref("elsewhere", "main")
        .await
        .expect("wildcard token reads any repository under the root");

    // Per-repo token: its own repo works, any other repo is denied —
    // even though the server is single-root (no existence leak).
    let other = server.token("other-repo").await;
    let err = other.get_ref(REPO, "main").await.unwrap_err();
    assert_denied(&err, REPO);
    other
        .get_ref("elsewhere", "main")
        .await
        .expect("per-repo token reads its own repo (any name it owns)");

    server.shutdown().await;
}

/// An invalid auth.toml refuses startup: non-zero exit, no listening line.
#[tokio::test]
async fn bad_auth_config_refuses_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = fixture_root(&dir);
    let auth_path = dir.path().join("auth.toml");
    std::fs::write(
        &auth_path,
        "[tokens.t]\nrepos = [\"garden\"]\nrole = \"chief\"\n",
    )
    .expect("auth.toml written");

    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
        .arg("serve")
        .arg("--auth")
        .arg(&auth_path)
        .arg("--addr")
        .arg("127.0.0.1:0")
        .arg("--root")
        .arg(&root)
        .output()
        .await
        .expect("server binary spawns");
    assert_ne!(out.status.code(), Some(0), "startup fails");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("listening on"),
        "no address advertised: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("role") && stderr.contains("read"),
        "error names the field and the fix: {stderr}"
    );
}

/// The open server (no --auth) answers anonymous clients as before.
#[tokio::test]
async fn open_server_accepts_anonymous() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = fixture_root(&dir);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
        .arg("--addr")
        .arg("127.0.0.1:0")
        .arg("--root")
        .arg(&root)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("server binary spawns");
    use tokio::io::{AsyncBufReadExt, BufReader};
    let stdout = child.stdout.take().expect("stdout piped");
    let mut lines = BufReader::new(stdout).lines();
    let line = lines
        .next_line()
        .await
        .expect("server stdout open")
        .expect("server prints a line");
    let addr = line
        .strip_prefix("listening on ")
        .expect("listening line")
        .to_owned();
    let client = GardenbedClient::connect(&format!("http://{addr}"))
        .await
        .expect("anonymous client connects");
    let main = client
        .get_ref("garden", "main")
        .await
        .expect("anonymous read works on an open server")
        .expect("main exists");
    let _ = client
        .root_tree("garden", main)
        .await
        .expect("anonymous RootTree works")
        .expect("non-empty root");
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Collects a client-streaming response into a Vec.
async fn drain<T>(stream: Result<tonic::Streaming<T>, Status>) -> Vec<T>
where
    T: Send + 'static,
{
    use tokio_stream::StreamExt;
    let mut stream = stream.expect("stream opens");
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item.expect("stream item ok"));
    }
    out
}
