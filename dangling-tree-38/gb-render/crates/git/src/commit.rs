//! Parser for raw git commit objects.
//!
//! Git commit objects are header lines (tree/parent/author/committer/…)
//! followed by a blank line and the message. Continuation lines of
//! multi-line headers (e.g. `gpgsig`) start with a single space. This
//! parser is zero-assumption: anything that does not match the documented
//! object format is an error, never a silently mangled value.
//!
//! UTF-8 divergence (documented): commit objects are byte sequences; names,
//! emails and messages that are not valid UTF-8 are decoded with
//! `String::from_utf8_lossy`. gardenbed stores `String` fields, so such
//! commits get a lossily-transcoded representation (and therefore a
//! different changeset id than a hypothetical byte-preserving model would
//! produce). Real-world corpora are UTF-8.

use gardenbed_model::Identity;

use crate::oid::GitOid;

/// The parsed fields of a raw commit object.
#[derive(Clone, Debug)]
pub(crate) struct RawCommit {
    /// The tree the commit points at.
    pub tree: GitOid,
    /// Parent commit ids in stored order (empty for root commits).
    pub parents: Vec<GitOid>,
    /// Author identity.
    pub author: Identity,
    /// Committer identity.
    pub committer: Identity,
    /// The `gpgsig` header, decoded: continuation lines have their single
    /// leading space stripped and are joined with `\n`, reproducing the
    /// original signature text. Only the first `gpgsig` header is captured.
    pub signature: Option<Vec<u8>>,
    /// The commit message, verbatim (lossy UTF-8).
    pub message: String,
}

/// A parse failure with a human-readable detail (the caller attaches the
/// commit sha for context).
pub(crate) type ParseResult<T> = Result<T, String>;

/// Parses the raw bytes of a commit object.
pub(crate) fn parse_commit(data: &[u8]) -> ParseResult<RawCommit> {
    let mut tree: Option<GitOid> = None;
    let mut parents: Vec<GitOid> = Vec::new();
    let mut author: Option<Identity> = None;
    let mut committer: Option<Identity> = None;
    let mut signature: Option<Vec<u8>> = None;

    let mut pos = 0usize;
    let mut message_start: Option<usize> = None;
    while pos < data.len() {
        let line_end = data[pos..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|i| pos + i)
            .unwrap_or(data.len());
        let line = &data[pos..line_end];
        if line.is_empty() {
            // The blank line separates headers from the message.
            message_start = Some(line_end + 1);
            break;
        }
        if line[0] == b' ' {
            // Continuation of the previous header. Only `gpgsig` is kept.
            if let Some(sig) = signature.as_mut() {
                sig.push(b'\n');
                sig.extend_from_slice(&line[1..]);
            }
        } else {
            let sp = line
                .iter()
                .position(|b| *b == b' ')
                .ok_or_else(|| "malformed header line (no key separator)".to_owned())?;
            let key = std::str::from_utf8(&line[..sp])
                .map_err(|_| "header key is not UTF-8".to_owned())?;
            let value = &line[sp + 1..];
            match key {
                "tree" => {
                    if tree.is_some() {
                        return Err("duplicate tree header".into());
                    }
                    tree = Some(parse_oid_header(value, "tree")?);
                }
                "parent" => parents.push(parse_oid_header(value, "parent")?),
                "author" => {
                    author = Some(parse_identity(value).map_err(|e| format!("author: {e}"))?)
                }
                "committer" => {
                    committer = Some(parse_identity(value).map_err(|e| format!("committer: {e}"))?)
                }
                "gpgsig" => {
                    if signature.is_none() {
                        signature = Some(value.to_vec());
                    }
                }
                // encoding, mergetag, gpgsig-sha256, …: ignored (their
                // continuation lines, if any, are consumed above and only
                // feed a signature that is not kept).
                _ => {}
            }
        }
        pos = line_end + 1;
    }

    let tree = tree.ok_or("commit has no tree header")?;
    let author = author.ok_or("commit has no author header")?;
    let committer = committer.ok_or("commit has no committer header")?;
    let message = match message_start {
        Some(start) => String::from_utf8_lossy(&data[start.min(data.len())..]).into_owned(),
        // No blank line: git always writes one; treat the object as
        // malformed rather than guessing.
        None => return Err("commit has no message separator".into()),
    };

    Ok(RawCommit {
        tree,
        parents,
        author,
        committer,
        signature,
        message,
    })
}

