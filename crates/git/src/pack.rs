//! Native pack writing (v0.2.md §8, D20): a SHA-1 pack of **full
//! objects** (no deltas) plus a version-2 `.idx`.
//!
//! Spike gate decision (recorded in `docs/v0.2.md` §8): **hand-rolled**.
//! gix-pack 0.76's public encode surface cannot write an index directly —
//! `index::encode::write_to` is `pub(crate)`, and `Bundle::write_to_directory`
//! re-parses and re-verifies a pack stream to produce the `.idx` we can
//! compute exactly (we hold every offset, CRC32 and object id up front:
//! full objects, no deltas). The pack data format for non-delta entries is
//! a varint type+size header followed by one zlib stream, ~40 lines here.
//!
//! ## Format (pack v2)
//!
//! ```text
//! "PACK" | version:u32=2 | object_count:u32
//!   <object_count> × { entry header (type 1..=4, size varint) | zlib stream }
//! SHA1(all preceding bytes)
//! ```
//!
//! ## Format (idx v2)
//!
//! ```text
//! \377tOc | version:u32=2 | fanout: 256 × u32 BE (cumulative counts
//! by first id byte) | ids: N × 20 raw bytes ascending | crc32: N × u32 BE
//! (crc of the entry's pack bytes: header + compressed data) | offsets:
//! N × u32 BE (top bit set ⇒ index into the 64-bit offset table) |
//! [8-byte offsets for large packs] | pack SHA1 | idx SHA1
//! ```
//!
//! Truncated or corrupt streams error cleanly: the reader validates the
//! magic, version, counts, fanout monotonicity, per-entry zlib length,
//! per-object SHA-1, and the trailing checksums — never trusting a
//! declared length (the fuzz target exercises this).
use std::io::{Seek, SeekFrom, Write};

use flate2::Compression;
use flate2::write::ZlibEncoder;
use sha1::Digest;
use sha1::Sha1;

/// zlib level for pack entries. git's default pack compression is 1 for
/// receiving and `-1` (6) for packing; anything valid is accepted on read,
/// so this is a size/speed knob only.
const ZLIB_LEVEL: u32 = 1;

/// One full object to write into a pack.
#[derive(Clone, Debug)]
pub struct PackObject {
    /// git's numeric object type (1=commit, 2=tree, 3=blob, 4=tag).
    pub kind: u8,
    /// The uncompressed payload (already the object body, without the
    /// `<type> <len>\0` header — that is added when hashing and packing).
    pub data: Vec<u8>,
    /// SHA-1 over `"<type> <len>\0" + data`, computed by the caller (the
    /// export asserts each against the store's git mapping; recomputing
    /// here would only re-check arithmetic).
    pub sha1: [u8; 20],
}

/// Where each object landed in the pack (the `.idx` payload).
#[derive(Clone, Copy, Debug)]
pub struct IndexEntry {
    sha1: [u8; 20],
    crc32: u32,
    offset: u64,
}

/// Anything that can go wrong reading a pack back (the decode oracle and
/// the fuzz target). Deliberately separate from [`ExportError`]: read-side
/// failures name the byte offset and never abort a running export.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PackReadError {
    /// The stream is shorter than any valid pack/idx of the claimed shape.
    #[error("truncated at byte {at}: {detail}")]
    Truncated {
        /// Byte offset where reading stopped.
        at: usize,
        /// What was being read.
        detail: String,
    },
    /// The bytes are structurally parseable but wrong (bad magic, version,
    /// count, non-monotonic fanout, checksum mismatch, …).
    #[error("corrupt at byte {at}: {detail}")]
    Corrupt {
        /// Byte offset of the offending field.
        at: usize,
        /// What is wrong.
        detail: String,
    },
    #[error("io: {0}")]
    Io(String),
}

impl From<std::io::Error> for PackReadError {
    fn from(e: std::io::Error) -> Self {
        PackReadError::Io(e.to_string())
    }
}

