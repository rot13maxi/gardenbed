//! `objects` — the filesystem content-addressed object store.
//!
//! Layout under the store root (2-level sharding by lowercase hex id):
//!
//! ```text
//! <root>/objects/<first 2 hex chars>/<remaining 62 hex chars>
//! ```
//!
//! Blobs are stored under their `SHA-256(raw content)` id using the on-disk
//! envelope from `docs/canonical-encoding.md` §6:
//!
//! ```text
//! "GBLO1" (5 bytes magic) | raw_len: u64 little-endian | one zstd frame
//! ```
//!
//! Compression is a storage detail and never participates in identity: on
//! read, the decompressed length and the SHA-256 of the content are verified
//! against the addressed id, and any mismatch is [`StoreError::Corrupt`] — a
//! torn or tampered object can never be handed back as wrong bytes.
//!
//! Trees are stored as their canonical payload bytes (which carry their own
//! format/version bytes, so no envelope is needed to type them).
//!
//! Writes are tmp-file + atomic rename: the payload is fully written and
//! fsynced to a unique temp file inside the destination shard directory,
//! the directory is fsynced, the rename publishes the object atomically,
//! and the directory is fsynced again so the rename itself is durable.
//! A crash therefore leaves the object either absent or byte-complete.

use crate::StoreError;
use crate::fastcdc;
use gardenbed_model::Id;
use gardenbed_model::blob_id;
use sha2::Digest;
use sha2::Sha256;
use std::fs;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Envelope magic (`docs/canonical-encoding.md` §6).
const MAGIC: &[u8; 5] = b"GBLO1";
/// GBLO2 magic (`docs/v0.2.md` §7): content-defined chunked storage for
/// blobs above [`CHUNK_THRESHOLD`].
const MAGIC2: &[u8; 5] = b"GBLO2";
/// Magic + `u64` raw length.
const HEADER_LEN: usize = MAGIC.len() + 8;
/// GBLO2 fixed header: magic + `raw_len: u64` + `chunk_count: u32`.
const HEADER2_LEN: usize = MAGIC2.len() + 8 + 4;
/// Per-chunk table entry: `raw_len: u32` + `comp_len: u32`.
const CHUNK_ENTRY_LEN: usize = 8;
/// Blobs strictly larger than this store GBLO2 (FastCDC-chunked); blobs at
/// or below it stay GBLO1 byte-identically (docs/v0.2.md §7, D15).
const CHUNK_THRESHOLD: usize = 1 << 20;
/// Sanity bound on decompressed blob size (4 GiB). Far above anything v0.1
/// stores, while capping the damage of a hostile or corrupt envelope
/// claiming an enormous `raw_len`. Part of reading defensively, not part of
/// the storage contract.
const MAX_BLOB_LEN: u64 = 4 << 30;
/// Bound on the on-disk object file itself (envelope overhead slack on top
/// of [`MAX_BLOB_LEN`]; GBLO2's chunk table is ≤ ~786 KiB at the cap).
const MAX_ENVELOPE_LEN: u64 = MAX_BLOB_LEN + (1 << 21);
/// zstd compression level. Storage detail; never part of identity.
const ZSTD_LEVEL: i32 = 3;

/// Process-unique counter so concurrent tmp writes never collide.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The filesystem (objects) object store.
///
/// Durability: writes are fully fsynced by default. `GARDENBED_FSYNC=off`
/// (read once at [`FsObjectStore::open`]) relaxes durability to
/// atomicity-only (tmp+rename is kept, fsync calls are skipped). This is
/// meant for rebuildable bulk loads — corpus imports and demo seeds, where
/// a crash costs a re-import, not data. Real repositories must run with
/// durability on; the torn-write torture tests always do.
#[derive(Debug)]
pub struct FsObjectStore {
    /// `<root>/objects` — holds the two shard levels.
    objects_dir: PathBuf,
    /// fsync enabled? See the type-level docs.
    durable: bool,
}

