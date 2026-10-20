"""Independent implementation of docs/canonical-encoding.md.

Written against the spec document only, with no reference to the Rust code,
so byte-level agreement between the two is evidence that the spec is
complete and unambiguous. Used by verify_fixtures.py to cross-check the
golden fixtures and recompute every Id.
"""

CHGSET_FORMAT = 0x01
CHGSET_VERSION = 0x01
TREE_FORMAT = 0x02
TREE_VERSION = 0x01
MAX_FIELD_BYTES = 64 << 20
MAX_ELEMS = 1 << 20

OPS = {0: "add", 1: "modify", 2: "delete", 3: "rename"}
MODES = {0: "regular", 1: "exec", 2: "symlink", 3: "gitlink"}


class CodecError(Exception):
    pass


class Reader:
    __slots__ = ("buf", "pos")

    def __init__(self, buf):
        self.buf = buf
        self.pos = 0

    def take(self, n):
        if self.pos + n > len(self.buf):
            raise CodecError("truncated input")
        out = self.buf[self.pos : self.pos + n]
        self.pos += n
        return out

    def u8(self):
        return self.take(1)[0]

    def varint(self):
        v = 0
        count = 0
        while True:
            b = self.u8()
            count += 1
            if count > 10:
                raise CodecError("varint too long")
            group = b & 0x7F
            shift = 7 * (count - 1)
            # Payload bits beyond 64 cannot be represented: a group that
            # would lose any of them is non-canonical (the value would
            # change on re-encode). Matches the Rust codec exactly — at
            # shift 63 only groups 0 and 1 survive.
            if shift >= 64 or (64 - shift < 7 and group >> (64 - shift) != 0):
                raise CodecError("varint overflow")
            v |= group << shift
            if not (b & 0x80):
                if count > 1 and b == 0:
                    raise CodecError("non-minimal varint")
                return v

    def i32_zigzag(self):
        v = self.varint()
        wide = (v >> 1) ^ -(v & 1)
        if not (-(2**31) <= wide < 2**31):
            raise CodecError("zigzag out of i32 range")
        return wide

    def byte_slice(self):
        n = self.varint()
        if n > MAX_FIELD_BYTES:
            raise CodecError("field length out of range")
        return self.take(n)

    def string(self):
        try:
            return self.byte_slice().decode("utf-8")
        except UnicodeDecodeError:
            raise CodecError("invalid UTF-8")

    def id32(self):
        return self.take(32)

    def opt_id(self):
        f = self.u8()
        if f == 0:
            return None
        if f == 1:
            return self.id32()
        raise CodecError("invalid option tag")

    def opt_str(self):
        f = self.u8()
        if f == 0:
            return None
        if f == 1:
            return self.string()
        raise CodecError("invalid option tag")

    def opt_bytes(self):
        f = self.u8()
        if f == 0:
            return None
        if f == 1:
            return self.byte_slice()
        raise CodecError("invalid option tag")

    def count(self):
        n = self.varint()
        if n > MAX_ELEMS:
            raise CodecError("element count out of range")
        return n


def _validate_path(p):
    if not p:
        raise CodecError("empty path")
    if p.startswith("/"):
        raise CodecError(f"absolute path {p!r}")
    if "\x00" in p:
        raise CodecError("NUL in path")
    for comp in p.split("/"):
        if comp in ("", ".", ".."):
            raise CodecError(f"bad component in {p!r}")


def decode_changeset(b):
    """Returns a dict; raises CodecError on any non-canonical input."""
    r = Reader(b)
    if r.u8() != CHGSET_FORMAT:
        raise CodecError("bad changeset format byte")
    if r.u8() != CHGSET_VERSION:
        raise CodecError("unsupported changeset version")

    n_parents = r.count()
    parents = [bytes(r.id32()) for _ in range(n_parents)]

    n_changes = r.count()
    changes = []
    for _ in range(n_changes):
        op = r.u8()
        if op not in OPS:
            raise CodecError("invalid op byte")
        path = r.string()
        blob = r.opt_id()
        src_path = r.opt_str()
        mode = r.u8()
        if mode not in MODES:
            raise CodecError("invalid mode byte")
        changes.append({"op": op, "path": path, "blob": blob, "src_path": src_path, "mode": mode})

    message = r.string()

    def identity():
        return {
            "name": r.string(),
            "email": r.string(),
            "time_secs": r.varint(),
            "tz_min": r.i32_zigzag(),
        }

    author = identity()
    committer = identity()
    signature = r.opt_bytes()

    if r.pos != len(b):
        raise CodecError("trailing data after changeset")

    _validate_changeset(parents, changes, message, signature)
    return {"parents": parents, "changes": changes, "message": message, "author": author, "committer": committer, "signature": signature}


