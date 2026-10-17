//! gardenbed-web — the web GUI for one gardenbed repository.
//!
//! The library is a single axum [`router`] over a
//! [`gardenbed_server::Repo`]: the same service layer the gRPC server and
//! the MCP tools speak through, so a browser answer and a `bed`-over-gRPC
//! answer come from the same function calls on the same types. There is no
//! working copy anywhere in this path.
//!
//! HTTP surface (all JSON, same origin; the SPA itself is three static
//! files embedded at build time):
//!
//! * `GET /api/refs` — all refs, `{name, cs}` pairs.
//! * `GET /api/log?cs=<revspec>&limit=<n>` — git-`log`-ordered ancestry
//!   from `cs` (default `main`), `limit` default 100.
//! * `GET /api/changeset?id=<revspec>` — one changeset with full changes.
//! * `GET /api/tree?cs=<revspec>&path=<p>` — directory listing at `p`
//!   (`""` is the root); directories sort before files, then by name.
//! * `GET /api/file?cs=<revspec>&path=<p>` — file content, UTF-8 text or
//!   base64, capped at [`MAX_FILE_PAYLOAD`] bytes (`truncated` reports
//!   the cut); symlinks answer with their target text.
//! * `GET /api/blame?cs=<revspec>&path=<p>` — first-parent line ranges.
//! * `GET /api/diff?a=<revspec>&b=<revspec>` — tree diff `a` → `b`.
//! * `GET /api/files?cs=<revspec>&q=<substring>` — full file-path
//!   listing (fuzzy-finder data), case-insensitive substring filter on
//!   the full path, capped at [`MAX_FILES`] paths.
//! * `GET /api/search?cs=<revspec>&q=<string>&limit=<n>` — content grep
//!   across file blobs: case-insensitive line matches, skipping symlinks,
//!   gitlinks, and binary blobs, with scan/match/file caps reported as
//!   `truncated: true`.
//! * `GET /api/stats` — the server's per-RPC counters.
//! * `GET /api/server-info` — serving-process facts (crate version,
//!   repository root, bound address) for connect-instructions UIs.
//! * `GET /`, `GET /static/app.js`, `GET /static/styles.css` — the SPA.
//!
//! A revspec is a ref name, a full 64-hex changeset id, or an
//! unambiguous id prefix (8+ hex chars), resolved through
//! [`Repo::get_ref`] — the MCP tools' pattern (`crates/mcp/src/
//! tools.rs::resolve_cs`). Resolution failures map to statuses the way
//! `From<ServerError> for Status` does in the gRPC layer: unknown → 404,
//! ambiguous / too-short-prefix / bad 64-hex → 400. Every error body is
//! `{"error": "<message>"}`.
//!
//! `Repo` calls are synchronous; every handler runs them inside
//! `tokio::task::spawn_blocking` so the store never stalls the executor.

pub mod seed;

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, Tree, TreeEntry, TreeKind};
use gardenbed_server::{Repo, ServerError};

/// The ref every revspec defaults to when `cs`-style parameters are
/// absent or empty (the same default the MCP tools use).
pub const DEFAULT_REF: &str = "main";

/// Largest file payload served before truncation (512 KiB).
pub const MAX_FILE_PAYLOAD: usize = 512 * 1024;

/// Most paths `/api/files` reports before `truncated: true`.
pub const MAX_FILES: usize = 2_000;

/// Content `/api/search` scans at most before `truncated: true` (25 MiB
/// of blob bytes across the whole walk).
pub const MAX_SEARCH_SCANNED: usize = 25 * 1024 * 1024;

/// Most matches `/api/search` reports per file.
pub const MAX_MATCHES_PER_FILE: usize = 20;

/// The single-page app assets, embedded so the binary is self-contained.
const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const STYLES_CSS: &str = include_str!("../static/styles.css");

/// Facts about the serving process that are not derivable from the
/// repository: the on-disk root and the address the binary bound. Served
/// by `/api/server-info` so the UI can generate accurate
/// connect-this-repo instructions for agents.
#[derive(Clone, Debug, Default)]
pub struct ServerInfo {
    /// The `--root` the repository was opened from (absolute), when known.
    pub root: Option<String>,
    /// The `--addr`/bound address, when known (e.g. `"0.0.0.0:8430"`).
    pub addr: Option<String>,
}

