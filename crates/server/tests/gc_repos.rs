//! M13 V5 (v0.2.md §M13): the gc report's numbers must equal an
//! independent walk of the object shards on real repositories. Three
//! distinct fixtures — two generated git histories imported through the
//! real `import` pipeline, one in-process generated history — each seeded
//! with a known orphan population; the report's scanned/live/deleted
//! counts must match a from-scratch directory census, and a subsequent
//! gc must find nothing further.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gardenbed_harness::{GenOptions, generate};
use gardenbed_model::{Id, Mode, blob_id};
use gardenbed_server::Repo;

/// Backdate distance.
const OLD_SECS: u64 = 100_000;

/// Runs `gardenbed-server gc`; returns (exit code, stdout).
fn run_gc(root: &Path, args: &[&str]) -> (i32, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
        .arg("gc")
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("gc subprocess runs");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Parses the report line into its counts:
/// (scanned, live, blobs_deleted, trees_deleted, tmp_reaped, bytes).
fn parse_report(out: &str) -> (u64, u64, u64, u64, u64, u64) {
    let nums: Vec<u64> = out
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .collect();
    // gc: S scanned, L live, B blob(s) and T tree(s) deleted, R tmp
    // file(s) reaped, X byte(s) reclaimed [timings]
    assert!(nums.len() >= 6, "report line parses: {out}");
    (nums[0], nums[1], nums[2], nums[3], nums[4], nums[5])
}

/// Independent shard census: (object count, total bytes, tmp count,
/// tmp bytes, tree-shaped count by first byte, blob count).
fn census(root: &Path) -> (u64, u64, u64, u64, u64, u64) {
    let objects = root.join("objects");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&objects).expect("objects dir reads") {
        let shard = entry.expect("shard entry").path();
        if !shard.is_dir() {
            continue;
        }
        for f in std::fs::read_dir(&shard).expect("shard reads") {
            files.push(f.expect("file entry").path());
        }
    }
    let (mut n, mut bytes, mut tmp_n, mut tmp_bytes, mut trees, mut blobs) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    for path in files {
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let meta = std::fs::metadata(&path).expect("metadata reads");
        if name.starts_with(".tmp-") {
            tmp_n += 1;
            tmp_bytes += meta.len();
            continue;
        }
        n += 1;
        bytes += meta.len();
        let mut buf = [0u8; 1];
        use std::io::Read as _;
        std::fs::File::open(&path)
            .expect("open")
            .read_exact(&mut buf)
            .expect("first byte");
        if buf[0] == 0x02 {
            trees += 1;
        } else {
            blobs += 1;
        }
    }
    (n, bytes, tmp_n, tmp_bytes, trees, blobs)
}

/// Backdates a path's mtime.
fn backdate(path: &Path) {
    let old = std::time::SystemTime::now()
        .checked_sub(Duration::from_secs(OLD_SECS))
        .expect("clock");
    let f = std::fs::File::open(path).expect("backdate target opens");
    f.set_modified(old).expect("mtime set");
}