def _validate_changeset(parents, changes, _message, _signature):
    seen = set()
    for c in changes:
        _validate_path(c["path"])
        op, blob, src, mode = c["op"], c["blob"], c["src_path"], c["mode"]
        if op in (0, 1):
            if blob is None:
                raise CodecError(f"{c['path']} requires a blob")
            if src is not None:
                raise CodecError(f"{c['path']}: src_path forbidden for op {op}")
        elif op == 2:
            if blob is not None:
                raise CodecError(f"delete {c['path']} must not carry a blob")
            if src is not None:
                raise CodecError(f"delete {c['path']} must not carry src_path")
            if mode != 0:
                raise CodecError(f"delete {c['path']} must carry mode regular")
        elif op == 3:
            if src is None:
                raise CodecError(f"rename {c['path']} requires src_path")
            _validate_path(src)
            if src == c["path"]:
                raise CodecError("rename src_path must differ from path")
            if blob is None:
                raise CodecError(f"rename {c['path']} requires a blob")
        if mode == 3 and blob is None:
            raise CodecError(f"gitlink {c['path']} requires a blob")
        if c["path"] in seen:
            raise CodecError(f"duplicate path {c['path']}")
        seen.add(c["path"])
    order = [c["path"].encode() for c in changes]
    if any(a >= b for a, b in zip(order, order[1:])):
        raise CodecError("changes not strictly ascending")


def decode_tree(b):
    r = Reader(b)
    if r.u8() != TREE_FORMAT:
        raise CodecError("bad tree format byte")
    if r.u8() != TREE_VERSION:
        raise CodecError("unsupported tree version")

    n = r.count()
    entries = []
    for _ in range(n):
        name = r.string()
        kind = r.u8()
        if kind not in (0, 1):
            raise CodecError("invalid tree kind byte")
        mode = r.u8()
        if mode not in MODES:
            raise CodecError("invalid mode byte")
        if kind == 0:
            blob, tree = bytes(r.id32()), None
        else:
            blob, tree = None, bytes(r.id32())
            if mode != 0:
                raise CodecError(f"dir entry {name!r} must carry mode regular")
        if not name or "/" in name or "\x00" in name:
            raise CodecError(f"invalid tree entry name {name!r}")
        changes_ok = (blob is not None) if kind == 0 else (tree is not None)
        if not changes_ok:
            raise CodecError(f"entry {name!r}: kind/id mismatch")
        entries.append({"name": name, "kind": kind, "mode": mode, "blob": blob, "tree": tree})

    if r.pos != len(b):
        raise CodecError("trailing data after tree")

    names = [e["name"].encode() for e in entries]
    if any(a >= b for a, b in zip(names, names[1:])):
        raise CodecError("entries not strictly ascending")
    if len(set(names)) != len(names):
        raise CodecError("duplicate entry")
    return {"entries": entries}


# ------------------------------------------------------------------ encode

def _put_varint(w, v):
    while True:
        group = v & 0x7F
        v >>= 7
        if v:
            w.append(group | 0x80)
        else:
            w.append(group)
            return


def _put_zigzag(w, v):
    _put_varint(w, (v << 1) ^ (v >> 63))


def _put_bytes(w, b):
    _put_varint(w, len(b))
    w.extend(b)


def _put_opt(w, value, put_present):
    if value is None:
        w.append(0)
    else:
        w.append(1)
        put_present(value)


def _encode_identity(w, ident):
    _put_bytes(w, ident["name"].encode())
    _put_bytes(w, ident["email"].encode())
    _put_varint(w, ident["time_secs"])
    _put_zigzag(w, ident["tz_min"])


def encode(obj):
    """Re-encodes a decoded structure; must reproduce the input bytes."""
    w = bytearray()
    if "parents" in obj:
        w.append(CHGSET_FORMAT)
        w.append(CHGSET_VERSION)
        _put_varint(w, len(obj["parents"]))
        for p in obj["parents"]:
            w.extend(p)
        changes = sorted(obj["changes"], key=lambda c: c["path"].encode())
        _put_varint(w, len(changes))
        for c in changes:
            w.append(c["op"])
            _put_bytes(w, c["path"].encode())
            _put_opt(w, c["blob"], lambda b: w.extend(b))
            _put_opt(w, c["src_path"], lambda s: _put_bytes(w, s.encode()))
            w.append(c["mode"])
        _put_bytes(w, obj["message"].encode())
        _encode_identity(w, obj["author"])
        _encode_identity(w, obj["committer"])
        _put_opt(w, obj["signature"], lambda b: _put_bytes(w, b))
    else:
        w.append(TREE_FORMAT)
        w.append(TREE_VERSION)
        entries = sorted(obj["entries"], key=lambda e: e["name"].encode())
        _put_varint(w, len(entries))
        for e in entries:
            _put_bytes(w, e["name"].encode())
            w.append(e["kind"])
            w.append(e["mode"])
            w.extend(e["blob"] if e["kind"] == 0 else e["tree"])
    return bytes(w)
