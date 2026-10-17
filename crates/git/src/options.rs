//! Import options and ref-pattern matching.

/// Knobs for a git_bridge import.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    /// Detect renames (and copies, should git ever report them) with the
    /// `git diff-tree -M` subprocess and emit [`gardenbed_model::Op::Rename`] changes for
    /// them (decision D8). When `false`, git runs with `--no-renames` and a
    /// rename is imported as a plain delete+add pair — identical trees,
    /// different changeset shape. Default: `true`.
    pub detect_renames: bool,
    /// Ingest only the newest `N` commits of the whole reachable graph,
    /// taken as a suffix of the topological (Kahn) order.
    ///
    /// This is a quick-verification convenience, **not** a fidelity mode:
    /// commits whose first parent falls outside the slice are re-rooted
    /// (stored as root changesets carrying their full tree as adds), and
    /// extra parents outside the slice are dropped, so the mapping is not a
    /// bijection of the full history. Later parents that already have a
    /// mapping (e.g. from an earlier full import) are linked normally.
    /// Refs whose tips fall outside the slice are not recorded.
    pub limit: Option<usize>,
    /// Glob patterns (`*` and `?`) matched against full ref names such as
    /// `refs/heads/main`; empty means every branch. Annotated tags are
    /// deliberately not dereferenced in v0.1 (and are never read at all:
    /// only `refs/heads/*` is consulted).
    pub refs: Vec<String>,
    /// Permit ref moves that would lose changesets (full imports) or any
    /// non-descendant ref move (`--limit` imports). The default (`false`)
    /// still reports every backward move (see
    /// [`ImportReport::refs_rewound`]) but fails the import with
    /// [`ImportError::Ref`] — before any ref is touched — when any
    /// selected ref's move would take it off its current history: a full
    /// import refuses only moves that would actually lose changesets,
    /// while a `--limit` import cannot compute that count (slice
    /// re-rooting breaks real ancestry) and so refuses every
    /// non-descendant move, reporting the count as uncounted.
    /// `true` keeps force-push parity: refs move and every rewind is
    /// reported in the report.
    pub allow_rewind: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        ImportOptions {
            detect_renames: true,
            limit: None,
            refs: Vec::new(),
            allow_rewind: false,
        }
    }
}

/// Shell-style glob match: `*` spans any run of bytes, `?` matches one
/// byte; everything else is literal. No escaping (ref names cannot contain
/// `*` or `?` in practice).
pub(crate) fn glob_match(pattern: &str, name: &str) -> bool {
    let p = pattern.as_bytes();
    let n = name.as_bytes();
    glob_inner(p, n)
}

fn glob_inner(p: &[u8], n: &[u8]) -> bool {
    // Two-pointer matcher with backtracking on the last `*`.
    let mut star: Option<(usize, usize)> = None; // (position in p after *, position in n)
    let (mut pi, mut ni) = (0usize, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some((pi + 1, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp;
            ni = sn + 1;
            star = Some((sp, ni));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn glob_patterns() {
        assert!(glob_match("refs/heads/main", "refs/heads/main"));
        assert!(glob_match("refs/heads/*", "refs/heads/main"));
        assert!(glob_match("refs/heads/rel*", "refs/heads/release-1.2"));
        assert!(!glob_match("refs/heads/rel*", "refs/heads/dev"));
        assert!(glob_match("refs/heads/*-stable", "refs/heads/v3-stable"));
        assert!(!glob_match(
            "refs/heads/*-stable",
            "refs/heads/v3-stable/next"
        ));
        assert!(glob_match("*", "refs/heads/anything"));
        assert!(glob_match("refs/heads/ma?n", "refs/heads/main"));
        assert!(glob_match("refs/heads/ma?n", "refs/heads/maan"));
        assert!(!glob_match("refs/heads/ma?n", "refs/heads/man"));
    }
}
