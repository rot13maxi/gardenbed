//! `fuzz_mcp_line` — the MCP stdio line handler fed random bytes
//! (v0.1.md M8.3; the MCP surface must never panic on hostile input).
//!
//! Each input is lossily decoded to a `String` and run through
//! [`gardenbed_mcp::handle_line`] against a fixed, empty in-memory
//! arboretum. The contract: every line produces a protocol answer (or
//! `None` for a notification), never a panic — malformed JSON, wrong
//! shapes, hostile ids, and enormous limits must all land in JSON-RPC
//! errors or `isError` tool results.

#![no_main]

use std::sync::{Arc, OnceLock};

use libfuzzer_sys::fuzz_target;
use gardenbed_mcp::handle_line;
use gardenbed_server::Repo;
use gardenbed_store::{MemoryStore, MetadataStore, ObjectStore};

/// One shared empty arboretum over memory stores (the tools answer with
/// `isError` results on it; nothing panics).
fn repo() -> &'static Repo {
    static REPO: OnceLock<Repo> = OnceLock::new();
    REPO.get_or_init(|| {
        let memory = Arc::new(MemoryStore::new());
        let objects: Arc<dyn ObjectStore> = memory.clone();
        let metadata: Arc<dyn MetadataStore> = memory;
        Repo::new(objects, metadata)
    })
}

fuzz_target!(|data: &[u8]| {
    let line = String::from_utf8_lossy(data);
    let _ = handle_line(repo(), &line);
});
