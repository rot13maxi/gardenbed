#![no_main]

use gardenbed_model::Tree;
use libfuzzer_sys::fuzz_target;

// Same contract as decode_changeset, for trees.
fuzz_target!(|data: &[u8]| {
    if let Ok(t) = Tree::decode(data) {
        let re = t.canonical_bytes().expect("decoded tree must re-encode");
        assert_eq!(re, data, "decoder accepted non-canonical bytes");
    }
});
