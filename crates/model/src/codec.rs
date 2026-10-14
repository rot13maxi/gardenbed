//! Canonical byte encoding for gardenbed objects.
//!
//! This module implements `docs/canonical-encoding.md`. The acceptance
//! property it must uphold: `decode(x)` succeeds iff `x` is canonical, and
//! `encode(decode(x)) == x`.

use crate::{Change, Changeset, Id, Identity, Mode, Op, Tree, TreeEntry, TreeKind};

/// Codec errors.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The input is not a valid canonical payload.
    #[error("decode error: {0}")]
    Decode(String),
    /// The value cannot be canonically encoded.
    #[error("encode error: {0}")]
    Encode(String),
}

/// Max length of one length-prefixed field (64 MiB) — decoders must not
/// allocate unboundedly on hostile input. Part of the spec.
pub const MAX_FIELD_BYTES: usize = 64 << 20;
/// Max element count. Part of the spec.
pub const MAX_ELEMS: usize = 1 << 20;

pub const CHGSET_FORMAT: u8 = 0x01;
pub const CHGSET_VERSION: u8 = 0x01;
pub const TREE_FORMAT: u8 = 0x02;
pub const TREE_VERSION: u8 = 0x01;

// ---------------------------------------------------------------- writer

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let group = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(group);
            return;
        }
        buf.push(group | 0x80);
    }
}

fn put_zigzag(buf: &mut Vec<u8>, v: i64) {
    put_varint(buf, ((v << 1) ^ (v >> 63)) as u64);
}

fn put_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    put_varint(buf, b.len() as u64);
    buf.extend_from_slice(b);
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_bytes(buf, s.as_bytes());
}

fn put_opt_id(buf: &mut Vec<u8>, id: &Option<Id>) {
    match id {
        None => buf.push(0x00),
        Some(i) => {
            buf.push(0x01);
            buf.extend_from_slice(i.as_bytes());
        }
    }
}

fn put_opt_str(buf: &mut Vec<u8>, s: &Option<String>) {
    match s {
        None => buf.push(0x00),
        Some(v) => {
            buf.push(0x01);
            put_str(buf, v);
        }
    }
}

fn put_opt_bytes(buf: &mut Vec<u8>, b: &Option<Vec<u8>>) {
    match b {
        None => buf.push(0x00),
        Some(v) => {
            buf.push(0x01);
            put_bytes(buf, v);
        }
    }
}

fn put_identity(buf: &mut Vec<u8>, id: &Identity) {
    put_str(buf, &id.name);
    put_str(buf, &id.email);
    put_varint(buf, id.time_secs);
    put_zigzag(buf, id.tz_min as i64);
}

