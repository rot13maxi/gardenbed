//! Plot semantics — the sparse checkout path prefixes this bed materializes.
//!
//! Plot entries are *path prefixes*, never globs: `"src"`, `"src/lib"`, or
//! an exact file like `"Cargo.toml"`. Matching is component-boundary aware
//! — the prefix `"src"` matches `"src/a.rs"` but not `"srcfoo/a.rs"` — and
//! the whole garden can be plotted with the root prefix `"."`.

use crate::error::BedError;

/// Normalizes a plot prefix as typed on the command line or read from a
/// hand-edited config.
///
/// Trims whitespace and trailing slashes, rejects absolute paths, `..`
/// escapes, empty components, and embedded `.`/`..` parts. The root of the
/// garden is spelled `.` (a bare `/` normalizes to `.` too, since repo
/// paths are always repo-relative).
///
/// Returns the normalized prefix (e.g. `"src/"` → `"src"`).
pub fn normalize_prefix(raw: &str) -> Result<String, BedError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(BedError::PlotPrefix {
            raw: raw.to_owned(),
            reason: "it is empty".into(),
        });
    }

    let mut prefix = trimmed.to_owned();
    while prefix.ends_with('/') {
        prefix.pop();
    }
    // A lone "/" (or a path that collapses to one) covers the whole garden;
    // the repo-side spelling for that is ".".
    if prefix.is_empty() || prefix == "." {
        return Ok(".".to_owned());
    }

    if prefix.starts_with('/') {
        return Err(BedError::PlotPrefix {
            raw: raw.to_owned(),
            reason: "paths in the plot are repo-relative, not absolute".into(),
        });
    }
    for comp in prefix.split('/') {
        if comp.is_empty() {
            return Err(BedError::PlotPrefix {
                raw: raw.to_owned(),
                reason: "empty path component (double slash)".into(),
            });
        }
        if comp == "." {
            return Err(BedError::PlotPrefix {
                raw: raw.to_owned(),
                reason: "use '.' alone for the whole garden, not inside a prefix".into(),
            });
        }
        if comp == ".." {
            return Err(BedError::PlotPrefix {
                raw: raw.to_owned(),
                reason: "prefixes must stay inside the garden (no '..')".into(),
            });
        }
    }
    Ok(prefix)
}

/// Is `path` inside the plot?
///
/// Component-boundary aware: prefix `"src"` matches `"src/a.rs"` and the
/// path `"src"` itself, but never `"srcfoo/a.rs"`. An exact-file prefix
/// (`"Cargo.toml"`) matches only that path. The root prefixes `.` (or an
/// empty string, from a hand-edited config) match everything.
pub fn in_plot(path: &str, plot: &[String]) -> bool {
    if path.is_empty() {
        return false;
    }
    for prefix in plot {
        let prefix = prefix.trim_end_matches('/');
        if prefix.is_empty() || prefix == "." {
            return true;
        }
        if path == prefix || path.starts_with(&format!("{prefix}/")) {
            return true;
        }
        // Note the boundary above: a bare `starts_with(prefix)` would
        // wrongly pull "srcfoo/a.rs" under the "src" prefix.
    }
    false
}

