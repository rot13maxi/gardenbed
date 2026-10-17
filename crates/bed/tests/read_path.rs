//! The M4 read path, end to end against a real server: byte fidelity
//! (incl. symlinks, exec bits, submodule pointers), `log`/`diff` parity
//! with git as the oracle, sparse sparses, switch, idempotent reruns, and the
//! offline failure contract (validate + connect before any local write).
//!
//! Each test spawns the real `gardenbed-server` binary and ingests a
//! seeded synthetic history through the server library (fixture setup
//! never touches the request counters the tests assert on).

mod common;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use gardenbed::render;
use gardenbed::treestate::{MODE_EXEC, MODE_GITLINK, MODE_REGULAR, MODE_SYMLINK};
use gardenbed_harness::generator::{GenOptions, GeneratedHistory, files_at, generate};
use gardenbed_harness::materialize::materialize;
use gardenbed_harness::oracle;
use gardenbed_model::{Id, Mode};
use tempfile::TempDir;

use common::{Server, bed, bed_raw};

/// One running server with an ingested synthetic history, a git
/// materialization of the same history (the oracle), and a garden the
/// tests plant beds in.
struct Garden {
    /// Temp root holding the server, the git oracle, and the repository.
    _dir: TempDir,
    /// The garden: bed commands run here; working files live here.
    garden: PathBuf,
    server: Server,
    history: GeneratedHistory,
    /// `(changeset id, git sha)` aligned with `history.changesets`.
    mapping: Vec<(Id, String)>,
    /// The git oracle repository.
    git: PathBuf,
}

impl Garden {
    fn state_dir(&self) -> PathBuf {
        self.garden.join(".bed")
    }
    fn cs_id(&self, index: usize) -> Id {
        self.mapping[index].0
    }
    fn cs_hex(&self, index: usize) -> String {
        self.mapping[index].0.to_hex()
    }
    fn sha(&self, index: usize) -> String {
        self.mapping[index].1.clone()
    }
    fn head_index(&self) -> usize {
        self.history.changesets.len() - 1
    }
}

/// Generates a history that contains at least one file of each interesting
/// mode (exec, symlink, gitlink) so the fidelity checks always have
/// material to work with. Deterministic: the first seed in `1..` that
/// qualifies.
fn qualifying_history() -> GeneratedHistory {
    let opts = GenOptions {
        commits: 30,
        ..GenOptions::default()
    };
    for seed in 1..100u64 {
        let history = generate(seed, &opts);
        let files = files_at(&history, history.changesets.len() - 1);
        let has = |want: Mode| files.values().any(|(_, m)| *m == want);
        if has(Mode::Exec) && has(Mode::Symlink) && has(Mode::Gitlink) {
            return history;
        }
    }
    panic!("no seed in 1..100 produced an exec + symlink + gitlink history");
}

/// Boots the whole world for one test.
async fn setup() -> Garden {
    setup_with(qualifying_history()).await
}

async fn setup_with(history: GeneratedHistory) -> Garden {
    let dir = tempfile::tempdir().expect("tempdir");
    let server_root = dir.path().join("server");
    let git_repo = dir.path().join("git-oracle");
    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir created");
    let head = common::ingest(&history, &server_root);
    let materialized = materialize(&history, &git_repo).expect("git oracle materializes");
    assert_eq!(materialized.mapping.last().map(|(id, _)| *id), Some(head));
    let server = Server::spawn(&server_root).await;
    Garden {
        _dir: dir,
        garden,
        server,
        history,
        mapping: materialized.mapping,
        git: git_repo,
    }
}

/// The working-copy files the sparse selection should hold at `index`, as
/// `path → (blob content, mode)`.
fn expected_files(history: &GeneratedHistory, index: usize) -> BTreeMap<String, (Vec<u8>, Mode)> {
    files_at(history, index)
        .into_iter()
        .map(|(path, (id, mode))| {
            let content = history
                .blob(&id)
                .expect("history carries its blobs")
                .to_vec();
            (path, (content, mode))
        })
        .collect()
}

