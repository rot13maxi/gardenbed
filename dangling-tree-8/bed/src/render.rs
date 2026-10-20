//! Rendering `bed log` and `bed diff` output.
//!
//! **Log** (structural parity target: same entries, same order as `git
//! log` — the server does the date-ordered walk; documented mapping):
//!
//! ```text
//! <short-id>  <author name> <author email>
//! Date: <YYYY-MM-DD HH:MM:SS ±hhmm>     ← author time in its own tz
//!
//!     <message line>                      ← every message line, indented 4
//! ```
//!
//! entries separated by one blank line. The id is working copy's 16-hex short form;
//! the date renders git-natively (epoch seconds + tz offset, never
//! normalized) in an ISO-like shape rather than git's locale words.
//!
//! **Diff** (structural parity target: same changed path set and op
//! classification as `git diff-tree -r --no-renames`): a `diff --git`
//! header per path, git-compatible `new file mode` / `deleted file mode` /
//! `old mode` / `new mode` lines, `---`/`+++` markers, then unified hunks
//! from a hand-rolled LCS line diff with 3 context lines. **Hunk internals
//! are a documented divergence** — git's Myers implementation may split or
//! align hunks differently; the path/op classification above is the parity
//! contract. Files whose last line lacks a trailing newline get git's
//! `\ No newline at end of file` marker.
//!
//! Two entry classes render git's payload-free forms instead of hunks:
//!
//! - **Binary blobs** — content with a NUL byte in its first 8 KiB (git's
//!   buffer-text check) renders the single line
//!   `Binary files a/<path> and b/<path> differ`, with git's `/dev/null`
//!   convention for the absent side of adds and deletes. Like git,
//!   mode-only changes (identical blob on both sides) render no binary
//!   line. (git applies binary patches only from `--binary` payloads;
//!   bed prints the same informational line plain `git diff` does.)
//! - **Gitlinks** — mode lines only, never content hunks: the import's
//!   gitlink blob ids are synthetic padded ids, *not* git's submodule
//!   SHAs, so no honest `Subproject commit <sha>` line exists to print.
//!   A gitlink add/delete keeps its `new file mode` / `deleted file mode`
//!   line (git apply accepts those); a surviving gitlink→gitlink pointer
//!   change has no mode lines to print, so its change rides the
//!   `Binary files … differ` line.
//!
//! A payload-free entry whose header would carry neither mode lines nor
//! `---`/`+++` markers (a plain content modification, or the gitlink
//! pointer change) also prints the `---`/`+++` markers: git apply only
//! consumes a header it can pair with a payload or metadata, and a bare
//! `diff --git` header leaks its name into the *next* entry's parse.

use gardenbed_model::Mode;

use crate::error::ClientError;

/// One log entry, as the server streamed it (structurally `git log`).
pub struct LogEntry {
    /// The changeset's id.
    pub id: gardenbed_model::Id,
    /// Author name.
    pub author_name: String,
    /// Author email.
    pub author_email: String,
    /// Author time: git-native epoch seconds.
    pub time_secs: u64,
    /// Author tz offset in minutes.
    pub tz_min: i32,
    /// The message, verbatim as stored.
    pub message: String,
}

/// Renders one log entry in working copy's git-log-like shape.
pub fn render_log_entry(entry: &LogEntry, out: &mut String) {
    out.push_str(&entry.id.short());
    out.push_str("  ");
    out.push_str(&entry.author_name);
    out.push_str(" <");
    out.push_str(&entry.author_email);
    out.push_str(">\n");
    out.push_str("Date: ");
    out.push_str(&render_date(entry.time_secs, entry.tz_min));
    out.push('\n');
    out.push('\n');
    let trimmed = entry.message.trim_end_matches('\n');
    for line in trimmed.split('\n') {
        if line.is_empty() {
            out.push('\n');
        } else {
            out.push_str("    ");
            out.push_str(line);
            out.push('\n');
        }
    }
}

/// Renders a whole log: entries separated by one blank line.
pub fn render_log(entries: &[LogEntry]) -> String {
    let mut out = String::new();
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        render_log_entry(entry, &mut out);
    }
    out
}

/// Renders a git-native timestamp in its **own** timezone offset
/// (`time_secs` verbatim, `tz_min` applied): `YYYY-MM-DD HH:MM:SS ±hhmm`.
/// No wall-clock, no locale — the changeset's own fields decide.
pub fn render_date(time_secs: u64, tz_min: i32) -> String {
    let local = time_secs as i64 + tz_min as i64 * 60;
    let days = local.div_euclid(86_400);
    let secs_of_day = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let sign = if tz_min < 0 { '-' } else { '+' };
    let a = tz_min.unsigned_abs();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {}{:02}{:02}",
        y,
        m,
        d,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
        sign,
        a / 60,
        a % 60
    )
}

/// Days since 1970-01-01 → `(year, month, day)` (proleptic Gregorian;
/// Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // year of era
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year
    let mp = (5 * doy + 2) / 153; // month index [0, 11], March = 0
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ------------------------------------------------------------------ diff

