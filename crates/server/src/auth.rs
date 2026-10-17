//! Token authentication and per-repository role-based access control.
//!
//! An `auth.toml` file maps each bearer token to a role (read, write, or
//! admin) and a list of repositories it may access, where `"*"` matches any
//! repository name:
//!
//! ```toml
//! [tokens."operator-token"]
//! repos = ["garden"]
//! role = "write"
//!
//! [tokens."auditor-token"]
//! repos = ["*"]
//! role = "read"
//! ```
//!
//! Roles form a lattice: admin implies write, write implies read. An absent
//! [`AuthPolicy`] means an open server (no authentication); a policy with no
//! matching token entry means every request is denied.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

/// What a token may do in a repository. Ordered from weakest to strongest;
/// admin implies write, write implies read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    /// Read-only access to repository contents and history.
    Read,
    /// Read access plus the ability to add objects and update refs.
    Write,
    /// Full access, reserved for future administrative operations.
    Admin,
}

impl Role {
    /// The role as spelled in `auth.toml`.
    fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Admin => "admin",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Role {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "read" => Ok(Role::Read),
            "write" => Ok(Role::Write),
            "admin" => Ok(Role::Admin),
            _ => Err(serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(&s),
                &"one of \"read\", \"write\", or \"admin\"",
            )),
        }
    }
}

/// One `[tokens.<token>]` entry: the repositories the token may access and
/// its role.
#[derive(Debug, Clone, serde::Deserialize)]
struct TokenEntry {
    repos: Vec<String>,
    role: Role,
}

/// The parsed policy from an `auth.toml` file: token -> entry.
///
/// Cheap to share behind an `Arc` by the service.
#[derive(Debug, Clone, Default)]
pub struct AuthPolicy {
    tokens: BTreeMap<String, TokenEntry>,
}

/// Outcome of [`AuthPolicy::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The token grants `needed` on `repo`.
    Allow,
    /// No token was presented.
    Anonymous,
    /// A token was presented but grants neither the repo nor the role.
    Denied,
}

