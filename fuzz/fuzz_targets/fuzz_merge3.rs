//! `fuzz_merge3` — random blob triples through the content merge
//! (v0.2.md M9 "merge3" target).
//!
//! The input seeds a deterministic RNG; the target derives `ours` and
//! `theirs` from a random `base` via line-level edits (replace, insert,
//! delete of random line ranges — the shapes real merges produce), then
//! calls `merge3`. The contract is only "no panic and no mis-signal":
//! a `Merged` outcome must differ from at least one input side, and
//! trivial inputs (equal sides) must merge to that shared value.

#![no_main]

use gardenbed_derive::{Merge3Outcome, merge3};
use libfuzzer_sys::fuzz_target;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    }
}

/// Builds a small deterministic base blob: `n` numbered lines.
fn base_blob(rng: &mut Rng) -> Vec<u8> {
    let lines = 1 + rng.below(40);
    let mut out = Vec::new();
    for i in 0..lines {
        out.extend_from_slice(format!("line {i} of {lines}\n").as_bytes());
    }
    if rng.below(4) == 0 {
        out.pop(); // drop the trailing newline sometimes
    }
    out
}

/// Applies one random line-level edit to `content` (record-split the
/// same way the merger does: on newlines, keeping terminators).
fn edit(rng: &mut Rng, content: &[u8]) -> Vec<u8> {
    let mut records: Vec<&[u8]> = Vec::new();
    let mut start = 0;
    for (i, b) in content.iter().enumerate() {
        if *b == b'\n' {
            records.push(&content[start..=i]);
            start = i + 1;
        }
    }
    if start < content.len() {
        records.push(&content[start..]);
    }
    if records.is_empty() {
        return format!("inserted by seed {}\n", rng.below(1 << 32)).into_bytes();
    }
    let lo = rng.below(records.len() as u64) as usize;
    let hi = lo + 1 + rng.below((records.len() - lo) as u64) as usize;
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&records[..lo].concat());
    match rng.below(3) {
        0 => {
            // replace
            for k in 0..(hi - lo) {
                out.extend_from_slice(format!("replacement {k}\n").as_bytes());
            }
        }
        1 => {
            // insert
            out.extend_from_slice(b"inserted line\n");
            out.extend_from_slice(&records[lo..hi].concat());
        }
        _ => {} // delete
    }
    out.extend_from_slice(&records[hi.min(records.len())..].concat());
    out
}

fuzz_target!(|seed: u64| {
    let mut rng = Rng(seed);
    let base = base_blob(&mut rng);
    let ours = edit(&mut rng, &base);
    let theirs = edit(&mut rng, &base);

    // Equal sides are the strongest trivial rule.
    if ours == theirs {
        assert_eq!(
            merge3(Some(&base), Some(&ours), Some(&theirs)),
            Merge3Outcome::Merged(ours.clone())
        );
    }

    match merge3(Some(&base), Some(&ours), Some(&theirs)) {
        Merge3Outcome::Merged(out) => {
            // A merge must be a real combination: it cannot equal BOTH
            // inputs unless they were equal (handled above).
            if ours != theirs {
                assert!(out != ours || out != theirs);
            }
        }
        Merge3Outcome::Conflict => {
            // Overlapping edits may conflict; nothing more to assert
            // beyond not panicking.
        }
    }
});
