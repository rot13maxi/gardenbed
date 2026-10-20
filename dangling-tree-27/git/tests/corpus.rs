//! The real-corpus check: a full import of the ripgrep mirror (4590
//! commits) with a random-sampled differential sweep and throughput
//! measurement, with and without rename detection.
//!
//! Ignored by default (it is a multi-minute real-data run):
//!
//! ```text
//! cargo test -p gardenbed-git --test corpus -- --ignored --nocapture
//! ```
//!
//! Requires the mirror at `/tmp/opencode/ripgrep.git` (a bare
//! `git clone --mirror` of https://github.com/BurntSushi/ripgrep).

mod common;

use std::collections::BTreeMap;
use std::path::Path;

use common::{CatFile, Fixture, import, open_stores, sweep};
use gardenbed_git::{ImportOptions, import_from_path};
use gardenbed_model::{Id, Mode};
use gardenbed_store::{MetadataStore, ObjectStore};

const MIRROR: &str = "/tmp/opencode/ripgrep.git";
const SAMPLE_COMMITS: usize = 240;
const SAMPLES_PER_COMMIT: usize = 5;

/// Deterministic xorshift64* so the sampled sweep is reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn hex_bytes(s: &str) -> Vec<u8> {
    assert_eq!(s.len() % 2, 0, "odd hex: {s}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex digit"))
        .collect()
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(repo);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    let out = cmd.args(args).output().expect("git runs");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

/// `git ls-tree -r -z <sha>` as (path, mode, raw sha bytes).
fn ls_tree(repo: &Path, sha: &str) -> Vec<(String, String, Vec<u8>)> {
    let out = {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(repo);
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(&k);
            }
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
        let o = cmd
            .args(["ls-tree", "-r", "-z", sha])
            .output()
            .expect("ls-tree");
        assert!(o.status.success(), "ls-tree failed for {sha}");
        o.stdout
    };
    let mut entries = Vec::new();
    for tok in out.split(|b| *b == 0) {
        if tok.is_empty() {
            continue;
        }
        let tab = tok.iter().position(|b| *b == b'\t').expect("TAB");
        let meta = String::from_utf8_lossy(&tok[..tab]).to_string();
        let path = String::from_utf8_lossy(&tok[tab + 1..]).to_string();
        let mut it = meta.splitn(3, ' ');
        let mode = it.next().expect("mode").to_string();
        let _kind = it.next().expect("kind");
        let oid = it.next().expect("oid").to_string();
        entries.push((path, mode, hex_bytes(&oid)));
    }
    entries
}

