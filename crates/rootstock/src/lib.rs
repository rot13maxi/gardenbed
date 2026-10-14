//! rootstock: the git import/export bridge.
//!
//! Import: git fetch → packfile parse (gitoxide) → changesets + blobs.
//! Export: changesets → derived trees → packfile → git push.
//! Differential tests against git's own answers are the trust anchor.
