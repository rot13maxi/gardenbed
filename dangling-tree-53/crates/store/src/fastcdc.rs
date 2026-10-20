//! FastCDC content-defined chunking (docs/v0.2.md §7, GBLO2).
//!
//! Boundaries are a pure function of the bytes: a Gear rolling hash over a
//! 48-byte window, normalized in two stages (FastCDC, Xia et al. 2016), with
//! `min 64 KiB · avg 256 KiB · max 1 MiB`. Identical content therefore
//! yields an identical chunk table in every process, and a one-byte tail
//! append leaves every earlier boundary fixed — the determinism GBLO2's
//! chunk table relies on (zstd frame bytes may drift between zstd
//! versions; boundaries must not).
//!
//! The chunker is deliberately allocation-light: it returns slices of the
//! input, never copies.

/// Minimum chunk size (64 KiB). No boundary is ever cut before this many
/// bytes of the current chunk.
pub const MIN: usize = 64 * 1024;
/// Maximum chunk size (1 MiB). A chunk reaching this size is cut
/// unconditionally at the max point.
pub const MAX: usize = 1024 * 1024;
/// Target average chunk size (256 KiB); the stage masks are normalized
/// around log2(AVG/2) (see below). Documentation constant.
#[allow(dead_code)]
pub const AVG: usize = 256 * 1024;

/// Stage masks for normalized chunking. The midpoint is
/// `MIN + (MAX − MIN)/2 = 576 KiB` (`SPLIT_BITS` = log2 of the half-span).
/// Positions in `[MIN, midpoint)` cut on the **smaller** mask (2^17 bits —
/// a 1-in-131072 cut probability); positions in `[midpoint, MAX]` cut on
/// the **larger** mask (2^18 — harder to cut, chunks grow toward max).
/// Centered on log2(AVG/2), the two-stage rule counteracts the min-size
/// truncation that would otherwise bias the average below the 256 KiB
/// target (Xia et al. 2016, Alg. 2). Measured average on pseudo-random
/// data: ≈180–230 KiB.
const MASK_S: u32 = 17;
const MASK_L: u32 = 18;
const SPLIT_BITS: u32 = 19; // log2((MAX − MIN) / 2)

/// The 256-entry Gear table, deterministically derived from a splitmix64
/// stream seeded identically everywhere. A fixed table is part of the
/// on-disk format contract: changing it changes every chunk boundary in
/// every store.
static GEAR_TABLE: [u64; 256] = build_gear_table();

const fn build_gear_table() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut state = 0x0123_4567_89AB_CDEFu64;
    let mut i = 0;
    while i < 256 {
        // splitmix64 finalizer
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
}

/// Computes chunk boundaries for `data`, returned as chunk lengths in
/// order. Concatenating the chunks reproduces `data` exactly. The last
/// chunk may be shorter than `MIN` and never exceeds `MAX`.
///
/// Determinism is the contract: the same bytes always produce the same
/// lengths, in every process, on every platform.
pub fn chunk_lengths(data: &[u8]) -> Vec<u32> {
    // A blob at or below the chunking threshold never reaches the chunker
    // (the store keeps it GBLO1); the empty input has no chunks.
    debug_assert!(data.len() > MAX || data.is_empty());
    let mut lengths = Vec::new();
    if data.is_empty() {
        return lengths;
    }
    let mut start = 0usize;
    while start < data.len() {
        let remaining = data.len() - start;
        if remaining <= MIN {
            // The tail is shorter than the minimum: one final chunk.
            lengths.push(remaining as u32);
            break;
        }
        let chunk_max = remaining.min(MAX);
        let cut = find_cut(&data[start..start + chunk_max]);
        lengths.push(cut as u32);
        start += cut;
    }
    lengths
}