fn parse_oid_header(value: &[u8], what: &str) -> ParseResult<GitOid> {
    let hex = std::str::from_utf8(value).map_err(|_| format!("{what} id is not UTF-8"))?;
    GitOid::from_hex(hex).ok_or_else(|| format!("{what} id is not 40-hex: {hex:?}"))
}

/// Parses an identity line value: `Name <email> secs tz`.
///
/// - `secs` is a Unix timestamp; negative (pre-1970) values are rejected
///   because the model stores `u64` seconds (v0.1 limitation).
/// - `tz` is git's `±hhmm` form, converted to minutes east of UTC.
pub(crate) fn parse_identity(value: &[u8]) -> ParseResult<Identity> {
    let lt = value
        .iter()
        .position(|b| *b == b'<')
        .ok_or("missing '<' before email")?;
    let gt = value[lt..]
        .iter()
        .position(|b| *b == b'>')
        .map(|i| lt + i)
        .ok_or("missing '>' after email")?;
    // The name is everything before '<', minus the single separator space.
    let mut name_raw = &value[..lt];
    if name_raw.last() == Some(&b' ') {
        name_raw = &name_raw[..name_raw.len() - 1];
    }
    let name = String::from_utf8_lossy(name_raw).into_owned();
    let email = String::from_utf8_lossy(&value[lt + 1..gt]).into_owned();

    let rest = &value[gt + 1..];
    let text = std::str::from_utf8(rest).map_err(|_| "timestamp is not UTF-8")?;
    let mut parts = text.split_whitespace();
    let secs_raw = parts.next().ok_or("missing timestamp")?;
    let secs: i64 = secs_raw
        .parse()
        .map_err(|_| format!("bad timestamp {secs_raw:?}"))?;
    if secs < 0 {
        return Err(format!("negative timestamp {secs} is not supported"));
    }
    let tz_raw = parts.next().ok_or("missing timezone")?;
    let tz_min = parse_tz(tz_raw)?;
    // Exactly the two fields, no trailing junk (git writes `secs +hhmm`).
    if parts.next().is_some() {
        return Err(format!("trailing data after timezone in {text:?}"));
    }

    Ok(Identity {
        name,
        email,
        time_secs: secs as u64,
        tz_min,
    })
}

/// Parses git's `±hhmm` timezone into minutes east of UTC.
fn parse_tz(tz: &str) -> ParseResult<i32> {
    let bytes = tz.as_bytes();
    let (sign, digits) = match bytes.first() {
        Some(b'+') => (1i32, &bytes[1..]),
        Some(b'-') => (-1i32, &bytes[1..]),
        _ => return Err(format!("bad timezone {tz:?}")),
    };
    if digits.len() != 4 || !digits.iter().all(|b| b.is_ascii_digit()) {
        return Err(format!("bad timezone {tz:?}"));
    }
    let hh = (digits[0] - b'0') as i32 * 10 + (digits[1] - b'0') as i32;
    let mm = (digits[2] - b'0') as i32 * 10 + (digits[3] - b'0') as i32;
    if hh > 24 || mm > 59 {
        return Err(format!("bad timezone {tz:?}"));
    }
    Ok(sign * (hh * 60 + mm))
}

/// Reconstructs the commit object bytes implied by `raw`'s parsed fields,
/// given the tree and parent git ids it must point at — the exact inverse
/// of [`parse_commit`] for the reconstructable subset (tree/parent/author/
/// committer in canonical order, no extra headers, UTF-8 identities,
/// message verbatim).
///
/// Byte-equality with the original object bytes **proves** the commit is
/// fully represented by the model (v0.2 D21's sparse-capture predicate):
/// any difference — a `gpgsig`/`mergetag`/`encoding` header, a non-UTF-8
/// name, a header-order deviation, a message the lossy decode changed —
/// makes the bytes differ, and the importer captures the original raw.
pub(crate) fn reconstruct_bytes(
    raw: &RawCommit,
    tree_hex: &str,
    parent_hexes: &[String],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(256 + raw.message.len());
    body.extend_from_slice(b"tree ");
    body.extend_from_slice(tree_hex.as_bytes());
    body.push(b'\n');
    for hex in parent_hexes {
        body.extend_from_slice(b"parent ");
        body.extend_from_slice(hex.as_bytes());
        body.push(b'\n');
    }
    body.extend_from_slice(identity_line("author", &raw.author).as_bytes());
    body.extend_from_slice(identity_line("committer", &raw.committer).as_bytes());
    body.push(b'\n');
    body.extend_from_slice(raw.message.as_bytes());
    body
}

