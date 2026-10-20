# Canonical encoding — gardenbed objects (spec v1)

This document is the **normative** byte-level specification for hashing
changesets and trees. It is written so that an independent implementation in
any language can reproduce gardenbed Ids byte-for-byte from this text alone.
`crates/model/src/codec.rs` is one implementation; `tools/py-canonical/` is a
second, written from this document, and the fixture corpus in
`crates/model/tests/fixtures/` is the executable contract between them.

## 1. Identity

- `Changeset::id` = SHA-256 over the canonical changeset payload (§3).
- `Tree::id` = SHA-256 over the canonical tree payload (§4).
- `Blob::id` = SHA-256 over **raw, uncompressed content**. Compression is a
  storage concern and never participates in identity.
- Ids are 32 raw bytes; their canonical display form is lowercase hex
  (64 characters). Parsers must accept uppercase hex but must not emit it.

## 2. Primitive encodings

| Primitive | Encoding |
|---|---|
| `u8` | one raw byte |
| `bytes32` | exactly 32 raw bytes (a SHA-256 digest) |
| `varint` | unsigned LEB128: 7 payload bits per byte, little-endian groups, bit 7 of each byte = continuation flag |
| `svarint` | signed integer mapped to `varint` by zigzag: `enc(v) = (v << 1) ^ (v >> 63)` on the 64-bit two's-complement value; signed values are always carried as 64-bit |
| `str` | `varint(len)` + UTF-8 bytes; decoders must validate UTF-8 |
| `bytes` | `varint(len)` + raw bytes |
| `opt<T>` | `0x00` (absent) or `0x01` + T |
| `seq<T>` | `varint(count)` + elements |

**Minimality rules.** Encoders must emit *minimal* varints: the encoding of
a value never ends in a `0x00` continuation-final byte unless the value is 0
and the encoding is exactly one byte. Decoders must reject non-minimal
varints (a final group of `0x00` when more than one byte was consumed) and
varints that exceed 64 bits (more than 10 bytes, or a 10th byte with payload
bits above bit 63).

**Bounds.** Decoders must refuse hostile sizes before allocating:
`len > 2^26` (64 MiB) for any `str`/`bytes` field and element counts
`> 2^20` are decode errors. These bounds are part of the spec; encoders
producing larger values are non-conformant.

**Framing.** A payload is `format_byte`, `version_byte`, then fields.
Decoders must reject trailing bytes after the last field, unknown format
bytes, and unknown versions.

**Canonical acceptance property.** `decode(x)` succeeds **iff** `x` is a
canonical payload, and `encode(decode(x)) == x`. Acceptance is not enough:
a decoder that accepts non-canonical bytes is buggy. Fuzz targets enforce
the re-encode equality.

## 3. Changeset payload

Format byte `0x01`, version byte `0x01`. Field order is fixed:

| # | Field | Encoding | Notes |
|---|---|---|---|
| 1 | parents | `seq<bytes32>` | empty ⇒ root changeset; else ≥ 1 |
| 2 | changes | `seq<change>` | see below |
| 3 | message | `str` | may be empty |
| 4 | author | identity | see below |
| 5 | committer | identity | see below |
| 6 | signature | `opt<bytes>` | arbitrary bytes (e.g. SSH/GPG) |

`change` = `op:u8`, `path:str`, `blob:opt<bytes32>`, `src_path:opt<str>`,
`mode:u8` — in that order.

| `op` | Meaning | blob | src_path | mode |
|---|---|---|---|---|
| 0 add | path created | **required** | forbidden | any file mode |
| 1 modify | content/mode changed | **required** | forbidden | any file mode |
| 2 delete | path removed | forbidden | forbidden | must be `regular` |
| 3 rename | moved from `src_path` | **required** (a pure rename reuses the source blob id) | **required**, ≠ path | any file mode |

`mode`: 0 regular · 1 exec · 2 symlink · 3 gitlink. A `gitlink` entry
records a submodule pointer; its `blob` field carries the upstream commit's
git SHA-1, **left-padded with zero bytes to 32** (a git SHA-256 repo pads
naturally). The codec treats it as opaque bytes.

identity = `{ name:str, email:str, time_secs:varint, tz_min:svarint }`.
Timestamps are git-native **epoch seconds + tz offset in minutes** (git's
`±hhmm`), *not* normalized instants: preserving both exactly is what makes
git round-trips reproduce identical git SHAs.

**Semantic validation (decode and encode both enforce):**

1. Every `path`/`src_path` is non-empty, relative, NUL-free, and has no
   empty, `.`, or `..` components.
2. `changes` are **strictly ascending by path bytes** (this is the canonical
   form; encoders sort, decoders reject anything else) with no duplicate
   paths.
3. The op/blob/src_path/mode table above holds exactly.
4. Parents are 0 (root) or ≥ 1.

## 4. Tree payload

Format byte `0x02`, version byte `0x01`. Trees are the derived, cached
hierarchical view; the changeset is authoritative.

| # | Field | Encoding | Notes |
|---|---|---|---|
| 1 | entries | `seq<entry>` | see below |

`entry` = `name:str`, `kind:u8`, `mode:u8`, then exactly one id:

| `kind` | Meaning | id carried | mode |
|---|---|---|---|
| 0 file | leaf object (regular/exec/symlink/gitlink) | `bytes32` blob id | any file mode |
| 1 dir | child subtree | `bytes32` tree id | must be `regular` |

**Validation:** entry names are non-empty, NUL-free, and contain no `/`
(names are single components); entries are strictly ascending by name bytes;
no duplicate names; kind/id pairing per the table.

## 5. Worked layout (root changeset, minimal metadata)

`01 01 | 00 | 00 | 00 | 00 00 00 00 | 00 00 00 00 | 00` — 13 bytes:
format, version, zero parents, zero changes, empty message, empty author
(name, email, secs 0, tz 0), empty committer, absent signature. The fixture
`changeset-root-empty.hex` is this payload, and `changeset-root-empty.id`
its SHA-256.

## 6. Stored object envelope (loam) — informative

On disk, blobs are stored under their `Blob::id` as:
`"GBLO1"` magic (5 bytes), `raw_len:u64` little-endian, one zstd frame of
the content. Readers verify the decompressed length and its SHA-256 against
the addressed Id. Trees and changesets are stored as their canonical
payloads (with their own format bytes, so no envelope is needed to type
them).

## 7. Versioning policy

The version byte participates in the hash. A breaking format change bumps
the version byte (and this document); old payloads remain readable by their
own version forever, and cross-version migration is an explicit mapping
table, never a silent reinterpretation.

## 8. Verification protocol (how this spec is tested)

1. **Golden vectors** — hand-built objects; exact hex + SHA-256 committed in
   `crates/model/tests/fixtures/` (`*.hex`, `*.id`). Rust blesses on first
   run; later runs must match exactly.
2. **Cross-language check** — `tools/py-canonical/verify_fixtures.py`
   decodes every fixture, re-encodes it, and recomputes Ids *from this
   document's rules alone*. Any disagreement is a spec or implementation
   bug.
3. **Rejection vectors** — `invalid-*.hex` fixtures (truncation, trailing
   bytes, non-minimal varints, unsorted/duplicate entries, bad op/kind
   bytes, op-field contradictions) must be rejected by both implementations.
4. **Properties** — roundtrip stability, order-independence, and
   decode-accepts-only-canonical are property-tested in Rust and asserted
   implicitly by the Python re-encode check.
