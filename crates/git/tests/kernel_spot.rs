//! M17.3 kernel-corpus spot check (verification harness, not a unit test).
//!
//! Opens the local kernel store directly: `FsObjectStore` on the corpus
//! `loam/` tree (read-only), `SqliteMetadataStore` on a scratch copy of
//! `seedbed.sqlite3` (opening writes WAL/schema, so `GARDENBED_KERNEL_META`
//! must point at a copy — the corpus store stays pristine). Exports
//! `refs/heads/master` with the native writer.
//!
//! The pinned corpus is a v0.1 store (no raw captures), and kernel history
//! carries pre-canonical `100664` tree modes (docs/divergences.md §4), so
//! the export MUST refuse loudly at the first such commit instead of
//! emitting a silently-diverging pack — that refusal is the verified
//! outcome here (D20/D21). A corpus re-imported with raw-tree capture (the
//! gated M18.5 successor work) takes the success arm: bijection against
//! `git_map`, sampled SHA parity, and the exported tip.
//!
//! Run: `GARDENBED_KERNEL_META=/dev/shm/kernel-meta.sqlite3 cargo test \
//!       --release -p gardenbed-git --test kernel_spot -- --ignored --nocapture`

use gardenbed_git::{ExportOptions, export_to_path};
use gardenbed_store::{FsObjectStore, MetadataStore as _, SqliteMetadataStore};
use std::sync::Arc;

#[test]
#[ignore = "kernel corpus: multi-minute export; run explicitly (GARDENBED_KERNEL_GB)"]
fn kernel_export_spot_check() {
    let root = std::path::PathBuf::from(
        std::env::var("GARDENBED_KERNEL_GB")
            .unwrap_or_else(|_| "/home/user/.corpora/linux-gb".to_owned()),
    );
    // `SqliteMetadataStore::open` writes (WAL pragma + schema), so the
    // metadata is opened from a scratch copy — the corpus store stays
    // pristine. Objects are read-only and stay in place.
    //
    // The layout comes from the server's own resolver, not from a local
    // guess: the database FILE is the discriminator (a repository literally
    // named `loam` also has a `loam/` directory, and a bare `loam/` is not a
    // layout). Same rule as every one-shot path after finding #27.
    let layout = gardenbed_server::resolve_layout(&root);
    let meta_path = std::env::var("GARDENBED_KERNEL_META")
        .map(std::path::PathBuf::from)
        .unwrap_or(layout.metadata.clone());
    let objects_root = std::env::var("GARDENBED_KERNEL_OBJECTS")
        .map(std::path::PathBuf::from)
        .unwrap_or(layout.objects.clone());
    let objects = Arc::new(FsObjectStore::open(&objects_root).expect("open kernel object store"));
    let metadata = Arc::new(SqliteMetadataStore::open(&meta_path).expect("open kernel metadata"));

    let target = std::path::PathBuf::from("/tmp/m17-kernel-export.git");
    let started = std::time::Instant::now();
    let result = export_to_path(
        &target,
        objects.clone(),
        metadata.clone(),
        &ExportOptions {
            refs: vec!["refs/heads/master".to_owned()],
        },
    );
    let report = match result {
        Ok(report) => report,
        Err(gardenbed_git::ExportError::Changeset { detail, .. }) => {
            // v0.1 corpus: the export must refuse at the first commit whose
            // tree carries pre-canonical modes or non-UTF-8 names — loudly,
            // naming the divergence, never silently diverging (D20/D21;
            // docs/divergences.md §2/§4). Raw-tree capture stays gated on
            // the M18.5 kernel-throughput successor work.
            assert!(
                detail.contains("reconstruction diverges from the imported commit"),
                "unexpected refusal: {detail}"
            );
            eprintln!(
                "refused loudly as designed after {:.0}s: {detail}",
                started.elapsed().as_secs_f64()
            );
            return;
        }
        Err(e) => panic!("unexpected export error: {e:?}"),
    };
    eprintln!(
        "exported {} commits, {} blobs, {} refs in {:.1}s",
        report.commits_exported,
        report.blobs_exported,
        report.refs.len(),
        started.elapsed().as_secs_f64()
    );
    let mapped: u64 = {
        let conn = rusqlite::Connection::open(&meta_path).expect("open kernel metadata sqlite");
        conn.query_row("SELECT COUNT(*) FROM git_map", [], |r| r.get(0))
            .expect("count git_map rows")
    };
    assert_eq!(
        report.commits_exported, mapped,
        "exported commits == git_map rows (bijection)"
    );

    // 2. sampled SHAs: every 1000th exported commit must have its mapping
    // row equal to the computed SHA (the export already asserts this
    // internally — this re-checks against the store independently).
    let mut checked = 0u64;
    for (cs, sha) in report.commits.iter().step_by(1000) {
        let row = metadata
            .git_for_cs(*cs)
            .expect("git_for_cs")
            .unwrap_or_else(|| panic!("changeset {cs:?} has no mapping row"));
        assert_eq!(&row, sha, "mapping row disagrees for {cs:?}");
        checked += 1;
    }
    eprintln!("sampled {checked} mapping rows: all equal");

    // tip check: the exported master tip must equal the expected tip —
    // the known kernel mirror tip for the full corpus, or the segment's
    // own tip via GARDENBED_KERNEL_TIP for a --limit slice.
    let tip = report
        .refs
        .iter()
        .find(|(name, _)| name == "refs/heads/master")
        .expect("master exported")
        .1
        .clone();
    eprintln!("tip {tip}");
    let expected = std::env::var("GARDENBED_KERNEL_TIP")
        .unwrap_or_else(|_| "0fcb05b7b8b1b9c9ed54684807b35fa8b0f6c14f".to_owned());
    assert_eq!(tip, expected);
}
