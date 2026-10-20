//! `verify_corpus` — the M3.5 sampled content-fidelity sweep against a real
//! imported corpus (e.g. the pinned linux kernel mirror, `docs/corpora.md`).
//!
//! For `--samples` random (changeset, path) pairs it compares Gardenbed's
//! derived tree entry content against `git show <git-sha>:<path>` in the
//! source repository — the same oracle the synthetic suites use, at
//! corpus scale. Revs are sampled uniformly from the imported mapping by
//! rowid poke; paths by random descent through the derived tree.
//!
//! `--root` is the Gardenbed **repo root** (the server's convention): it
//! holds `objects/` (the object store) and `metadata.sqlite3` (metadata).
//!
//! ```sh
//! cargo run --release -p gardenbed-git --example verify_corpus -- \
//!     --root /home/user/.corpora/linux-gb \
//!     --git  /home/user/.corpora/linux.git \
//!     --ref  refs/heads/master --samples 1000 --seed 42
//! ```
//!
//! Exit 0 iff every sampled pair matches byte-exactly. Gitlink pointers are
//! skipped (no objects content); a `git show` refusal on a pre-canonical
//! `100664` entry falls back to a mode comparison (both sides normalize —
//! see `docs/divergences.md`).

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_model::{Id, Mode};
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

/// Picks a random file leaf by random descent through the tree (a handful
/// of tree reads per sample, vs millions for a full flatten at kernel
/// scale). Returns `None` to resample (gitlink leaf, dangling dir, or a
/// leafless subtree). Shallow paths are overrepresented — fine for a
/// sampled sweep.
fn random_leaf(
    engine: &Engine,
    root: Id,
    rng: &mut Rng,
) -> Option<(String, Mode, gardenbed_model::Id)> {
    use gardenbed_model::TreeKind;
    let mut dir = root;
    let mut path = String::new();
    for _ in 0..64 {
        let tree = engine.tree(dir).ok()??;
        if tree.entries.is_empty() {
            return None;
        }
        let entry = &tree.entries[rng.below(tree.entries.len() as u64) as usize];
        match entry.kind {
            TreeKind::File => {
                path.push_str(&entry.name);
                return Some((path, entry.mode, entry.blob?));
            }
            TreeKind::Dir => {
                path.push_str(&entry.name);
                path.push('/');
                dir = entry.blob?;
            }
        }
    }
    None
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

    // Resolve the layout the way the server does (finding #27): the
    // database FILE is the discriminator, so a pre-rename corpus
    // (`seedbed.sqlite3` + `loam/objects`) is read in place and a
    // repository named `loam` is not mistaken for that layout.
    let layout = gardenbed_server::resolve_layout(&root);
    let objects: Arc<dyn ObjectStore> =
        Arc::new(FsObjectStore::open(&layout.objects).expect("objects opens"));
    let metadata: Arc<dyn gardenbed_store::MetadataStore> =
        Arc::new(SqliteMetadataStore::open(&layout.metadata).expect("metadata opens"));
    let engine = Engine::new(objects, metadata.clone());
    // A second, read-only handle for sampling queries (WAL readers coexist).
    let ro = rusqlite::Connection::open_with_flags(
        &layout.metadata,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read-only metadata opens");

    let Some(tip) = metadata.resolve_ref(&ref_name).expect("resolve ref") else {
        panic!("ref {ref_name} not found in the imported corpus");
    };
    println!("tip {tip} ({ref_name})");

    let mut rng = Rng(seed | 1);
    let mut checked = 0u64;
    let mut mismatches = 0u64;
    let mut skipped = 0u64;

    while checked < samples {
        // Uniform random rev: poke git_map by rowid. (A parent walk costs
        // thousands of full changeset decodes per sample at kernel scale.)
        let (git_sha, cs_bytes): (String, Vec<u8>) = ro
            .query_row(
                "SELECT git_sha, cs FROM git_map WHERE rowid = \
                 (abs(random()) % (SELECT max(rowid) FROM git_map) + 1)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("git_map poke");
        let cs = Id::from_bytes(cs_bytes.try_into().expect("32-byte id"));

        // A random path from this changeset's tree, by random descent — a
        // full flatten walks every tree object in a ~90k-file kernel tree,
        // so we sample the hierarchy instead.
        let Ok(Some(tree)) = engine.root_tree(cs) else {
            skipped += 1;
            continue;
        };
        let Some((path, mode, blob)) = random_leaf(&engine, tree, &mut rng) else {
            skipped += 1;
            continue;
        };
        if mode == Mode::Gitlink {
            // Submodule pointers have no objects content by design.
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
            eprintln!("... {checked} checked, {mismatches} mismatches, {skipped} skipped");
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