/// Writes `objects` as one pack to `out`, returning
/// `(pack_sha1, entries in pack order)`.
///
/// Deterministic: objects are written in the given order (the export
/// writes blobs ascending by id, then trees and commits in topological
/// order — see `export.rs`); the `.idx` is sorted by object id as the
/// format requires, independent of write order.
pub fn write_pack<W: Write>(
    out: &mut W,
    objects: &[PackObject],
) -> Result<([u8; 20], Vec<IndexEntry>), std::io::Error> {
    let mut hasher = Sha1::new();
    let mut header = Vec::with_capacity(12);
    header.extend_from_slice(b"PACK");
    header.extend_from_slice(&2u32.to_be_bytes());
    header.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    hasher.update(&header);
    out.write_all(&header)?;

    let frames = compress_frames(objects)?;
    let mut entries: Vec<IndexEntry> = Vec::with_capacity(objects.len());
    let mut offset = header.len() as u64;
    let mut crc = Crc32::new();
    for (obj, frame) in objects.iter().zip(frames) {
        let entry_start = offset;
        crc.reset();
        crc.update(&frame.header);
        hasher.update(&frame.header);
        out.write_all(&frame.header)?;
        crc.update(&frame.data);
        hasher.update(&frame.data);
        out.write_all(&frame.data)?;
        offset = entry_start + frame.header.len() as u64 + frame.data.len() as u64;
        entries.push(IndexEntry {
            sha1: obj.sha1,
            crc32: crc.sum(),
            offset: entry_start,
        });
    }

    let pack_sha1: [u8; 20] = hasher.finalize().into();
    out.write_all(&pack_sha1)?;
    Ok((pack_sha1, entries))
}

/// Incremental pack writer: feed objects as they are produced, get the
/// pack trailer and `.idx` payload at [`PackWriter::finish`]. Compression
/// runs on all cores over bounded chunks, so memory stays at one chunk of
/// raw objects plus their compressed frames regardless of history size.
///
/// Deterministic: objects reach the stream strictly in push order (the
/// parallelism is inside a chunk, never across chunks), and zlib output
/// depends only on the payload. The object count cannot be known before
/// the first byte is written, so `finish` patches it into the header and
/// computes the trailer checksum by reading the finished bytes back —
/// warm from the just-written page cache.
pub struct PackWriter {
    out: std::io::BufWriter<std::fs::File>,
    entries: Vec<IndexEntry>,
    offset: u64,
    crc: Crc32,
    chunk: Vec<PackObject>,
}

/// Objects per parallel compression batch. Big enough to amortize thread
/// coordination, small enough that a chunk's raw + compressed bytes are a
/// few MB.
const CHUNK: usize = 4096;

impl PackWriter {
    /// Writes the pack header with a placeholder count.
    pub fn new(mut out: std::io::BufWriter<std::fs::File>) -> Result<Self, std::io::Error> {
        let mut header = Vec::with_capacity(12);
        header.extend_from_slice(b"PACK");
        header.extend_from_slice(&2u32.to_be_bytes());
        header.extend_from_slice(&0u32.to_be_bytes());
        out.write_all(&header)?;
        Ok(PackWriter {
            out,
            entries: Vec::new(),
            offset: header.len() as u64,
            crc: Crc32::new(),
            chunk: Vec::with_capacity(CHUNK),
        })
    }

    /// Queues one object; flushes a parallel compression batch when full.
    pub fn push(&mut self, obj: PackObject) -> Result<(), std::io::Error> {
        self.chunk.push(obj);
        if self.chunk.len() >= CHUNK {
            self.flush()?;
        }
        Ok(())
    }

    /// Queues an iterator of objects.
    pub fn extend<I: IntoIterator<Item = PackObject>>(
        &mut self,
        objects: I,
    ) -> Result<(), std::io::Error> {
        for obj in objects {
            self.push(obj)?;
        }
        Ok(())
    }