impl FsObjectStore {
    /// Opens (creating if needed) the object store rooted at `root`.
    ///
    /// Idempotent: an existing store is reopened as-is.
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        let durable = !matches!(
            std::env::var("GARDENBED_FSYNC").as_deref(),
            Ok("off") | Ok("0") | Ok("false")
        );
        let objects_dir = root.join("objects");
        fs::create_dir_all(&objects_dir).map_err(io_err)?;
        // Make the directory structure itself durable before any object is
        // written into it.
        fsync_dir(root)?;
        fsync_dir(&objects_dir)?;
        Ok(FsObjectStore {
            objects_dir,
            durable,
        })
    }

    /// The `objects/` directory holding all shards.
    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    /// On-disk path for an id: `objects/<2 hex>/<62 hex>`.
    fn path_for(&self, id: Id) -> PathBuf {
        let hex = id.to_hex();
        self.objects_dir.join(&hex[..2]).join(&hex[2..])
    }

    /// Writes `bytes` to `path` atomically (tmp file + fsync + rename).
    fn atomic_write(&self, path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
        let dir = path
            .parent()
            .ok_or_else(|| StoreError::Invalid("object path has no parent directory".into()))?;
        fs::create_dir_all(dir).map_err(io_err)?;
        if self.durable {
            // The shard directory must hold the tmp entry durably before the
            // payload write. With `GARDENBED_FSYNC=off` this (like every
            // other fsync here) is skipped: bulk loads only promise
            // atomicity, and one journal-commit wait per object dominated
            // import wall clock (each fsync parks in the ext4 journal).
            fsync_dir(dir)?;
        }

        // Unique tmp name; `create_new` makes collisions impossible.
        let mut tmp;
        loop {
            tmp = dir.join(format!(
                ".tmp-{}-{}",
                std::process::id(),
                TMP_COUNTER.fetch_add(1, Ordering::Relaxed),
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
            {
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(StoreError::Io(e.to_string())),
            }
        }

        let write = || -> Result<(), StoreError> {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .open(&tmp)
                .map_err(io_err)?;
            f.write_all(bytes).map_err(io_err)?;
            if !self.durable {
                drop(f);
                return Ok(());
            }
            // 1. fsync the file: payload fully on disk.
            f.sync_all().map_err(io_err)?;
            // 2. fsync the parent dir before rename (durable tmp entry).
            drop(f);
            fsync_dir(dir)
        };
        if let Err(e) = write() {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }

        // 3. Atomic publish: rename never leaves a partial object at `path`.
        if let Err(e) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(io_err(e));
        }
        // 4. fsync the dir again so the rename survives a crash.
        if self.durable {
            fsync_dir(dir)?;
        }
        Ok(())
    }
}

impl crate::traits::ObjectStore for FsObjectStore {
    fn put_blob(&self, content: &[u8]) -> Result<Id, StoreError> {
        if content.len() as u64 > MAX_BLOB_LEN {
            return Err(StoreError::Invalid(format!(
                "blob of {} bytes exceeds the {}-byte limit",
                content.len(),
                MAX_BLOB_LEN
            )));
        }
        let id = blob_id(content);
        let path = self.path_for(id);

        // Absent files read as `None` below; present ones are checked for
        // immutability. A *structurally decodable* envelope with different
        // content is an immutability violation — someone claims this Id for
        // other bytes. A structurally corrupt file (bad magic, broken frame)
        // is healed: we hold content that hashes to this id, so overwriting
        // restores the store's invariant. Parsing without the hash check is
        // what distinguishes the two.
        if let Some(raw) = read_file_bounded(&path)? {
            match parse_envelope(&raw) {
                Ok(existing) if existing == content => return Ok(id),
                Ok(_) => return Err(StoreError::ImmutabilityViolation(id)),
                Err(_) => {}
            }
        }

        let envelope = encode_envelope(content)?;
        self.atomic_write(&path, &envelope)?;
        Ok(id)
    }

    fn get_blob(&self, id: Id) -> Result<Option<Vec<u8>>, StoreError> {
        match read_file_bounded(&self.path_for(id))? {
            None => Ok(None),
            Some(raw) => Ok(Some(decode_envelope(&raw, id)?)),
        }
    }

