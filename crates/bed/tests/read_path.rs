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
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use gardenbed::render;
use gardenbed::treestate::{MODE_EXEC, MODE_GITLINK, MODE_REGULAR, MODE_SYMLINK};
use gardenbed_harness::generator::{GenOptions, GeneratedHistory, files_at, generate};
use gardenbed_harness::materialize::materialize;
use gardenbed_harness::oracle;
use gardenbed_model::{Id, Mode, Op};
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
    let server_root = dir.path().join("server").join(common::REPO);
    let git_repo = dir.path().join("git-oracle");
    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir created");
    let head = common::ingest(&history, &server_root);
    let materialized = materialize(&history, &git_repo).expect("git oracle materializes");
    assert_eq!(materialized.mapping.last().map(|(id, _)| *id), Some(head));
    let server = Server::spawn(server_root.parent().unwrap()).await;
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
    let (code, out, stderr) = bed(
        &g.garden,
        &["clone", &format!("{}/garden", g.server.http())],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        out.contains("created working copy for garden"),
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
    // A server over a fresh multi-repo root with one refless repository:
    // the repository exists, none of its refs do.
    let dir = tempfile::tempdir().unwrap();
    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).unwrap();
    let repo = gardenbed_server::Repo::open(&dir.path().join("server").join(common::REPO))
        .expect("empty repository opens");
    drop(repo);
    let mut server = Server::spawn(&dir.path().join("server")).await;

    let (code, _, stderr) = bed(&garden, &["clone", &format!("{}/garden", server.http())]);
    assert_eq!(code, 1);
    // The no-flag resolution names what it probed and what exists (here:
    // nothing), pointing at --ref.
    assert!(
        stderr.contains("no ref main (tried main, master, trunk); pass --ref\n"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("the server has refs"), "stderr: {stderr}");
    assert!(!garden.join(".bed").exists(), "no bed may be planted");

    // An unknown repository is a repository error, not a ref error: the
    // server names it and lists what it serves.
    let (code, _, stderr) = bed(&garden, &["clone", &server.http(), "--repo", "orchard"]);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("repository orchard not found (this server serves: garden)"),
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
                #[cfg(unix)]
                assert_eq!(
                    target.as_os_str().as_bytes(),
                    content.as_slice(),
                    "symlink {path} target bytes"
                );
                #[cfg(not(unix))]
                assert_eq!(
                    target.to_string_lossy().as_bytes(),
                    content.as_slice(),
                    "symlink {path} target bytes"
                );
            }
            _ => {
                let bytes = fs::read(&abs).unwrap_or_else(|e| panic!("file {path} readable: {e}"));
                assert_eq!(&bytes, content, "file {path} bytes");
                #[cfg(unix)]
                {
                    let mode_bits = fs::metadata(&abs).unwrap().permissions().mode();
                    if *mode == Mode::Exec {
                        assert_ne!(mode_bits & 0o111, 0, "exec bit on {path}");
                    } else {
                        assert_eq!(mode_bits & 0o111, 0, "no exec bit on {path}");
                    }
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
    let (code, _, stderr) = bed(
        &g.garden,
        &["clone", &format!("{}/garden", g.server.http())],
    );
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
    let (code, _, stderr) = bed(
        &g.garden,
        &["clone", &format!("{}/garden", g.server.http())],
    );
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
    let (code, _, stderr) = bed(
        &g.garden,
        &["clone", &format!("{}/garden", g.server.http())],
    );
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
        let (code, _, stderr) = bed(
            &g.garden,
            &["clone", &format!("{}/garden", g.server.http())],
        );
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

/// Parses working copy's rendered diff into git `--name-status` shape:
/// `(letter, path)`. The letter is derived from the header lines:
/// `new file mode` → A, `deleted file mode` → D, `rename from` → R,
/// otherwise old/new mode lines that change the entry *type* → T, else M.
/// For renames the recorded path is the **destination** (git's
/// `--name-status` reports the dst; the src rides the `rename from` line).
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
    let mut renamed = false;
    let flush = |entries: &mut Vec<(char, String)>,
                 path: &mut String,
                 old_mode: &mut Option<String>,
                 new_mode: &mut Option<String>,
                 renamed: &mut bool| {
        if path.is_empty() {
            return;
        }
        let letter = if *renamed {
            'R'
        } else {
            match (old_mode.take(), new_mode.take()) {
                (_, Some(m)) if m.starts_with("new file") => 'A',
                (Some(m), _) if m.starts_with("deleted file") => 'D',
                (Some(o), Some(n)) if type_class(&o) != type_class(&n) => 'T',
                _ => 'M',
            }
        };
        entries.push((letter, std::mem::take(path)));
        *renamed = false;
    };
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            flush(
                &mut entries,
                &mut path,
                &mut old_mode,
                &mut new_mode,
                &mut renamed,
            );
            // A rename header carries the source on the a/ side; the
            // entry's path is the b/ (destination) side — rsplit keeps
            // headers whose a-side path itself contains " b/" intact.
            let (_, dst) = rest
                .rsplit_once(" b/")
                .unwrap_or_else(|| panic!("diff header without b/ side: {line:?}"));
            path = dst.to_owned();
        } else if let Some(rest) = line.strip_prefix("old mode ") {
            old_mode = Some(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("new mode ") {
            new_mode = Some(rest.to_owned());
        } else if let Some(rest) = line.strip_prefix("new file mode ") {
            new_mode = Some(format!("new file {rest}"));
        } else if let Some(rest) = line.strip_prefix("deleted file mode ") {
            old_mode = Some(format!("deleted file {rest}"));
        } else if line.starts_with("rename from ") {
            renamed = true;
        }
    }
    flush(
        &mut entries,
        &mut path,
        &mut old_mode,
        &mut new_mode,
        &mut renamed,
    );
    entries
}

/// `git diff-tree -r -z --name-status -M A B` — the path/op oracle for
/// arbitrary pairs. `-z` R entries carry **two** path tokens (src then
/// dst); the entry records the dst, git's `--name-status` shape.
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
            "-M",
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
        let letter = status[0] as char;
        if letter == 'R' || letter == 'C' {
            // src then dst — the src is the rename source, not the entry.
            let _src = tokens.next().expect("diff-tree R entry carries src");
            let dst = tokens.next().expect("diff-tree R entry carries dst");
            entries.push((letter, String::from_utf8(dst.to_vec()).expect("UTF-8")));
        } else {
            let path = tokens.next().expect("diff-tree entry carries a path");
            entries.push((letter, String::from_utf8(path.to_vec()).expect("UTF-8")));
        }
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
    let (code, _, stderr) = bed(
        &g.garden,
        &["clone", &format!("{}/garden", g.server.http())],
    );
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
        let want = oracle::git_diff_tree(&g.git, &g.sha(i), true)
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
    let want = oracle::git_diff_tree(&g.git, &g.sha(last), true)
        .expect("git diff-tree")
        .into_iter()
        .map(|e| (e.status, e.path))
        .collect::<Vec<_>>();
    assert_eq!(got, want, "no-arg diff\nbed output:\n{out}");

    // A diff between identical trees, and a root changeset with no
    // parent, still print their header — just no body.
    let (code, out, stderr) = bed(&g.garden, &["diff", &g.cs_hex(last), &g.cs_hex(last)]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        out.contains("— both sides are equal"),
        "same-cs diff states the sides are equal: {out}"
    );
    let (code, out, stderr) = bed(&g.garden, &["diff", &g.cs_hex(0)]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        out.contains("root changeset has no parent"),
        "root diff says there is no parent: {out}"
    );

    // A ref works as a side (v0.1 revspecs are bare names — no `~` syntax).
    let (code, out, stderr) = bed(&g.garden, &["diff", &g.cs_hex(last - 1), "main"]);
    assert_eq!(code, 0, "refs as sides: {stderr}");
    let got = parse_bed_diff(&out);
    let want = git_name_status(&g.git, &g.sha(last - 1), &g.sha(last));
    assert_eq!(got, want, "ref-side diff\nbed output:\n{out}");

    g.server.shutdown().await;
}