/// Might files under the relative directory `dir` fall inside the plot?
///
/// The pruning predicate for the [`crate::status`] walk: descend into `dir`
/// only when some prefix lives inside it, equals it, or contains it. An
/// empty `dir` is the garden root — always descend.
pub fn dir_may_hold_plotted(dir: &str, plot: &[String]) -> bool {
    if dir.is_empty() {
        return true;
    }
    for prefix in plot {
        let prefix = prefix.trim_end_matches('/');
        if prefix.is_empty() || prefix == "." {
            return true;
        }
        if dir == prefix
            || dir.starts_with(&format!("{prefix}/"))
            || prefix.starts_with(&format!("{dir}/"))
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plot(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn component_boundaries_are_respected() {
        let p = plot(&["src"]);
        assert!(in_plot("src/a.rs", &p));
        assert!(in_plot("src", &p));
        assert!(in_plot("src/deep/nested/x.rs", &p));
        // The whole point: no partial-component matches.
        assert!(!in_plot("srcfoo/a.rs", &p));
        assert!(!in_plot("src-latest/a.rs", &p));
    }

    #[test]
    fn exact_file_prefixes_match_only_themselves() {
        let p = plot(&["Cargo.toml"]);
        assert!(in_plot("Cargo.toml", &p));
        assert!(!in_plot("Cargo.toml.bak", &p));
        assert!(!in_plot("sub/Cargo.toml", &p));

        let p = plot(&["src/main.rs"]);
        assert!(in_plot("src/main.rs", &p));
        // Component boundary again: "src/main" must not swallow "src/main.rs".
        assert!(!in_plot("src/main.rs", &plot(&["src/main"])));
    }

    #[test]
    fn nested_prefixes() {
        let p = plot(&["src/lib"]);
        assert!(in_plot("src/lib/x.rs", &p));
        assert!(!in_plot("src/libfoo/x.rs", &p));
        assert!(!in_plot("src/other/x.rs", &p));
        assert!(!in_plot("src/lib.rs", &p));
    }

    #[test]
    fn root_prefix_matches_everything() {
        let p = plot(&["."]);
        assert!(in_plot("src/a.rs", &p));
        assert!(in_plot("Cargo.toml", &p));
        assert!(in_plot("a/b/c/d/e", &p));
        // An empty prefix (hand-edited config) behaves as the root too.
        assert!(in_plot("src/a.rs", &plot(&[""])));
    }

    #[test]
    fn empty_plot_matches_nothing() {
        assert!(!in_plot("src/a.rs", &[]));
    }

    #[test]
    fn any_prefix_matches() {
        let p = plot(&["docs", "src"]);
        assert!(in_plot("docs/x.md", &p));
        assert!(in_plot("src/a.rs", &p));
        assert!(!in_plot("tests/x.rs", &p));
    }

    #[test]
    fn trailing_slashes_are_tolerated_in_config() {
        let p = plot(&["src/"]);
        assert!(in_plot("src/a.rs", &p));
        assert!(!in_plot("srcfoo/a.rs", &p));
    }

    #[test]
    fn normalize_trims_and_rejects_escapes() {
        assert_eq!(normalize_prefix("src/").unwrap(), "src");
        assert_eq!(normalize_prefix("  src/lib/  ").unwrap(), "src/lib");
        assert_eq!(normalize_prefix(".").unwrap(), ".");
        assert_eq!(normalize_prefix("/").unwrap(), ".");
        assert!(normalize_prefix("").is_err());
        assert!(normalize_prefix("   ").is_err());
        assert!(normalize_prefix("/abs").is_err());
        assert!(normalize_prefix("../escape").is_err());
        assert!(normalize_prefix("a//b").is_err());
        assert!(normalize_prefix("a/./b").is_err());
        assert!(normalize_prefix("a/../b").is_err());
    }

    #[test]
    fn walk_pruning_never_skips_a_plotted_file() {
        let p = plot(&["src/lib/x.rs"]);
        assert!(dir_may_hold_plotted("", &p));
        assert!(dir_may_hold_plotted("src", &p));
        assert!(dir_may_hold_plotted("src/lib", &p));
        assert!(!dir_may_hold_plotted("src/other", &p));
        assert!(!dir_may_hold_plotted("docs", &p));

        let p = plot(&["src"]);
        assert!(dir_may_hold_plotted("src", &p));
        assert!(dir_may_hold_plotted("src/deep", &p));
        assert!(!dir_may_hold_plotted("srcfoo", &p));

        // Root prefix descends everywhere; empty plot descends nowhere.
        assert!(dir_may_hold_plotted("a/b", &plot(&["."])));
        assert!(!dir_may_hold_plotted("a/b", &[]));
    }
}