    fn put_tree(&self, id: Id, canonical_bytes: &[u8]) -> Result<(), StoreError> {
        // A tree Id is defined as SHA-256(canonical tree bytes); enforce the
        // pairing so every stored object verifies under its address.
        let actual = Id::from_bytes(Sha256::digest(canonical_bytes).into());
        if actual != id {
            return Err(StoreError::Invalid(format!(
                "tree canonical bytes hash to {actual}, not {id}"
            )));
        }
        let path = self.path_for(id);
        if let Some(raw) = read_file_bounded(&path)? {
            if raw == canonical_bytes {
                return Ok(());
            }
            return Err(StoreError::ImmutabilityViolation(id));
        }
        // Absent file at the path: fall through and write.
        self.atomic_write(&path, canonical_bytes)
    }

    fn get_tree(&self, id: Id) -> Result<Option<Vec<u8>>, StoreError> {
        match read_file_bounded(&self.path_for(id))? {
            None => Ok(None),
            Some(bytes) => {
                let actual = Id::from_bytes(Sha256::digest(&bytes).into());
                if actual != id {
                    return Err(StoreError::Corrupt(format!(
                        "tree {id}: stored bytes hash to {actual}"
                    )));
                }
                Ok(Some(bytes))
            }
        }
    }

    fn has(&self, id: Id) -> Result<bool, StoreError> {
        self.path_for(id)
            .try_exists()
            .map_err(|e| StoreError::Io(e.to_string()))
    }
}

/// Encodes the on-disk envelope: GBLO1 for content at or below
/// [`CHUNK_THRESHOLD`], GBLO2 (FastCDC-chunked, docs/v0.2.md §7) above it.
///
/// The choice is a pure function of `content.len()` — every writer in every
/// process stores the same bytes under the same id.
pub(crate) fn encode_envelope(content: &[u8]) -> Result<Vec<u8>, StoreError> {
    if content.len() > CHUNK_THRESHOLD {
        encode_envelope_chunked(content)
    } else {
        encode_envelope_whole(content)
    }
}

/// Encodes the §6 GBLO1 envelope: `"GBLO1"` + `u64` LE raw length + one
/// zstd frame.
fn encode_envelope_whole(content: &[u8]) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::with_capacity(HEADER_LEN + content.len() / 2 + 64);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(content.len() as u64).to_le_bytes());
    let frame = zstd::bulk::compress(content, ZSTD_LEVEL).map_err(io_err)?;
    out.extend_from_slice(&frame);
    Ok(out)
}

/// Encodes the §7 GBLO2 envelope:
/// `"GBLO2" | raw_len: u64 LE | chunk_count: u32 LE | chunk_count ×
/// {raw_len: u32 LE, comp_len: u32 LE} | chunk_count zstd frames`.
///
/// Chunk boundaries come from FastCDC (content-defined, deterministic for
/// identical content); each chunk is one zstd frame. Boundaries are part of
/// the on-disk format; frame bytes are not (identity hashes the
/// reassembled raw bytes only).
fn encode_envelope_chunked(content: &[u8]) -> Result<Vec<u8>, StoreError> {
    let lengths = crate::fastcdc::chunk_lengths(content);
    let count = lengths.len();
    let mut out = Vec::with_capacity(HEADER2_LEN + count * CHUNK_ENTRY_LEN + content.len() / 2);
    out.extend_from_slice(MAGIC2);
    out.extend_from_slice(&(content.len() as u64).to_le_bytes());
    out.extend_from_slice(&(count as u32).to_le_bytes());
    // One pass: compress each chunk, hold the frames, and write the table
    // first (the frames region follows the table in file order).
    let mut frames: Vec<Vec<u8>> = Vec::with_capacity(count);
    let mut offset = 0usize;
    for &len in &lengths {
        let chunk = &content[offset..offset + len as usize];
        let frame = zstd::bulk::compress(chunk, ZSTD_LEVEL).map_err(io_err)?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        frames.push(frame);
        offset += len as usize;
    }
    for frame in frames {
        out.extend_from_slice(&frame);
    }
    Ok(out)
}

/// Decodes and fully verifies an envelope (either magic) against the
/// addressed id.
///
/// Returns [`StoreError::Corrupt`] on any mismatch — bad magic, truncated
/// envelope, decompressed length differing from `raw_len`, or content whose
/// SHA-256 differs from `id`.
pub(crate) fn decode_envelope(raw: &[u8], id: Id) -> Result<Vec<u8>, StoreError> {
    let content = parse_envelope(raw)?;
    let actual = Id::from_bytes(Sha256::digest(&content).into());
    if actual != id {
        return Err(StoreError::Corrupt(format!(
            "content hashes to {actual}, addressed as {id}"
        )));
    }
    Ok(content)
}