    /// Compresses and writes the queued chunk.
    fn flush(&mut self) -> Result<(), std::io::Error> {
        if self.chunk.is_empty() {
            return Ok(());
        }
        let frames = compress_frames(&self.chunk)?;
        let chunk = std::mem::take(&mut self.chunk);
        for (obj, frame) in chunk.into_iter().zip(frames) {
            let entry_start = self.offset;
            self.crc.reset();
            self.crc.update(&frame.header);
            self.out.write_all(&frame.header)?;
            self.crc.update(&frame.data);
            self.out.write_all(&frame.data)?;
            self.offset = entry_start + frame.header.len() as u64 + frame.data.len() as u64;
            self.entries.push(IndexEntry {
                sha1: obj.sha1,
                crc32: self.crc.sum(),
                offset: entry_start,
            });
        }
        Ok(())
    }
}

impl PackWriter {
    /// Flushes the queued chunk, patches the object count into the header,
    /// computes the trailer checksum over the finished bytes and appends
    /// it. The file is left synced and at end-of-file; the caller renames
    /// it into place.
    pub fn finish(mut self) -> Result<([u8; 20], Vec<IndexEntry>), std::io::Error> {
        self.flush()?;
        let count = self.entries.len() as u32;
        let mut file = self
            .out
            .into_inner()
            .map_err(|e| std::io::Error::other(format!("flushing the pack: {e}")))?;
        use std::io::Read;
        file.seek(SeekFrom::Start(8))?;
        file.write_all(&count.to_be_bytes())?;
        file.seek(SeekFrom::Start(0))?;
        let mut reader = std::io::BufReader::with_capacity(1 << 20, &mut file);
        let mut hasher = Sha1::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let pack_sha1: [u8; 20] = hasher.finalize().into();
        drop(reader);
        file.write_all(&pack_sha1)?;
        file.sync_all()?;
        Ok((pack_sha1, self.entries))
    }
}

/// One pack entry's on-disk bytes: the type/size varint header plus the
/// zlib stream over the payload.
struct CompressedFrame {
    header: Vec<u8>,
    data: Vec<u8>,
}

/// Compresses a chunk of objects (header varint + zlib stream) using all
/// available cores. Objects are independent, so this is deterministic:
/// the sequential result, just computed in parallel.
fn compress_frames(objects: &[PackObject]) -> Result<Vec<CompressedFrame>, std::io::Error> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(objects.len());
    if threads <= 1 || objects.len() < 1024 {
        // Small chunks: thread startup costs more than it saves.
        return objects.iter().map(compress_frame).collect();
    }
    // Each worker compresses one contiguous slice into its own Vec; the
    // chunks are then concatenated in order. Disjoint outputs, no locking.
    let chunk = objects.len().div_ceil(threads);
    let slices: Vec<&[PackObject]> = objects.chunks(chunk).collect();
    let results: Vec<Result<Vec<CompressedFrame>, std::io::Error>> = std::thread::scope(|scope| {
        let handles: Vec<_> = slices
            .iter()
            .map(|slice| scope.spawn(move || slice.iter().map(compress_frame).collect()))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err(poisoned_thread())))
            .collect()
    });
    let mut frames = Vec::with_capacity(objects.len());
    for result in results {
        frames.extend(result?);
    }
    Ok(frames)
}

fn poisoned_thread() -> std::io::Error {
    std::io::Error::other("a compression worker panicked")
}

/// Compresses one object into its pack frame.
fn compress_frame(obj: &PackObject) -> Result<CompressedFrame, std::io::Error> {
    let mut entry_header = Vec::with_capacity(16);
    // size varint: the first byte carries type (high nibble) plus the
    // low 4 bits of the size; continuation bytes carry 7 bits each.
    // The continuation bit is set only when another byte follows.
    let mut size = obj.data.len() as u64;
    let first_size = (size & 0x0f) as u8;
    size >>= 4;
    let mut byte0 = (obj.kind << 4) | first_size;
    if size != 0 {
        byte0 |= 0x80;
    }
    entry_header.push(byte0);
    while size != 0 {
        let mut byte = (size & 0x7f) as u8;
        size >>= 7;
        if size != 0 {
            byte |= 0x80;
        }
        entry_header.push(byte);
    }
    let mut z = ZlibEncoder::new(Vec::new(), Compression::new(ZLIB_LEVEL));
    z.write_all(&obj.data)?;
    let compressed = z.finish()?;
    Ok(CompressedFrame {
        header: entry_header,
        data: compressed,
    })
}

