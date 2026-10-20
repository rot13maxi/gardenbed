//! gardenbed-harness — the differential-testing backbone for gardenbed.
//!
//! The harness generates deterministic synthetic histories as gardenbed
//! changesets ([`generate`]) and materializes the *same* history into a real
//! git repository using git plumbing ([`mod@materialize`]), so later milestones
//! can compare Gardenbed's answers against git as the oracle.
//!
//! Layout:
//!
//! - [`generator`]: seeded history generator + [`files_at`] replay.
//! - [`mod@materialize`]: renders changesets into a git repo via plumbing
//!   (`hash-object`, `update-index --index-info`, `write-tree`,
//!   `commit-tree`, `update-ref`).
//! - [`oracle`]: subprocess-git oracle wrappers (`show`, `ls-tree -r -z`,
//!   `diff-tree -r -z --name-status`, `blame --first-parent --porcelain`,
//!   `log`, `fsck`).
//!
//! Everything that shells out to git parses `-z` / porcelain formats with
//! zero assumptions about path shapes (spaces, non-ASCII bytes, leading
//! dashes are all handled byte-exactly).
//!
//! Determinism contract: `generate(seed, opts)` is a pure function of its
//! inputs — same seed + options ⇒ byte-identical changesets and blob
//! contents. `materialize` is likewise deterministic: running it twice on
//! the same history yields byte-identical git objects and identical SHAs.

pub mod generator;
pub mod materialize;
pub mod oracle;

pub use generator::{GenOptions, GeneratedHistory, files_at, format_tz, generate};
pub use materialize::{Materialized, materialize};
pub use oracle::{
    BlameRange, DiffEntry, LogEntry, git_blame_first_parent, git_diff_tree, git_fsck,
    git_hash_object, git_log, git_ls_tree_r, git_show,
};