/// Parses an envelope (either magic) without the final SHA-256-vs-id check.
/// Used by `put_blob`'s existence check, which must distinguish "envelope
/// of *different* content" (immutability violation) from "structurally
/// corrupt file" (healable).
pub(crate) fn parse_envelope(raw: &[u8]) -> Result<Vec<u8>, StoreError> {
    if raw.len() >= MAGIC2.len() && &raw[..MAGIC2.len()] == MAGIC2 {
        return parse_envelope_chunked(raw);
    }
    if raw.len() < HEADER_LEN {
        return Err(StoreError::Corrupt(format!(
            "envelope is {} bytes, need at least {HEADER_LEN}",
            raw.len()
        )));
    }
    if &raw[..MAGIC.len()] != MAGIC {
        return Err(StoreError::Corrupt("bad envelope magic".into()));
    }
    let mut len_bytes = [0u8; 8];
    len_bytes.copy_from_slice(&raw[MAGIC.len()..HEADER_LEN]);
    let raw_len = u64::from_le_bytes(len_bytes);
    if raw_len > MAX_BLOB_LEN {
        return Err(StoreError::Corrupt(format!(
            "declared raw_len {raw_len} exceeds the {MAX_BLOB_LEN}-byte limit"
        )));
    }
    let frame = &raw[HEADER_LEN..];
    let content = zstd_decode_bounded(frame, raw_len as usize)?;
    if content.len() as u64 != raw_len {
        return Err(StoreError::Corrupt(format!(
            "decompressed to {} bytes, envelope declares {raw_len}",
            content.len()
        )));
    }
    Ok(content)
}

/// Parses a GBLO2 envelope: fixed header, bounds-checked chunk table, one
/// bounded zstd decode per chunk, reassembled in order. Any structural
/// fault — truncated file, table/frame length disagreement, chunk raw
/// lengths not summing to `raw_len` — is [`StoreError::Corrupt`].
fn parse_envelope_chunked(raw: &[u8]) -> Result<Vec<u8>, StoreError> {
    if raw.len() < HEADER2_LEN {
        return Err(StoreError::Corrupt(format!(
            "GBLO2 envelope is {} bytes, need at least {HEADER2_LEN}",
            raw.len()
        )));
    }
    let raw_len = u64::from_le_bytes(raw[5..13].try_into().expect("8 raw_len bytes"));
    let count = u32::from_le_bytes(raw[13..17].try_into().expect("4 count bytes")) as usize;
    if raw_len > MAX_BLOB_LEN {
        return Err(StoreError::Corrupt(format!(
            "declared raw_len {raw_len} exceeds the {MAX_BLOB_LEN}-byte limit"
        )));
    }
    let table_end = HEADER2_LEN
        + count
            .checked_mul(CHUNK_ENTRY_LEN)
            .ok_or_else(|| StoreError::Corrupt("chunk table overflows".into()))?;
    if raw.len() < table_end {
        return Err(StoreError::Corrupt(format!(
            "GBLO2 envelope is {} bytes, chunk table needs {table_end}",
            raw.len()
        )));
    }
    // The frames region is exactly the remaining bytes; every comp_len
    // must slice it without slack (a torn or tampered file fails here).
    let frames = &raw[table_end..];
    let mut frame_offset = 0usize;
    let mut total_raw = 0u64;
    let mut content = Vec::with_capacity(raw_len as usize);
    for i in 0..count {
        let entry = HEADER2_LEN + i * CHUNK_ENTRY_LEN;
        let chunk_raw =
            u32::from_le_bytes(raw[entry..entry + 4].try_into().expect("4 raw bytes")) as usize;
        let chunk_comp =
            u32::from_le_bytes(raw[entry + 4..entry + 8].try_into().expect("4 comp bytes"))
                as usize;
        if chunk_raw == 0 {
            return Err(StoreError::Corrupt(format!(
                "chunk {i} declares zero raw length"
            )));
        }
        if chunk_raw > fastcdc::MAX {
            return Err(StoreError::Corrupt(format!(
                "chunk {i} declares {chunk_raw} bytes, above the {}-byte FastCDC max",
                fastcdc::MAX
            )));
        }
        if frame_offset + chunk_comp > frames.len() {
            return Err(StoreError::Corrupt(format!(
                "chunk {i} frame of {chunk_comp} bytes runs past the {}-byte frames region",
                frames.len()
            )));
        }
        let frame = &frames[frame_offset..frame_offset + chunk_comp];
        frame_offset += chunk_comp;
        let chunk = zstd_decode_bounded(frame, chunk_raw)?;
        if chunk.len() != chunk_raw {
            return Err(StoreError::Corrupt(format!(
                "chunk {i} decompressed to {} bytes, table declares {chunk_raw}",
                chunk.len()
            )));
        }
        total_raw = total_raw
            .checked_add(chunk_raw as u64)
            .ok_or_else(|| StoreError::Corrupt("chunk lengths overflow".into()))?;
        content.extend_from_slice(&chunk);
    }
    if frame_offset != frames.len() {
        return Err(StoreError::Corrupt(format!(
            "frames region has {} trailing bytes after {count} chunks",
            frames.len() - frame_offset
        )));
    }
    if total_raw != raw_len {
        return Err(StoreError::Corrupt(format!(
            "chunks reassemble to {total_raw} bytes, envelope declares {raw_len}"
        )));
    }
    Ok(content)
}