/// Shared handler state: one repo behind an `Arc` (every `Repo` method
/// takes `&self`), plus the [`ServerInfo`] facts.
#[derive(Clone)]
struct AppState {
    repo: Arc<Repo>,
    info: Arc<ServerInfo>,
}

/// The web router over `repo`: every `/api/*` endpoint plus the embedded
/// static assets. Mount or serve directly; the binary serves it at `/`.
pub fn router(repo: Repo) -> Router {
    router_with_info(repo, ServerInfo::default())
}

/// Like [`router`], but `/api/server-info` also reports the given root
/// and bound address. The binary passes its own `--root`/`--addr`;
/// embedded users (tests, tiers) get a `null` root/addr via [`router`].
pub fn router_with_info(repo: Repo, info: ServerInfo) -> Router {
    Router::new()
        .route("/api/refs", get(handlers::refs))
        .route("/api/log", get(handlers::log))
        .route("/api/changeset", get(handlers::changeset))
        .route("/api/tree", get(handlers::tree))
        .route("/api/file", get(handlers::file))
        .route("/api/blame", get(handlers::blame))
        .route("/api/diff", get(handlers::diff))
        .route("/api/files", get(handlers::files))
        .route("/api/search", get(handlers::search))
        .route("/api/stats", get(handlers::stats))
        .route("/api/server-info", get(handlers::server_info))
        .route("/", get(handlers::index))
        .route("/static/app.js", get(handlers::app_js))
        .route("/static/styles.css", get(handlers::styles_css))
        .with_state(AppState {
            repo: Arc::new(repo),
            info: Arc::new(info),
        })
}

// ---------------------------------------------------------------- errors