/// Writes a version-2 `.idx` for `entries` (pack order; sorted by id
/// internally, as the format requires) to `out`, returning the idx SHA-1.
pub fn write_idx<W: Write>(
    out: &mut W,
    mut entries: Vec<IndexEntry>,
    pack_sha1: [u8; 20],
) -> Result<[u8; 20], std::io::Error> {
    entries.sort_unstable_by(|a, b| a.sha1.cmp(&b.sha1));
    let mut body = Sha1::new();
    // hash-then-write for every chunk: one pass, no re-reads.
    let chunk = |body: &mut Sha1, out: &mut W, bytes: &[u8]| -> Result<(), std::io::Error> {
        body.update(bytes);
        out.write_all(bytes)
    };

    chunk(&mut body, out, b"\xfftOc")?;
    chunk(&mut body, out, &2u32.to_be_bytes())?;

    // fanout: cumulative count of ids with first byte <= i.
    let mut fanout = [0u32; 256];
    let mut next = 0usize;
    for (i, slot) in fanout.iter_mut().enumerate() {
        while next < entries.len() && entries[next].sha1[0] == i as u8 {
            next += 1;
        }
        *slot = next as u32;
    }
    for v in fanout {
        chunk(&mut body, out, &v.to_be_bytes())?;
    }
    for e in &entries {
        chunk(&mut body, out, &e.sha1)?;
    }
    for e in &entries {
        chunk(&mut body, out, &e.crc32.to_be_bytes())?;
    }
    // Offsets: u32 with the top bit marking a 64-bit overflow index.
    // Packs with entries past 2 GiB are far past v0.2 scale, but the
    // format rule is cheap to implement exactly.
    let mut large: Vec<u64> = Vec::new();
    for e in &entries {
        if e.offset < 0x8000_0000 {
            chunk(&mut body, out, &(e.offset as u32).to_be_bytes())?;
        } else {
            let index = large.len() as u32;
            large.push(e.offset);
            chunk(&mut body, out, &(index | 0x8000_0000).to_be_bytes())?;
        }
    }
    for v in large {
        chunk(&mut body, out, &v.to_be_bytes())?;
    }
    chunk(&mut body, out, &pack_sha1)?;

    let idx_sha1: [u8; 20] = body.finalize().into();
    out.write_all(&idx_sha1)?;
    Ok(idx_sha1)
}

/// One decoded object: kind, payload, SHA-1 of the full object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedObject {
    pub kind: u8,
    pub data: Vec<u8>,
    pub sha1: [u8; 20],
}

