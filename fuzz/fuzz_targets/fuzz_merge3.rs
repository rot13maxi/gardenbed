//! Fuzz `merge3`: three byte-strings carved out of one input buffer.
//!
//! The gate is no-panic (merge3 must never panic on any byte strings,
//! including invalid UTF-8, CRLF mixes, missing trailing newlines and
//! NUL-prefixed binary). The weak content invariant — every output
//! line, modulo its trailing newline, must equal some input line —
//! catches gross output corruption on clean merges.

use gardenbed_derive::{Merge3Outcome, merge3};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Need at least 6 bytes: two 3-byte little-endian length fields
    // followed by some payload.
    if data.len() < 6 {
        return;
    }
    let n1 = u16::from_le_bytes([data[0], data[1]]) as usize;
    let n2 = u16::from_le_bytes([data[2], data[3]]) as usize;
    // Cap both lengths at the remaining input so the split always fits;
    // this biases toward small sides, which is where the interesting
    // boundary shapes live anyway.
    let rest = &data[4..];
    let n1 = n1.min(rest.len());
    let n2 = n2.min(rest.len() - n1);
    let base = &rest[..n1];
    let ours = &rest[n1..n1 + n2];
    let theirs = &rest[n1 + n2..];

    match merge3(base, ours, theirs) {
        Merge3Outcome::Merged(out) => {
            // Weak mix invariant: every output line (sans trailing
            // newline) appears as a line of one of the three inputs.
            for line in out.split(|b| *b == b'\n') {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if line.is_empty() {
                    continue;
                }
                let appears = |in_lines: &[u8]| {
                    in_lines
                        .split(|b| *b == b'\n')
                        .any(|l| l.strip_suffix(b"\r").unwrap_or(l) == line)
                };
                assert!(
                    appears(base) || appears(ours) || appears(theirs),
                    "merge3 invented a line not present in any input: {:?} (base {:?} ours {:?} theirs {:?})",
                    String::from_utf8_lossy(line),
                    String::from_utf8_lossy(base),
                    String::from_utf8_lossy(ours),
                    String::from_utf8_lossy(theirs),
                );
            }
        }
        Merge3Outcome::Conflict(_) => {}
    }
});