/// `author Name <email> secs ±hhmm\n` (the inverse of `parse_identity`).
fn identity_line(role: &str, who: &gardenbed_model::Identity) -> String {
    let sign = if who.tz_min < 0 { '-' } else { '+' };
    let abs = who.tz_min.unsigned_abs();
    format!(
        "{role} {} <{}> {} {}{:02}{:02}\n",
        who.name,
        who.email,
        who.time_secs,
        sign,
        abs / 60,
        abs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::{parse_commit, parse_identity};

    fn identity(name: &str, email: &str, secs: u64, tz: i32) -> gardenbed_model::Identity {
        gardenbed_model::Identity {
            name: name.into(),
            email: email.into(),
            time_secs: secs,
            tz_min: tz,
        }
    }

    #[test]
    fn parses_a_plain_commit() {
        let raw = b"tree 0123456789abcdef0123456789abcdef01234567\n\
                    parent 89abcdef0123456789abcdef0123456789abcdef\n\
                    parent fedcba9876543210fedcba9876543210fedcba98\n\
                    author A U Thor <author@example.com> 1700000000 +0530\n\
                    committer C O Mitter <committer@example.com> 1700000001 -0700\n\
                    \n\
                    subject line\n\nbody\n";
        let cs = parse_commit(raw).expect("parses");
        assert_eq!(cs.tree.to_hex(), "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(cs.parents.len(), 2);
        assert_eq!(
            cs.author,
            identity("A U Thor", "author@example.com", 1700000000, 330)
        );
        assert_eq!(
            cs.committer,
            identity("C O Mitter", "committer@example.com", 1700000001, -420)
        );
        assert_eq!(cs.message, "subject line\n\nbody\n");
        assert!(cs.signature.is_none());
    }

    #[test]
    fn parses_gpgsig_continuations() {
        let raw = b"tree 0123456789abcdef0123456789abcdef01234567\n\
                    author A <a@e> 1 +0000\n\
                    committer A <a@e> 1 +0000\n\
                    gpgsig -----BEGIN PGP SIGNATURE-----\n \
                    line one\n \
                    line two\n \
                    -----END PGP SIGNATURE-----\n\
                    \n\
                    msg\n";
        let cs = parse_commit(raw).expect("parses");
        let sig = cs.signature.expect("signature captured");
        assert_eq!(
            String::from_utf8(sig).unwrap(),
            "-----BEGIN PGP SIGNATURE-----\nline one\nline two\n-----END PGP SIGNATURE-----"
        );
    }

    #[test]
    fn identity_edge_cases() {
        // Empty name and empty email (UTF-8 inside the email).
        let id = parse_identity(" <电机> 0 +0000".as_bytes()).expect("parses");
        assert_eq!(id.name, "");
        assert_eq!(id.email, "电机");
        assert_eq!(id.time_secs, 0);
        assert_eq!(id.tz_min, 0);
        assert!(parse_identity(b"name <a@b> -5 +0000").is_err());
        assert!(parse_identity(b"name <a@b> 5 0000").is_err());
        assert!(parse_identity(b"name <a@b> 5 +2500").is_err());
        assert!(parse_identity(b"name <a@b> 5 +0000 junk").is_err());
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_commit(b"author A <a@e> 1 +0000\n\nmsg\n").is_err());
        assert!(parse_commit(b"tree 01\nauthor A <a@e> 1 +0000\n\nmsg").is_err());
        assert!(parse_commit(b"tree 0123456789abcdef0123456789abcdef01234567\n\nmsg").is_err());
        // No message separator at all.
        assert!(parse_commit(b"tree 0123456789abcdef0123456789abcdef01234567\n").is_err());
    }
}