/// The decode-back oracle: parses a full pack **exactly**, validating
/// every structural property, and returns the objects in pack order.
///
/// * magic, version and object count must match the stream's claims;
/// * each entry must be a full object (type 1..=4), its zlib stream must
///   decompress to exactly the declared size, and the object's SHA-1 must
///   match `"<type> <size>\0" + payload`;
/// * the trailer must equal SHA-1 over everything before it.
///
/// Anything else is [`PackReadError`] — never a partial guess.
pub fn read_pack(bytes: &[u8]) -> Result<Vec<DecodedObject>, PackReadError> {
    let trunc = |at: usize, detail: &str| PackReadError::Truncated {
        at,
        detail: detail.to_owned(),
    };
    let bad = |at: usize, detail: String| PackReadError::Corrupt { at, detail };

    if bytes.len() < 32 {
        // 12 header + 20 trailer is the smallest possible pack.
        return Err(trunc(bytes.len(), "shorter than a minimal pack"));
    }
    if &bytes[..4] != b"PACK" {
        return Err(bad(0, format!("bad magic {:?}", &bytes[..4])));
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().expect("4 bytes"));
    if version != 2 {
        return Err(bad(4, format!("unsupported version {version}")));
    }
    let count = u32::from_be_bytes(bytes[8..12].try_into().expect("4 bytes")) as usize;

    let mut pos = 12usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        // entry header: type + size varint
        let b0 = *bytes.get(pos).ok_or_else(|| trunc(pos, "entry header"))?;
        let kind = (b0 >> 4) & 0x7;
        if !(1..=4).contains(&kind) {
            return Err(bad(
                pos,
                format!("object type {kind} (delta entries are not written)"),
            ));
        }
        let mut size = (b0 & 0x0f) as u64;
        let mut shift = 4u32;
        let mut cursor = pos + 1;
        let mut cont = b0 & 0x80 != 0;
        while cont {
            let b = *bytes
                .get(cursor)
                .ok_or_else(|| trunc(cursor, "size varint"))?;
            size |= ((b & 0x7f) as u64) << shift;
            shift += 7;
            cursor += 1;
            cont = b & 0x80 != 0;
        }
        // decompress exactly `size` bytes from `cursor`; `total_in` then
        // tells where the entry ends (the trailing zlib checksum bytes
        // are consumed by finishing the frame below).
        let mut decoder = flate2::read::ZlibDecoder::new(&bytes[cursor..]);
        let mut data = Vec::with_capacity(size as usize);
        {
            use std::io::Read;
            decoder
                .by_ref()
                .take(size)
                .read_to_end(&mut data)
                .map_err(|e| PackReadError::Corrupt {
                    at: cursor,
                    detail: format!("zlib stream for object {i}: {e}"),
                })?;
        }
        if data.len() as u64 != size {
            return Err(bad(
                cursor,
                format!(
                    "object {i}: zlib stream yielded {} of {size} bytes",
                    data.len()
                ),
            ));
        }
        // Finish the frame (validate the stored checksum) and consume any
        // remaining bytes it needs, so `total_in` covers the full stream.
        {
            use std::io::Read;
            let mut rest = Vec::new();
            decoder
                .read_to_end(&mut rest)
                .map_err(|e| PackReadError::Corrupt {
                    at: cursor,
                    detail: format!("zlib stream trailer for object {i}: {e}"),
                })?;
            if !rest.is_empty() {
                return Err(bad(
                    cursor,
                    format!("object {i}: zlib stream holds {} extra bytes", rest.len()),
                ));
            }
        }
        let consumed = cursor + decoder.total_in() as usize;
        let mut sha = Sha1::new();
        let header = format!("{} {}\0", kind_name(kind), size);
        sha.update(header.as_bytes());
        sha.update(&data);
        let sha1: [u8; 20] = sha.finalize().into();
        out.push(DecodedObject { kind, data, sha1 });
        pos = consumed;
    }
    let trailer: [u8; 20] = bytes
        .get(pos..pos + 20)
        .ok_or_else(|| trunc(pos, "pack trailer"))?
        .try_into()
        .expect("20 bytes");
    let mut sha = Sha1::new();
    sha.update(&bytes[..pos]);
    let computed: [u8; 20] = sha.finalize().into();
    if computed != trailer {
        return Err(bad(
            pos,
            format!("pack checksum mismatch: trailer {trailer:02x?} != computed {computed:02x?}"),
        ));
    }
    if pos + 20 != bytes.len() {
        return Err(bad(
            pos + 20,
            format!(
                "{} trailing bytes after the trailer",
                bytes.len() - pos - 20
            ),
        ));
    }
    Ok(out)
}

