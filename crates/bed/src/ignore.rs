//! `.gitignore` support for untracked-file discovery.
//!
//! The dogfood gap this closes: every agent runs cargo inside its bed
//! checkout, and `target/` lands at the checkout root with thousands of
//! files. Before this module, every one of them showed up as untracked
//! in `bed status` and rode into `bed commit`.
//!
//! Gitignore files are read from the working copy itself — `.gitignore`
//! at the root plus one in every directory the status walk enters
//! (nested files are git's standard mechanism: a `.gitignore` beside the
//! code it excludes stays local to that subtree). The pattern language
//! is the git subset the dogfood needs: blank lines, `#` comments,
//! trailing `/` (dir-only), `*` `?` `**` globs, and a leading `/`
//! (root-anchored). Out of scope, deliberately:
//!
//! - **Negation (`!`)** — an `!`-prefixed line is skipped entirely
//!   (treated as if commented out). Re-inclusion interacts with
//!   directory pruning and rule ordering; without it, "any match in any
//!   active `.gitignore` means ignored" is exactly right.
//! - **Character classes (`[…]`)** and backslash escapes other than a
//!   leading `\#` — they parse as their literal bytes and simply never
//!   match a path worth ignoring.
//! - `core.excludesFile` and `.git/info/exclude` — bed has no git
//!   configuration to read them from.
//!
//! Ignore applies to **untracked discovery only**, with git's
//! precedence: a file already recorded in the treestate stays tracked
//! and keeps reporting changes even when a pattern matches it. The
//! walk prunes an ignored directory only when it holds no tracked
//! paths, so tracked edits inside an ignored directory still report.
//! The working copy's `.bed` stays invisible exactly as before — the
//! state-path skip happens before any ignore logic.
//!
//! Matching is delegated to [`gix_glob`] — already in the workspace
//! lock (a transitive of the `gix` dependency in `crates/git`) and
//! tested against git's own `wildmatch`. The bed-side logic is
//! gitignore's scoping: each `.gitignore`'s patterns match paths
//! relative to its directory, and the trailing-`/` dir-only rule means
//! a pattern matches a directory (the walk then ignores everything
//! inside it), never a plain file.

use std::fs;
use std::path::Path;

use bstr::BStr;
use gix_glob::Pattern;
use gix_glob::pattern::Case;
use gix_glob::wildmatch;

/// The file name git and bed both read.
const FILE_NAME: &str = ".gitignore";

/// One parsed `.gitignore` line.
struct Rule {
    pattern: Pattern,
}

/// The `.gitignore` of one directory: rules matched against paths
/// relative to `dir` (repo-relative, forward slashes; `""` is the root).
struct Scope {
    dir: String,
    rules: Vec<Rule>,
}

/// The ignore state the status walk carries: every `.gitignore` loaded
/// this scan, plus the set currently in effect for the directory being
/// walked.
///
/// The walk snapshots [`Ignores::scopes_in_effect`] onto its directory
/// stack and restores it when it processes that directory, so a nested
/// `.gitignore` applies to its own subtree only: while `src/`'s entries
/// are classified, the root and `src` scopes are active; a sibling
/// directory never sees `src`'s rules.
#[derive(Default)]
pub struct Ignores {
    /// Every scope loaded this scan — an arena; scopes are never removed.
    scopes: Vec<Scope>,
    /// Indices into `scopes` currently in effect, outermost first.
    active: Vec<usize>,
}

impl Ignores {
    /// Loads `abs_dir`'s `.gitignore` (repo-relative `rel_dir`) into the
    /// active set. A missing, unreadable, or non-UTF-8 file contributes
    /// no rules — untracked discovery then behaves exactly as if the
    /// file did not exist.
    pub fn enter_dir(&mut self, abs_dir: &Path, rel_dir: &str) {
        let Ok(text) = fs::read_to_string(abs_dir.join(FILE_NAME)) else {
            return;
        };
        let mut rules = Vec::new();
        for raw in text.lines() {
            // CRLF tolerance, then git's trailing-space rule (escaped
            // trailing spaces are out of scope).
            let line = raw.strip_suffix('\r').unwrap_or(raw).trim_end();
            // Blank lines and comments carry no pattern; a leading `\#`
            // escapes a literal hash (gix-glob unescapes it).
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some(pattern) = Pattern::from_bytes(line.as_bytes()) else {
                continue;
            };
            if pattern.is_negative() {
                continue; // `!` re-inclusion: see the module docs
            }
            rules.push(Rule { pattern });
        }
        if !rules.is_empty() {
            self.active.push(self.scopes.len());
            self.scopes.push(Scope {
                dir: rel_dir.to_owned(),
                rules,
            });
        }
    }

    /// The active scope set — snapshot this before pushing a directory
    /// onto the walk stack.
    pub fn scopes_in_effect(&self) -> Vec<usize> {
        self.active.clone()
    }

    /// Restores a snapshot taken by [`Ignores::scopes_in_effect`].
    pub fn restore(&mut self, scopes: Vec<usize>) {
        self.active = scopes;
    }

    /// Is the repo-relative `path` (forward slashes) ignored by any
    /// active scope?
    ///
    /// `is_dir` is required for git's dir-only semantics: a trailing-`/`
    /// pattern matches directories — and, at the walk level, everything
    /// inside them once the directory is ignored — but never a plain
    /// file. Because `!` negation is unsupported, any match in any
    /// active scope means ignored; there is no re-inclusion to order.
    pub fn path_ignored(&self, path: &str, is_dir: bool) -> bool {
        self.active
            .iter()
            .any(|&i| self.scope_matches(&self.scopes[i], path, is_dir))
    }

