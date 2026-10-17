//! `fuzz_difftree_parse` — git_bridge's `git diff-tree -z --name-status`
//! batch parser fed random bytes (v0.1.md M8.3 "import parser" target).
//!
//! The input's first byte picks how many *expected* commit ids to allow
//! (0–7, built deterministically from 20-byte chunks of the input so the
//! fuzzer can reach the "id token" branch of the parser); the remainder is
//! fed to [`gardenbed_git::difftree::parse_batch`] verbatim. The
//! contract: any byte string parses to `Ok` (with only requested ids in the
//! map) or a descriptive `Err` — never a panic, never an accepted
//! unrequested id.

#![no_main]

use libfuzzer_sys::fuzz_target;
use gardenbed_git::difftree::parse_batch;
use gardenbed_git::oid::GitOid;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // Up to 7 expected ids, each hex-encoded from 20 raw input bytes.
    let want = (data[0] % 8) as usize;
    let mut expected = Vec::with_capacity(want);
    let mut pos = 1;
    for _ in 0..want {
        if pos + 20 > data.len() {
            break;
        }
        let raw: [u8; 20] = data[pos..pos + 20].try_into().expect("20-byte chunk");
        let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        expected.push(GitOid::from_hex(&hex).expect("hex of raw bytes parses"));
        pos += 20;
    }
    let out = &data[pos..];

    match parse_batch(out, &expected) {
        Ok(map) => {
            // Everything attributed must have been requested; requested
            // ids may legitimately be absent (empty diffs print nothing).
            for oid in map.keys() {
                assert!(
                    expected.contains(oid),
                    "parse_batch attributed records to an unrequested id"
                );
            }
        }
        Err(e) => {
            // Rejections must explain themselves; the message is required
            // to name the shape of the problem.
            assert!(!e.to_string().is_empty(), "parse_batch rejected with an empty error");
        }
    }
});
