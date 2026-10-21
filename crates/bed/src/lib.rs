//! Gardenbed's working-copy client and CLI.
//!
//! [`workspace::WorkingCopy`] owns local configuration, the tracked-file index,
//! object cache, and local changeset heads. [`status`] inspects working files
//! offline. [`rpc`] and [`ops`] query the repository server and materialize
//! selected sparse paths. [`commit`] records local changes on the server.
//!
//! State lives under `--state-dir` (default `./.bed`): `config.toml`,
//! `index.sqlite3`, `objects/`, and `heads`. Repository history is authoritative
//! on the server; the local object store is a cache.

pub mod cli;
pub mod commit;
pub mod config;
pub mod daemon;
pub mod error;
pub mod heads;
pub mod ignore;
#[cfg(unix)]
pub mod mount;
pub mod ops;
pub mod queue;
pub mod render;
pub mod revspec;
pub mod rpc;
pub mod sparse;
pub mod status;
pub mod sync;
pub mod treestate;
pub mod workspace;

mod checkout_fs;