/// How a changed path is classified — git's `--name-status` letters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffOp {
    /// Created (git `A`).
    Add,
    /// Removed (git `D`).
    Delete,
    /// Content or mode changed, same entry type (git `M`).
    Modify,
    /// Entry type changed (regular↔symlink↔gitlink; git `T`).
    TypeChange,
}

impl DiffOp {
    /// git's letter.
    pub fn letter(self) -> char {
        match self {
            DiffOp::Add => 'A',
            DiffOp::Delete => 'D',
            DiffOp::Modify => 'M',
            DiffOp::TypeChange => 'T',
        }
    }
}

/// One side of a diff entry as known from the trees.
#[derive(Clone, Copy, Debug)]
pub struct DiffSide {
    /// The blob id (present whenever this side has an entry).
    pub blob: gardenbed_model::Id,
    /// The §4 mode (regular/exec/symlink/gitlink).
    pub mode: Mode,
}

/// One changed path, old side → new side.
pub struct DiffEntry {
    /// Repo-relative path.
    pub path: String,
    /// Classification (git's rules).
    pub op: DiffOp,
    /// The `a` side (absent for adds).
    pub old: Option<DiffSide>,
    /// The `b` side (absent for deletes).
    pub new: Option<DiffSide>,
}

/// The entry *type* git's `T` classification compares: regular and exec
/// files are one type; symlinks and gitlinks are their own.
fn type_of(mode: Option<Mode>) -> u8 {
    match mode {
        Some(Mode::Symlink) => b's',
        Some(Mode::Gitlink) => b'g',
        Some(Mode::Exec) | Some(Mode::Regular) | None => b'f',
    }
}

/// Classifies a changed path exactly as `git diff-tree --name-status`
/// would: adds and deletes by presence; a surviving path is Modify unless
/// the entry *type* moved, which git spells `T`.
pub fn classify(old: Option<DiffSide>, new: Option<DiffSide>) -> DiffOp {
    match (old, new) {
        (None, Some(_)) => DiffOp::Add,
        (Some(_), None) => DiffOp::Delete,
        (Some(o), Some(n)) => {
            if type_of(Some(o.mode)) != type_of(Some(n.mode)) {
                DiffOp::TypeChange
            } else {
                DiffOp::Modify
            }
        }
        (None, None) => unreachable!("a diff entry has at least one side"),
    }
}

/// The git octal mode string for a §4 mode.
pub fn git_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Regular => "100644",
        Mode::Exec => "100755",
        Mode::Symlink => "120000",
        Mode::Gitlink => "160000",
    }
}

/// How many leading bytes the binary probe samples. git's buffer-text
/// check (`BUFFER_TEXT_CHECK`) samples its `FIRST_FEW_BYTES` = 8000; bed
/// samples 8 KiB — the same heuristic class: a NUL byte in the prefix
/// means the content is not text.
const BINARY_PROBE_BYTES: usize = 8192;

/// git's buffer-text heuristic: content with a NUL byte in its first
/// 8 KiB is not text (rendering it as hunks would transcode raw bytes
/// through `String::from_utf8_lossy` — U+FFFD garbage, and a patch
/// `git apply` rejects as corrupt).
fn is_binary(content: &[u8]) -> bool {
    content[..content.len().min(BINARY_PROBE_BYTES)].contains(&0)
}

/// git's exact single-line form for an entry whose content is not
/// rendered: `Binary files a/<path> and b/<path> differ`, with git's
/// `/dev/null` convention for an absent side (deletes name `a/<path>` and
/// `/dev/null`; adds `/dev/null` and `b/<path>`).
fn binary_differ_line(path: &str, has_old: bool, has_new: bool) -> String {
    let old = if has_old {
        format!("a/{path}")
    } else {
        "/dev/null".to_owned()
    };
    let new = if has_new {
        format!("b/{path}")
    } else {
        "/dev/null".to_owned()
    };
    format!("Binary files {old} and {new} differ\n")
}

/// Renders a full diff. `content_of` fetches old/new blob bytes on demand
/// (cache-first at the caller); entries render in the order given.
pub fn render_diff(
    entries: &[DiffEntry],
    content_of: &dyn Fn(gardenbed_model::Id) -> Result<Vec<u8>, ClientError>,
) -> Result<String, ClientError> {
    let mut out = String::new();
    for entry in entries {
        render_diff_entry(entry, content_of, &mut out)?;
    }
    Ok(out)
}

