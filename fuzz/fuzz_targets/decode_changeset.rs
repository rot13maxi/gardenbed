#![no_main]

use gardenbed_model::Changeset;
use libfuzzer_sys::fuzz_target;

// The decoder's contract under arbitrary bytes: never panic, and never
// accept non-canonical input. Nightly escalation of the in-CI mutation
// tests — run with `cargo +nightly fuzz run decode_changeset` (1 CPU-hour
// per the M0 verification criteria).
fuzz_target!(|data: &[u8]| {
    if let Ok(cs) = Changeset::decode(data) {
        let re = cs
            .canonical_bytes()
            .expect("decoded changeset must re-encode");
        assert_eq!(re, data, "decoder accepted non-canonical bytes");
    }
});
