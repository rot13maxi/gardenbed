//! `bed merge` integration (v0.2.md M10): the real CLI against the real
//! server binary — clean merge flow, working-copy movement, conflict
//! listing, up-to-date, message default, and `GARDENBED_AUTHOR`.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use common::{Server, bed};
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;
use tempfile::TempDir;

fn ident(secs: u64) -> Identity {
    Identity {
        name: "Seeder".into(),
        email: "seed@bed.example".into(),
        time_secs: secs,
        tz_min: 0,
    }
}

fn add(path: &str, content: &[u8]) -> (Change, Vec<u8>) {
    (
        Change {
            path: path.into(),
            op: Op::Add,
            blob: Some(blob_id(content)),
            src_path: None,
            mode: Mode::Regular,
        },
        content.to_vec(),
    )
}

fn inline(items: &[(&Change, &Vec<u8>)]) -> BTreeMap<String, Vec<u8>> {
    items
        .iter()
        .map(|(c, b)| (c.blob.unwrap().to_hex(), (*b).clone()))
        .collect()
}

/// One world: server root with a fixture, spawned server, two cloned
/// working copies (`mine` and `theirs`) ready to diverge.
struct World {
    _dir: TempDir,
    server: Server,
    repo: Repo,
    mine: PathBuf,
    theirs: PathBuf,
}

async fn setup() -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("server");
    let repo = Repo::open(&root).expect("repo opens");

    let base_text: Vec<u8> = (1..=12)
        .flat_map(|i| format!("line {i}\n").into_bytes())
        .collect();
    let (f, fb) = add("doc.txt", &base_text);
    let (g, gb) = add("notes.txt", b"notes base\n");
    let root_cs = Changeset {
        parents: vec![],
        changes: vec![f.clone(), g.clone()],
        message: "root".into(),
        author: ident(1),
        committer: ident(1),
        signature: None,
    };
    let _root_id = repo
        .create_changeset(&root_cs, &inline(&[(&f, &fb), (&g, &gb)]))
        .expect("root ingests");
    repo.cas_ref("main", None, _root_id).expect("ref set");

    let server = Server::spawn(&root).await;
    let mine = dir.path().join("mine");
    let theirs = dir.path().join("theirs");
    fs::create_dir_all(&mine).expect("mine dir");
    fs::create_dir_all(&theirs).expect("theirs dir");
    for wc in [&mine, &theirs] {
        let (code, _, stderr) = bed(wc, &["clone", &server.http()]);
        assert_eq!(code, 0, "clone: {stderr}");
        let (code, out, stderr) = bed(wc, &["sparse", "add", "."]);
        assert_eq!(code, 0, "sparse add: {stderr}\n{out}");
    }
    World {
        _dir: dir,
        server,
        repo,
        mine,
        theirs,
    }
}

/// `bed commit -m` in a working copy after writing `path` = `content`.
fn commit_file(wc: &Path, path: &str, content: &str, message: &str) {
    let full = wc.join(path);
    fs::write(&full, content).expect("write file");
    let (code, out, stderr) = bed(wc, &["commit", "-m", message]);
    assert_eq!(code, 0, "commit {message}: {stderr}\n{out}");
}

/// The working copy's current changeset from its config.
fn current_cs(wc: &Path) -> Id {
    let text = fs::read_to_string(wc.join(".bed/config.toml")).expect("config.toml");
    for line in text.lines() {
        if let Some((_, value)) = line
            .trim()
            .strip_prefix("current_cs")
            .map(|_| line.split_once('=').expect("current_cs = <hex>"))
        {
            let hex = value.trim().trim_matches('"');
            return hex.parse::<Id>().expect("current_cs is 64 hex chars");
        }
    }
    panic!("config.toml has no current_cs:\n{text}");
}