/// Renders one changed path.
fn render_diff_entry(
    entry: &DiffEntry,
    content_of: &dyn Fn(gardenbed_model::Id) -> Result<Vec<u8>, ClientError>,
    out: &mut String,
) -> Result<(), ClientError> {
    out.push_str(&format!("diff --git a/{} b/{}\n", entry.path, entry.path));
    // git's `old mode` / `new mode` / `new file mode` / `deleted file mode`
    // lines. Their presence decides how git apply parses a payload-free
    // entry: a header that carries neither extended headers nor `---`/
    // `+++` markers is not consumed as a patch — its name leaks into the
    // next entry's parse ("inconsistent old filename") — so payload-free
    // entries without mode lines also print the `---`/`+++` markers.
    let mut mode_lines = true;
    match entry.op {
        DiffOp::Add => {
            let mode = entry.new.map(|s| s.mode).unwrap_or(Mode::Regular);
            out.push_str(&format!("new file mode {}\n", git_mode(mode)));
        }
        DiffOp::Delete => {
            let mode = entry.old.map(|s| s.mode).unwrap_or(Mode::Regular);
            out.push_str(&format!("deleted file mode {}\n", git_mode(mode)));
        }
        DiffOp::Modify | DiffOp::TypeChange => {
            if entry.old.map(|s| s.mode) != entry.new.map(|s| s.mode) {
                if let Some(s) = entry.old {
                    out.push_str(&format!("old mode {}\n", git_mode(s.mode)));
                }
                if let Some(s) = entry.new {
                    out.push_str(&format!("new mode {}\n", git_mode(s.mode)));
                }
            } else {
                mode_lines = false;
            }
        }
    }

    // Content hunks. Gitlinks and non-text content never render as hunks
    // (see the module doc); symlinks diff as their target bytes; adds and
    // deletes diff against nothing.
    let old_gitlink = entry.old.is_some_and(|s| s.mode == Mode::Gitlink);
    let new_gitlink = entry.new.is_some_and(|s| s.mode == Mode::Gitlink);
    if old_gitlink || new_gitlink {
        // The import's gitlink blob ids are synthetic padded ids, not git's
        // submodule SHAs — any `Subproject commit <sha>` line would be
        // wrong bytes, so gitlink entries render mode lines only. A
        // surviving pointer change (gitlink on both sides) has no mode
        // lines and no hunks to show: the change rides git's
        // `Binary files … differ` line, with the `---`/`+++` markers git's
        // own gitlink diffs carry keeping the entry parseable.
        if old_gitlink && new_gitlink {
            out.push_str(&format!("--- a/{}\n+++ b/{}\n", entry.path, entry.path));
            out.push_str(&binary_differ_line(&entry.path, true, true));
        }
        return Ok(());
    }

    let old_content = match entry.old {
        None => None,
        Some(side) => Some(content_of(side.blob)?),
    };
    let new_content = match entry.new {
        None => None,
        Some(side) => Some(content_of(side.blob)?),
    };
    // Non-text content renders git's single informational line instead of
    // lossy hunks. Only when the contents actually differ: a mode-only
    // change (identical blob on both sides) keeps git's mode-lines-only
    // form. One side absent (add/delete) means the surviving content is
    // all there is — its binary-ness decides.
    let contents_differ = match (entry.old, entry.new) {
        (Some(old), Some(new)) => old.blob != new.blob,
        _ => true,
    };
    if contents_differ
        && (old_content.as_deref().is_some_and(is_binary)
            || new_content.as_deref().is_some_and(is_binary))
    {
        if !mode_lines {
            out.push_str(&format!("--- a/{}\n+++ b/{}\n", entry.path, entry.path));
        }
        out.push_str(&binary_differ_line(
            &entry.path,
            entry.old.is_some(),
            entry.new.is_some(),
        ));
        return Ok(());
    }

    match (&old_content, &new_content) {
        (None, None) => {} // unreachable for classified entries; bare header
        (old, new) => {
            out.push_str("--- ");
            out.push_str(
                &old.as_ref()
                    .map(|_| format!("a/{}", entry.path))
                    .unwrap_or_else(|| "/dev/null".to_owned()),
            );
            out.push('\n');
            out.push_str("+++ ");
            out.push_str(
                &new.as_ref()
                    .map(|_| format!("b/{}", entry.path))
                    .unwrap_or_else(|| "/dev/null".to_owned()),
            );
            out.push('\n');
            let old_bytes = old.as_deref().unwrap_or(b"");
            let new_bytes = new.as_deref().unwrap_or(b"");
            render_unified(old_bytes, new_bytes, out);
        }
    }
    Ok(())
}

/// A byte file split into lines (without the `\n`), remembering whether
/// the final line ends with one (git's `\ No newline` marker needs this).
fn split_lines(bytes: &[u8]) -> (Vec<&[u8]>, bool) {
    if bytes.is_empty() {
        return (Vec::new(), true);
    }
    let mut lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    let ends_with_newline = if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
        true
    } else {
        false
    };
    (lines, ends_with_newline)
}

/// One step of the edit script between the old and new line sequences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The same line on both sides (one old line, one new line).
    Same,
    /// Remove the next old line.
    Del,
    /// Insert the next new line.
    Add,
}

/// Context lines shown around each change (git's default).
const CONTEXT: usize = 3;

/// Cap on the LCS dynamic-programming table (cells). Beyond it the diff
/// falls back to a whole-run replace — never wrong, only less minimal.
const LCS_CELLS_CAP: u64 = 16_000_000;