/// Decompresses one zstd frame, refusing to produce more than `declared`
/// bytes (zip-bomb / torn-data guard).
fn zstd_decode_bounded(frame: &[u8], declared: usize) -> Result<Vec<u8>, StoreError> {
    let decoder = zstd::stream::Decoder::new(frame)
        .map_err(|e| StoreError::Corrupt(format!("not a valid zstd frame: {}", squash_io(e))))?;
    // Allow exactly one byte beyond the declaration: anything longer is
    // already a length mismatch and must be rejected before it can grow.
    let limit = declared as u64 + 1;
    let mut out = Vec::new();
    decoder
        .take(limit)
        .read_to_end(&mut out)
        .map_err(|e| StoreError::Corrupt(format!("zstd decode failed: {}", squash_io(e))))?;
    if out.len() as u64 > declared as u64 {
        return Err(StoreError::Corrupt(format!(
            "decompressed beyond the declared length of {declared}"
        )));
    }
    Ok(out)
}

/// Reads a file, refusing absurd sizes before allocating. `None` when absent.
fn read_file_bounded(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StoreError::Io(e.to_string())),
    };
    let len = file.metadata().map_err(io_err)?.len();
    if len > MAX_ENVELOPE_LEN {
        return Err(StoreError::Corrupt(format!(
            "object file of {len} bytes exceeds the {MAX_ENVELOPE_LEN}-byte limit"
        )));
    }
    let mut buf = Vec::with_capacity(len as usize);
    file.read_to_end(&mut buf).map_err(io_err)?;
    Ok(Some(buf))
}

/// fsyncs a directory so entries created/renamed inside it are durable.
fn fsync_dir(dir: &Path) -> Result<(), StoreError> {
    File::open(dir).and_then(|d| d.sync_all()).map_err(io_err)
}

fn io_err(e: std::io::Error) -> StoreError {
    StoreError::Io(e.to_string())
}

/// Renders an I/O-shaped error for embedding in [`StoreError::Corrupt`].
fn squash_io(e: std::io::Error) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn envelope_roundtrips_and_verifies() {
        let content = b"stored object contents";
        let id = blob_id(content);
        let env = encode_envelope(content).unwrap();
        assert_eq!(&env[..5], b"GBLO1");
        assert_eq!(&env[5..13], &22u64.to_le_bytes());
        assert_eq!(decode_envelope(&env, id).unwrap(), content);
    }

    #[test]
    fn envelope_rejects_wrong_id_and_truncation() {
        let env = encode_envelope(b"payload").unwrap();
        let other =
            Id::from_str("0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap();
        assert!(matches!(
            decode_envelope(&env, other),
            Err(StoreError::Corrupt(_))
        ));
        assert!(matches!(
            decode_envelope(&env[..env.len() - 1], blob_id(b"payload")),
            Err(StoreError::Corrupt(_))
        ));
    }
}