/// A sparse prefix guaranteed to own at least one file at `index`.
fn some_prefix(history: &GeneratedHistory, index: usize) -> String {
    for path in files_at(history, index).keys() {
        if let Some((dir, _)) = path.rsplit_once('/') {
            return dir.split('/').next().unwrap().to_owned();
        }
    }
    panic!("history has no files under a directory");
}

/// Relative paths of all entries (files, symlinks) under `root`, excluding
/// the working copy itself (`.bed` is pruned wherever it appears).
fn files_under(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().unwrap().is_dir() {
                if name == ".bed" {
                    continue;
                }
                let path = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                walk(&entry.path(), &path, out);
            } else {
                let path = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, "", &mut out);
    out.sort();
    out
}

// ------------------------------------------------------------- clone

#[tokio::test]
async fn clone_plants_layout_and_materializes_nothing() {
    let mut g = setup().await;
    let (code, out, stderr) = bed(&g.garden, &["clone", &g.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        out.contains("created working copy for default"),
        "out: {out}"
    );
    assert!(out.contains("no files materialized"), "out: {out}");

    // The full layout exists; current changeset: main's changeset.
    let state_dir = g.state_dir();
    assert!(state_dir.join("config.toml").is_file());
    assert!(state_dir.join("index.sqlite3").is_file());
    assert!(state_dir.join("objects").is_dir());
    assert!(state_dir.join("heads").is_file());

    let head = g.cs_hex(g.head_index());
    let config = fs::read_to_string(state_dir.join("config.toml")).unwrap();
    assert!(
        config.contains(&format!("current_cs = \"{head}\"")),
        "config: {config}"
    );
    assert!(
        config.contains(&format!("server = \"{}\"", g.server.http())),
        "config: {config}"
    );

    let heads = fs::read_to_string(state_dir.join("heads")).unwrap();
    assert_eq!(heads.trim(), head, "heads records the tip");

    // Nothing materialized, no sparse, and status says so.
    assert!(
        files_under(&g.garden).is_empty(),
        "files: {:?}",
        files_under(&g.garden)
    );
    let (code, out, _) = bed(&g.garden, &["status"]);
    assert_eq!(code, 0);
    assert!(out.contains("no sparse paths selected"), "out: {out}");

    g.server.shutdown().await;
}

#[tokio::test]
async fn clone_with_a_refless_server_leaves_zero_state() {
    // A server over a fresh root: no repo, no refs.
    let dir = tempfile::tempdir().unwrap();
    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).unwrap();
    let mut server = Server::spawn(&dir.path().join("server")).await;

    let (code, _, stderr) = bed(&garden, &["clone", &server.http()]);
    assert_eq!(code, 1);
    // The no-flag resolution names what it probed and what exists (here:
    // nothing), pointing at --ref.
    assert!(
        stderr.contains("no ref main (tried main, master, trunk); pass --ref\n"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("the server has refs"), "stderr: {stderr}");
    assert!(!garden.join(".bed").exists(), "no bed may be planted");

    // An unknown repo is the same story.
    let (code, _, stderr) = bed(&garden, &["clone", &server.http(), "--repo", "orchard"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("no ref main (tried main, master, trunk); pass --ref\n"),
        "stderr: {stderr}"
    );
    assert!(!garden.join(".bed").exists(), "no bed may be planted");

    server.shutdown().await;
}

// ------------------------------------------------- byte fidelity (M4.1)

/// Verifies the working copy exactly matches the changeset at `index`:
/// bytes, modes, symlink targets, gitlink absence, treestate rows, and a
/// clean status (hinted and full-scan).
fn assert_fidelity(g: &Garden, index: usize) {
    let expected = expected_files(&g.history, index);
    let ids = files_at(&g.history, index);
    let ts = gardenbed::workspace::WorkingCopy::open(&g.state_dir())
        .unwrap()
        .treestate()
        .unwrap();

    for (path, (content, mode)) in &expected {
        let abs = g.garden.join(path);
        let mode_code = match mode {
            Mode::Regular => MODE_REGULAR,
            Mode::Exec => MODE_EXEC,
            Mode::Symlink => MODE_SYMLINK,
            Mode::Gitlink => MODE_GITLINK,
        };
        match mode {
            Mode::Gitlink => {
                // Submodule pointers are never materialized as files; the
                // treestate row carries the upstream pointer.
                assert!(
                    fs::symlink_metadata(&abs).is_err(),
                    "gitlink {path} must not exist on disk"
                );
            }
            Mode::Symlink => {
                let target =
                    fs::read_link(&abs).unwrap_or_else(|e| panic!("symlink {path} readable: {e}"));
                assert_eq!(
                    target.as_os_str().as_bytes(),
                    content.as_slice(),
                    "symlink {path} target bytes"
                );
            }
            _ => {
                let bytes = fs::read(&abs).unwrap_or_else(|e| panic!("file {path} readable: {e}"));
                assert_eq!(&bytes, content, "file {path} bytes");
                let mode_bits = fs::metadata(&abs).unwrap().permissions().mode();
                if *mode == Mode::Exec {
                    assert_ne!(mode_bits & 0o111, 0, "exec bit on {path}");
                } else {
                    assert_eq!(mode_bits & 0o111, 0, "no exec bit on {path}");
                }
            }
        }

        // The treestate row records exactly what was initialized.
        let row = ts
            .lookup(path)
            .expect("treestate lookup works")
            .unwrap_or_else(|| panic!("row for {path}"));
        assert_eq!(
            row.blob_id,
            ids[path.as_str()].0,
            "treestate blob id for {path}"
        );
        assert_eq!(row.mode, mode_code, "treestate mode for {path}");
    }

    // Nothing extra on disk (gitlink pointers are recorded in the
    // treestate but never materialized), nothing extra in the treestate.
    let tracked: Vec<String> = expected
        .iter()
        .filter(|(_, (_, mode))| *mode != Mode::Gitlink)
        .map(|(path, _)| path.clone())
        .collect();
    let on_disk = files_under(&g.garden);
    assert_eq!(on_disk, tracked, "the repository holds exactly the sparse");

    assert_eq!(ts.count().unwrap() as usize, expected.len(), "row count");

    let (code, out, stderr) = bed(&g.garden, &["status"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains("working copy is clean"), "out: {out}");
    let (code, out, _) = bed(&g.garden, &["status", "--full-scan"]);
    assert_eq!(code, 0);
    assert!(out.contains("working copy is clean"), "out: {out}");
}

#[tokio::test]
async fn sparse_add_dot_materializes_everything_faithfully() {
    let mut g = setup().await;
    let (code, _, stderr) = bed(&g.garden, &["clone", &g.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let (code, out, stderr) = bed(&g.garden, &["sparse", "add", "."]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains("selected ."), "out: {out}");
    assert!(out.contains("materialized"), "out: {out}");

    assert_fidelity(&g, g.head_index());

    // `bed file` agrees with git show byte-for-byte for every
    // non-gitlink path at the head changeset.
    let head_sha = g.sha(g.head_index());
    for (path, (_, mode)) in files_at(&g.history, g.head_index()) {
        if mode == Mode::Gitlink {
            continue;
        }
        let (code, out, stderr) = bed_raw(&g.garden, &["file", &path]);
        assert_eq!(code, 0, "file {path}: {}", String::from_utf8_lossy(&stderr));
        let expected = oracle::git_show(&g.git, &head_sha, &path).unwrap();
        assert_eq!(out, expected, "file {path} content");
    }

    g.server.shutdown().await;
}

#[tokio::test]
async fn sparse_sparse_materializes_only_its_prefix() {
    let mut g = setup().await;
    let (code, _, stderr) = bed(&g.garden, &["clone", &g.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let prefix = some_prefix(&g.history, g.head_index());
    let (code, out, stderr) = bed(&g.garden, &["sparse", "add", &prefix]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains(&format!("selected {prefix}")), "out: {out}");

    // Only the prefix's files are on disk; status is clean.
    let expected = expected_files(&g.history, g.head_index());
    let in_prefix: Vec<String> = expected
        .iter()
        .filter(|(p, (_, mode))| p.starts_with(&format!("{prefix}/")) && *mode != Mode::Gitlink)
        .map(|(p, _)| p.clone())
        .collect();
    let on_disk = files_under(&g.garden);
    assert_eq!(on_disk, in_prefix, "only the prefix is materialized");

    let (code, out, _) = bed(&g.garden, &["status"]);
    assert_eq!(code, 0);
    assert!(out.contains("working copy is clean"), "out: {out}");

    // A prefix that names nothing in the tree is allowed to plant ahead:
    // recorded, materialized as nothing, still clean.
    let (code, out, _) = bed(&g.garden, &["sparse", "add", "future/rows"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("selected future/rows"), "out: {out}");
    let (code, out, _) = bed(&g.garden, &["status"]);
    assert_eq!(code, 0);
    assert!(out.contains("working copy is clean"), "out: {out}");

    g.server.shutdown().await;
}

// ------------------------------------------------------------ file

#[tokio::test]
async fn file_serves_bytes_symlinks_and_refuses_gitlinks() {
    let mut g = setup().await;
    let (code, _, stderr) = bed(&g.garden, &["clone", &g.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");

    let files = files_at(&g.history, g.head_index());
    let pick = |want: Mode| {
        files
            .iter()
            .find(|(_, (_, m))| *m == want)
            .map(|(p, _)| p.clone())
            .expect("fixture has such a file")
    };
    let gitlink_path = pick(Mode::Gitlink);
    let symlink_path = pick(Mode::Symlink);
    let (plain_path, plain_id) = files
        .iter()
        .find(|(p, (_, m))| {
            !p.contains('/') && *m != Mode::Gitlink && **p != gitlink_path && **p != symlink_path
        })
        .map(|(p, (id, _))| (p.clone(), *id))
        .expect("fixture has a root-level regular file");

    // A plain file: exact bytes.
    let (code, out, stderr) = bed_raw(&g.garden, &["file", &plain_path]);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
    assert_eq!(out, g.history.blob(&plain_id).unwrap().to_vec());

    // A symlink prints its target bytes, not the target's content.
    let (code, out, stderr) = bed_raw(&g.garden, &["file", &symlink_path]);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&stderr));
    let target = g.history.blob(&files[&symlink_path].0).unwrap().to_vec();
    assert_eq!(out, target);

    // A submodule pointer has no content of its own.
    let (code, _, stderr) = bed(&g.garden, &["file", &gitlink_path]);
    assert_eq!(code, 1);
    assert!(stderr.contains("submodule pointer"), "stderr: {stderr}");

    // A directory path is not a file; neither is a missing path.
    let dir_prefix = gitlink_path.rsplit_once('/').map(|(d, _)| d).unwrap_or(".");
    let (code, _, stderr) = bed(&g.garden, &["file", dir_prefix]);
    if !dir_prefix.contains('/') {
        // The gitlink sits at the root; use a known dir instead.
        let (code, _, stderr) = bed(
            &g.garden,
            &["file", &some_prefix(&g.history, g.head_index())],
        );
        assert_eq!(code, 1);
        assert!(stderr.contains("no file at"), "stderr: {stderr}");
    } else {
        assert_eq!(code, 1);
        assert!(stderr.contains("no file at"), "stderr: {stderr}");
    }

    // --cs resolves refs and ids alike.
    for cs in ["main", g.cs_hex(g.head_index()).as_str()] {
        let (code, _, stderr) = bed(&g.garden, &["file", &plain_path, "--cs", cs]);
        assert_eq!(code, 0, "cs {cs}: {stderr}");
    }
    let (code, _, stderr) = bed(&g.garden, &["file", "no/such/file.txt", "--cs", "main"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no file at"), "stderr: {stderr}");

    // A changeset id that is not on the server is refused cleanly.
    let (code, _, stderr) = bed(&g.garden, &["file", &plain_path, "--cs", &"ab".repeat(32)]);
    assert_eq!(code, 1);
    assert!(stderr.contains("is not on the server"), "stderr: {stderr}");

    g.server.shutdown().await;
}

// ------------------------------------------------------------ log (M4.2)

/// Builds the exact `bed log` output git's log implies: the server's
/// date-ordered walk should reproduce git's order, identities and
/// messages, and working copy's renderer (unit-tested) shapes them.
fn expected_log(g: &Garden, rev_index: usize, limit: usize) -> String {
    let sha_to_index: HashMap<&str, usize> = g
        .mapping
        .iter()
        .enumerate()
        .map(|(i, (_, sha))| (sha.as_str(), i))
        .collect();
    let entries = oracle::git_log(&g.git, g.sha(rev_index).as_str(), limit).expect("git log");
    let mut out = String::new();
    for (i, ge) in entries.iter().enumerate() {
        let cs_index = sha_to_index[ge.sha.as_str()];
        let cs = &g.history.changesets[cs_index];
        let entry = render::LogEntry {
            id: g.mapping[cs_index].0,
            author_name: cs.author.name.clone(),
            author_email: cs.author.email.clone(),
            time_secs: cs.author.time_secs,
            tz_min: cs.author.tz_min,
            message: cs.message.clone(),
        };
        if i > 0 {
            out.push('\n');
        }
        render::render_log_entry(&entry, &mut out);
    }
    out
}

#[tokio::test]
async fn log_matches_git_on_two_histories() {
    let opts = GenOptions {
        commits: 25,
        ..GenOptions::default()
    };
    for seed in [11u64, 12] {
        let mut g = setup_with(generate(seed, &opts)).await;
        let (code, _, stderr) = bed(&g.garden, &["clone", &g.server.http()]);
        assert_eq!(code, 0, "seed {seed}: {stderr}");

        // Whole history, newest first, in git's order.
        let (code, out, stderr) = bed(&g.garden, &["log"]);
        assert_eq!(code, 0, "seed {seed}: {stderr}");
        assert_eq!(out, expected_log(&g, g.head_index(), 10_000), "seed {seed}");

        // --limit matches git --max-count.
        let (code, out, _) = bed(&g.garden, &["log", "--limit", "3"]);
        assert_eq!(code, 0);
        assert_eq!(out, expected_log(&g, g.head_index(), 3), "seed {seed}");

        // --cs at an older changeset walks from there.
        let older = 7;
        let (code, out, _) = bed(&g.garden, &["log", "--cs", &g.cs_hex(older)]);
        assert_eq!(code, 0);
        assert_eq!(out, expected_log(&g, older, 10_000), "seed {seed}");

        g.server.shutdown().await;
    }
}

// ----------------------------------------------------------- diff (M4.2)

/// Parses working copy's rendered diff into git `--name-status` shape: `(letter,
/// path)`. The letter is derived from the header lines: `new file mode` →
/// A, `deleted file mode` → D, otherwise old/new mode lines that change
/// the entry *type* → T, else M.
fn parse_bed_diff(out: &str) -> Vec<(char, String)> {
    let type_class = |mode: &str| match mode {
        "120000" => 's',
        "160000" => 'g',
        _ => 'f',
    };
    let mut entries: Vec<(char, String)> = Vec::new();
    let mut path = String::new();
    let mut old_mode: Option<String> = None;
    let mut new_mode: Option<String> = None;
    let flush = |entries: &mut Vec<(char, String)>,
                 path: &mut String,
                 old_mode: &mut Option<String>,
                 new_mode: &mut Option<String>| {
        if path.is_empty() {
            return;
        }
        let letter = match (old_mode.take(), new_mode.take()) {
            (_, Some(m)) if m.starts_with("new file") => 'A',
            (Some(m), _) if m.starts_with("deleted file") => 'D',
            (Some(o), Some(n)) if type_class(&o) != type_class(&n) => 'T',
            _ => 'M',
        };
        entries.push((letter, std::mem::take(path)));
    };
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            flush(&mut entries, &mut path, &mut old_mode, &mut new_mode);
            let (p, _) = rest
                .split_once(" b/")
                .unwrap_or_else(|| panic!("diff header without b/ side: {line:?}"));
            path = p.to_owned();
        } else if let Some(rest) = line.strip_prefix("old mode ") {
            old_mode = Some(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("new mode ") {
            new_mode = Some(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("new file mode ") {
            new_mode = Some(format!("new file {rest}"));
        } else if let Some(rest) = line.strip_prefix("deleted file mode ") {
            old_mode = Some(format!("deleted file {rest}"));
        }
    }
    flush(&mut entries, &mut path, &mut old_mode, &mut new_mode);
    entries
}

/// `git diff-tree -r -z --name-status --no-renames A B` — the path/op
/// oracle for arbitrary pairs.
fn git_name_status(git_repo: &Path, a: &str, b: &str) -> Vec<(char, String)> {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "core.quotePath=false",
            "diff-tree",
            "--root",
            "-r",
            "-z",
            "--name-status",
            "--no-commit-id",
            "--no-renames",
            "--diff-merges=first-parent",
            a,
            b,
        ])
        .current_dir(git_repo)
        .output()
        .expect("git diff-tree runs")
        .stdout;
    let mut entries = Vec::new();
    let mut tokens = out.split(|b| *b == 0).filter(|t| !t.is_empty());
    while let Some(status) = tokens.next() {
        let path = tokens.next().expect("diff-tree entry carries a path");
        entries.push((
            status[0] as char,
            String::from_utf8(path.to_vec()).expect("git paths are UTF-8 here"),
        ));
    }
    entries
}

#[tokio::test]
async fn diff_matches_git_name_status_over_many_pairs() {
    let opts = GenOptions {
        commits: 40,
        ..GenOptions::default()
    };
    let mut g = setup_with(generate(21, &opts)).await;
    let (code, _, stderr) = bed(&g.garden, &["clone", &g.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let last = g.head_index();

    // Two-argument form over adjacent pairs plus long-range pairs
    // (≥ 30 pairs; adjacent pairs cover every commit).
    let mut pairs: Vec<(usize, usize)> = (0..last).map(|i| (i, i + 1)).collect();
    pairs.extend([
        (0, last),
        (0, last / 2),
        (last / 2, last),
        (5, 25.min(last)),
        (2, last - 1),
    ]);
    let mut checked = 0usize;
    for (i, j) in &pairs {
        let (code, out, stderr) = bed(&g.garden, &["diff", &g.cs_hex(*i), &g.cs_hex(*j)]);
        assert_eq!(code, 0, "pair ({i},{j}): {stderr}");
        let got = parse_bed_diff(&out);
        let want = git_name_status(&g.git, &g.sha(*i), &g.sha(*j));
        assert_eq!(got, want, "pair ({i},{j})\nbed output:\n{out}");
        checked += 1;
    }
    assert!(checked >= 30, "at least 30 pairs were compared: {checked}");

    // One-argument form: against the first parent (git diff-tree --root).
    for i in 1..=last {
        let (code, out, stderr) = bed(&g.garden, &["diff", &g.cs_hex(i)]);
        assert_eq!(code, 0, "single {i}: {stderr}");
        let got = parse_bed_diff(&out);
        let want = oracle::git_diff_tree(&g.git, &g.sha(i), false)
            .expect("git diff-tree")
            .into_iter()
            .map(|e| (e.status, e.path))
            .collect::<Vec<_>>();
        assert_eq!(got, want, "single {i}\nbed output:\n{out}");
    }

    // No-argument form: the current changeset (main's tip) vs its parent.
    let (code, out, stderr) = bed(&g.garden, &["diff"]);
    assert_eq!(code, 0, "{stderr}");
    let got = parse_bed_diff(&out);
    let want = oracle::git_diff_tree(&g.git, &g.sha(last), false)
        .expect("git diff-tree")
        .into_iter()
        .map(|e| (e.status, e.path))
        .collect::<Vec<_>>();
    assert_eq!(got, want, "no-arg diff\nbed output:\n{out}");

    // A diff between identical trees, and a root changeset with no
    // parent, both print only the header naming the sides.
    let (code, ..) = bed(&g.garden, &["diff", &g.cs_hex(last), &g.cs_hex(last)]);
    assert_eq!(code, 0);
    let (code, ..) = bed(&g.garden, &["diff", &g.cs_hex(0)]);
    assert_eq!(code, 0);

    // A ref works as a side (v0.1 revspecs are bare names — no `~` syntax).
    let (code, out, stderr) = bed(&g.garden, &["diff", &g.cs_hex(last - 1), "main"]);
    assert_eq!(code, 0, "refs as sides: {stderr}");
    let got = parse_bed_diff(&out);
    let want = git_name_status(&g.git, &g.sha(last - 1), &g.sha(last));
    assert_eq!(got, want, "ref-side diff\nbed output:\n{out}");

    g.server.shutdown().await;
}

// ------------------------------------------------------------ switch

#[tokio::test]
async fn switch_moves_the_bed_and_status_stays_clean() {
    let mut g = setup().await;
    let (code, _, stderr) = bed(&g.garden, &["clone", &g.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let prefix = some_prefix(&g.history, g.head_index());
    let (code, out, stderr) = bed(&g.garden, &["sparse", "add", &prefix]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains(&format!("selected {prefix}")), "out: {out}");
    let head = g.head_index();

    // Move to an older changeset: the sparse selection is refreshed to match it.
    let older = head / 2;
    let (code, out, stderr) = bed(&g.garden, &["switch", &g.cs_hex(older)]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains("working copy at"), "out: {out}");

    let want: Vec<String> = expected_files(&g.history, older)
        .into_iter()
        .filter(|(p, (_, mode))| p.starts_with(&format!("{prefix}/")) && *mode != Mode::Gitlink)
        .map(|(p, _)| p)
        .collect();
    assert_eq!(
        files_under(&g.garden),
        want,
        "the sparse matches the older changeset"
    );
    let config = fs::read_to_string(g.state_dir().join("config.toml")).unwrap();
    assert!(
        config.contains(&format!("current_cs = \"{}\"", g.cs_hex(older))),
        "config: {config}"
    );
    let (code, out, _) = bed(&g.garden, &["status"]);
    assert_eq!(code, 0);
    assert!(out.contains("working copy is clean"), "out: {out}");

    // Refs work; going back to main restores the head state.
    let (code, _, stderr) = bed(&g.garden, &["switch", "main"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let want: Vec<String> = expected_files(&g.history, head)
        .into_iter()
        .filter(|(p, (_, mode))| p.starts_with(&format!("{prefix}/")) && *mode != Mode::Gitlink)
        .map(|(p, _)| p)
        .collect();
    assert_eq!(files_under(&g.garden), want, "back at the head changeset");
    let (code, out, _) = bed(&g.garden, &["status"]);
    assert_eq!(code, 0);
    assert!(out.contains("working copy is clean"), "out: {out}");

    // A failing switch (unknown ref) moves nothing.
    let config_before = fs::read_to_string(g.state_dir().join("config.toml")).unwrap();
    let (code, _, stderr) = bed(&g.garden, &["switch", "no-such-ref"]);
    assert_eq!(code, 1);
    assert!(stderr.contains("no ref"), "stderr: {stderr}");
    let config_after = fs::read_to_string(g.state_dir().join("config.toml")).unwrap();
    assert_eq!(config_before, config_after, "a failed switch moves nothing");

    // An id that is not on the server too.
    let (code, _, stderr) = bed(&g.garden, &["switch", &"cd".repeat(32)]);
    assert_eq!(code, 1);
    assert!(stderr.contains("is not on the server"), "stderr: {stderr}");

    g.server.shutdown().await;
}

// ------------------------------------------------------- idempotence

#[tokio::test]
async fn reruns_fetch_nothing_and_leave_the_sparse_faithful() {
    let mut g = setup().await;
    let (code, _, stderr) = bed(&g.garden, &["clone", &g.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let client = g.server.client().await;
    let (t0, b0) = g.server.counted(&client).await;

    let (code, out, stderr) = bed(&g.garden, &["sparse", "add", "."]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains("fetched"), "out: {out}");
    let (t1, b1) = g.server.counted(&client).await;
    let (want_trees, want_blobs) =
        common::want_list(&g.server.root, g.cs_id(g.head_index()), &["."]);
    assert_eq!(
        (t1 - t0, b1 - b0),
        (want_trees.len() as u64, want_blobs.len() as u64),
        "first fetch pulls exactly the want-list"
    );

    // Re-adding the whole sparse: cache-first fetch pulls zero, files kept.
    let (code, out, stderr) = bed(&g.garden, &["sparse", "add", "."]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains("0 tree(s) and 0 blob(s)"), "out: {out}");
    let (t2, b2) = g.server.counted(&client).await;
    assert_eq!((t2, b2), (t1, b1), "the rerun fetched nothing");

    // A switch to where the working copy already stands costs nothing either.
    let (code, out, stderr) = bed(&g.garden, &["switch", "main"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains("fetched 0 tree(s), 0 blob(s)"), "out: {out}");
    let (t3, b3) = g.server.counted(&client).await;
    assert_eq!((t3, b3), (t1, b1), "switch fetched nothing");

    // And the working copy is still exactly right.
    assert_fidelity(&g, g.head_index());

    g.server.shutdown().await;
}

// ------------------------------------------------- offline contract

#[tokio::test]
async fn server_down_fails_cleanly_never_half_states() {
    let mut g = setup().await;
    let (code, _, stderr) = bed(&g.garden, &["clone", &g.server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let prefix = some_prefix(&g.history, g.head_index());
    let (code, _, stderr) = bed(&g.garden, &["sparse", "add", &prefix]);
    assert_eq!(code, 0, "stderr: {stderr}");

    g.server.shutdown().await;

    let config_before = fs::read_to_string(g.state_dir().join("config.toml")).unwrap();
    let ts = gardenbed::workspace::WorkingCopy::open(&g.state_dir())
        .unwrap()
        .treestate()
        .unwrap();
    let rows_before = ts.count().unwrap();
    let files_before = files_under(&g.garden);

    // status works fully offline.
    let (code, out, _) = bed(&g.garden, &["status"]);
    assert_eq!(code, 0);
    assert!(out.contains("working copy is clean"), "out: {out}");

    // Server-backed commands fail with the connect error and leave
    // everything exactly as it stood.
    let cases: &[&[&str]] = &[
        &["switch", "main"],
        &["sparse", "add", "another/dir"],
        &["file", &prefix],
        &["log"],
        &["diff"],
    ];
    for args in cases {
        let (code, stdout, stderr) = bed(&g.garden, args);
        assert_eq!(code, 1, "args {args:?}: stdout {stdout:?}");
        assert!(
            stderr.contains("cannot reach the server"),
            "args {args:?}: stderr {stderr:?}"
        );
    }

    let config_after = fs::read_to_string(g.state_dir().join("config.toml")).unwrap();
    assert_eq!(config_before, config_after, "config untouched");
    assert_eq!(rows_before, ts.count().unwrap(), "treestate untouched");
    assert_eq!(
        files_before,
        files_under(&g.garden),
        "working copy untouched"
    );
}
