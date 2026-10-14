//! gardenbed — the local state layer and CLI of **bed**, the tool you tend
//! your gardenbed with.
//!
//! bed is the client half of gardenbed (see `DESIGN.md`): a working copy
//! plus a cache, never a repository. The crate splits into the offline
//! heart (config, treestate, status — everything that works with no
//! arboretum running) and the server-backed read path ([`rpc`], [`ops`]):
//! connect, resolve, want-list fetch, materialize.
//!
//! The local state (v0.1.md §6) lives under one directory, `--bed-dir`
//! (default `./.bed`):
//!
//! | File | Purpose | Module |
//! |---|---|---|
//! | `config.toml` | server, repo, plot, current changeset | [`config`] |
//! | `treestate` | sqlite index of the materialized plot | [`treestate`] |
//! | `objects/` | local CAS cache — loam's `FsObjectStore`, reused as-is | [`bed`] |
//! | `heads` | local stack tips, one hex id per line | [`heads`] |
//!
//! The offline heart of the crate is the status engine ([`status`]): it
//! compares the working files under the plot ([`plot`]) against the
//! treestate, hashing only suspects. The read path ([`rpc`], [`ops`]) keeps
//! that state true against the arboretum: every fetch is cache-first, so
//! interrupted commands resume exactly and reruns cost nothing.

pub mod bed;
pub mod cli;
pub mod config;
pub mod error;
pub mod heads;
pub mod ops;
pub mod plot;
pub mod render;
pub mod revspec;
pub mod rpc;
pub mod sow;
pub mod status;
pub mod sync;
pub mod treestate;