impl AuthPolicy {
    /// Parses the auth config at `path`.
    ///
    /// Returns an actionable error (naming the file, token, and field) on any
    /// malformed input, so the server can refuse startup.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read auth config at {}: {}", path.display(), e))?;
        Self::from_str(&text, path)
    }

    /// Parses auth config text; `origin` names the file in error messages.
    fn from_str(text: &str, origin: &Path) -> Result<Self, String> {
        let raw: toml::Value = toml::from_str(text)
            .map_err(|e| format!("cannot parse auth config at {}: {}", origin.display(), e))?;
        let tokens = raw
            .get("tokens")
            .cloned()
            .unwrap_or_else(|| toml::Value::Table(Default::default()));
        let table = tokens.as_table().ok_or_else(|| {
            format!(
                "cannot parse auth config at {}: [tokens] must be a table of token entries",
                origin.display()
            )
        })?;
        let mut out = BTreeMap::new();
        for (token, value) in table {
            if token.trim().is_empty() {
                return Err(format!(
                    "cannot parse auth config at {}: token keys must not be empty",
                    origin.display()
                ));
            }
            let entry = TokenEntry::deserialize(value.clone()).map_err(|e| {
                format!(
                    "cannot parse auth config at {}: invalid entry for token {token:?}: {e}",
                    origin.display()
                )
            })?;
            if entry.repos.is_empty() {
                return Err(format!(
                    "cannot parse auth config at {}: token {token:?} lists no repositories; add at least one repository name or \"*\"",
                    origin.display()
                ));
            }
            out.insert(token.clone(), entry);
        }
        Ok(Self { tokens: out })
    }

    /// Number of tokens in the policy (startup log).
    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Whether `token` may perform an operation requiring `needed` on `repo`.
    pub fn check(&self, token: Option<&str>, repo: &str, needed: Role) -> Decision {
        let Some(token) = token else {
            return Decision::Anonymous;
        };
        let Some(entry) = self.tokens.get(token) else {
            return Decision::Denied;
        };
        let repo_ok = entry.repos.iter().any(|r| r == "*" || r == repo);
        if repo_ok && entry.role >= needed {
            Decision::Allow
        } else {
            Decision::Denied
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(text: &str) -> AuthPolicy {
        AuthPolicy::from_str(text, Path::new("auth.toml")).expect("policy should parse")
    }

    #[test]
    fn role_lattice() {
        let p = policy("[tokens.t]\nrepos = [\"*\"]\nrole = \"admin\"");
        assert_eq!(p.check(Some("t"), "any", Role::Read), Decision::Allow);
        assert_eq!(p.check(Some("t"), "any", Role::Write), Decision::Allow);
        assert_eq!(p.check(Some("t"), "any", Role::Admin), Decision::Allow);

        let p = policy("[tokens.t]\nrepos = [\"*\"]\nrole = \"write\"");
        assert_eq!(p.check(Some("t"), "any", Role::Read), Decision::Allow);
        assert_eq!(p.check(Some("t"), "any", Role::Write), Decision::Allow);
        assert_eq!(p.check(Some("t"), "any", Role::Admin), Decision::Denied);

        let p = policy("[tokens.t]\nrepos = [\"*\"]\nrole = \"read\"");
        assert_eq!(p.check(Some("t"), "any", Role::Read), Decision::Allow);
        assert_eq!(p.check(Some("t"), "any", Role::Write), Decision::Denied);
    }

    #[test]
    fn wildcard_matches_any_repo() {
        let p = policy("[tokens.t]\nrepos = [\"*\"]\nrole = \"read\"");
        assert_eq!(p.check(Some("t"), "garden", Role::Read), Decision::Allow);
        assert_eq!(p.check(Some("t"), "other", Role::Read), Decision::Allow);
    }

    #[test]
    fn per_repo_entry_only_matches_its_repo() {
        let p = policy("[tokens.t]\nrepos = [\"garden\"]\nrole = \"write\"");
        assert_eq!(p.check(Some("t"), "garden", Role::Write), Decision::Allow);
        assert_eq!(p.check(Some("t"), "other", Role::Read), Decision::Denied);
    }

    #[test]
    fn anonymous_and_unknown_tokens_are_denied() {
        let p = policy("[tokens.t]\nrepos = [\"*\"]\nrole = \"read\"");
        assert_eq!(p.check(None, "garden", Role::Read), Decision::Anonymous);
        assert_eq!(
            p.check(Some("nope"), "garden", Role::Read),
            Decision::Denied
        );
    }

    #[test]
    fn empty_policy_denies_everything() {
        let p = AuthPolicy::default();
        assert_eq!(p.check(None, "garden", Role::Read), Decision::Anonymous);
        assert_eq!(p.check(Some("t"), "garden", Role::Read), Decision::Denied);
    }

    #[test]
    fn parse_errors_name_the_problem() {
        let bad = "[tokens.t]\nrepos = [\"garden\"]\nrole = \"owner\"";
        let err = AuthPolicy::from_str(bad, Path::new("auth.toml")).unwrap_err();
        assert!(err.contains("invalid entry for token \"t\""), "{err}");
        assert!(err.contains("read"), "{err}");

        let bad = "[tokens.t]\nrepos = []\nrole = \"read\"";
        let err = AuthPolicy::from_str(bad, Path::new("auth.toml")).unwrap_err();
        assert!(err.contains("no repositories"), "{err}");

        let bad = "[tokens.\"\"]\nrepos = [\"garden\"]\nrole = \"read\"";
        let err = AuthPolicy::from_str(bad, Path::new("auth.toml")).unwrap_err();
        assert!(err.contains("must not be empty"), "{err}");

        let bad = "[tokens.t]\nrole = \"read\"";
        let err = AuthPolicy::from_str(bad, Path::new("auth.toml")).unwrap_err();
        assert!(err.contains("invalid entry for token \"t\""), "{err}");
        assert!(err.contains("repos"), "{err}");

        let bad = "not toml at all";
        let err = AuthPolicy::from_str(bad, Path::new("cfg/x.toml")).unwrap_err();
        assert!(err.contains("cfg/x.toml"), "{err}");
    }

    #[test]
    fn load_reports_unreadable_file() {
        let err = AuthPolicy::load(Path::new("/nonexistent/auth.toml")).unwrap_err();
        assert!(err.contains("cannot read auth config"), "{err}");
    }
}