/// An HTTP-visible failure: a status plus the contract's
/// `{"error": "<message>"}` body.
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl From<ServerError> for ApiError {
    /// The status taxonomy of the gRPC layer's `From<ServerError> for
    /// Status`, transposed to HTTP: absent objects and unresolvable
    /// revspecs are 404, bad caller input is 400, anything else is 500.
    fn from(e: ServerError) -> Self {
        match &e {
            ServerError::RevspecNotFound(_)
            | ServerError::ChangesetNotFound(_)
            | ServerError::ObjectNotFound(_)
            | ServerError::RefNotFound(_) => ApiError::not_found(e.to_string()),
            ServerError::AmbiguousPrefix { .. } | ServerError::Invalid(_) => {
                ApiError::bad_request(e.to_string())
            }
            _ => ApiError::internal(e.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

// ------------------------------------------------------------ revspecs

/// Resolves an HTTP revspec the way `resolve_cs` in the MCP tools does:
/// absent/empty → the [`DEFAULT_REF`]; 64 chars → must parse as hex; any
/// other token → [`Repo::get_ref`], which itself tries ref names, then
/// full ids, then 8+-char prefixes (ambiguous and too-short prefixes come
/// back as its [`ServerError::AmbiguousPrefix`] /
/// [`ServerError::Invalid`] → 400). A token naming nothing is 404.
fn resolve(repo: &Repo, spec: Option<&str>) -> Result<Id, ApiError> {
    let spec = spec.map(str::trim).unwrap_or("");
    if spec.is_empty() {
        return repo
            .get_ref(DEFAULT_REF)?
            .ok_or_else(|| ApiError::not_found(format!("ref {DEFAULT_REF:?} does not exist")));
    }
    if spec.len() == 64 {
        return Id::from_str(spec)
            .map_err(|e| ApiError::bad_request(format!("invalid changeset id {spec:?}: {e}")));
    }
    repo.get_ref(spec)?
        .ok_or_else(|| ApiError::not_found(format!("no ref or changeset matching {spec:?}")))
}

// -------------------------------------------------------------- shapes

fn identity_json(identity: &Identity) -> Value {
    json!({
        "name": identity.name,
        "email": identity.email,
        "time_secs": identity.time_secs,
        "tz_min": identity.tz_min,
    })
}

fn op_str(op: Op) -> &'static str {
    match op {
        Op::Add => "add",
        Op::Modify => "modify",
        Op::Delete => "delete",
        Op::Rename => "rename",
    }
}

fn mode_str(mode: Mode) -> &'static str {
    match mode {
        Mode::Regular => "regular",
        Mode::Exec => "exec",
        Mode::Symlink => "symlink",
        Mode::Gitlink => "gitlink",
    }
}

/// The shared wire shape of one [`Change`] (`/api/changeset`,
/// `/api/diff`): op/mode as lowercase words, `blob` the 64-hex id or
/// null, `src_path` set exactly for renames.
fn change_json(change: &Change) -> Value {
    json!({
        "op": op_str(change.op),
        "path": change.path,
        "src_path": change.src_path,
        "mode": mode_str(change.mode),
        "blob": change.blob.map(|id| id.to_hex()),
    })
}

fn entry_kind(entry: &TreeEntry) -> &'static str {
    match entry.kind {
        TreeKind::Dir => "directory",
        TreeKind::File => match entry.mode {
            Mode::Symlink => "symlink",
            Mode::Gitlink => "gitlink",
            _ => "file",
        },
    }
}

/// The RFC 4648 standard alphabet; gardenbed has no base64 dependency, so
/// the handful of binary blobs the file endpoint meets get this 20-line
/// encoder instead of a new crate.
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// --------------------------------------------------- tree- and path-walks

/// The tree at `path` under `cs`: `""` is the root. A path naming a file
/// is 400 ("not a directory"); any missing component is 404.
fn tree_at(repo: &Repo, cs: Id, path: &str) -> Result<Tree, ApiError> {
    let root = repo.root_tree(cs)?;
    let mut tree = match root {
        Some(t) => repo
            .get_tree(t)?
            .ok_or_else(|| ApiError::internal(format!("tree {t} missing from store")))?,
        None if path.is_empty() => return Ok(Tree::default()),
        None => {
            return Err(ApiError::not_found(format!(
                "no such path: {path:?} (empty history)"
            )));
        }
    };
    let mut components = path.split('/').filter(|c| !c.is_empty()).peekable();
    while let Some(component) = components.next() {
        let Some(entry) = tree.entries.iter().find(|e| e.name == component) else {
            return Err(ApiError::not_found(format!("no such path: {path:?}")));
        };
        match (entry.kind, entry.tree) {
            (TreeKind::Dir, Some(t)) => {
                tree = repo
                    .get_tree(t)?
                    .ok_or_else(|| ApiError::internal(format!("tree {t} missing from store")))?;
            }
            _ if components.peek().is_some() => {
                return Err(ApiError::not_found(format!(
                    "no such path: {path:?} ({component:?} is not a directory)"
                )));
            }
            _ => return Err(ApiError::bad_request(format!("not a directory: {path}"))),
        }
    }
    Ok(tree)
}

/// The tree entry at `path` under `cs`. A path naming a directory is 400
/// ("not a file"); any missing component is 404.
fn file_entry(repo: &Repo, cs: Id, path: &str) -> Result<TreeEntry, ApiError> {
    if path.trim_matches('/').is_empty() {
        return Err(ApiError::bad_request("not a file: ".to_owned() + path));
    }
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    if name.is_empty() {
        return Err(ApiError::bad_request(format!("not a file: {path}")));
    }
    let tree = tree_at(repo, cs, dir)?;
    let entry = tree.entries.iter().find(|e| e.name == name).cloned();
    match entry {
        None => Err(ApiError::not_found(format!("no such path: {path:?}"))),
        Some(e) if e.kind == TreeKind::Dir => {
            Err(ApiError::bad_request(format!("not a file: {path}")))
        }
        Some(e) => Ok(e),
    }
}

/// Every non-directory entry under `cs`, as `(full path, entry)` pairs
/// sorted by full-path bytes — the walk behind `/api/files` and
/// `/api/search`.
fn walk_files(repo: &Repo, cs: Id) -> Result<Vec<(String, TreeEntry)>, ApiError> {
    fn descend(
        repo: &Repo,
        prefix: &str,
        tree: &Tree,
        out: &mut Vec<(String, TreeEntry)>,
    ) -> Result<(), ApiError> {
        for entry in &tree.entries {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            match (entry.kind, entry.tree) {
                (TreeKind::Dir, Some(t)) => {
                    let subtree = repo.get_tree(t)?.ok_or_else(|| {
                        ApiError::internal(format!("tree {t} missing from store"))
                    })?;
                    descend(repo, &path, &subtree, out)?;
                }
                (TreeKind::Dir, None) => {} // empty derived directory: nothing inside
                (TreeKind::File, _) => out.push((path, entry.clone())),
            }
        }
        Ok(())
    }

    let mut out = Vec::new();
    let root = repo.root_tree(cs)?;
    let Some(root) = root else {
        return Ok(out); // no files anywhere in this history
    };
    let tree = repo
        .get_tree(root)?
        .ok_or_else(|| ApiError::internal(format!("tree {root} missing from store")))?;
    descend(repo, "", &tree, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

// ------------------------------------------------------------- handlers

/// Runs one synchronous `Repo` operation on the blocking pool and joins
/// it: handlers stay `async`, the store never stalls the executor.
async fn blocking<T, F>(state: AppState, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Repo) -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&state.repo))
        .await
        .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))?
}

