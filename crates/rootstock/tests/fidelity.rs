//! Content fidelity: full sweeps of imported histories against git.

mod common;

use common::{import, sweep, Fixture};
use gardenbed_harness::GenOptions;

fn base_opts() -> GenOptions {
    GenOptions {
        commits: 60,
        merge_ratio: 0.15,
        octopus_ratio: 0.05,
        rename_ratio: 0.2,
        delete_ratio: 0.1,
        ..GenOptions::default()
    }
}

/// Three seeded histories (linear + merges + octopus + renames): every
/// changeset × every path is compared against git (`ls-tree -r`,
/// `cat-file` byte-exact) and the generator.
#[test]
fn content_fidelity_three_seeded_histories() {
    for seed in [7u64, 4242, 918_273] {
        let fixture = Fixture::generate(seed, &base_opts());
        let report = import(&fixture.repo, &fixture.garden, &Default::default());
        assert_eq!(report.commits_new as usize, fixture.history.changesets.len());
        assert_eq!(report.commits_skipped, 0);
        sweep(&fixture, &fixture.garden);
    }
}

/// Edge-case options: unicode paths, exec bits, symlinks, CRLF content,
/// empty files, occasional big blobs.
#[test]
fn content_fidelity_edge_case_content() {
    let opts = GenOptions {
        commits: 50,
        merge_ratio: 0.1,
        octopus_ratio: 0.02,
        rename_ratio: 0.15,
        delete_ratio: 0.1,
        unicode_paths: true,
        crlf_content: true,
        empty_files: true,
        big_blob_bytes: Some(64 * 1024),
        ..GenOptions::default()
    };
    let fixture = Fixture::generate(31337, &opts);
    let report = import(&fixture.repo, &fixture.garden, &Default::default());
    assert_eq!(report.commits_new as usize, fixture.history.changesets.len());
    sweep(&fixture, &fixture.garden);
}

/// A deeper linear history to exercise a longer first-parent chain.
#[test]
fn content_fidelity_long_linear_history() {
    let opts = GenOptions {
        commits: 200,
        merge_ratio: 0.0,
        rename_ratio: 0.15,
        delete_ratio: 0.1,
        max_files: 30,
        ..GenOptions::default()
    };
    let fixture = Fixture::generate(8675309, &opts);
    let report = import(&fixture.repo, &fixture.garden, &Default::default());
    assert_eq!(report.commits_new as usize, fixture.history.changesets.len());
    sweep(&fixture, &fixture.garden);
}