/// The edit script between two line sequences: a hand-rolled LCS with
/// common prefix/suffix trimming. Deterministic; minimal for inputs under
/// the table cap.
fn diff_lines(old: &[&[u8]], new: &[&[u8]]) -> Vec<Step> {
    let mut pre = 0;
    while pre < old.len() && pre < new.len() && old[pre] == new[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < old.len() - pre
        && suf < new.len() - pre
        && old[old.len() - 1 - suf] == new[new.len() - 1 - suf]
    {
        suf += 1;
    }
    let mid_o = &old[pre..old.len() - suf];
    let mid_n = &new[pre..new.len() - suf];

    let mut out = Vec::with_capacity(old.len() + new.len());
    out.extend(std::iter::repeat_n(Step::Same, pre));

    let n = mid_o.len();
    let m = mid_n.len();
    if (n as u64) * (m as u64) > LCS_CELLS_CAP {
        // Fallback: the whole differing run replaces wholesale.
        out.extend(std::iter::repeat_n(Step::Del, n));
        out.extend(std::iter::repeat_n(Step::Add, m));
    } else {
        let mut table = vec![0u32; (n + 1) * (m + 1)];
        let idx = |i: usize, j: usize| i * (m + 1) + j;
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                table[idx(i, j)] = if mid_o[i] == mid_n[j] {
                    table[idx(i + 1, j + 1)] + 1
                } else {
                    table[idx(i + 1, j)].max(table[idx(i, j + 1)])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if mid_o[i] == mid_n[j] {
                out.push(Step::Same);
                i += 1;
                j += 1;
            } else if table[idx(i + 1, j)] >= table[idx(i, j + 1)] {
                out.push(Step::Del);
                i += 1;
            } else {
                out.push(Step::Add);
                j += 1;
            }
        }
        out.extend(std::iter::repeat_n(Step::Del, n - i));
        out.extend(std::iter::repeat_n(Step::Add, m - j));
    }
    out.extend(std::iter::repeat_n(Step::Same, suf));
    out
}

/// Renders unified hunks of `old` → `new` onto `out`.
pub fn render_unified(old: &[u8], new: &[u8], out: &mut String) {
    let (old_lines, old_nl) = split_lines(old);
    let (new_lines, new_nl) = split_lines(new);
    let script = diff_lines(&old_lines, &new_lines);

    // Positions of the changed steps, then grouping into hunks: expand each
    // change by `CONTEXT` steps and merge overlapping windows.
    let changed: Vec<usize> = script
        .iter()
        .enumerate()
        .filter(|(_, s)| !matches!(s, Step::Same))
        .map(|(k, _)| k)
        .collect();
    if changed.is_empty() {
        return;
    }
    let mut regions: Vec<(usize, usize)> = Vec::new(); // step-index [lo, hi)
    for &k in &changed {
        match regions.last_mut() {
            // git merges hunks whose unchanged gap is ≤ 2 × context lines.
            Some(last) if k <= last.1 + 2 * CONTEXT => last.1 = k + 1,
            _ => regions.push((k.saturating_sub(CONTEXT), k + 1)),
        }
    }

    let n_old = old_lines.len();
    let n_new = new_lines.len();
    for (lo, hi) in regions {
        let lo = lo.max(0);
        let hi = hi.min(script.len());
        // Expand the window's trailing context, clamped to the file.
        let mut end = hi + CONTEXT;
        if end > script.len() {
            end = script.len();
        }
        let (o_start, n_start) = starts_before(&script[..lo]);
        let (o_len, n_len) = region_counts(&script[lo..end]);
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(o_start, o_len),
            range(n_start, n_len)
        ));
        let mut o = o_start;
        let mut n = n_start;
        for step in &script[lo..end] {
            match step {
                Step::Same => {
                    emit(out, b' ', old_lines[o]);
                    o += 1;
                    n += 1;
                }
                Step::Del => {
                    emit(out, b'-', old_lines[o]);
                    if o + 1 == n_old && !old_nl {
                        out.push_str("\\ No newline at end of file\n");
                    }
                    o += 1;
                }
                Step::Add => {
                    emit(out, b'+', new_lines[n]);
                    if n + 1 == n_new && !new_nl {
                        out.push_str("\\ No newline at end of file\n");
                    }
                    n += 1;
                }
            }
        }
    }
}

/// `(old, new)` line positions reached just before `steps` — the 0-based
/// line indexes each hunk region starts from.
fn starts_before(steps: &[Step]) -> (usize, usize) {
    let mut o = 0;
    let mut n = 0;
    for s in steps {
        match s {
            Step::Same => {
                o += 1;
                n += 1;
            }
            Step::Del => o += 1,
            Step::Add => n += 1,
        }
    }
    (o, n)
}

/// `(old, new)` line counts of a region.
fn region_counts(steps: &[Step]) -> (usize, usize) {
    let o = steps
        .iter()
        .filter(|s| matches!(s, Step::Same | Step::Del))
        .count();
    let n = steps
        .iter()
        .filter(|s| matches!(s, Step::Same | Step::Add))
        .count();
    (o, n)
}