/// Builds a git repo with `commits` deterministic commits, imports it
/// into `root` through the real `gardenbed-server import` binary, and
/// plants `orphans` unreferenced blobs + one tmp file — all backdated.
/// Returns (object ids of orphans, tmp path).
fn import_fixture(tag: &str, root: &Path, commits: u32, orphans: usize) -> (Vec<Id>, PathBuf) {
    // 1. The git source: deterministic commits, one file each plus a
    //    stable README rewrite for real tree variety.
    let git_dir = root.parent().unwrap().join(format!("{tag}.git"));
    let work = root.parent().unwrap().join(format!("{tag}.work"));
    let _ = std::fs::remove_dir_all(&git_dir);
    let _ = std::fs::remove_dir_all(&work);
    run(&["git", "init", "-q", &work.to_string_lossy()]);
    run(&[
        "git",
        "-C",
        &work.to_string_lossy(),
        "config",
        "user.email",
        "v5@bed.example",
    ]);
    run(&[
        "git",
        "-C",
        &work.to_string_lossy(),
        "config",
        "user.name",
        "V5",
    ]);
    run(&["git", "init", "-q", "--bare", &git_dir.to_string_lossy()]);
    for i in 0..commits {
        let readme = work.join("README.md");
        std::fs::write(
            &readme,
            format!(
                "# {tag}\n\ncommit {i} of {commits}\n{}",
                "line\n".repeat(i as usize + 1)
            ),
        )
        .expect("readme writes");
        let file = work.join(format!("src/file-{i:03}.txt"));
        std::fs::create_dir_all(file.parent().expect("dir")).expect("dir");
        std::fs::write(&file, format!("{tag} content {i}\n")).expect("file writes");
        run(&["git", "-C", &work.to_string_lossy(), "add", "-A"]);
        run(&[
            "git",
            "-C",
            &work.to_string_lossy(),
            "commit",
            "-qm",
            &format!("{tag} commit {i}"),
        ]);
    }
    run(&[
        "git",
        "-C",
        &work.to_string_lossy(),
        "push",
        "-q",
        &git_dir.to_string_lossy(),
        "HEAD:main",
    ]);

    // 2. Import through the real binary, into a named repository under
    //    the multi-repo root (v0.2 D12): the gc target is `<root>/<tag>`,
    //    the same directory the import wrote.
    let repo_dir = root.join(tag);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
        .arg("import")
        .arg("--from")
        .arg(&git_dir)
        .arg("--root")
        .arg(root)
        .arg("--repo")
        .arg(tag)
        .output()
        .expect("import runs");
    assert!(
        out.status.success(),
        "import succeeds: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 3. Orphans: real blobs no one references, backdated. Plus one tmp.
    let repo = Repo::open(&repo_dir).expect("repo opens");
    let mut ids = Vec::new();
    for i in 0..orphans {
        let content = format!("{tag} orphan {i}").into_bytes();
        let id = repo.put_blob(&content).expect("orphan uploads");
        let hex = id.to_hex();
        backdate(&repo_dir.join("objects").join(&hex[..2]).join(&hex[2..]));
        ids.push(id);
    }
    let hex = blob_id(format!("{tag}-tmp").as_bytes()).to_hex();
    let tmp_dir = repo_dir.join("objects").join(&hex[..2]);
    std::fs::create_dir_all(&tmp_dir).expect("tmp shard dir");
    let tmp = tmp_dir.join(".tmp-777-1");
    std::fs::write(&tmp, b"stale v5 staging").expect("tmp writes");
    backdate(&tmp);
    (ids, tmp)
}

fn run(args: &[&str]) {
    let out = std::process::Command::new(args[0])
        .args(&args[1..])
        .output()
        .expect("command runs");
    assert!(
        out.status.success(),
        "{} {} failed: {}",
        args[0],
        args[1..].join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An in-process generated-history fixture (the third repository shape).
fn generated_fixture(root: &Path, orphans: usize) -> Vec<Id> {
    let repo = Repo::open(root).expect("repo opens");
    let history = generate(
        0x7E11,
        &GenOptions {
            commits: 50,
            merge_ratio: 0.2,
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
        let mut inline = std::collections::BTreeMap::new();
        for change in &cs.changes {
            let Some(blob) = change.blob else { continue };
            if change.mode == Mode::Gitlink || is_gitlink(&blob) {
                continue;
            }
            inline.insert(blob.to_hex(), history.blob(&blob).expect("blob").to_vec());
        }
        repo.create_changeset(cs, &inline).expect("history ingests");
    }
    repo.cas_ref("main", None, history.head)
        .expect("main ref set");

    let mut ids = Vec::new();
    for i in 0..orphans {
        let content = format!("generated orphan {i}").into_bytes();
        let id = repo.put_blob(&content).expect("orphan uploads");
        let hex = id.to_hex();
        backdate(&root.join("objects").join(&hex[..2]).join(&hex[2..]));
        ids.push(id);
    }
    ids
}

/// Cross-checks one repository: dry-run report == census-derived
/// expectations, live gc matches dry-run, second gc deletes zero.
fn cross_check(tag: &str, root: &Path, orphan_count: u64) {
    let (objs, _bytes, tmp_n, _tmp_bytes, _trees, _blobs) = census(root);
    assert!(objs > 10, "{tag}: fixture is non-trivial ({objs} objects)");

    // Dry run: scanned == census objects; live == census - orphans;
    // deletions == orphans; tmp == census tmp count.
    let (code, out) = run_gc(root, &["--dry-run", "--min-age", "0"]);
    assert_eq!(code, 0, "{tag}: dry-run exits 0: {out}");
    let (scanned, live, bdel, tdel, treap, _reclaimed) = parse_report(&out);
    assert_eq!(scanned, objs, "{tag}: scanned == census objects");
    assert_eq!(live, objs - orphan_count, "{tag}: live == census - orphans");
    assert_eq!(bdel + tdel, orphan_count, "{tag}: deletions == orphans");
    assert_eq!(treap, tmp_n, "{tag}: tmp reaped == census tmp count");

    // Live run matches the dry run exactly.
    let (code, out2) = run_gc(root, &["--min-age", "0"]);
    assert_eq!(code, 0, "{tag}: gc exits 0: {out2}");
    assert_eq!(
        parse_report(&out),
        parse_report(&out2),
        "{tag}: dry-run and live-run reports agree"
    );

    // Second gc: zero, zero.
    let (code, out3) = run_gc(root, &["--min-age", "0"]);
    assert_eq!(code, 0, "{tag}: second gc exits 0: {out3}");
    let (_, _, bdel3, tdel3, treap3, _) = parse_report(&out3);
    assert_eq!(bdel3 + tdel3 + treap3, 0, "{tag}: second gc deletes zero");
}

/// V5: report accuracy vs an independent shard walk on three distinct
/// repositories (two real git imports, one generated history).
#[test]
fn v5_report_matches_independent_shard_walk() {
    let base = tempfile::tempdir().expect("tempdir");

    // Repo 1: git import, 40 commits, 7 orphans, 1 tmp. The import
    // lands in the multi-repo layout `<root>/v5a`; gc and census both
    // target that repository directory.
    let r1 = base.path().join("repo1");
    std::fs::create_dir_all(&r1).expect("repo1 dir");
    import_fixture("v5a", &r1, 40, 7);
    cross_check("repo1 (git import 40)", &r1.join("v5a"), 7);

    // Repo 2: git import with a different shape, 12 orphans, 1 tmp.
    let r2 = base.path().join("repo2");
    std::fs::create_dir_all(&r2).expect("repo2 dir");
    import_fixture("v5b", &r2, 25, 12);
    cross_check("repo2 (git import 25)", &r2.join("v5b"), 12);

    // Repo 3: in-process generated history (merge-dense), 5 orphans.
    let r3 = base.path().join("repo3");
    std::fs::create_dir_all(&r3).expect("repo3 dir");
    generated_fixture(&r3, 5);
    cross_check("repo3 (generated history)", &r3, 5);
}