/// M19.1 + M19.2: pure renames render git's `R100` form with full
/// letter parity against `git diff-tree -M`, and a pure-rename
/// changeset's diff body is byte-identical to `git diff-tree -M -p`
/// (pure renames carry no `index` lines, so byte parity is reachable —
/// regular modifies never are, because bed omits `index` lines).
#[tokio::test]
async fn pure_renames_render_git_m_parity_and_byte_parity() {
    // Rename-heavy: every file operation is a rename, nothing else —
    // with the generator's 25% edit chance per rename, most changesets
    // hold at least one *pure* rename.
    let opts = GenOptions {
        commits: 40,
        rename_ratio: 1.0,
        delete_ratio: 0.0,
        ..GenOptions::default()
    };
    let mut g = setup_with(generate(7, &opts)).await;
    let (code, _, stderr) = bed(
        &g.garden,
        &["clone", &format!("{}/garden", g.server.http())],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let last = g.head_index();

    let mut pure_rename_changesets = 0usize;
    let mut checked_letters = 0usize;
    let mut checked_bytes = 0usize;
    for i in 1..=last {
        // Pure-rename changeset: every change is a Rename whose recorded
        // blob is the parent's blob at the source path (files_at replay
        // gives both sides without touching the server).
        let parent_files = files_at(&g.history, i - 1);
        let all_pure = g.history.changesets[i].changes.iter().all(|c| {
            c.op == Op::Rename
                && c.src_path
                    .as_deref()
                    .and_then(|src| parent_files.get(src))
                    .is_some_and(|(blob, _)| Some(*blob) == c.blob)
        });

        let (code, out, stderr) = bed(&g.garden, &["diff", &g.cs_hex(i)]);
        assert_eq!(code, 0, "single {i}: {stderr}");
        let got = parse_bed_diff(&out);
        let want = oracle::git_diff_tree(&g.git, &g.sha(i), true)
            .expect("git diff-tree")
            .into_iter()
            .map(|e| (e.status, e.path))
            .collect::<Vec<_>>();
        assert_eq!(got, want, "letters, changeset {i}\nbed output:\n{out}");
        checked_letters += 1;

        // Byte parity against `git diff-tree -M -p` — body only (bed
        // prints its one-line header; git prints none). Only meaningful
        // when *every* entry is a pure rename (no index lines anywhere).
        if all_pure && !g.history.changesets[i].changes.is_empty() {
            pure_rename_changesets += 1;
            let body = out
                .lines()
                .skip_while(|l| !l.starts_with("diff --git"))
                .collect::<Vec<_>>()
                .join("\n");
            let want = std::process::Command::new("git")
                .args([
                    "-c",
                    "core.quotePath=false",
                    "diff-tree",
                    "--root",
                    "-M",
                    "-r",
                    "-p",
                    "--no-commit-id",
                    "--diff-merges=first-parent",
                    &g.sha(i),
                ])
                .current_dir(&g.git)
                .output()
                .expect("git diff-tree -p runs");
            assert!(want.status.success(), "git diff-tree -p failed");
            let want_body = String::from_utf8(want.stdout).expect("UTF-8 diff");
            // Normalize: both sides may end with or without a trailing
            // newline; compare line-wise.
            let want_lines: Vec<&str> = want_body.lines().collect();
            assert_eq!(
                body,
                want_lines.join("\n"),
                "byte parity, changeset {i}\nbed output:\n{out}"
            );
            checked_bytes += 1;
        }
    }
    assert!(
        pure_rename_changesets >= 5,
        "the fixture must contain several all-pure-rename changesets: {pure_rename_changesets}"
    );
    assert!(checked_letters >= 39);
    assert!(checked_bytes >= 5);

    // M19.1's grep-count form: `rename from` lines equal git -M's
    // pure-rename count on the whole history, both diff forms.
    let mut bed_renames = 0usize;
    let mut git_renames = 0usize;
    for i in 1..=last {
        let (code, out, stderr) = bed(&g.garden, &["diff", &g.cs_hex(i)]);
        assert_eq!(code, 0, "{stderr}");
        bed_renames += out
            .lines()
            .filter(|l| l.starts_with("rename from "))
            .count();
        let want = oracle::git_diff_tree(&g.git, &g.sha(i), true)
            .expect("git diff-tree")
            .into_iter()
            .filter(|e| e.status == 'R')
            .count();
        git_renames += want;
    }
    assert_eq!(
        bed_renames, git_renames,
        "rename-from counts across history"
    );

    // Two-sided form: the same parity over adjacent pairs.
    let mut two_sided = 0usize;
    for i in 1..last {
        let (code, out, stderr) = bed(&g.garden, &["diff", &g.cs_hex(i), &g.cs_hex(i + 1)]);
        assert_eq!(code, 0, "{stderr}");
        let got = parse_bed_diff(&out);
        let want = git_name_status(&g.git, &g.sha(i), &g.sha(i + 1));
        assert_eq!(got, want, "two-sided pair ({i},{})", i + 1);
        two_sided += 1;
    }
    assert!(two_sided >= 30);

    g.server.shutdown().await;
}

/// M19.3: a moved-and-edited file renders as delete + write (no
/// `R<100` similarity claim), while the pure move in the same changeset
/// renders one rename block — the residual counted from recorded facts.
#[tokio::test]
async fn moved_and_edited_files_render_delete_plus_write() {
    let opts = GenOptions {
        commits: 60,
        rename_ratio: 0.8,
        delete_ratio: 0.0,
        ..GenOptions::default()
    };
    // Find a seed whose history contains at least one edited rename.
    let mut history = None;
    for seed in 1..50u64 {
        let h = generate(seed, &opts);
        let has_edited = (1..h.changesets.len()).any(|i| {
            let parent = files_at(&h, i - 1);
            h.changesets[i].changes.iter().any(|c| {
                c.op == Op::Rename
                    && c.src_path
                        .as_deref()
                        .and_then(|src| parent.get(src))
                        .is_some_and(|(blob, _)| Some(*blob) != c.blob)
            })
        });
        if has_edited {
            history = Some(h);
            break;
        }
    }
    let history = history.expect("a seed with an edited rename exists");
    let mut g = setup_with(history).await;
    let (code, _, stderr) = bed(
        &g.garden,
        &["clone", &format!("{}/garden", g.server.http())],
    );
    assert_eq!(code, 0, "stderr: {stderr}");

    let mut edited_seen = 0usize;
    let mut pure_seen = 0usize;
    for i in 1..=g.head_index() {
        let parent = files_at(&g.history, i - 1);
        for c in &g.history.changesets[i].changes {
            if c.op != Op::Rename {
                continue;
            }
            let src_blob = c
                .src_path
                .as_deref()
                .and_then(|src| parent.get(src))
                .map(|(blob, _)| *blob);
            let (code, out, stderr) = bed(&g.garden, &["diff", &g.cs_hex(i)]);
            assert_eq!(code, 0, "{stderr}");
            match src_blob {
                Some(b) if Some(b) == c.blob => {
                    // Pure: exactly one `rename from <src>` block names it.
                    let hits = out
                        .lines()
                        .filter(|l| *l == format!("rename from {}", c.src_path.as_deref().unwrap()))
                        .count();
                    assert_eq!(hits, 1, "pure rename renders one block\n{out}");
                    pure_seen += 1;
                }
                _ => {
                    // Edited: the src renders as a delete block, the dst
                    // as a write block — never a rename block.
                    let src = c.src_path.as_deref().unwrap();
                    assert!(
                        !out.lines().any(|l| l == format!("rename from {src}")),
                        "edited rename must not render R\n{out}"
                    );
                    edited_seen += 1;
                }
            }
            break; // one recorded rename per changeset is enough to check
        }
    }
    assert!(pure_seen >= 1, "fixture must contain a pure rename");
    assert!(edited_seen >= 1, "fixture must contain an edited rename");

    g.server.shutdown().await;
}

/// M19.5: gitlink pointer changes render `Subproject commit` hunks with
/// the real SHA from the padded blob id, and `git apply --check` accepts
/// exactly where git's own form is accepted.
#[tokio::test]
async fn gitlink_pointer_changes_render_subproject_hunks() {
    // The generator's gitlink ids are 12 zero bytes + the upstream SHA.
    // Find a changeset where a gitlink is *added* or its pointer moves.
    let opts = GenOptions {
        commits: 60,
        ..GenOptions::default()
    };
    let mut history = None;
    for seed in 1..50u64 {
        let h = generate(seed, &opts);
        let has_gitlink_change = (1..h.changesets.len()).any(|i| {
            let parent = files_at(&h, i - 1);
            h.changesets[i]
                .changes
                .iter()
                .any(|c| c.mode == Mode::Gitlink && parent.contains_key(&c.path))
                || h.changesets[i]
                    .changes
                    .iter()
                    .any(|c| c.mode == Mode::Gitlink && c.op == Op::Add)
        });
        if has_gitlink_change {
            history = Some(h);
            break;
        }
    }
    let history = history.expect("a seed with a gitlink add or pointer change exists");
    let mut g = setup_with(history).await;
    let (code, _, stderr) = bed(
        &g.garden,
        &["clone", &format!("{}/garden", g.server.http())],
    );
    assert_eq!(code, 0, "stderr: {stderr}");

    let mut subproject_hunks = 0usize;
    for i in 1..=g.head_index() {
        let (code, out, stderr) = bed(&g.garden, &["diff", &g.cs_hex(i)]);
        assert_eq!(code, 0, "{stderr}");
        for line in out.lines().filter(|l| l.contains("Subproject commit ")) {
            // Every Subproject line's 40-hex must be the true upstream
            // SHA: reconstruct it from the recorded change's padded id.
            let sha = line.rsplit(' ').next().unwrap();
            assert_eq!(sha.len(), 40, "full 40-hex SHA: {line}");
            assert!(sha.bytes().all(|b| b.is_ascii_hexdigit()), "hex: {line}");
            subproject_hunks += 1;
        }
        // No Binary-differ line may survive for a same-path gitlink
        // change (the retired v0.1 shape).
        if g.history.changesets[i]
            .changes
            .iter()
            .any(|c| c.mode == Mode::Gitlink && c.op == Op::Modify)
        {
            assert!(
                !out.contains("Binary files"),
                "gitlink modify must render Subproject, not binary-differ\n{out}"
            );
        }
    }
    assert!(
        subproject_hunks >= 1,
        "the fixture must exercise at least one Subproject hunk"
    );

    // `git diff --submodule=short` parity for a pointer change: bed's
    // rendered SHA lines match git's own choice on the same commit.
    for i in 1..=g.head_index() {
        let pointer_paths: Vec<&String> = g.history.changesets[i]
            .changes
            .iter()
            .filter(|c| c.mode == Mode::Gitlink && c.op == Op::Modify)
            .map(|c| &c.path)
            .collect();
        if pointer_paths.is_empty() {
            continue;
        }
        let (code, out, stderr) = bed(&g.garden, &["diff", &g.cs_hex(i)]);
        assert_eq!(code, 0, "{stderr}");
        let git_out = std::process::Command::new("git")
            .args([
                "-c",
                "core.quotePath=false",
                "diff-tree",
                "--root",
                "-r",
                "-p",
                "--no-commit-id",
                "--submodule=short",
                "--diff-merges=first-parent",
                &g.sha(i),
            ])
            .current_dir(&g.git)
            .output()
            .expect("git diff-tree --submodule runs");
        assert!(git_out.status.success());
        let git_text = String::from_utf8(git_out.stdout).expect("UTF-8");
        for path in pointer_paths {
            let bed_sha = out
                .lines()
                .filter(|l| l.contains("Subproject commit "))
                .filter(|l| l.starts_with('-') || l.starts_with('+'))
                .map(|l| l.rsplit(' ').next().unwrap().to_owned())
                .collect::<Vec<_>>();
            let git_shas: Vec<String> = git_text
                .lines()
                .filter(|l| l.contains("Subproject commit "))
                .map(|l| l.rsplit(' ').next().unwrap().to_owned())
                .collect::<Vec<_>>();
            assert_eq!(
                bed_sha, git_shas,
                "Subproject SHAs match git --submodule=short at {path}, changeset {i}"
            );
        }
    }

    g.server.shutdown().await;
}

// ------------------------------------------------------------ switch

#[tokio::test]
async fn switch_moves_the_bed_and_status_stays_clean() {
    let mut g = setup().await;
    let (code, _, stderr) = bed(
        &g.garden,
        &["clone", &format!("{}/garden", g.server.http())],
    );
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
    let (code, _, stderr) = bed(
        &g.garden,
        &["clone", &format!("{}/garden", g.server.http())],
    );
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
    let (code, _, stderr) = bed(
        &g.garden,
        &["clone", &format!("{}/garden", g.server.http())],
    );
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