// ---------------------------------------------------------------- reader

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        if self.buf.len() - self.pos < n {
            return Err(CodecError::Decode("truncated input".into()));
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    fn varint(&mut self) -> Result<u64, CodecError> {
        let mut v: u64 = 0;
        let mut count = 0usize;
        loop {
            let b = self.u8()?;
            count += 1;
            if count > 10 {
                return Err(CodecError::Decode("varint too long".into()));
            }
            let group = (b & 0x7f) as u64;
            let shift = 7 * (count - 1);
            if shift >= 64 {
                if group > 1 {
                    return Err(CodecError::Decode("varint overflow".into()));
                }
                v |= group << 63;
            } else {
                v |= group << shift;
            }
            if b & 0x80 == 0 {
                if count > 1 && b == 0 {
                    return Err(CodecError::Decode("non-minimal varint".into()));
                }
                return Ok(v);
            }
        }
    }

    fn i32_zigzag(&mut self) -> Result<i32, CodecError> {
        let v = self.varint()?;
        let wide = ((v >> 1) as i64) ^ -((v & 1) as i64);
        i32::try_from(wide).map_err(|_| CodecError::Decode("zigzag out of i32 range".into()))
    }

    fn byte_slice(&mut self) -> Result<&'a [u8], CodecError> {
        let len = self.varint()? as usize;
        if len > MAX_FIELD_BYTES {
            return Err(CodecError::Decode("field length out of range".into()));
        }
        self.take(len)
    }

    fn string(&mut self) -> Result<String, CodecError> {
        let raw = self.byte_slice()?;
        std::str::from_utf8(raw)
            .map(str::to_owned)
            .map_err(|_| CodecError::Decode("invalid UTF-8".into()))
    }

    fn opt_str(&mut self) -> Result<Option<String>, CodecError> {
        match self.u8()? {
            0x00 => Ok(None),
            0x01 => Ok(Some(self.string()?)),
            _ => Err(CodecError::Decode("invalid option tag".into())),
        }
    }

    fn opt_bytes(&mut self) -> Result<Option<Vec<u8>>, CodecError> {
        match self.u8()? {
            0x00 => Ok(None),
            0x01 => Ok(Some(self.byte_slice()?.to_vec())),
            _ => Err(CodecError::Decode("invalid option tag".into())),
        }
    }

    fn id(&mut self) -> Result<Id, CodecError> {
        let raw = self.take(32)?;
        let mut out = [0u8; 32];
        out.copy_from_slice(raw);
        Ok(Id::from_bytes(out))
    }

    fn opt_id(&mut self) -> Result<Option<Id>, CodecError> {
        match self.u8()? {
            0x00 => Ok(None),
            0x01 => Ok(Some(self.id()?)),
            _ => Err(CodecError::Decode("invalid option tag".into())),
        }
    }

    fn count(&mut self) -> Result<usize, CodecError> {
        let n = self.varint()? as usize;
        if n > MAX_ELEMS {
            return Err(CodecError::Decode("element count out of range".into()));
        }
        Ok(n)
    }
}

// ------------------------------------------------------- semantic rules

/// Path rules (spec §3.1): non-empty, relative, NUL-free, no empty/./..
/// components.
pub fn validate_path(path: &str) -> Result<(), CodecError> {
    if path.is_empty() {
        return Err(CodecError::Encode("path must be non-empty".into()));
    }
    if path.starts_with('/') {
        return Err(CodecError::Encode(format!("path {path:?} must be relative")));
    }
    if path.bytes().any(|b| b == 0) {
        return Err(CodecError::Encode("path must not contain NUL".into()));
    }
    for comp in path.split('/') {
        if comp.is_empty() {
            return Err(CodecError::Encode(format!("path {path:?} has empty component")));
        }
        if comp == "." || comp == ".." {
            return Err(CodecError::Encode(format!(
                "path {path:?} has invalid component {comp:?}"
            )));
        }
    }
    Ok(())
}

/// Shared semantic rules. `require_sorted` is set by the decoder (canonical
/// form is strictly ascending); the encoder sorts first, so it only needs
/// the duplicate check.
pub fn validate_changeset(cs: &Changeset, require_sorted: bool) -> Result<(), CodecError> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    for c in &cs.changes {
        validate_path(&c.path)?;
        match c.op {
            Op::Add | Op::Modify => {
                if c.blob.is_none() {
                    return Err(CodecError::Encode(format!("{} requires a blob", c.path)));
                }
                if c.src_path.is_some() {
                    return Err(CodecError::Encode(format!(
                        "{}: src_path is forbidden for {}",
                        c.path, c.op as u8
                    )));
                }
            }
            Op::Delete => {
                if c.blob.is_some() {
                    return Err(CodecError::Encode(format!(
                        "delete {} must not carry a blob",
                        c.path
                    )));
                }
                if c.src_path.is_some() {
                    return Err(CodecError::Encode(format!(
                        "delete {} must not carry src_path",
                        c.path
                    )));
                }
                if c.mode != Mode::Regular {
                    return Err(CodecError::Encode(format!(
                        "delete {} must carry mode regular",
                        c.path
                    )));
                }
            }
            Op::Rename => {
                let src = c
                    .src_path
                    .as_deref()
                    .ok_or_else(|| CodecError::Encode(format!("rename {} requires src_path", c.path)))?;
                validate_path(src)?;
                if src == c.path {
                    return Err(CodecError::Encode(format!(
                        "rename {src:?}: src_path must differ from path"
                    )));
                }
                if c.blob.is_none() {
                    return Err(CodecError::Encode(format!(
                        "rename {} requires a blob (a pure rename reuses the old blob id)",
                        c.path
                    )));
                }
            }
        }
        if c.mode == Mode::Gitlink && c.blob.is_none() {
            return Err(CodecError::Encode(format!(
                "gitlink {} requires a blob (the padded upstream git SHA)",
                c.path
            )));
        }
        if !seen.insert(c.path.as_str()) {
            return Err(CodecError::Encode(format!("duplicate path {}", c.path)));
        }
    }
    if require_sorted {
        if let Some(bad) = cs
            .changes
            .windows(2)
            .find(|w| w[0].path.as_bytes() >= w[1].path.as_bytes())
        {
            return Err(CodecError::Decode(format!(
                "changes not strictly ascending at {:?}",
                bad[1].path
            )));
        }
    }
    Ok(())
}