/// Full import + sampled differential sweep of one configuration; prints
/// the throughput line for the report.
fn run_corpus(detect_renames: bool, tag: &str) {
    if !Path::new(MIRROR).exists() {
        panic!("mirror missing at {MIRROR}");
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let garden = dir.path().join("garden");
    let (objects, metadata, engine) = open_stores(&garden);
    let opts = ImportOptions {
        detect_renames,
        ..Default::default()
    };
    let report =
        import_from_path(Path::new(MIRROR), objects, metadata, engine, &opts).expect("import");
    println!(
        "corpus[{tag}]: {report} — objects_written={}",
        report.objects_written
    );

    // Bijection against git's own count of branch-reachable commits.
    let rev_count = git_out(Path::new(MIRROR), &["rev-list", "--branches", "--count"])
        .trim()
        .parse::<usize>()
        .expect("count");
    assert_eq!(
        report.commits_total as usize, rev_count,
        "walk covers rev-list --branches"
    );
    assert_eq!(report.commits_new as usize, rev_count);
    assert_eq!(report.commits_skipped, 0);
    assert!(rev_count > 1_000, "unexpectedly few commits");
    let conn = rusqlite::Connection::open(garden.join("metadata.sqlite3")).expect("sqlite");
    let rows: u64 = conn
        .query_row("SELECT COUNT(*) FROM git_map", [], |r| r.get(0))
        .expect("count");
    assert_eq!(rows as usize, rev_count, "one mapping row per commit");

    // Sampled sweep: SAMPLE_COMMITS random commits, each fully compared
    // (paths + modes) against ls-tree, with SAMPLES_PER_COMMIT random
    // paths byte-compared against cat-file.
    let revs: Vec<String> = git_out(Path::new(MIRROR), &["rev-list", "--branches"])
        .lines()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(revs.len(), rev_count);

    let (_, metadata, engine) = open_stores(&garden);
    let (objects, _, _) = open_stores(&garden);
    let mut cat = CatFile::spawn(Path::new(MIRROR));
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ (detect_renames as u64 + 1));
    let mut samples = 0usize;
    let mut checked_commits = 0usize;

    for _ in 0..SAMPLE_COMMITS {
        let sha = &revs[rng.below(revs.len())];
        let cs_id = metadata
            .cs_for_git(sha)
            .expect("cs_for_git")
            .unwrap_or_else(|| panic!("commit {sha} unmapped"));
        let root = engine
            .derive_tree(cs_id)
            .expect("derive")
            .expect("non-empty tree");
        let flat: BTreeMap<String, (Mode, Id)> = engine
            .flatten_tree(Some(root))
            .expect("flatten")
            .into_iter()
            .map(|(p, m, b)| (p, (m, b)))
            .collect();

        let entries = ls_tree(Path::new(MIRROR), sha);
        assert_eq!(
            flat.len(),
            entries.len(),
            "commit {sha}: file count differs from git"
        );
        for (path, mode, gsha) in &entries {
            let (m, b) = flat
                .get(path)
                .unwrap_or_else(|| panic!("commit {sha}: path {path:?} missing"));
            assert_eq!(
                m,
                &common::mode_from_git(mode),
                "commit {sha}: mode of {path:?}"
            );
            if *m == Mode::Gitlink {
                assert_eq!(&b.as_bytes()[12..], gsha.as_slice());
            }
        }
        checked_commits += 1;

        for _ in 0..SAMPLES_PER_COMMIT {
            let (path, mode, _gsha) = &entries[rng.below(entries.len())];
            let (m, b) = flat
                .get(path)
                .unwrap_or_else(|| panic!("commit {sha}: path {path:?} missing"));
            assert_eq!(m, &common::mode_from_git(mode));
            if *m == Mode::Gitlink {
                continue; // pointers are compared structurally above
            }
            let ours = objects
                .get_blob(*b)
                .expect("get_blob")
                .unwrap_or_else(|| panic!("blob {b} missing"));
            let theirs = cat.read(&format!("{sha}:{path}"));
            assert_eq!(
                ours, theirs,
                "commit {sha}: content of {path:?} differs from git"
            );
            samples += 1;
        }
    }
    println!(
        "corpus[{tag}]: sampled sweep — {checked_commits} commits fully compared, {samples} (path,rev) byte samples"
    );
    assert!(samples >= 1_000, "need ≥1000 samples, got {samples}");
}

/// Ripgrep, renames on (the default).
#[test]
#[ignore = "real-corpus run: cargo test -p gardenbed-git --test corpus -- --ignored --nocapture"]
fn corpus_ripgrep_with_renames() {
    run_corpus(true, "renames-on");
}

/// Ripgrep, renames off — same graph, git runs with --no-renames.
#[test]
#[ignore = "real-corpus run: cargo test -p gardenbed-git --test corpus -- --ignored --nocapture"]
fn corpus_ripgrep_without_renames() {
    run_corpus(false, "renames-off");
}

/// M3 criterion 1's heavy case: a 10,000-changeset synthetic corpus with a
/// FULL sweep — every changeset × every path byte-compared against git.
/// Ignored by default (multi-minute run); execute with `--ignored`.
#[test]
#[ignore = "10k full-sweep run: cargo test -p gardenbed-git --test corpus synthetic_10k -- --ignored --nocapture"]
fn synthetic_10k_full_sweep() {
    let opts = gardenbed_harness::GenOptions {
        commits: 10_000,
        merge_ratio: 0.15,
        octopus_ratio: 0.05,
        rename_ratio: 0.2,
        delete_ratio: 0.1,
        ..Default::default()
    };
    let fixture = Fixture::generate(2_060_101, &opts);
    assert_eq!(fixture.history.changesets.len(), 10_000);
    let report = import(&fixture.repo, &fixture.garden, &Default::default());
    assert_eq!(report.commits_new as usize, 10_000);
    assert_eq!(report.commits_skipped, 0);
    println!("synthetic-10k import: {report}");
    sweep(&fixture, &fixture.garden);
    println!("synthetic-10k: full sweep passed (10000 changesets × every path)");
}