/// git's hunk-header conventions: a non-empty range names its 1-based
/// start line, an empty range the line *after* which the gap sits
/// (0-based) — and a one-line range drops the `,1`
/// (`@@ -1 +0,0 @@`, never `@@ -1,1 …`).
fn range(start: usize, len: usize) -> String {
    match len {
        0 => format!("{start},0"),
        1 => (start + 1).to_string(),
        _ => format!("{},{len}", start + 1),
    }
}

fn emit(out: &mut String, sign: u8, line: &[u8]) {
    out.push(sign as char);
    out.push_str(&String::from_utf8_lossy(line));
    out.push('\n');
}

// ----------------------------------------------------------------- blame

/// One blame attribution group: lines `lo..=hi` (1-based, inclusive) of
/// the file were introduced by changeset `id`, authored by `author_name`
/// at author time (`time_secs`, `tz_min`) — git blame shows the *author*
/// identity, not the committer's.
pub struct BlameGroup {
    /// First line of the range (1-based, inclusive).
    pub lo: u64,
    /// Last line of the range (1-based, inclusive).
    pub hi: u64,
    /// The changeset that introduced these lines.
    pub id: gardenbed_model::Id,
    /// The author's name.
    pub author_name: String,
    /// Author time, git-native epoch seconds.
    pub time_secs: u64,
    /// Author tz offset in minutes.
    pub tz_min: i32,
}

/// The author date as `YYYY-MM-DD` (its own timezone offset applied) —
/// the day-precision date `bed blame` prints.
pub fn render_date_short(time_secs: u64, tz_min: i32) -> String {
    render_date(time_secs, tz_min)[..10].to_owned()
}