    /// Does any rule of `scope` match `path`? Patterns match paths
    /// relative to the scope's own directory.
    fn scope_matches(&self, scope: &Scope, path: &str, is_dir: bool) -> bool {
        // Relativize: a scope's patterns never apply outside its
        // directory, and a directory's .gitignore never applies to the
        // directory itself (only to its contents).
        let rel = if scope.dir.is_empty() {
            path
        } else if path == scope.dir {
            return false;
        } else {
            match path.strip_prefix(&format!("{}/", scope.dir)) {
                Some(rel) => rel,
                None => return false,
            }
        };
        // `None` for a rel path without '/' is what
        // matches_repo_relative_path expects (basename = whole path).
        let basename_pos = rel.rfind('/').map(|p| p + 1);
        scope.rules.iter().any(|rule| {
            rule.pattern.matches_repo_relative_path(
                BStr::new(rel.as_bytes()),
                basename_pos,
                Some(is_dir),
                Case::Sensitive,
                wildmatch::Mode::NO_MATCH_SLASH_LITERAL,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `Ignores` walked through `dirs` (repo-relative), as the status
    /// walk would: each directory's `.gitignore` loads when it is entered.
    fn walked(root: &Path, dirs: &[&str]) -> Ignores {
        let mut ignores = Ignores::default();
        for dir in dirs {
            ignores.enter_dir(&root.join(dir), dir);
        }
        ignores
    }

    #[test]
    fn root_patterns_match_git_subset() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".gitignore"),
            "# comment\n\n/target\n*.tmp\nbuild/\nsrc/**/*.gen.rs\nlogs\n",
        )
        .unwrap();
        let ig = walked(dir.path(), &[""]);

        // Leading-/ anchor: the root target only, never a/target.
        assert!(ig.path_ignored("target", true));
        assert!(ig.path_ignored("target", false));
        assert!(!ig.path_ignored("a/target", true));
        assert!(!ig.path_ignored("a/target", false));

        // Basename glob, anywhere.
        assert!(ig.path_ignored("x/y/z.tmp", false));
        assert!(!ig.path_ignored("x/y/z.tmpx", false));

        // Trailing-/ is dir-only.
        assert!(ig.path_ignored("a/build", true));
        assert!(ig.path_ignored("build", true));
        assert!(!ig.path_ignored("a/build", false));

        // ** spans directories but stays inside src/.
        assert!(ig.path_ignored("src/a/b.gen.rs", false));
        assert!(ig.path_ignored("src/x.gen.rs", false));
        assert!(!ig.path_ignored("src/x.rs", false));
        assert!(!ig.path_ignored("other/a.gen.rs", false));

        // Slash-less pattern matches at any depth.
        assert!(ig.path_ignored("logs", false));
        assert!(ig.path_ignored("a/b/logs", false));
        assert!(!ig.path_ignored("logsx", false));
    }

    #[test]
    fn nested_gitignore_scopes_to_its_subtree() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "*.tmp\n").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/.gitignore"), "/local\ngen/\n").unwrap();
        let ig = walked(dir.path(), &["", "src"]);

        // The nested file's anchored pattern applies inside src/ only…
        assert!(ig.path_ignored("src/local", false));
        assert!(!ig.path_ignored("local", false));
        assert!(!ig.path_ignored("other/local", false));
        // …its dir-only pattern likewise…
        assert!(ig.path_ignored("src/a/gen", true));
        assert!(!ig.path_ignored("a/gen", true));
        // …while the root's rules still apply under src/.
        assert!(ig.path_ignored("src/x.tmp", false));
    }

    #[test]
    fn scopes_restore_to_their_directory() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "*.tmp\n").unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("doc")).unwrap();
        fs::write(dir.path().join("src/.gitignore"), "/local\n").unwrap();

        // Walking src/ (snapshot in effect)…
        let mut ig = Ignores::default();
        ig.enter_dir(&dir.path().join(""), "");
        let at_root = ig.scopes_in_effect();
        ig.enter_dir(&dir.path().join("src"), "src");
        assert!(ig.path_ignored("src/local", false));

        // …then a sibling directory restores the root-only snapshot.
        ig.restore(at_root);
        assert!(!ig.path_ignored("src/local", false));
        assert!(ig.path_ignored("doc/a.tmp", false));
    }

    #[test]
    fn comments_blank_lines_and_negation_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".gitignore"),
            "   \n# full line comment\n!keep.txt\n\\#literal\n",
        )
        .unwrap();
        let ig = walked(dir.path(), &[""]);

        assert!(!ig.path_ignored("keep.txt", false), "! negation is skipped");
        assert!(!ig.path_ignored("comment", false));
        // The escaped literal `#literal` parses and matches its text.
        assert!(ig.path_ignored("#literal", false));
    }

    #[test]
    fn unreadable_or_missing_files_mean_no_rules() {
        let dir = tempfile::tempdir().unwrap();
        let mut ig = Ignores::default();
        ig.enter_dir(&dir.path().join("nowhere"), "nowhere");
        assert!(!ig.path_ignored("target/debug/x", false));
        // Non-UTF-8 bytes are not a gitignore bed can read.
        fs::write(dir.path().join(".gitignore"), [0xFF, 0xFE]).unwrap();
        ig.enter_dir(dir.path(), "");
        assert!(!ig.path_ignored("target/debug/x", false));
    }
}
