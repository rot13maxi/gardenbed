//! Integration tests for the web JSON API: an in-memory repo (MemoryStore
//! backing both traits), a small hand-built history, and `tower`'s
//! oneshot driving `gardenbed_web::router` directly — no sockets.
//!
//! The fixture history:
//!
//! ```text
//! c1  add a.txt, run.sh (exec)
//! c2  modify a.txt, add dir/nested.txt, add notes.txt
//! c3  rename dir/nested.txt -> dir/moved.txt, add symlink link -> dir/moved.txt
//! c4  add bin.dat (non-UTF-8), add big.txt (600 KiB of text)
//! c5  delete notes.txt, modify a.txt
//! ```
//!
//! `main` points at c5.

use std::collections::BTreeMap;
use std::sync::Arc;

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;
use gardenbed_store::MemoryStore;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use serde_json::Value;
use tower::ServiceExt;

/// Fixed identity; commit `i` gets a fixed timestamp so ids are stable.
fn ident(i: u64) -> Identity {
    Identity {
        name: format!("Tester {i}"),
        email: format!("tester{i}@example.com"),
        time_secs: 1_700_000_000 + i * 86_400,
        tz_min: 60,
    }
}

/// Builds the fixture repo (in-memory) with ids [c1..c5]; `main` = c5.
fn fixture() -> (Repo, [Id; 5]) {
    let repo = Repo::new(Arc::new(MemoryStore::new()), Arc::new(MemoryStore::new()));

    let a_v1 = b"line1\nline2\n".to_vec();
    let a_v2 = b"line1\nline2\nline3\n".to_vec();
    let a_v3 = b"line1\nline2\nline3\nline4\n".to_vec();

    let mut inline = BTreeMap::new();
    let add = |inline: &mut BTreeMap<String, Vec<u8>>, path: &str, content: &[u8], mode: Mode| {
        let id = blob_id(content);
        inline.insert(id.to_hex(), content.to_vec());
        Change {
            path: path.to_owned(),
            op: Op::Add,
            blob: Some(id),
            src_path: None,
            mode,
        }
    };
    let modify =
        |inline: &mut BTreeMap<String, Vec<u8>>, path: &str, content: &[u8], mode: Mode| {
            let id = blob_id(content);
            inline.insert(id.to_hex(), content.to_vec());
            Change {
                path: path.to_owned(),
                op: Op::Modify,
                blob: Some(id),
                src_path: None,
                mode,
            }
        };

    let mk = |i: u64,
              parents: Vec<Id>,
              mut changes: Vec<Change>,
              inline: &mut BTreeMap<String, Vec<u8>>| {
        // Canonical order: strictly ascending by path bytes.
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        let cs = Changeset {
            parents,
            changes,
            message: format!("commit {i}"),
            author: ident(i),
            committer: ident(i),
            signature: None,
        };
        repo.create_changeset(&cs, inline).unwrap()
    };

    inline.clear();
    let c1 = mk(
        1,
        vec![],
        vec![
            add(&mut inline, "a.txt", &a_v1, Mode::Regular),
            add(&mut inline, "run.sh", b"#!/bin/sh\necho sow\n", Mode::Exec),
        ],
        &mut inline,
    );

    inline.clear();
    let c2 = mk(
        2,
        vec![c1],
        vec![
            modify(&mut inline, "a.txt", &a_v2, Mode::Regular),
            add(&mut inline, "dir/nested.txt", b"nested\n", Mode::Regular),
            add(&mut inline, "notes.txt", b"note\n", Mode::Regular),
        ],
        &mut inline,
    );

    inline.clear();
    let c3 = mk(
        3,
        vec![c2],
        vec![
            Change {
                path: "dir/moved.txt".to_owned(),
                op: Op::Rename,
                // A pure rename reuses the old blob id.
                blob: Some(blob_id(b"nested\n")),
                src_path: Some("dir/nested.txt".to_owned()),
                mode: Mode::Regular,
            },
            add(&mut inline, "link", b"dir/moved.txt", Mode::Symlink),
        ],
        &mut inline,
    );

    inline.clear();
    let big = vec![b'x'; 600 * 1024];
    let c4 = mk(
        4,
        vec![c3],
        vec![
            add(
                &mut inline,
                "bin.dat",
                &[0xff, 0xfe, 0x00, 0x01],
                Mode::Regular,
            ),
            add(&mut inline, "big.txt", &big, Mode::Regular),
        ],
        &mut inline,
    );

    inline.clear();
    let c5 = mk(
        5,
        vec![c4],
        vec![
            modify(&mut inline, "a.txt", &a_v3, Mode::Regular),
            Change {
                path: "notes.txt".to_owned(),
                op: Op::Delete,
                blob: None,
                src_path: None,
                mode: Mode::Regular,
            },
        ],
        &mut inline,
    );

    match repo.cas_ref("main", None, c5).unwrap() {
        gardenbed_store::RefCas::Won { .. } => {}
        other => panic!("fixture ref CAS lost: {other:?}"),
    }
    (repo, [c1, c2, c3, c4, c5])
}