/// Renders blame output: one line per file line, in git blame's default
/// format shape:
///
/// ```text
/// <16-hex-short> (<author name> <YYYY-MM-DD> <lineno>) <line content>
/// ```
///
/// `groups` are the blame ranges (ascending, coalesced, covering every
/// line — the blame walk's contract); `content` is the file's raw bytes at
/// the blamed changeset. An empty file renders empty output (git prints
/// nothing for one). The date is day-precision (v0.1: git's default also
/// shows the clock time — a documented cosmetic divergence).
pub fn render_blame(groups: &[BlameGroup], content: &[u8]) -> String {
    let (lines, _ends_with_newline) = split_lines(content);
    let mut out = String::new();
    let mut gi = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let lineno = i as u64 + 1;
        while gi + 1 < groups.len() && groups[gi].hi < lineno {
            gi += 1;
        }
        let g = groups
            .get(gi)
            .unwrap_or_else(|| panic!("blame range {gi} must cover line {lineno} (walk contract)"));
        assert!(
            g.lo <= lineno && lineno <= g.hi,
            "blame ranges must cover every line; line {lineno} not covered"
        );
        out.push_str(&g.id.short());
        out.push_str(" (");
        out.push_str(&g.author_name);
        out.push(' ');
        out.push_str(&render_date_short(g.time_secs, g.tz_min));
        out.push_str(&format!(" {lineno}) "));
        out.push_str(&String::from_utf8_lossy(line));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_renders_native_offsets() {
        // 2023-11-14 22:13:20 UTC.
        let t = 1_700_000_000u64;
        assert_eq!(render_date(t, 0), "2023-11-14 22:13:20 +0000");
        assert_eq!(render_date(t, 60), "2023-11-14 23:13:20 +0100");
        assert_eq!(render_date(t, -300), "2023-11-14 17:13:20 -0500");
        assert_eq!(render_date(t, 330), "2023-11-15 03:43:20 +0530");
        // +05:45 shifts 22:13:20 by 5h45m, landing at 03:58:20.
        assert_eq!(render_date(t, 345), "2023-11-15 03:58:20 +0545");
        // Crossing midnight backwards and forwards.
        assert_eq!(render_date(t + 3600, 480), "2023-11-15 07:13:20 +0800");
        // 21:13:20 UTC minus 14:00 lands at 07:13:20 the same UTC day.
        assert_eq!(render_date(t - 3600, -840), "2023-11-14 07:13:20 -1400");
    }

    #[test]
    fn log_renders_git_like_shape() {
        let entry = LogEntry {
            id: gardenbed_model::Id::from_bytes([0xAB; 32]),
            author_name: "Ada Gardener".into(),
            author_email: "ada@garden.example".into(),
            time_secs: 1_700_000_000,
            tz_min: 0,
            message: "seed 3: touch 2 path(s)\n\nCompost body.".into(),
        };
        let out = render_log(&[entry]);
        assert_eq!(
            out,
            "abababababababab  Ada Gardener <ada@garden.example>\n\
             Date: 2023-11-14 22:13:20 +0000\n\
             \n\
             \x20   seed 3: touch 2 path(s)\n\
             \n\
             \x20   Compost body.\n"
        );
    }

    #[test]
    fn diff_classifies_like_git_name_status() {
        let blob = gardenbed_model::blob_id(b"x");
        assert_eq!(
            classify(
                None,
                Some(DiffSide {
                    blob,
                    mode: Mode::Regular
                })
            ),
            DiffOp::Add
        );
        assert_eq!(
            classify(
                Some(DiffSide {
                    blob,
                    mode: Mode::Exec
                }),
                None
            ),
            DiffOp::Delete
        );
        // Regular ↔ exec is M (same type); regular ↔ symlink is T.
        assert_eq!(
            classify(
                Some(DiffSide {
                    blob,
                    mode: Mode::Regular
                }),
                Some(DiffSide {
                    blob,
                    mode: Mode::Exec
                })
            ),
            DiffOp::Modify
        );
        assert_eq!(
            classify(
                Some(DiffSide {
                    blob,
                    mode: Mode::Regular
                }),
                Some(DiffSide {
                    blob,
                    mode: Mode::Symlink
                })
            ),
            DiffOp::TypeChange
        );
        assert_eq!(
            classify(
                Some(DiffSide {
                    blob,
                    mode: Mode::Gitlink
                }),
                Some(DiffSide {
                    blob,
                    mode: Mode::Symlink
                })
            ),
            DiffOp::TypeChange
        );
    }

    fn hunk_of(old: &str, new: &str) -> String {
        let mut out = String::new();
        render_unified(old.as_bytes(), new.as_bytes(), &mut out);
        out
    }

    #[test]
    fn unified_diff_basics() {
        // Plain modify.
        let h = hunk_of("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(h, "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n");

        // Insertion at the very start: trailing context only, git's
        // `-1,2 +1,3` shape.
        let h = hunk_of("b\nc\n", "a\nb\nc\n");
        assert_eq!(h, "@@ -1,2 +1,3 @@\n+a\n b\n c\n");

        // Deletion to empty: `+0,0`, and a one-line old range drops `,1`.
        let h = hunk_of("a\n", "");
        assert_eq!(h, "@@ -1 +0,0 @@\n-a\n");

        // No change → no hunks.
        assert_eq!(hunk_of("same\n", "same\n"), "");

        // Missing trailing newline gets git's marker.
        let h = hunk_of("a\nb", "a\nB");
        assert!(h.contains("-b\n\\ No newline at end of file\n"), "{h}");
        assert!(h.contains("+B\n\\ No newline at end of file\n"), "{h}");
    }

    #[test]
    fn unified_diff_merges_nearby_changes_and_context() {
        let old: String = (0..20).map(|i| format!("{i}\n")).collect();
        // Changes 5 unchanged lines apart: within git's 2×context merge
        // window — one hunk.
        let mut near: Vec<String> = (0..20).map(|i| format!("{i}\n")).collect();
        near[2] = "two\n".to_owned();
        near[8] = "eight\n".to_owned();
        let new: String = near.concat();
        let h = hunk_of(&old, &new);
        // One header = one hunk; `@@` appears twice per header.
        assert_eq!(h.matches("@@ -").count(), 1, "{h}");
        assert!(h.contains("-2\n+two\n"), "{h}");
        assert!(h.contains("-8\n+eight\n"), "{h}");

        // Far-apart changes split into two hunks.
        let mut far: Vec<String> = (0..40).map(|i| format!("{i}\n")).collect();
        far[1] = "one!\n".to_owned();
        far[30] = "thirty\n".to_owned();
        let new2: String = far.concat();
        let old2: String = (0..40).map(|i| format!("{i}\n")).collect();
        let h2 = hunk_of(&old2, &new2);
        assert!(h2.matches("@@ -").count() >= 2, "{h2}");
        assert!(h2.contains("-1\n+one!\n"), "{h2}");
        assert!(h2.contains("-30\n+thirty\n"), "{h2}");
    }

    #[test]
    fn empty_files_and_crlf_diff_faithfully() {
        // Empty → content: git's `-0,0 +1` shape.
        let h = hunk_of("", "one\n");
        assert_eq!(h, "@@ -0,0 +1 @@\n+one\n");
        // CRLF lines keep their \r bytes.
        let h = hunk_of("a\r\nb\r\n", "a\r\nB\r\n");
        assert!(h.contains("-b\r\n"), "{h}");
        // Mode-only change (identical bytes) renders no hunks at all.
        assert_eq!(hunk_of("x\n", "x\n"), "");
    }

    // --------------------------------------------- binary and gitlink rules

    /// A gitlink blob id as the import stores it: synthetic padded bytes,
    /// not a git submodule SHA.
    fn gitlink_id(byte: u8) -> gardenbed_model::Id {
        let mut padded = [0u8; 32];
        padded[12..].copy_from_slice(&[byte; 20]);
        gardenbed_model::blob_id(&padded)
    }

    fn diff_entry(
        path: &str,
        op: DiffOp,
        old: Option<(gardenbed_model::Id, Mode)>,
        new: Option<(gardenbed_model::Id, Mode)>,
    ) -> DiffEntry {
        DiffEntry {
            path: path.to_owned(),
            op,
            old: old.map(|(blob, mode)| DiffSide { blob, mode }),
            new: new.map(|(blob, mode)| DiffSide { blob, mode }),
        }
    }

    #[test]
    fn binary_entries_render_git_differ_line() {
        let text = gardenbed_model::blob_id(b"one\ntwo\n");
        let bin_old = gardenbed_model::blob_id(b"icns\0\0\x01\x02old");
        let bin_new = gardenbed_model::blob_id(b"icns\0\0\x01\x02NEW");
        let store = [
            (text, b"one\ntwo\n".to_vec()),
            (bin_old, b"icns\0\0\x01\x02old".to_vec()),
            (bin_new, b"icns\0\0\x01\x02NEW".to_vec()),
        ];
        let content_of = move |id: gardenbed_model::Id| {
            store
                .iter()
                .find(|(k, _)| *k == id)
                .map(|(_, v)| v.clone())
                .ok_or(ClientError::BlobHashMismatch(id))
        };

        // Modified: `Binary files a/x and b/x differ` — a plain modify has
        // no mode lines to carry the header, so the `---`/`+++` markers
        // keep the entry one git apply parses cleanly.
        let out = render_diff(
            &[diff_entry(
                "x.icns",
                DiffOp::Modify,
                Some((bin_old, Mode::Regular)),
                Some((bin_new, Mode::Regular)),
            )],
            &content_of,
        )
        .unwrap();
        assert_eq!(
            out,
            "diff --git a/x.icns b/x.icns\n--- a/x.icns\n+++ b/x.icns\n\
             Binary files a/x.icns and b/x.icns differ\n"
        );

        // Deleted: git's `/dev/null` convention for the gone side.
        let out = render_diff(
            &[diff_entry(
                "x.icns",
                DiffOp::Delete,
                Some((bin_old, Mode::Exec)),
                None,
            )],
            &content_of,
        )
        .unwrap();
        assert_eq!(
            out,
            "diff --git a/x.icns b/x.icns\ndeleted file mode 100755\n\
             Binary files a/x.icns and /dev/null differ\n"
        );

        // Added: `/dev/null` on the old side.
        let out = render_diff(
            &[diff_entry(
                "x.icns",
                DiffOp::Add,
                None,
                Some((bin_new, Mode::Regular)),
            )],
            &content_of,
        )
        .unwrap();
        assert_eq!(
            out,
            "diff --git a/x.icns b/x.icns\nnew file mode 100644\n\
             Binary files /dev/null and b/x.icns differ\n"
        );

        // A content change that also flips the mode keeps git's mode lines
        // ahead of the binary line.
        let out = render_diff(
            &[diff_entry(
                "x.icns",
                DiffOp::Modify,
                Some((bin_old, Mode::Regular)),
                Some((bin_new, Mode::Exec)),
            )],
            &content_of,
        )
        .unwrap();
        assert_eq!(
            out,
            "diff --git a/x.icns b/x.icns\nold mode 100644\nnew mode 100755\n\
             Binary files a/x.icns and b/x.icns differ\n"
        );

        // A type change to/from a binary side renders the same single line
        // (git splits the two halves into its own two sections; bed keeps
        // one entry per path — the documented shape divergence).
        let out = render_diff(
            &[diff_entry(
                "link",
                DiffOp::TypeChange,
                Some((text, Mode::Regular)),
                Some((bin_new, Mode::Symlink)),
            )],
            &content_of,
        )
        .unwrap();
        assert_eq!(
            out,
            "diff --git a/link b/link\nold mode 100644\nnew mode 120000\n\
             Binary files a/link and b/link differ\n"
        );
    }

    #[test]
    fn binary_mode_only_change_stays_mode_lines_only() {
        // Identical blob on both sides (mode flip): git prints no binary
        // line here, and working copy's earlier mode-only form is preserved.
        let bin = gardenbed_model::blob_id(b"\0\0binary\0");
        let store = [(bin, b"\0\0binary\0".to_vec())];
        let content_of = move |id: gardenbed_model::Id| {
            store
                .iter()
                .find(|(k, _)| *k == id)
                .map(|(_, v)| v.clone())
                .ok_or(ClientError::BlobHashMismatch(id))
        };
        let out = render_diff(
            &[diff_entry(
                "x.dat",
                DiffOp::Modify,
                Some((bin, Mode::Regular)),
                Some((bin, Mode::Exec)),
            )],
            &content_of,
        )
        .unwrap();
        // Byte-identical to the pre-fix form: mode lines plus the ---
        // /+++ markers (no hunks), never a binary line.
        assert_eq!(
            out,
            "diff --git a/x.dat b/x.dat\nold mode 100644\nnew mode 100755\n\
             --- a/x.dat\n+++ b/x.dat\n"
        );
    }

    #[test]
    fn binary_probe_samples_first_8kib() {
        // NUL inside the first 8 KiB → binary.
        let mut inside = vec![b'a'; 8192];
        inside[8191] = 0;
        assert!(is_binary(&inside));
        // NUL just past it → text.
        let mut past = vec![b'a'; 8193];
        past[8192] = 0;
        assert!(!is_binary(&past));
        assert!(is_binary(b"\0leading nul"));
        assert!(!is_binary(b"plain text\n"));
    }

    #[test]
    fn gitlink_entries_render_mode_lines_only() {
        let sub_old = gitlink_id(0x19);
        let sub_new = gitlink_id(0x5a);
        let file = gardenbed_model::blob_id(b"file bytes\n");
        let fetched = std::cell::RefCell::new(Vec::new());
        let content_of = |id: gardenbed_model::Id| {
            fetched.borrow_mut().push(id);
            if id == file {
                Ok(b"file bytes\n".to_vec())
            } else {
                Err(ClientError::BlobHashMismatch(id))
            }
        };

        // Add: git's `new file mode 160000` alone — git apply accepts it.
        let out = render_diff(
            &[diff_entry(
                "sub",
                DiffOp::Add,
                None,
                Some((sub_new, Mode::Gitlink)),
            )],
            &content_of,
        )
        .unwrap();
        assert_eq!(out, "diff --git a/sub b/sub\nnew file mode 160000\n");

        // Delete: `deleted file mode 160000` alone.
        let out = render_diff(
            &[diff_entry(
                "sub",
                DiffOp::Delete,
                Some((sub_old, Mode::Gitlink)),
                None,
            )],
            &content_of,
        )
        .unwrap();
        assert_eq!(out, "diff --git a/sub b/sub\ndeleted file mode 160000\n");

        // Surviving pointer change: no mode lines exist (both sides are
        // 160000) and no honest Subproject sha to print — the binary line
        // carries the change, with the markers git's own gitlink diffs
        // use so the entry parses as a patch.
        let out = render_diff(
            &[diff_entry(
                "sub",
                DiffOp::Modify,
                Some((sub_old, Mode::Gitlink)),
                Some((sub_new, Mode::Gitlink)),
            )],
            &content_of,
        )
        .unwrap();
        assert_eq!(
            out,
            "diff --git a/sub b/sub\n--- a/sub\n+++ b/sub\n\
             Binary files a/sub and b/sub differ\n"
        );

        // Type changes to/from a gitlink render mode lines only — never
        // content hunks.
        let out = render_diff(
            &[diff_entry(
                "sub",
                DiffOp::TypeChange,
                Some((sub_old, Mode::Gitlink)),
                Some((file, Mode::Regular)),
            )],
            &content_of,
        )
        .unwrap();
        assert_eq!(
            out,
            "diff --git a/sub b/sub\nold mode 160000\nnew mode 100644\n"
        );
        let out = render_diff(
            &[diff_entry(
                "sub",
                DiffOp::TypeChange,
                Some((file, Mode::Regular)),
                Some((sub_new, Mode::Gitlink)),
            )],
            &content_of,
        )
        .unwrap();
        assert_eq!(
            out,
            "diff --git a/sub b/sub\nold mode 100644\nnew mode 160000\n"
        );

        // No blob was ever fetched: gitlink entries render from tree
        // metadata alone (the synthetic gitlink blobs are not even in the
        // local store), even when the other side is a regular file.
        assert!(fetched.borrow().is_empty(), "{:?}", fetched.borrow());
    }

    // ---------------------------------------------------------- blame

    fn group(lo: u64, hi: u64, byte: u8, name: &str, secs: u64, tz: i32) -> BlameGroup {
        BlameGroup {
            lo,
            hi,
            id: gardenbed_model::Id::from_bytes([byte; 32]),
            author_name: name.into(),
            time_secs: secs,
            tz_min: tz,
        }
    }

    #[test]
    fn blame_renders_git_like_lines() {
        let groups = [
            group(1, 2, 0xAA, "Ada", 1_700_000_000, 0),
            group(3, 4, 0xBB, "Bob", 1_700_086_400 + 3600, 60),
        ];
        let out = render_blame(&groups, b"one\ntwo\nthree\nfour\n");
        assert_eq!(
            out,
            "aaaaaaaaaaaaaaaa (Ada 2023-11-14 1) one\n\
             aaaaaaaaaaaaaaaa (Ada 2023-11-14 2) two\n\
             bbbbbbbbbbbbbbbb (Bob 2023-11-16 3) three\n\
             bbbbbbbbbbbbbbbb (Bob 2023-11-16 4) four\n"
        );
    }

    #[test]
    fn blame_day_dates_render_in_their_own_tz() {
        // +0545 pushes 22:13 UTC into the next civil day.
        assert_eq!(render_date_short(1_700_000_000, 345), "2023-11-15");
        assert_eq!(render_date_short(1_700_000_000, 0), "2023-11-14");
        assert_eq!(render_date_short(1_700_000_000, -840), "2023-11-14");
    }

    #[test]
    fn blame_of_an_empty_file_is_empty_output() {
        let groups = [group(1, 1, 0xAA, "Ada", 1, 0)];
        assert_eq!(render_blame(&groups, b""), "");
    }

    #[test]
    fn blame_keeps_a_missing_trailing_newline_line() {
        let groups = [group(1, 2, 0xAA, "Ada", 1, 0)];
        let out = render_blame(&groups, b"x\ny");
        assert!(out.ends_with(" 2) y\n"), "{out}");
    }
}
