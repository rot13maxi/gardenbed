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
//! entries separated by one blank line. The id is bed's 16-hex short form;
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

use gardenbed_model::Mode;

use crate::error::BedError;

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

/// Renders one log entry in bed's git-log-like shape.
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

/// Renders a full diff. `content_of` fetches old/new blob bytes on demand
/// (cache-first at the caller); entries render in the order given.
pub fn render_diff(
    entries: &[DiffEntry],
    content_of: &dyn Fn(gardenbed_model::Id) -> Result<Vec<u8>, BedError>,
) -> Result<String, BedError> {
    let mut out = String::new();
    for entry in entries {
        render_diff_entry(entry, content_of, &mut out)?;
    }
    Ok(out)
}

/// Renders one changed path.
fn render_diff_entry(
    entry: &DiffEntry,
    content_of: &dyn Fn(gardenbed_model::Id) -> Result<Vec<u8>, BedError>,
    out: &mut String,
) -> Result<(), BedError> {
    out.push_str(&format!("diff --git a/{} b/{}\n", entry.path, entry.path));
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
            }
        }
    }

    // Content hunks. Gitlinks have no local content; symlinks diff as
    // their target bytes; adds and deletes diff against nothing.
    let old_content = match entry.old {
        Some(DiffSide { mode: Mode::Gitlink, .. }) | None => None,
        Some(side) => Some(content_of(side.blob)?),
    };
    let new_content = match entry.new {
        Some(DiffSide { mode: Mode::Gitlink, .. }) | None => None,
        Some(side) => Some(content_of(side.blob)?),
    };
    match (&old_content, &new_content) {
        (None, None) => {} // pure pointer change: mode lines carry it
        (old, new) => {
            out.push_str("--- ");
            out.push_str(&old.as_ref().map(|_| format!("a/{}", entry.path)).unwrap_or_else(|| "/dev/null".to_owned()));
            out.push('\n');
            out.push_str("+++ ");
            out.push_str(&new.as_ref().map(|_| format!("b/{}", entry.path)).unwrap_or_else(|| "/dev/null".to_owned()));
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
        assert_eq!(classify(None, Some(DiffSide { blob, mode: Mode::Regular })), DiffOp::Add);
        assert_eq!(classify(Some(DiffSide { blob, mode: Mode::Exec }), None), DiffOp::Delete);
        // Regular ↔ exec is M (same type); regular ↔ symlink is T.
        assert_eq!(
            classify(
                Some(DiffSide { blob, mode: Mode::Regular }),
                Some(DiffSide { blob, mode: Mode::Exec })
            ),
            DiffOp::Modify
        );
        assert_eq!(
            classify(
                Some(DiffSide { blob, mode: Mode::Regular }),
                Some(DiffSide { blob, mode: Mode::Symlink })
            ),
            DiffOp::TypeChange
        );
        assert_eq!(
            classify(
                Some(DiffSide { blob, mode: Mode::Gitlink }),
                Some(DiffSide { blob, mode: Mode::Symlink })
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
}