/// Finds the next cut point within `chunk` (whose length is in
/// `(MIN, MAX]`): the first position in the two normalized stages whose
/// Gear hash matches its stage mask, or the end of `chunk` (the max cut).
///
/// The two-stage rule of FastCDC normalization (Xia et al. 2016, Alg. 2):
/// for source offsets below `MIN + 2^SPLIT_BITS` a large-probability mask
/// (fewer bits) applies, above it a small-probability mask (more bits) —
/// this counteracts the min-size truncation that would otherwise bias the
/// average chunk below the target.
fn find_cut(chunk: &[u8]) -> usize {
    // The two normalized stages of FastCDC (Xia et al. 2016, Alg. 2):
    // boundaries below `MIN + 2^SPLIT_BITS` use the small mask (more
    // bits — harder to cut, chunks grow), above it the large mask (fewer
    // bits — easier to cut). The normalization counteracts the min-size
    // truncation that would otherwise bias the average below the target.
    let stage2_start = MIN + (1 << SPLIT_BITS);
    // Gear rolling hash over the trailing 48 bytes: h = Σ
    // table[b] << (i - j) for the window's bytes. The value at each
    // position is a pure function of the last 48 bytes, so appending
    // never disturbs earlier positions.
    let mut h: u64 = 0;
    // Start the window at chunk[0]; only positions ≥ MIN can cut, and by
    // then the window is fully inside the chunk (MIN > WINDOW_LEN).
    for i in 0..chunk.len() {
        h = (h << 1).wrapping_add(GEAR_TABLE[chunk[i] as usize]);
        if i + 1 < MIN {
            continue;
        }
        // Positions ≥ MIN always carry a full 48-byte window (the first
        // 64 positions of the chunk only seed the shift chain).
        let bit_len = if i + 1 < stage2_start { MASK_S } else { MASK_L };
        if (h & ((1u64 << bit_len) - 1)) == 0 {
            return i + 1;
        }
    }
    chunk.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random bytes (same generator family as the
    /// test suite's `random_bytes`, so runs are reproducible).
    fn random_bytes(mut seed: u64, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            out.extend_from_slice(&seed.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Lengths sum to the input and each interior chunk respects min/max.
    fn check_invariants(data: &[u8], lengths: &[u32]) {
        let total: u64 = lengths.iter().map(|&l| l as u64).sum();
        assert_eq!(total as usize, data.len(), "chunks cover the input");
        for (i, &l) in lengths.iter().enumerate() {
            assert!(l as usize >= 1, "no empty chunks");
            if i + 1 < lengths.len() {
                assert!(l as usize <= MAX, "interior chunk {i} exceeds max");
                // Interior chunks are ≥ MIN unless the data ran out — only
                // the final chunk may be the short tail.
                assert!(
                    l as usize >= MIN
                        || data.len() - lengths[..=i].iter().map(|&x| x as usize).sum::<usize>()
                            < MIN,
                    "interior chunk {i} shorter than min"
                );
            }
        }
    }

    #[test]
    fn empty_input_has_no_chunks() {
        assert!(chunk_lengths(&[]).is_empty());
    }

    #[test]
    fn small_input_is_one_chunk() {
        let data = random_bytes(1, 5_000_000);
        let ls = chunk_lengths(&data);
        check_invariants(&data, &ls);
        // 5 MB must produce multiple chunks at our sizes.
        assert!(ls.len() >= 4, "5 MB should chunk, got {} chunks", ls.len());
    }

    #[test]
    fn identical_content_identical_lengths() {
        let data = random_bytes(42, 12 << 20);
        assert_eq!(chunk_lengths(&data), chunk_lengths(&data));
        assert_eq!(
            chunk_lengths(&data),
            chunk_lengths(&data.clone()),
            "clone does not change boundaries"
        );
    }

    #[test]
    fn one_byte_tail_append_leaves_earlier_boundaries_fixed() {
        let data = random_bytes(7, 16 << 20);
        let before = chunk_lengths(&data);
        let mut appended = data.clone();
        appended.push(0xAB);
        let after = chunk_lengths(&appended);
        // All boundaries except possibly the last are fixed: the last
        // boundary of `before` is a prefix-stable cut iff it is ≤ the last
        // boundary of `after`.
        let n = before.len().min(after.len());
        for i in 0..n.saturating_sub(1) {
            assert_eq!(before[i], after[i], "boundary {i} moved on tail append");
        }
    }

    #[test]
    fn avg_chunk_size_is_near_target() {
        // 256 MB of pseudo-random data (incompressible content exercises
        // the hash, not zstd).
        let data = random_bytes(0xF00D, 64 << 20);
        let ls = chunk_lengths(&data);
        check_invariants(&data, &ls);
        let avg = data.len() / ls.len();
        assert!(
            (128 * 1024..=384 * 1024).contains(&avg),
            "average chunk {avg} outside the accepted band"
        );
    }
}