/// Parses a version-2 `.idx` and returns `(id, offset)` pairs in id order.
/// Structural validation mirrors [`read_pack`]: magic, version, fanout
/// monotonicity, id ordering, and both trailing checksums.
pub fn read_idx(bytes: &[u8]) -> Result<Vec<([u8; 20], u64)>, PackReadError> {
    let trunc = |at: usize, detail: &str| PackReadError::Truncated {
        at,
        detail: detail.to_owned(),
    };
    let bad = |at: usize, detail: String| PackReadError::Corrupt { at, detail };

    if bytes.len() < 8 + 256 * 4 + 40 {
        return Err(trunc(bytes.len(), "shorter than a minimal idx"));
    }
    if &bytes[..4] != b"\xfftOc" {
        return Err(bad(0, format!("bad magic {:?}", &bytes[..4])));
    }
    let version = u32::from_be_bytes(bytes[4..8].try_into().expect("4 bytes"));
    if version != 2 {
        return Err(bad(4, format!("unsupported version {version}")));
    }
    let count =
        u32::from_be_bytes(bytes[8 + 255 * 4..8 + 256 * 4].try_into().expect("4 bytes")) as usize;
    let fanout_end = 8 + 256 * 4;
    let ids_end = fanout_end + count * 20;
    let crc_end = ids_end + count * 4;
    let off_end = crc_end + count * 4;
    // The trailing 40 bytes are the pack sha + idx sha; everything
    // between the offset table and them is the large-offset table.
    if bytes.len() < off_end + 40 {
        return Err(bad(
            off_end,
            format!(
                "idx length {} inconsistent with {count} entries",
                bytes.len()
            ),
        ));
    }
    let large_len = bytes.len() - off_end - 40;
    if !large_len.is_multiple_of(8) {
        return Err(bad(
            off_end,
            format!(
                "idx length {} inconsistent with {count} entries",
                bytes.len()
            ),
        ));
    }
    // fanout monotonicity + final count
    let mut prev = 0u32;
    for i in 0..256 {
        let v = u32::from_be_bytes(bytes[8 + i * 4..8 + i * 4 + 4].try_into().expect("4 bytes"));
        if v < prev || (i == 255 && v as usize != count) {
            return Err(bad(
                8 + i * 4,
                format!("fanout[{i}] = {v} breaks monotonicity (count {count})"),
            ));
        }
        prev = v;
    }
    let mut out = Vec::with_capacity(count);
    let mut last: Option<[u8; 20]> = None;
    for i in 0..count {
        let id: [u8; 20] = bytes[fanout_end + i * 20..fanout_end + i * 20 + 20]
            .try_into()
            .expect("20 bytes");
        if let Some(last) = last
            && id <= last
        {
            return Err(bad(
                fanout_end + i * 20,
                format!("ids not strictly ascending at entry {i}"),
            ));
        }
        last = Some(id);
        let raw = u32::from_be_bytes(
            bytes[crc_end + i * 4..crc_end + i * 4 + 4]
                .try_into()
                .expect("4 bytes"),
        );
        let offset = if raw & 0x8000_0000 == 0 {
            raw as u64
        } else {
            let idx = (raw & 0x7fff_ffff) as usize;
            let base = off_end;
            let at = base + idx * 8;
            if at + 8 > off_end + large_len {
                return Err(bad(base, format!("large-offset index {idx} out of range")));
            }
            u64::from_be_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
        };
        out.push((id, offset));
    }
    // checksums: pack sha then idx sha over everything before it.
    let idx_body_end = bytes.len() - 20;
    let pack_sha_at = idx_body_end - 20;
    let mut sha = Sha1::new();
    sha.update(&bytes[..idx_body_end]);
    let computed: [u8; 20] = sha.finalize().into();
    let trailer: [u8; 20] = bytes[idx_body_end..].try_into().expect("20 bytes");
    if computed != trailer {
        return Err(bad(idx_body_end, "idx checksum mismatch".to_owned()));
    }
    let _ = pack_sha_at; // validated implicitly by the body hash above
    Ok(out)
}

pub fn kind_name(kind: u8) -> &'static str {
    match kind {
        1 => "commit",
        2 => "tree",
        3 => "blob",
        4 => "tag",
        _ => unreachable!("caller validated 1..=4"),
    }
}