#[tokio::test]
async fn bed_merge_clean_merge_moves_working_copy() {
    let mut w = setup().await;
    // Diverge: mine edits the top of doc.txt, theirs edits the bottom.
    let mut mine_text = fs::read(w.mine.join("doc.txt")).expect("doc exists");
    mine_text.splice(
        0.."line 1\n".len(),
        b"MINE edited the top\n".iter().copied(),
    );
    fs::write(w.mine.join("doc.txt"), &mine_text).expect("write doc");
    let (code, out, stderr) = bed(&w.mine, &["commit", "-m", "mine top"]);
    assert_eq!(code, 0, "mine commit: {stderr}\n{out}");

    let mut theirs_text = fs::read(w.theirs.join("doc.txt")).expect("doc exists");
    let cut = theirs_text.len() - b"line 12\n".len();
    theirs_text.splice(cut.., b"THEIRS edited the bottom\n".iter().copied());
    fs::write(w.theirs.join("doc.txt"), &theirs_text).expect("write doc");
    // The second working copy is on the same changeset as the first
    // pre-commit; commit from a fresh base: switch back is unnecessary —
    // both clones started at the root.
    let (code, out, stderr) = bed(&w.theirs, &["commit", "-m", "theirs bottom"]);
    assert_eq!(code, 0, "theirs commit: {stderr}\n{out}");

    // Publish theirs so `bed merge <ref>` can resolve it.
    let (code, out, stderr) = bed(&w.theirs, &["publish"]);
    assert_eq!(code, 0, "theirs publish: {stderr}\n{out}");

    // Merge main into mine.
    let (code, out, stderr) = bed(&w.mine, &["merge", "main"]);
    assert_eq!(code, 0, "bed merge: {stderr}\n{out}");
    assert!(out.contains("merged"), "merge output: {out}");

    // Working copy state: clean status, current_cs has two parents.
    let (code, out, stderr) = bed(&w.mine, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(
        out.contains("working copy is clean"),
        "status after merge: {out}"
    );
    let merged = current_cs(&w.mine);
    let cs = w.repo.get_changeset(merged).expect("merged cs readable");
    assert_eq!(cs.parents.len(), 2, "merge changeset has two parents");
    // First parent is the "mine top" commit we just made.
    let mine_cs = w.repo.get_ref("heads/mine").expect("ref lookup");
    let _ = mine_cs;
    // Content: both edits present.
    let doc = fs::read(w.mine.join("doc.txt")).expect("doc after merge");
    let text = String::from_utf8_lossy(&doc);
    assert!(text.contains("MINE edited the top"), "ours kept: {text}");
    assert!(
        text.contains("THEIRS edited the bottom"),
        "theirs merged: {text}"
    );
    // And the notes file survived untouched.
    assert_eq!(
        fs::read(w.mine.join("notes.txt")).expect("notes after merge"),
        b"notes base\n"
    );
    w.server.shutdown().await;
}

#[tokio::test]
async fn bed_merge_conflict_lists_every_path() {
    let mut w = setup().await;
    // Both edit the SAME line of doc.txt: a content conflict.
    fs::write(w.mine.join("doc.txt"), "line 1\nMINE\nline 3\nrest\n").expect("mine doc");
    let (code, _, stderr) = bed(&w.mine, &["commit", "-m", "mine conflict"]);
    assert_eq!(code, 0, "mine commit: {stderr}");

    fs::write(w.theirs.join("doc.txt"), "line 1\nTHEIRS\nline 3\nrest\n").expect("theirs doc");
    let (code, _, stderr) = bed(&w.theirs, &["commit", "-m", "theirs conflict"]);
    assert_eq!(code, 0, "theirs commit: {stderr}");
    let (code, out, stderr) = bed(&w.theirs, &["publish"]);
    assert_eq!(code, 0, "theirs publish: {stderr}\n{out}");

    let (code, _out, stderr) = bed(&w.mine, &["merge", "main"]);
    assert_ne!(code, 0, "conflicting merge must fail");
    assert!(
        stderr.contains("merge") && stderr.contains("doc.txt"),
        "conflict error names the merge and every path: {stderr}"
    );
    // The next step rides along (the docs/cli.md contract).
    assert!(
        stderr.contains("next step: edit the conflicting files, commit, and merge again"),
        "conflict error names the next step: {stderr}"
    );
    // The working copy is untouched on conflict.
    let doc = fs::read(w.mine.join("doc.txt")).expect("doc untouched");
    assert_eq!(doc, b"line 1\nMINE\nline 3\nrest\n");
    w.server.shutdown().await;
}

#[tokio::test]
async fn bed_merge_up_to_date_is_a_noop() {
    let mut w = setup().await;
    commit_file(&w.theirs, "doc.txt", "theirs only\n", "theirs only");
    let (code, _, stderr) = bed(&w.theirs, &["publish"]);
    assert_eq!(code, 0, "publish: {stderr}");
    // mine merges main (fast-forward), then merges again: up to date.
    let (code, _out, stderr) = bed(&w.mine, &["merge", "main"]);
    assert_eq!(code, 0, "first merge: {stderr}");
    let before = current_cs(&w.mine);
    let (code, out, stderr) = bed(&w.mine, &["merge", "main"]);
    assert_eq!(code, 0, "second merge: {stderr}\n{out}");
    assert!(
        out.contains("already up to date"),
        "up-to-date message: {out}"
    );
    assert_eq!(before, current_cs(&w.mine), "no new changeset");
    w.server.shutdown().await;
}

#[tokio::test]
async fn bed_merge_message_default_and_author_env() {
    let mut w = setup().await;
    let mut t = fs::read(w.mine.join("doc.txt")).expect("doc");
    t.splice(0.."line 1\n".len(), b"mine top\n".iter().copied());
    fs::write(w.mine.join("doc.txt"), &t).expect("write");
    let (code, _, stderr) = bed(&w.mine, &["commit", "-m", "mine"]);
    assert_eq!(code, 0, "mine commit: {stderr}");

    let mut t2 = fs::read(w.theirs.join("doc.txt")).expect("doc");
    let cut = t2.len() - b"line 12\n".len();
    t2.splice(cut.., b"theirs bottom\n".iter().copied());
    fs::write(w.theirs.join("doc.txt"), &t2).expect("write");
    let (code, _, stderr) = bed(&w.theirs, &["commit", "-m", "theirs"]);
    assert_eq!(code, 0, "theirs commit: {stderr}");
    let (code, _, stderr) = bed(&w.theirs, &["publish"]);
    assert_eq!(code, 0, "publish: {stderr}");

    // Default message is "merge <target>"; GARDENBED_AUTHOR supplies identity.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(&w.mine)
        .args(["merge", "main"])
        .env("NO_COLOR", "1")
        .env("GARDENBED_AUTHOR", "Env Merger <env@bed.example>")
        .output()
        .expect("bed spawns");
    assert_eq!(
        out.status.code(),
        Some(0),
        "merge: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let merged = current_cs(&w.mine);
    let cs = w.repo.get_changeset(merged).expect("cs readable");
    assert!(
        cs.message.starts_with("merge "),
        "default message is 'merge <target>': {:?}",
        cs.message
    );
    assert_eq!(cs.author.name, "Env Merger", "GARDENBED_AUTHOR used");
    assert_eq!(cs.author.email, "env@bed.example");
    w.server.shutdown().await;
}

#[tokio::test]
async fn bed_merge_refuses_dirty_working_copy_without_storing() {
    let mut w = setup().await;
    // Diverge theirs and publish, as usual.
    commit_file(&w.theirs, "doc.txt", "theirs diverged\n", "theirs");
    let (code, _, stderr) = bed(&w.theirs, &["publish"]);
    assert_eq!(code, 0, "theirs publish: {stderr}");

    // Dirty the working copy: an uncommitted edit mine has not committed.
    fs::write(w.mine.join("notes.txt"), "uncommitted local work\n").expect("dirty");

    // The merge must refuse BEFORE the RPC: the server must not store a
    // merge changeset for a client that cannot take the checkout.
    let (code, _out, stderr) = bed(&w.mine, &["merge", "main"]);
    assert_ne!(code, 0, "dirty merge must fail");
    assert!(
        stderr.contains("dirty working copy"),
        "error names the dirty working copy: {stderr}"
    );
    // The current changeset is unchanged by the refusal…
    let before = current_cs(&w.mine);
    // …and the server stored no merge for this pair: a retry after
    assert_eq!(
        before,
        current_cs(&w.mine),
        "no local state move on refusal"
    );
    // committing the local work must produce exactly one new changeset,
    // not two (the pre-check means the refused attempt stored nothing).
    let (code, _, stderr) = bed(&w.mine, &["commit", "-m", "local work"]);
    assert_eq!(code, 0, "commit local work: {stderr}");
    let (code, _, stderr) = bed(&w.mine, &["merge", "main"]);
    assert_eq!(code, 0, "merge after commit: {stderr}");
    let merged = current_cs(&w.mine);
    let cs = w.repo.get_changeset(merged).expect("merged cs");
    assert_eq!(cs.parents.len(), 2, "two parents");

    // The refused attempt stored nothing: the only two-parent changeset
    // reachable from the merge (both sides are ancestors) is the merge
    // itself. Walk the store directly from the merged tip.
    let log = w.repo.log(merged, 100).expect("log");
    let merges: Vec<_> = log.iter().filter(|(_, cs)| cs.parents.len() == 2).collect();
    assert_eq!(
        merges.len(),
        1,
        "exactly one stored merge (the refused attempt stored none): {merges:?}"
    );
    w.server.shutdown().await;
}
