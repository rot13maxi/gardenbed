//! gardenbed — the local state layer and CLI of **bed**, the tool you tend
//! your gardenbed with.
//!
//! bed is the client half of gardenbed (see `DESIGN.md`): a working copy
//! plus a cache, never a repository. Everything here is *local* — the
//! server-facing RPC read path is wired in a later wave, so every command
//! that needs the arboretum currently stops at [`crate::todo`-style] stubs
//! (see `src/main.rs::todo_rpc`).
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
//! treestate, hashing only suspects.

pub mod bed;
pub mod cli;
pub mod config;
pub mod error;
pub mod heads;
pub mod plot;
pub mod revspec;
pub mod status;
pub mod treestate;