/// Tree semantic rules (spec §4).
pub fn validate_tree(tree: &Tree, require_sorted: bool) -> Result<(), CodecError> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    for e in &tree.entries {
        if e.name.is_empty() || e.name.contains('/') || e.name.bytes().any(|b| b == 0) {
            return Err(CodecError::Encode(format!(
                "invalid tree entry name {:?}",
                e.name
            )));
        }
        match e.kind {
            TreeKind::File => {
                if e.blob.is_none() {
                    return Err(CodecError::Encode(format!(
                        "file entry {:?} must carry a blob id",
                        e.name
                    )));
                }
                if e.tree.is_some() {
                    return Err(CodecError::Encode(format!(
                        "file entry {:?} must not carry a tree id",
                        e.name
                    )));
                }
            }
            TreeKind::Dir => {
                if e.tree.is_none() {
                    return Err(CodecError::Encode(format!(
                        "dir entry {:?} must carry a tree id",
                        e.name
                    )));
                }
                if e.blob.is_some() {
                    return Err(CodecError::Encode(format!(
                        "dir entry {:?} must not carry a blob id",
                        e.name
                    )));
                }
                if e.mode != Mode::Regular {
                    return Err(CodecError::Encode(format!(
                        "dir entry {:?} must carry mode regular",
                        e.name
                    )));
                }
            }
        }
        if !seen.insert(e.name.as_str()) {
            return Err(CodecError::Encode(format!("duplicate entry {:?}", e.name)));
        }
    }
    if require_sorted {
        if let Some(bad) = tree
            .entries
            .windows(2)
            .find(|w| w[0].name.as_bytes() >= w[1].name.as_bytes())
        {
            return Err(CodecError::Decode(format!(
                "entries not strictly ascending at {:?}",
                bad[1].name
            )));
        }
    }
    Ok(())
}

// ------------------------------------------------------- changeset codec

/// Encodes the canonical changeset payload (spec §3). Sorts `changes` by
/// path bytes; rejects semantically invalid input.
pub fn encode_changeset(cs: &Changeset) -> Result<Vec<u8>, CodecError> {
    validate_changeset(cs, false)?;
    let mut changes: Vec<&Change> = cs.changes.iter().collect();
    changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

    let mut buf = Vec::with_capacity(32 + cs.changes.len() * 16);
    buf.push(CHGSET_FORMAT);
    buf.push(CHGSET_VERSION);

    put_varint(&mut buf, cs.parents.len() as u64);
    for p in &cs.parents {
        buf.extend_from_slice(p.as_bytes());
    }

    put_varint(&mut buf, changes.len() as u64);
    for c in changes {
        buf.push(c.op.to_u8());
        put_str(&mut buf, &c.path);
        put_opt_id(&mut buf, &c.blob);
        put_opt_str(&mut buf, &c.src_path);
        buf.push(c.mode.to_u8());
    }

    put_str(&mut buf, &cs.message);
    put_identity(&mut buf, &cs.author);
    put_identity(&mut buf, &cs.committer);
    put_opt_bytes(&mut buf, &cs.signature);
    Ok(buf)
}