/// One GET against the router; returns (status, content-type, body).
async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
    let resp: Response = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, ctype, body)
}

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let (status, _, body) = get(app, uri).await;
    let json: Value = serde_json::from_slice(&body).unwrap_or_else(|e| {
        panic!(
            "{uri}: body is not JSON ({e}): {}",
            String::from_utf8_lossy(&body)
        )
    });
    (status, json)
}

#[tokio::test]
async fn refs_lists_main() {
    let (repo, [_, _, _, _, tip]) = fixture();
    let (status, json) = get_json(&gardenbed_web::router(repo), "/api/refs").await;
    assert_eq!(status, StatusCode::OK);
    let refs = json["refs"].as_array().unwrap();
    assert!(
        refs.iter()
            .any(|r| r["name"] == "refs/heads/main" && r["cs"] == tip.to_hex())
    );
}

#[tokio::test]
async fn log_defaults_to_main_and_respects_limit() {
    let (repo, [c1, _, _, _, tip]) = fixture();
    let app = gardenbed_web::router(repo);

    let (status, json) = get_json(&app, "/api/log").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["resolved"], tip.to_hex());
    assert_eq!(json["entries"].as_array().unwrap().len(), 5);
    assert_eq!(json["entries"][0]["id"], tip.to_hex());
    assert_eq!(json["entries"][0]["author"]["name"], "Tester 5");
    assert_eq!(json["entries"][0]["changes"], 2);
    assert_eq!(json["entries"][4]["id"], c1.to_hex());
    assert_eq!(json["entries"][4]["parents"].as_array().unwrap().len(), 0);

    let (status, json) = get_json(&app, "/api/log?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["entries"].as_array().unwrap().len(), 2);

    // Ref name and 8+-char prefix resolve to the same changeset.
    let prefix = &c1.to_hex()[..8];
    let (status, json) = get_json(&app, &format!("/api/log?cs={prefix}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["resolved"], c1.to_hex());

    let (status, json) = get_json(&app, "/api/log?limit=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().contains("limit"));
}

#[tokio::test]
async fn changeset_returns_full_changes() {
    let (repo, [_, _, c3, _, _]) = fixture();
    let app = gardenbed_web::router(repo);

    let (status, json) = get_json(&app, &format!("/api/changeset?id={}", c3.to_hex())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["id"], c3.to_hex());
    assert_eq!(json["message"], "commit 3");
    let changes = json["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 2);
    let rename = changes
        .iter()
        .find(|c| c["op"] == "rename")
        .expect("rename change present");
    assert_eq!(rename["path"], "dir/moved.txt");
    assert_eq!(rename["src_path"], "dir/nested.txt");
    // A pure rename reuses the old blob id.
    assert_eq!(rename["blob"], blob_id(b"nested\n").to_hex());
    let link = changes.iter().find(|c| c["path"] == "link").unwrap();
    assert_eq!(link["op"], "add");
    assert_eq!(link["mode"], "symlink");
    assert!(link["blob"].is_string());
}

#[tokio::test]
async fn tree_lists_dirs_first_then_files() {
    let (repo, ..) = fixture();
    let app = gardenbed_web::router(repo);

    let (status, json) = get_json(&app, "/api/tree").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["path"], "");
    let entries = json["entries"].as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["dir", "a.txt", "big.txt", "bin.dat", "link", "run.sh"]
    );
    assert_eq!(entries[0]["kind"], "directory");
    let link = entries.iter().find(|e| e["name"] == "link").unwrap();
    assert_eq!(link["kind"], "symlink");
    assert!(entries.iter().all(|e| e["id"].is_string()));

    let (status, json) = get_json(&app, "/api/tree?path=dir").await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["moved.txt"]);

    // A file is not a directory: 400 with the contract's message.
    let (status, json) = get_json(&app, "/api/tree?path=a.txt").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"], "not a directory: a.txt");

    let (status, _) = get_json(&app, "/api/tree?path=nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn file_serves_utf8_base64_truncation_and_symlinks() {
    let (repo, ..) = fixture();
    let app = gardenbed_web::router(repo);

    let (status, json) = get_json(&app, "/api/file?path=a.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["content"], "line1\nline2\nline3\nline4\n");
    assert_eq!(json["encoding"], "utf8");
    assert_eq!(json["mode"], "regular");
    assert_eq!(json["size"], 24);
    assert_eq!(json["truncated"], false);

    // exec bit survives.
    let (status, json) = get_json(&app, "/api/file?path=run.sh").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["mode"], "exec");

    // >512 KiB: capped payload, truncated flag, still UTF-8 text.
    let (status, json) = get_json(&app, "/api/file?path=big.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["truncated"], true);
    assert_eq!(json["size"], 600 * 1024);
    assert_eq!(json["encoding"], "utf8");
    assert_eq!(json["content"].as_str().unwrap().len(), 512 * 1024);

    // Non-UTF-8 bytes ride as base64.
    let (status, json) = get_json(&app, "/api/file?path=bin.dat").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["encoding"], "base64");
    assert_eq!(json["content"], base64_of(&[0xff, 0xfe, 0x00, 0x01]));

    // Symlink: the content is the target text.
    let (status, json) = get_json(&app, "/api/file?path=link").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["mode"], "symlink");
    assert_eq!(json["encoding"], "utf8");
    assert_eq!(json["content"], "dir/moved.txt");

    let (status, _) = get_json(&app, "/api/file?path=ghost.txt").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A directory path is not a file: 400.
    let (status, _) = get_json(&app, "/api/file?path=dir").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn blame_covers_whole_file_with_known_changesets() {
    let (repo, [c1, c2, _, _, c5]) = fixture();
    let app = gardenbed_web::router(repo);

    let (status, json) = get_json(&app, "/api/blame?path=a.txt").await;
    assert_eq!(status, StatusCode::OK);
    let ranges = json["ranges"].as_array().unwrap();
    assert_eq!(ranges[0]["lo"], 1);
    assert_eq!(ranges.last().unwrap()["hi"], 4);
    let known: Vec<String> = [c1, c2, c5].iter().map(Id::to_hex).collect();
    for r in ranges {
        assert!(known.contains(&r["cs"].as_str().unwrap().to_owned()));
    }
}

#[tokio::test]
async fn diff_between_two_revspecs() {
    let (repo, ids) = fixture();
    let app = gardenbed_web::router(repo);

    let (status, json) = get_json(
        &app,
        &format!("/api/diff?a={}&b={}", ids[0].to_hex(), ids[1].to_hex()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["a"], ids[0].to_hex());
    assert_eq!(json["b"], ids[1].to_hex());
    let changes = json["changes"].as_array().unwrap();
    assert!(
        changes
            .iter()
            .any(|c| c["path"] == "a.txt" && c["op"] == "modify")
    );
    assert!(
        changes
            .iter()
            .any(|c| c["path"] == "notes.txt" && c["op"] == "add")
    );
}

#[tokio::test]
async fn stats_returns_counters() {
    let (repo, ..) = fixture();
    let (status, json) = get_json(&gardenbed_web::router(repo), "/api/stats").await;
    assert_eq!(status, StatusCode::OK);
    let counters = json["counters"].as_object().unwrap();
    // In-process Repo calls never bump the RPC counters.
    assert!(counters.values().all(|v| v == 0));
}

#[tokio::test]
async fn server_info_reports_version_and_given_facts() {
    let (repo, ..) = fixture();
    let info = gardenbed_web::ServerInfo {
        root: Some("/srv/repo".into()),
        addr: Some("0.0.0.0:8430".into()),
    };
    let (status, json) = get_json(
        &gardenbed_web::router_with_info(repo, info),
        "/api/server-info",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(json["root"], "/srv/repo");
    assert_eq!(json["addr"], "0.0.0.0:8430");

    // The plain router exposes nulls for the unknown facts.
    let (repo, ..) = fixture();
    let (status, json) = get_json(&gardenbed_web::router(repo), "/api/server-info").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["root"], Value::Null);
    assert_eq!(json["addr"], Value::Null);
}

#[tokio::test]
async fn revspec_errors_map_to_statuses() {
    let (repo, ..) = fixture();
    let app = gardenbed_web::router(repo);

    // Unknown ref/prefix -> 404 with the resolver's message.
    let (status, json) = get_json(&app, "/api/log?cs=no-such-ref").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("no ref or changeset matching")
    );

    let (status, json) = get_json(&app, "/api/changeset?id=deadbeef").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("no ref or changeset matching")
    );

    // Too-short hex prefix -> 400.
    let (status, json) = get_json(&app, "/api/log?cs=abcd").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().contains("at least 8 hex"));

    // 64 chars that are not hex -> 400.
    let bad = "z".repeat(64);
    let (status, json) = get_json(&app, &format!("/api/log?cs={bad}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        json["error"].as_str().unwrap().contains("not valid hex")
            || json["error"]
                .as_str()
                .unwrap()
                .contains("invalid changeset id")
    );

    // Unknown id at endpoints beyond revspec resolution stays 404.
    let unknown = "1".repeat(64);
    let (status, _) = get_json(&app, &format!("/api/changeset?id={unknown}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn static_assets_are_embedded() {
    let (repo, ..) = fixture();
    let app = gardenbed_web::router(repo);

    let (status, ctype, body) = get(&app, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.starts_with("text/html"));
    assert!(String::from_utf8(body).unwrap().contains("id=\"app\""));

    let (status, ctype, _) = get(&app, "/static/app.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.starts_with("text/javascript"));

    let (status, ctype, _) = get(&app, "/static/styles.css").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ctype.starts_with("text/css"));
}

#[tokio::test]
async fn files_lists_all_paths_sorted_with_kinds() {
    let (repo, [_, _, _, _, tip]) = fixture();
    let app = gardenbed_web::router(repo);

    let (status, json) = get_json(&app, "/api/files").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["cs"], tip.to_hex());
    assert_eq!(json["truncated"], false);
    let paths = json["paths"].as_array().unwrap();
    let names: Vec<&str> = paths.iter().map(|p| p["path"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "a.txt",
            "big.txt",
            "bin.dat",
            "dir/moved.txt",
            "link",
            "run.sh"
        ]
    );
    // No directories leak into the listing; symlinks keep their kind.
    assert!(paths.iter().all(|p| p["kind"] != "directory"));
    let link = paths.iter().find(|p| p["path"] == "link").unwrap();
    assert_eq!(link["kind"], "symlink");

    // Substring filter on the full path, case-insensitive.
    for needle in ["moved", "MOVED"] {
        let (status, json) = get_json(&app, &format!("/api/files?q={needle}")).await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = json["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["path"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["dir/moved.txt"]);
    }

    // Unknown revspec keeps the shared 404 contract.
    let (status, _) = get_json(&app, "/api/files?cs=deadbeef").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A repo with 25 hitting lines in one file, a second hitting file, and
/// a gitlink — for the search caps.
fn fixture_many_hits() -> Repo {
    let repo = Repo::new(Arc::new(MemoryStore::new()), Arc::new(MemoryStore::new()));
    let mut inline = BTreeMap::new();
    let hits: String = (0..25).map(|i| format!("hit on line {i}\n")).collect();
    let hits_id = blob_id(hits.as_bytes());
    let other_id = blob_id(b"a hit around here\n");
    inline.insert(hits_id.to_hex(), hits.into_bytes());
    inline.insert(other_id.to_hex(), b"a hit around here\n".to_vec());
    // A gitlink's blob is an opaque upstream pointer, never stored.
    let pointer = Id::from_bytes([7u8; 32]);
    let mut changes = vec![
        Change {
            path: "hits.txt".to_owned(),
            op: Op::Add,
            blob: Some(hits_id),
            src_path: None,
            mode: Mode::Regular,
        },
        Change {
            path: "other.txt".to_owned(),
            op: Op::Add,
            blob: Some(other_id),
            src_path: None,
            mode: Mode::Regular,
        },
        Change {
            path: "sub".to_owned(),
            op: Op::Add,
            blob: Some(pointer),
            src_path: None,
            mode: Mode::Gitlink,
        },
    ];
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    let cs = Changeset {
        parents: vec![],
        changes,
        message: "search fixture".to_owned(),
        author: ident(9),
        committer: ident(9),
        signature: None,
    };
    let tip = repo.create_changeset(&cs, &inline).unwrap();
    repo.cas_ref("main", None, tip).unwrap();
    repo
}

#[tokio::test]
async fn search_finds_line_matches_and_skips_binary() {
    let (repo, [_, _, _, _, tip]) = fixture();
    let app = gardenbed_web::router(repo);

    // Known content, known line number; case-insensitive.
    for needle in ["line3", "LINE3"] {
        let (status, json) = get_json(&app, &format!("/api/search?q={needle}")).await;
        assert_eq!(status, StatusCode::OK, "q={needle}");
        assert_eq!(json["query"], needle);
        assert_eq!(json["cs"], tip.to_hex());
        let files = json["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], "a.txt");
        assert_eq!(files[0]["matches"][0]["line"], 3);
        assert_eq!(files[0]["matches"][0]["text"], "line3");
    }

    // Matched lines are trimmed and hard-capped at 240 chars.
    let (status, json) = get_json(&app, "/api/search?q=xxx").await;
    assert_eq!(status, StatusCode::OK);
    let big = json["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "big.txt")
        .expect("big.txt matches q=xxx");
    assert_eq!(big["matches"].as_array().unwrap().len(), 1);
    assert_eq!(big["matches"][0]["text"].as_str().unwrap().len(), 240);

    // The binary blob yields no file entry; the symlink is skipped even
    // though its target text contains the query substring.
    let (status, json) = get_json(&app, "/api/search?q=%01").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["files"].as_array().unwrap().len(), 0);
    // q=nested hits dir/moved.txt's content; q=dir would hit the
    // symlink's target text ("dir/moved.txt") if symlinks were searched.
    let (status, json) = get_json(&app, "/api/search?q=nested").await;
    assert_eq!(status, StatusCode::OK);
    let files = json["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["path"], "dir/moved.txt");
    assert_eq!(files[0]["matches"][0]["line"], 1);
    let (status, json) = get_json(&app, "/api/search?q=dir%2Fmoved").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["files"].as_array().unwrap().len(), 0);

    // q required.
    for uri in ["/api/search", "/api/search?q=", "/api/search?q=%20"] {
        let (status, json) = get_json(&app, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(json["error"], "empty query");
    }
}

#[tokio::test]
async fn search_honors_caps() {
    let app = gardenbed_web::router(fixture_many_hits());

    // Per-file cap: 25 hits in the blob, 20 reported, truncation flagged.
    let (status, json) = get_json(&app, "/api/search?q=hit").await;
    assert_eq!(status, StatusCode::OK);
    let files = json["files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0]["path"], "hits.txt");
    assert_eq!(files[0]["matches"].as_array().unwrap().len(), 20);
    assert_eq!(files[0]["matches"][0]["line"], 1);
    assert_eq!(files[0]["matches"][19]["line"], 20);
    assert_eq!(files[1]["path"], "other.txt");
    assert_eq!(files[1]["matches"].as_array().unwrap().len(), 1);
    assert_eq!(json["truncated"], true); // hits.txt had hits beyond the cap

    // Gitlink entries land in /api/files but are never searched.
    let (status, json) = get_json(&app, "/api/files").await;
    assert_eq!(status, StatusCode::OK);
    let sub = json["paths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["path"] == "sub")
        .expect("gitlink listed");
    assert_eq!(sub["kind"], "gitlink");

    // File cap honored: limit=1 returns a single file. ("around" hits
    // only other.txt; hits.txt would still be scanned but not reported
    // past the cap.)
    let (status, json) = get_json(&app, "/api/search?q=hit&limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["files"].as_array().unwrap().len(), 1);
    assert_eq!(json["files"][0]["path"], "hits.txt");
    assert_eq!(json["truncated"], true);

    let (status, json) = get_json(&app, "/api/search?q=around&limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["files"].as_array().unwrap().len(), 1);
    assert_eq!(json["files"][0]["path"], "other.txt");
}

#[tokio::test]
async fn activity_lists_newest_arrival_first_including_unpublished() {
    let (repo, [c1, c2, c3, c4, tip]) = fixture();

    // One unpublished working-copy commit on top of the tip: stored, but
    // no ref can reach it.
    let draft_content = b"wip: not for mainline\n";
    let draft_blob = blob_id(draft_content);
    let draft = Changeset {
        parents: vec![tip],
        changes: vec![Change {
            path: "draft.txt".to_owned(),
            op: Op::Add,
            blob: Some(draft_blob),
            src_path: None,
            mode: Mode::Regular,
        }],
        message: "wip: unpublished draft".to_owned(),
        author: ident(6),
        committer: ident(6),
        signature: None,
    };
    let draft_id = repo
        .create_changeset(
            &draft,
            &BTreeMap::from([(draft_blob.to_hex(), draft_content.to_vec())]),
        )
        .unwrap();

    let app = gardenbed_web::router(repo);
    let (status, json) = get_json(&app, "/api/activity").await;
    assert_eq!(status, StatusCode::OK);
    let changesets = json["changesets"].as_array().unwrap();
    let got: Vec<String> = changesets
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect();
    let expected: Vec<String> = [draft_id, tip, c4, c3, c2, c1]
        .iter()
        .map(Id::to_hex)
        .collect();
    assert_eq!(got, expected, "arrival order, newest first");
    // Entry shape matches /api/log.
    assert_eq!(changesets[0]["message"], "wip: unpublished draft");
    assert_eq!(changesets[0]["author"]["name"], "Tester 6");
    assert_eq!(changesets[0]["changes"], 1);
    assert_eq!(changesets[0]["parents"].as_array().unwrap().len(), 1);

    // limit honored.
    let (status, json) = get_json(&app, "/api/activity?limit=2").await;
    assert_eq!(status, StatusCode::OK);
    let got: Vec<&str> = json["changesets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(got, [draft_id.to_hex(), tip.to_hex()]);

    // Over the cap behaves as the cap (200), not as an error.
    let (status, json) = get_json(&app, "/api/activity?limit=999").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["changesets"].as_array().unwrap().len(), 6);

    let (status, json) = get_json(&app, "/api/activity?limit=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().contains("limit"));
}

#[test]
fn seed_is_deterministic_and_hides_drafts_from_main() {
    let repo_a = Repo::new(Arc::new(MemoryStore::new()), Arc::new(MemoryStore::new()));
    let repo_b = Repo::new(Arc::new(MemoryStore::new()), Arc::new(MemoryStore::new()));
    let tip_a = gardenbed_web::seed::seed(&repo_a).unwrap().expect("seeded");
    let tip_b = gardenbed_web::seed::seed(&repo_b).unwrap().expect("seeded");
    assert_eq!(tip_a, tip_b, "two fresh seeds must be byte-identical");
    assert_eq!(repo_b.get_ref("main").unwrap(), Some(tip_a));
    // Second run against a reffed repo is a no-op.
    assert_eq!(gardenbed_web::seed::seed(&repo_b).unwrap(), None);

    // 17 changesets stored; 12 reachable from main, 5 unpublished. The
    // five newest arrivals are the drafts, newest first (17 stress, 16
    // rebase v2, 15 v1, 14 colors draft, 13 drip draft).
    let activity = repo_a.activity(0).unwrap();
    assert_eq!(activity.len(), 17);
    let published: Vec<Id> = repo_a
        .log(tip_a, 0)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(published.len(), 12);
    let unpublished: Vec<&(Id, Changeset)> = activity
        .iter()
        .filter(|(id, _)| !published.contains(id))
        .collect();
    assert_eq!(unpublished.len(), 5);
    assert_eq!(
        unpublished[0].1.message, "Stress fixture: rewrite garden.js wholesale",
        "newest arrival: the stress draft"
    );
    assert_eq!(unpublished[0].1.parents, vec![tip_a]);
    assert_eq!(unpublished[0].1.author.name, "Moss Hale");
    // The stress file must stay under the /api/file payload cap so the
    // GUI fetches the full body and the diff LINE budget is what trips.
    let stress_change = &unpublished[0].1.changes[0];
    assert_eq!(stress_change.path, "garden.js");
    let stress_blob = repo_a
        .get_blob(stress_change.blob.expect("modify carries a blob"))
        .unwrap()
        .expect("stress blob stored");
    let stress_text = String::from_utf8(stress_blob).unwrap();
    assert_eq!(stress_text.lines().count(), 6000);
    assert!(
        stress_text.len() < 512 * 1024,
        "must fit the file payload cap, got {} bytes",
        stress_text.len()
    );
    assert_eq!(
        unpublished[1].1.message, "Add evening mulch reminder",
        "next newest: rebase v2"
    );
    assert_eq!(unpublished[1].1.parents, vec![tip_a]);
    assert_eq!(unpublished[2].1.message, "Add evening mulch reminder"); // v1
    let c11 = published
        .iter()
        .find(|id| repo_a.get_changeset(**id).unwrap().message == "Add rotation notes for 2027")
        .copied()
        .expect("c11 reachable from main");
    assert_eq!(unpublished[2].1.parents, vec![c11]); // v1 sat on c11
    assert_ne!(
        unpublished[1].0, unpublished[2].0,
        "rebase pair must have distinct ids"
    );
    assert_eq!(unpublished[3].1.message, "draft: calmer empty-bed colors");
    assert_eq!(unpublished[3].1.author.name, "Moss Hale");
    assert_eq!(
        unpublished[4].1.message,
        "Draft: drip irrigation sketch\n\nNot ready: zones still hardcoded.\n"
    );
    assert_eq!(unpublished[4].1.author.name, "Pike Rowan");
    assert_eq!(unpublished[4].1.parents, vec![tip_a]);
    // Timestamps keep increasing across all seventeen commits.
    for (i, (_, cs)) in activity.iter().enumerate() {
        let n = (17 - i) as u64;
        assert_eq!(cs.committer.time_secs, 1_751_500_000 + n * 86_400);
    }
}

/// Reference base64 for the fixture's binary blob (kept local so the
/// assertion pins the server's encoding against an independent
/// implementation).
fn base64_of(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | (*chunk.get(2).unwrap_or(&0) as u32);
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}
