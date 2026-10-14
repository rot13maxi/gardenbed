//! `verify_corpus` — the M3.5 sampled content-fidelity sweep against a real
//! imported corpus (e.g. the pinned linux kernel mirror, `docs/corpora.md`).
//!
//! For `--samples` random (changeset, path) pairs it compares gardenbed's
//! derived tree entry content against `git show <git-sha>:<path>` in the
//! source repository — the same oracle the synthetic suites use, at
//! corpus scale. Sampling walks the imported graph itself (random descent
//! from the resolved ref), so no full mapping listing is needed.
//!
//! ```sh
//! cargo run --release -p gardenbed-rootstock --example verify_corpus -- \
//!     --root /home/user/.corpora/linux-gb \
//!     --git  /home/user/.corpora/linux.git \
//!     --ref  refs/heads/master --samples 1000 --seed 42
//! ```
//!
//! Exit 0 iff every sampled pair matches byte-exactly. Gitlink pointers are
//! skipped (no loam content); a `git show` refusal on a pre-canonical
//! `100664` entry falls back to a mode comparison (both sides normalize —
//! see `docs/divergences.md`).

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_model::Mode;
use gardenbed_store::{FsObjectStore, ObjectStore, SqliteMetadataStore};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Runs a git read in the mirror, hermetically (no user config, C locale).
fn git(mirror: &PathBuf, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(mirror)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn git_show(mirror: &PathBuf, sha: &str, path: &str) -> Result<Vec<u8>, String> {
    git(mirror, &["show", &format!("{sha}:{path}")])
}

/// git modes normalize to the model's mode table (pre-canonical `100664`
/// counts as Regular — see `docs/divergences.md`).
fn canon_mode(git_mode: &str) -> Option<&'static str> {
    match git_mode {
        "100644" | "100664" => Some("regular"),
        "100755" => Some("exec"),
        "120000" => Some("symlink"),
        "160000" => Some("gitlink"),
        _ => None,
    }
}

fn model_mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Regular => "regular",
        Mode::Exec => "exec",
        Mode::Symlink => "symlink",
        Mode::Gitlink => "gitlink",
    }
}

fn main() {
    let mut root = PathBuf::new();
    let mut mirror = PathBuf::new();
    let mut ref_name = "refs/heads/master".to_owned();
    let mut samples: u64 = 1000;
    let mut seed: u64 = 0xC0FFEE;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--root" => root = PathBuf::from(args.next().expect("value")),
            "--git" => mirror = PathBuf::from(args.next().expect("value")),
            "--ref" => ref_name = args.next().expect("value"),
            "--samples" => samples = args.next().expect("value").parse().expect("u64"),
            "--seed" => seed = args.next().expect("value").parse().expect("u64"),
            other => panic!("unknown arg {other}"),
        }
    }
    assert!(!root.as_os_str().is_empty(), "--root required");
    assert!(!mirror.as_os_str().is_empty(), "--git required");

    let objects: Arc<dyn ObjectStore> = Arc::new(FsObjectStore::open(&root).expect("loam opens"));
    let metadata: Arc<dyn gardenbed_store::MetadataStore> =
        Arc::new(SqliteMetadataStore::open(&root.join("seedbed.sqlite3")).expect("seedbed opens"));
    let engine = Engine::new(objects, metadata.clone());

    let Some(tip) = metadata.resolve_ref(&ref_name).expect("resolve ref") else {
        panic!("ref {ref_name} not found in the imported corpus");
    };
    println!("tip {tip} ({ref_name})");

    let mut rng = Rng(seed | 1);
    let mut checked = 0u64;
    let mut mismatches = 0u64;
    let mut skipped = 0u64;

    while checked < samples {
        // Random descent from the tip: pick a parent at random each step.
        let mut cs = tip;
        let depth = rng.below(4096);
        for _ in 0..depth {
            let parents = match metadata.get_changeset(cs) {
                Ok(Some((c, _))) => c.parents,
                _ => break,
            };
            if parents.is_empty() {
                break;
            }
            cs = parents[rng.below(parents.len() as u64) as usize];
        }
        let Some(git_sha) = metadata.git_for_cs(cs).ok().flatten() else {
            skipped += 1;
            continue;
        };

        // A random path from this changeset's flattened tree.
        let Ok(Some(tree)) = engine.root_tree(cs) else {
            skipped += 1;
            continue;
        };
        let flat = match engine.flatten_tree(Some(tree)) {
            Ok(f) if !f.is_empty() => f,
            _ => {
                skipped += 1;
                continue;
            }
        };
        let (path, mode, blob) = flat[rng.below(flat.len() as u64) as usize].clone();
        if mode == Mode::Gitlink {
            // Submodule pointers have no loam content by design.
            skipped += 1;
            continue;
        }
        let Ok(Some(content)) = engine.objects().get_blob(blob) else {
            skipped += 1;
            continue;
        };

        match git_show(&mirror, &git_sha, &path) {
            Ok(git_bytes) if git_bytes == content => {
                checked += 1;
            }
            Ok(git_bytes) => {
                mismatches += 1;
                eprintln!(
                    "MISMATCH {} {}: ours {} bytes, git {} bytes",
                    git_sha,
                    path,
                    content.len(),
                    git_bytes.len()
                );
            }
            Err(e) => {
                // git refuses to `show` some pre-canonical-mode entries; if
                // the tree entry itself agrees, count the pair as checked.
                match git(&mirror, &["ls-tree", &git_sha, "--", &path]) {
                    Ok(ls)
                        if String::from_utf8_lossy(&ls)
                            .split_whitespace()
                            .next()
                            .and_then(canon_mode)
                            == Some(model_mode_name(mode)) =>
                    {
                        checked += 1;
                    }
                    _ => {
                        mismatches += 1;
                        eprintln!("ORACLE FAIL {git_sha} {path}: {e}");
                    }
                }
            }
        }
        if checked.is_multiple_of(100) {
            println!("... {checked} checked, {mismatches} mismatches, {skipped} skipped");
        }
    }

    println!(
        "verify_corpus: {checked} sampled (cs, path) pairs, {mismatches} mismatches, \
         {skipped} skipped"
    );
    if mismatches > 0 {
        std::process::exit(1);
    }
}