/// Decodes canonical changeset bytes. Accepts only canonical payloads.
pub fn decode_changeset(bytes: &[u8]) -> Result<Changeset, CodecError> {
    let mut r = Reader::new(bytes);
    if r.u8()? != CHGSET_FORMAT {
        return Err(CodecError::Decode("bad changeset format byte".into()));
    }
    if r.u8()? != CHGSET_VERSION {
        return Err(CodecError::Decode("unsupported changeset version".into()));
    }

    let n_parents = r.count()?;
    let mut parents = Vec::with_capacity(n_parents.min(4096));
    for _ in 0..n_parents {
        parents.push(r.id()?);
    }

    let n_changes = r.count()?;
    let mut changes = Vec::with_capacity(n_changes.min(4096));
    for _ in 0..n_changes {
        let op = Op::from_u8(r.u8()?)
            .ok_or_else(|| CodecError::Decode("invalid op byte".into()))?;
        let path = r.string()?;
        let blob = r.opt_id()?;
        let src_path = r.opt_str()?;
        let mode = Mode::from_u8(r.u8()?)
            .ok_or_else(|| CodecError::Decode("invalid mode byte".into()))?;
        changes.push(Change {
            path,
            op,
            blob,
            src_path,
            mode,
        });
    }

    let message = r.string()?;
    let author = read_identity(&mut r)?;
    let committer = read_identity(&mut r)?;
    let signature = r.opt_bytes()?;

    if r.pos != bytes.len() {
        return Err(CodecError::Decode("trailing data after changeset".into()));
    }

    let cs = Changeset {
        parents,
        changes,
        message,
        author,
        committer,
        signature,
    };
    validate_changeset(&cs, true)?;
    Ok(cs)
}

fn read_identity(r: &mut Reader<'_>) -> Result<Identity, CodecError> {
    Ok(Identity {
        name: r.string()?,
        email: r.string()?,
        time_secs: r.varint()?,
        tz_min: r.i32_zigzag()?,
    })
}

// ------------------------------------------------------------ tree codec

/// Encodes the canonical tree payload (spec §4). Sorts entries by name
/// bytes; rejects semantically invalid input.
pub fn encode_tree(tree: &Tree) -> Result<Vec<u8>, CodecError> {
    validate_tree(tree, false)?;
    let mut entries: Vec<&TreeEntry> = tree.entries.iter().collect();
    entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));

    let mut buf = Vec::with_capacity(8 + entries.len() * 40);
    buf.push(TREE_FORMAT);
    buf.push(TREE_VERSION);

    put_varint(&mut buf, entries.len() as u64);
    for e in entries {
        put_str(&mut buf, &e.name);
        buf.push(e.kind.to_u8());
        buf.push(e.mode.to_u8());
        match e.kind {
            TreeKind::File => buf.extend_from_slice(e.blob.expect("validated").as_bytes()),
            TreeKind::Dir => buf.extend_from_slice(e.tree.expect("validated").as_bytes()),
        }
    }
    Ok(buf)
}

/// Decodes canonical tree bytes. Accepts only canonical payloads.
pub fn decode_tree(bytes: &[u8]) -> Result<Tree, CodecError> {
    let mut r = Reader::new(bytes);
    if r.u8()? != TREE_FORMAT {
        return Err(CodecError::Decode("bad tree format byte".into()));
    }
    if r.u8()? != TREE_VERSION {
        return Err(CodecError::Decode("unsupported tree version".into()));
    }

    let n = r.count()?;
    let mut entries = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let name = r.string()?;
        let kind = TreeKind::from_u8(r.u8()?)
            .ok_or_else(|| CodecError::Decode("invalid tree kind byte".into()))?;
        let mode = Mode::from_u8(r.u8()?)
            .ok_or_else(|| CodecError::Decode("invalid mode byte".into()))?;
        let (blob, tree) = match kind {
            TreeKind::File => (Some(r.id()?), None),
            TreeKind::Dir => (None, Some(r.id()?)),
        };
        entries.push(TreeEntry {
            name,
            kind,
            mode,
            blob,
            tree,
        });
    }

    if r.pos != bytes.len() {
        return Err(CodecError::Decode("trailing data after tree".into()));
    }

    let tree = Tree { entries };
    validate_tree(&tree, true)?;
    Ok(tree)
}