fn static_asset(content: &'static str, content_type: &'static str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    (headers, content).into_response()
}

mod handlers {
    use super::*;

    /// The serving-process facts for the UI's connect instructions:
    /// crate version plus the root/addr passed to
    /// [`router_with_info`] (`null` when unknown). No `Repo` call, so no
    /// blocking hop.
    pub(super) async fn server_info(State(s): State<AppState>) -> Json<Value> {
        Json(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "root": s.info.root,
            "addr": s.info.addr,
        }))
    }

    pub(super) async fn refs(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
        blocking(s, |repo| {
            let refs: Vec<Value> = repo
                .list_refs()?
                .into_iter()
                .map(|(name, id)| json!({ "name": name, "cs": id.to_hex() }))
                .collect();
            Ok(Json(json!({ "refs": refs })))
        })
        .await
    }

    pub(super) async fn log(
        State(s): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Result<Json<Value>, ApiError> {
        blocking(s, move |repo| {
            let cs = resolve(repo, q.get("cs").map(String::as_str))?;
            let limit: u32 = match q.get("limit") {
                None => 100,
                Some(raw) => raw
                    .parse()
                    .map_err(|_| ApiError::bad_request(format!("invalid limit {raw:?}")))?,
            };
            let entries: Vec<Value> = repo
                .log(cs, limit)?
                .into_iter()
                .map(|(id, cs): (Id, Changeset)| {
                    json!({
                        "id": id.to_hex(),
                        "parents": cs.parents.iter().map(Id::to_hex).collect::<Vec<_>>(),
                        "message": cs.message,
                        "author": identity_json(&cs.author),
                        "committer": identity_json(&cs.committer),
                        "changes": cs.changes.len(),
                    })
                })
                .collect();
            Ok(Json(json!({ "resolved": cs.to_hex(), "entries": entries })))
        })
        .await
    }

    pub(super) async fn changeset(
        State(s): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Result<Json<Value>, ApiError> {
        blocking(s, move |repo| {
            let id = resolve(repo, q.get("id").map(String::as_str))?;
            let cs = repo.get_changeset(id)?;
            Ok(Json(json!({
                "id": id.to_hex(),
                "parents": cs.parents.iter().map(Id::to_hex).collect::<Vec<_>>(),
                "message": cs.message,
                "author": identity_json(&cs.author),
                "committer": identity_json(&cs.committer),
                "changes": cs.changes.iter().map(change_json).collect::<Vec<_>>(),
            })))
        })
        .await
    }

    pub(super) async fn tree(
        State(s): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Result<Json<Value>, ApiError> {
        blocking(s, move |repo| {
            let cs = resolve(repo, q.get("cs").map(String::as_str))?;
            let path = q.get("path").cloned().unwrap_or_default();
            let tree = tree_at(repo, cs, &path)?;
            // Directories before files, then by name (byte order).
            let mut entries = tree.entries;
            entries.sort_by_key(|e| e.name.clone());
            entries.sort_by_key(|e| i32::from(e.kind != TreeKind::Dir));
            let entries: Vec<Value> = entries
                .iter()
                .map(|e| {
                    let id = e.tree.or(e.blob).map(|id| id.to_hex());
                    json!({
                        "name": e.name,
                        "kind": entry_kind(e),
                        "mode": mode_str(e.mode),
                        "id": id,
                    })
                })
                .collect();
            Ok(Json(
                json!({ "cs": cs.to_hex(), "path": path, "entries": entries }),
            ))
        })
        .await
    }

    pub(super) async fn file(
        State(s): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Result<Json<Value>, ApiError> {
        blocking(s, move |repo| {
            let cs = resolve(repo, q.get("cs").map(String::as_str))?;
            let Some(path) = q.get("path").cloned() else {
                return Err(ApiError::bad_request("missing parameter: path"));
            };
            let entry = file_entry(repo, cs, &path)?;
            if entry.mode == Mode::Gitlink {
                return Err(ApiError::bad_request(format!(
                    "gitlink has no file content: {path}"
                )));
            }
            let blob = entry
                .blob
                .ok_or_else(|| ApiError::internal(format!("file entry {path:?} has no blob")))?;
            let content = repo
                .get_blob(blob)?
                .ok_or_else(|| ApiError::not_found(format!("object {blob} not found")))?;
            let size = content.len();
            let truncated = size > MAX_FILE_PAYLOAD;
            let body = &content[..content.len().min(MAX_FILE_PAYLOAD)];
            // Symlinks carry their target as the blob; it is text by
            // contract (lossy-decode only as a fallback for odd imports).
            let (encoding, content) = if entry.mode == Mode::Symlink {
                (
                    "utf8",
                    String::from_utf8_lossy(&content[..content.len().min(MAX_FILE_PAYLOAD)])
                        .into_owned(),
                )
            } else if let Ok(text) = std::str::from_utf8(body) {
                ("utf8", text.to_owned())
            } else {
                ("base64", base64_encode(body))
            };
            Ok(Json(json!({
                "path": path,
                "mode": mode_str(entry.mode),
                "size": size,
                "encoding": encoding,
                "content": content,
                "truncated": truncated,
            })))
        })
        .await
    }

    pub(super) async fn blame(
        State(s): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Result<Json<Value>, ApiError> {
        blocking(s, move |repo| {
            let cs = resolve(repo, q.get("cs").map(String::as_str))?;
            let Some(path) = q.get("path") else {
                return Err(ApiError::bad_request("missing parameter: path"));
            };
            let ranges: Vec<Value> = repo
                .blame(cs, path)?
                .into_iter()
                .map(|(lo, hi, who)| json!({ "lo": lo, "hi": hi, "cs": who.to_hex() }))
                .collect();
            Ok(Json(json!({ "ranges": ranges })))
        })
        .await
    }

    pub(super) async fn diff(
        State(s): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Result<Json<Value>, ApiError> {
        blocking(s, move |repo| {
            let a = resolve(repo, q.get("a").map(String::as_str))?;
            let b = resolve(repo, q.get("b").map(String::as_str))?;
            let changes: Vec<Value> = repo.diff(a, b)?.iter().map(change_json).collect();
            Ok(Json(
                json!({ "a": a.to_hex(), "b": b.to_hex(), "changes": changes }),
            ))
        })
        .await
    }

    /// `/api/files`: the full path listing at `cs` — the data behind the
    /// client's fuzzy file finder. `q` is a case-insensitive substring on
    /// the full path; the listing caps at [`MAX_FILES`] paths
    /// (`truncated: true` beyond, never an error).
    pub(super) async fn files(
        State(s): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Result<Json<Value>, ApiError> {
        blocking(s, move |repo| {
            let cs = resolve(repo, q.get("cs").map(String::as_str))?;
            let needle = q.get("q").map(|s| s.to_lowercase());
            let files = walk_files(repo, cs)?;
            let truncated = files.len() > MAX_FILES;
            let paths: Vec<Value> = files
                .into_iter()
                .take(MAX_FILES)
                .filter(|(path, _)| {
                    needle
                        .as_ref()
                        .is_none_or(|n| path.to_lowercase().contains(n))
                })
                .map(|(path, entry)| json!({ "path": path, "kind": entry_kind(&entry) }))
                .collect();
            Ok(Json(json!({
                "cs": cs.to_hex(),
                "paths": paths,
                "truncated": truncated,
            })))
        })
        .await
    }

    /// `/api/search`: a case-insensitive content grep over the blobs at
    /// `cs`. Symlinks, gitlinks, and binary blobs (a NUL in the first
    /// 8 KiB) are skipped; the walk bails with `truncated: true` once
    /// [`MAX_SEARCH_SCANNED`] blob bytes have been read, once a file
    /// reaches [`MAX_MATCHES_PER_FILE`] hits, or once `limit` files have
    /// matched. Line numbers are 1-based; matched lines are trimmed and
    /// hard-capped at 240 chars.
    pub(super) async fn search(
        State(s): State<AppState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Result<Json<Value>, ApiError> {
        blocking(s, move |repo| {
            let cs = resolve(repo, q.get("cs").map(String::as_str))?;
            let query = q.get("q").map(String::as_str).unwrap_or("").trim();
            if query.is_empty() {
                return Err(ApiError::bad_request("empty query"));
            }
            let needle = query.to_lowercase();
            let limit: usize = match q.get("limit") {
                None => 25,
                Some(raw) => raw
                    .parse()
                    .map_err(|_| ApiError::bad_request(format!("invalid limit {raw:?}")))?,
            }
            .min(100);

            let mut files_out: Vec<Value> = Vec::new();
            let mut truncated = false;
            let mut scanned = 0usize;
            'files: for (path, entry) in walk_files(repo, cs)? {
                if entry.mode == Mode::Symlink || entry.mode == Mode::Gitlink {
                    continue;
                }
                let Some(blob) = entry.blob else { continue };
                let Some(content) = repo.get_blob(blob)? else {
                    continue;
                };
                scanned += content.len();
                if scanned > MAX_SEARCH_SCANNED {
                    truncated = true;
                    break;
                }
                // Binary blobs: a NUL in the first 8 KiB.
                if content[..content.len().min(8 * 1024)].contains(&0) {
                    continue;
                }
                let text = String::from_utf8_lossy(&content);
                let mut matches: Vec<Value> = Vec::new();
                let mut capped = false;
                for (idx, line) in text.lines().enumerate() {
                    if line.to_lowercase().contains(&needle) {
                        if matches.len() == MAX_MATCHES_PER_FILE {
                            capped = true; // hits remain beyond the per-file cap
                            break;
                        }
                        let clipped: String = line.trim().chars().take(240).collect();
                        matches.push(json!({ "line": idx + 1, "text": clipped }));
                    }
                }
                if capped {
                    truncated = true;
                }
                if !matches.is_empty() {
                    files_out.push(json!({ "path": path, "matches": matches }));
                    if files_out.len() == limit {
                        truncated = true;
                        break 'files;
                    }
                }
            }

            Ok(Json(json!({
                "query": query,
                "cs": cs.to_hex(),
                "files": files_out,
                "truncated": truncated,
            })))
        })
        .await
    }

    pub(super) async fn stats(State(s): State<AppState>) -> Result<Json<Value>, ApiError> {
        blocking(s, move |repo| {
            let counters: serde_json::Map<String, Value> = repo
                .stats()
                .into_iter()
                .map(|(k, v)| (k, Value::from(v)))
                .collect();
            Ok(Json(json!({ "counters": Value::Object(counters) })))
        })
        .await
    }

    pub(super) async fn index() -> Response {
        static_asset(INDEX_HTML, "text/html; charset=utf-8")
    }

    pub(super) async fn app_js() -> Response {
        static_asset(APP_JS, "text/javascript; charset=utf-8")
    }

    pub(super) async fn styles_css() -> Response {
        static_asset(STYLES_CSS, "text/css; charset=utf-8")
    }
}