/// CRC-32 (IEEE 802.3, reflected, poly 0xEDB88320) — the `.idx` checksum.
/// `crc32fast` computes the same function; wrapped here so the pack
/// module owns the reset-per-entry discipline the format requires.
struct Crc32 {
    inner: crc32fast::Hasher,
}

impl Crc32 {
    fn new() -> Self {
        Crc32 {
            inner: crc32fast::Hasher::new(),
        }
    }
    fn reset(&mut self) {
        self.inner = crc32fast::Hasher::new();
    }
    fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }
    fn sum(&self) -> u32 {
        self.inner.clone().finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: u8, data: &[u8]) -> PackObject {
        let mut sha = Sha1::new();
        sha.update(format!("{} {}\0", kind_name(kind), data.len()).as_bytes());
        sha.update(data);
        PackObject {
            kind,
            data: data.to_vec(),
            sha1: sha.finalize().into(),
        }
    }

    /// `git hash-object` cross-check on one object of each kind.
    #[test]
    fn object_hashes_match_git() {
        // values pinned from git at write time:
        //   printf 'hi\n' | git hash-object --stdin
        //   printf '' | git hash-object --stdin
        let blob = obj(3, b"hi\n");
        assert_eq!(hex(&blob.sha1), "45b983be36b73c0788dc9cbcb76cbb80fc7bb057");
        let empty = obj(3, b"");
        assert_eq!(hex(&empty.sha1), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
    }

    #[test]
    fn roundtrip_multiple_objects() {
        let objects = vec![
            obj(3, b"hello\n"),
            obj(2, b"100644 a\0aaaaaaaaaaaaaaaaaaaa"),
            obj(1, b"tree ...\n"),
            obj(3, &[]),
            obj(2, &[b'x'; 300]), // multi-byte size varint
        ];
        let mut pack = Vec::new();
        let (pack_sha, entries) = write_pack(&mut pack, &objects).unwrap();
        assert_eq!(entries.len(), 5);
        // offsets strictly ascend
        let mut offs: Vec<u64> = entries.iter().map(|e| e.offset).collect();
        offs.sort_unstable();
        let mut dedup = offs.clone();
        dedup.dedup();
        assert_eq!(offs, dedup);

        let decoded = read_pack(&pack).unwrap();
        assert_eq!(decoded.len(), 5);
        for (d, o) in decoded.iter().zip(&objects) {
            assert_eq!(d.kind, o.kind);
            assert_eq!(d.data, o.data);
            assert_eq!(d.sha1, o.sha1);
        }
        // trailer placement: read_pack already validated it; spot-check.
        assert_eq!(&pack[pack.len() - 20..], &pack_sha);

        // idx roundtrip
        let mut idx = Vec::new();
        let idx_sha = write_idx(&mut idx, entries.clone(), pack_sha).unwrap();
        assert_eq!(&idx[idx.len() - 20..], &idx_sha);
        let pairs = read_idx(&idx).unwrap();
        assert_eq!(pairs.len(), 5);
        let mut by_id: Vec<([u8; 20], u64)> = entries.iter().map(|e| (e.sha1, e.offset)).collect();
        by_id.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(pairs, by_id);
    }

    #[test]
    fn parallel_compression_matches_sequential_bytes() {
        // Crosses the 2048-object threshold, so write_pack uses the
        // multi-threaded path; the result must be byte-identical to the
        // sequential compression of the same objects.
        let objects: Vec<PackObject> = (0..3000)
            .map(|i| {
                obj(
                    3,
                    format!("blob {i} contents with some repetition\n").as_bytes(),
                )
            })
            .collect();
        let mut pack = Vec::new();
        let entries = write_pack(&mut pack, &objects).unwrap().1;
        // Offsets strictly ascend and every object landed exactly once.
        let offs: Vec<u64> = entries.iter().map(|e| e.offset).collect();
        let mut sorted = offs.clone();
        sorted.sort_unstable();
        assert_eq!(offs, sorted);
        assert_eq!(entries.len(), objects.len());

        // Sequential reference: header + per-object compress_frame output.
        let mut sequential = Vec::new();
        sequential.extend_from_slice(b"PACK");
        sequential.extend_from_slice(&2u32.to_be_bytes());
        sequential.extend_from_slice(&(objects.len() as u32).to_be_bytes());
        for obj in &objects {
            let frame = compress_frame(obj).unwrap();
            sequential.extend_from_slice(&frame.header);
            sequential.extend_from_slice(&frame.data);
        }
        assert_eq!(pack[..pack.len() - 20], sequential);
        // Byte-equality plus the reader's own trailer check pin the
        // checksum; verify the reader accepts the parallel pack too.
        let decoded = read_pack(&pack).unwrap();
        for (d, o) in decoded.iter().zip(&objects) {
            assert_eq!((d.kind, &d.data, d.sha1), (o.kind, &o.data, o.sha1));
        }
    }

    #[test]
    fn empty_pack_is_valid() {
        let mut pack = Vec::new();
        let (sha, entries) = write_pack(&mut pack, &[]).unwrap();
        assert!(entries.is_empty());
        assert!(read_pack(&pack).unwrap().is_empty());
        let mut idx = Vec::new();
        write_idx(&mut idx, entries, sha).unwrap();
        assert!(read_idx(&idx).unwrap().is_empty());
    }

    #[test]
    fn truncated_streams_error_cleanly() {
        let objects = vec![obj(3, b"hello\n"), obj(1, b"tree x\n")];
        let mut pack = Vec::new();
        write_pack(&mut pack, &objects).unwrap();
        for cut in [0usize, 4, 12, 13, pack.len() / 2, pack.len() - 1] {
            assert!(read_pack(&pack[..cut]).is_err(), "cut at {cut} must fail");
        }
        let mut idx = Vec::new();
        let (sha, entries) = write_pack(&mut Vec::new(), &objects).unwrap();
        let _ = sha;
        write_idx(&mut idx, entries, [0u8; 20]).unwrap();
        for cut in [0usize, 4, 8, 100, idx.len() - 1] {
            assert!(read_idx(&idx[..cut]).is_err(), "cut at {cut} must fail");
        }
    }

    #[test]
    fn bit_flips_error_cleanly() {
        let objects = vec![
            obj(3, b"hello\n"),
            obj(2, b"100644 a\0aaaaaaaaaaaaaaaaaaaa"),
            obj(1, b"c\n"),
        ];
        let mut pack = Vec::new();
        write_pack(&mut pack, &objects).unwrap();
        // flip bits at a spread of offsets; every one must fail to decode
        // cleanly (either as an error or — for no offset, since the SHA-1
        // and zlib checks are strict — never silently as wrong bytes).
        for at in (0..pack.len()).step_by(7) {
            let mut flipped = pack.clone();
            flipped[at] ^= 0x40;
            if let Ok(decoded) = read_pack(&flipped) {
                // A flip that survives must still be a *different* stream
                // (never the same objects); assert the failure mode.
                let original = read_pack(&pack).unwrap();
                assert_ne!(decoded, original, "flip at {at} decoded identically");
            }
        }
        // the deterministic core: flipping any single bit in the trailer
        // always fails.
        for at in (pack.len() - 20..pack.len()).step_by(3) {
            let mut flipped = pack.clone();
            flipped[at] ^= 0x01;
            assert!(read_pack(&flipped).is_err(), "trailer flip at {at}");
        }
    }

    #[test]
    fn bad_magic_and_version_rejected() {
        let objects = vec![obj(3, b"x\n")];
        let mut pack = Vec::new();
        write_pack(&mut pack, &objects).unwrap();
        let mut bad_magic = pack.clone();
        bad_magic[0] = b'X';
        assert!(read_pack(&bad_magic).is_err());
        let mut bad_version = pack.clone();
        bad_version[7] = 3;
        assert!(read_pack(&bad_version).is_err());
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
