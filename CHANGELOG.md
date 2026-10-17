# Changelog

All notable changes to gardenbed are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions are git
tags, and the verification record for a release lives in
[docs/release-0.1.md](docs/release-0.1.md).

## Unreleased

### Added

- **Multi-repository serving** (v0.2.md §5, M11): one server process
  serves any number of repositories from a root directory
  (`<root>/<repo>/objects` + `<root>/<repo>/metadata.sqlite3`); the
  directory tree is the registry. Repositories open lazily on first
  request (singleflighted), `--max-repos` (default 256) caps open handles
  with LRU eviction of idle ones, and per-repository request counters
  stay per-repository. Unknown repository names fail `not_found` listing
  the served names; illegal and reserved names fail `invalid_argument`.
- `gardenbed-server import --repo` and `export --repo` target a named
  repository; per-repository git mirrors live at
  `<root>/<repo>/git-mirror.git`.
- `gardenbed-server move --from NAME --into NEW-NAME` renames a
  repository under the root; history is byte-identical after the move.
  The move is an **offline operation**: it refuses (non-zero exit,
  nothing changed) while a server is serving the root, and it checkpoints
  the repository's WAL and carries `metadata.sqlite3-wal`/`-shm` with the
  database, so committed rows are never stranded at the old path.
- `bed clone http://host:port/<repo>` (M12): an http(s) URL's path names
  the repository; `--repo` wins on conflict. URL-clone and `--repo`-flag
  clone produce identical working-copy configs.
- `gardenbed-mcp --repo` is load-bearing: the named repository is the
  only one the session answers from; an unknown name exits 1 listing
  what the root serves.

- **Web GUI**: `gardenbed-web` (`crates/web`) — a read-only web interface
  over a repository: history timeline, ref cards, changeset detail with a
  grouped change list, tree browsing with breadcrumbs, file view with
  line numbers and first-parent blame, diff compare, and the server's
  request counters. It opens `--root` in process on the same `Repo`
  service layer as the gRPC server and MCP, serves an embedded
  no-build-step SPA plus a same-origin JSON API
  (`/api/refs|log|changeset|tree|file|blame|diff|stats`), and ships a
  `seed` subcommand that writes a deterministic demo history. The UI
  includes an agent hand-off dialog ("Use in agent") that generates a
  copyable connect-and-work prompt from `/api/server-info` plus the refs
  listing, an SVG commit-graph lane column on History, an activity
  heatmap and authors chart on Overview, and per-changeset diffstat
  chips. Browse directory pages render the directory's README
  GitHub-style (client-side, injection-escaped markdown subset; relative
  links and images resolve to the viewed revision's own files).
  Changeset and compare rows expand into real unified diffs (in-page
  Myers over both sides fetched at their revisions, with common
  prefix/suffix trimming, a 3000-line budget that renders a
  "too many lines to diff" note instead of freezing the page, and DOM
  built lazily only for expanded blocks); the UI polls
  `/api/refs` and toasts when the selected ref moves; blame gutters
  shade by relative age. New endpoints `/api/files` (path listing) and
  `/api/search` (case-insensitive content grep) back the `t` file
  finder, the topbar grep box (`#/search` route), and `j`/`k` keyboard
  navigation with a `?` help overlay. Multi-worker visibility: the
  Overview gains a Workers panel (per-author unpublished drafts,
  watchlist), an Events feed (publishes, rebases, landings), and a
  `#/review/<id>` route with the full diff and read-only land commands;
  backed by `MetadataStore::recent_changesets`, `Repo::activity`, and
  `GET /api/activity` (arrival order, unpublished included). See
  [docs/web.md](docs/web.md).
  Verified by in-memory router
  integration tests (`crates/web/tests/api.rs`) and a Playwright
  end-to-end suite (`e2e/`) that drives a real browser through every
  page against the seeded repository.

- Horizontal scale-out backends: Postgres metadata (`PostgresMetadataStore`)
  and S3 objects (`S3ObjectStore`) behind the unchanged storage traits, and
  `gardenbed-store` `StorageConfig` (TOML) to select them.
- `gardenbed-server --storage <config.toml>` (plus `GARDENBED_STORAGE` env
  fallback) for `serve`, `import`, and `export`: stateless replicas over
  off-box Postgres + S3 instead of local `--root` mode, which stays the
  default.
- `tools/scale/compose.yaml`: a Postgres + MinIO (`pgsty/minio`) + N-replica
  server deployment (see `tools/scale/README.md`); `crates/server/tests/scale.rs`
  proves read-your-writes, single-winner CAS, and idempotent creates across
  replicas, gated on `GARDENBED_SCALE_TEST` + live-service env vars.

- `bed publish --ref <branch>` (+ `--force`): publish to a named lane ref
  instead of `main` (dogfood finding #10 — lane agents committing
  off-main had no way to advance their branch, so a fresh clone's
  `bed switch <branch>` resolved the stale imported tip). The
  `PublishReq` wire message gained an optional `ref` field and a `force`
  flag (absent/empty `ref` = the exact v0.1 main publish; old clients
  are unchanged), lane ref targets get the same compare-and-swap
  semantics plus the import rewind guard (a non-descendant move is
  refused naming the ref, both tips, and the fell-off count unless
  `--force` is passed), and naming `main` in the `ref` field is rejected
  (main publishes stay the plain, review-gated form).

### Changed

- `gardenbed-server --root` takes the server root (one subdirectory per
  repository) rather than a single repository directory. A root laid out
  the v0.1 way (stores at the top level) keeps serving unchanged as the
  repository named `default`, so existing roots need no migration; use
  `gardenbed-server move --from default --into NAME` to adopt the
  per-directory layout. A serving server holds a **shared** advisory
  lock on `<root>/gardenbed.lock` for its lifetime (many servers may
  serve many roots, but file-relocating operations are fenced — see
  `move`), and repository resolution rechecks the directory under the
  open gate so a concurrent move cannot recreate an empty repository at
  a vacated name.
- Per-repository request counters are cumulative per name for the
  server's lifetime: LRU eviction closes idle repositories, and a reopen
  keeps counting where the evicted epoch left off (`get_stats` no longer
  resets after churn).
- `bed clone` refuses an http(s) URL whose path has more than one
  segment (`http://host:port/a/b` — it cannot name a repository) instead
  of silently cloning `default`.


### Fixed

- **Test harness refuses a stale `gardenbed-server` binary** (dogfood
  findings #12 and #13): `crates/bed/tests/common/mod.rs::server_bin()`
  panics with the fix command (`cargo build -p gardenbed-server`) when
  the workspace binary fails either staleness oracle — older mtime than
  the newest `crates/server/src` source, or (the #12 core) an embedded
  build fingerprint (`gardenbed_server::SRC_DIGEST`, SHA-256 over
  `src/**` computed by the new `crates/server/build.rs`) that does not
  match the digest of the working copy's current sources, catching a
  binary built from different tree state even when mtimes look fresh
  (`bed switch` materializes sources with commit-time mtimes). A stale
  binary otherwise fails protocol-dependent tests in ways that look like
  code regressions — and, per dogfood #13, a pre-protocol-change server
  silently turns `bed publish --ref <lane>` into a main publish
  (proto3 unknown-field skipping; see docs/DOGFOOD.md #13 — restart
  servers before using `--ref`). `BED_SERVER_BIN` overrides are trusted
  as-is. Regression proofs in `crates/bed/tests/server_bin_guard.rs`
  (refusal names the fix command; the tree-state probe holds mtime
  constant so only the digest can catch it; touches are net-zero). The
  publish `--ref` ref-row outcomes — new-lane row created + main
  untouched, force+non-ancestor lane move + main untouched — are pinned
  in `crates/bed/tests/write_path.rs`.

- **Rename pairing now guards destination and entry kind** (retroactive
  review of the M19 landing): `bed diff`'s render-time pairing, the
  scoped forms' recorded-`Rename` rendering, and `bed commit`'s
  `Op::Rename` recording all pair a delete with a write of the identical
  blob only when the destination was absent on the old side and the
  entry kinds match. Pre-fix, a delete paired with a *rewrite of a
  pre-existing path* or a *cross-kind write* (e.g. a regular file
  replaced by a symlink whose target equals its content) rendered an
  `R100` block git apply refuses ("already exists in working directory"
  / "new mode does not match"); those corners now render git's own
  `D`+`M` / `D`+`A` shape, which applies. Verified by new unit tests and
  a real-`git apply` regression pair
  (`crates/bed/tests/render_patch.rs::guarded_corners_render_delete_plus_write_and_apply`).
- `bed switch` now prunes directories its removals left empty (dogfood:
  switching off a changeset that introduced `crates/web` left the empty
  `crates/web/{src,static,tests}` behind, so a workspace glob `crates/*`
  matched a member with no `Cargo.toml` and `cargo check` failed). A
  directory is pruned only when it contains nothing at all — untracked
  or ignored content, other directories, or a sparse-selected prefix
  keep it on disk; the working-copy root and the state directory are
  never candidates; the sparse selection itself stays recorded in the
  config regardless of disk state.
- `bed sparse list` marks selected prefixes that match nothing at the
  current changeset (suffix ` (empty at this changeset)`), computed
  offline from the treestate's materialized rows — a dead selection no
  longer reads like live coverage. Recording empty selections ahead of
  history is unchanged by design.
- **No-argument `bed diff` now reads the working copy with the scanner's
  metadata discipline** (`symlink_metadata`, never following): a
  retargeted tracked symlink's new side is its link-target bytes — the
  same bytes `commit` records, not the target file's content — so a
  dangling link and a tracked file replaced by a directory both render
  (the latter as a deletion, matching `status`) instead of exiting 1
  ('No such file or directory' / 'Is a directory'). Also: a working
  copy dirty only by untracked files no longer prints a header-only
  empty working-copy diff — untracked files were never part of it, so
  the diff falls back to the committed current-vs-parent diff (git
  parity, `docs/cli.md`); empty diffs (identical sides, a root
  changeset with no parent) now print their header too, stating the
  fact (`— both sides are equal` / `— a root changeset has no parent`)
  instead of printing nothing; and the two-argument header claims
  "against its first parent" only when the older side really is the
  newer's first parent — any other pair reads `diff of <older> against
  <newer>` (regression tests in `crates/bed/tests/write_path.rs`:
  `diff_retargeted_symlink_shows_link_target_bytes_not_target_content`,
  `diff_dangling_symlink_renders_instead_of_failing`,
  `diff_tracked_file_replaced_by_directory_renders_as_deletion`,
  `diff_two_arg_header_names_both_sides_for_non_parent_pair`,
  `diff_untracked_only_dirt_falls_back_to_committed_diff`).
- **`gardenbed-server import` no longer silently rewinds refs** when an
  incremental import moves a ref to a tip that is not a descendant of
  the ref's current value (e.g. re-importing a git repository whose
  `main` is behind the store's published `main` — the dogfood incident:
  published work fell off `main` with exit 0 and no output). Such an
  import now fails non-zero before any ref is touched when the move
  would lose changesets, naming the ref, both tips, and the count;
  `--allow-rewind` opts in (force-push parity) and the move is reported
  in the import report (`refs_rewound`) plus a loud stderr warning.
  Fell-off changesets are counted exactly via a metadata-only BFS.
  Forward moves are unchanged (silent). Tests:
  `crates/git/tests/incremental.rs::backward_ref_move_is_refused_then_reported`,
  `crates/server/tests/import_rewind.rs::import_rewind_is_loud_and_off_by_default`.

- **`gardenbed-server import` no longer partially moves refs on a
  refused rewind**, and `--limit` no longer bypasses the rewind guard.
  The refusal is now decided for every selected ref before any ref is
  touched (two-phase): a multi-ref import where one ref would move
  forward and another would rewind fails without moving the forward
  ref either, and every offending ref is named in the single error.
  Under `--limit` a non-descendant ref move is refused by default too —
  the fell-off count is not computable there (slice re-rooting breaks
  real ancestry), so the loss is reported as uncounted;
  `--allow-rewind` still permits the move, reported in the import
  report (`refs_rewound`) plus a loud stderr warning. Tests:
  `crates/git/tests/incremental.rs::rewind_refusal_moves_no_refs_in_multi_ref_import`,
  `crates/git/tests/incremental.rs::limit_import_refuses_rewind_by_default`.

### Changed


- **`bed diff` renders renames and submodule pointer hunks** (v0.2
  D22/D23): a pure rename renders git's `R100` form (`similarity index
  100%` / `rename from` / `rename to`) in every diff form — the
  changeset-scoped forms (`bed diff <cs>`, no-arg on a clean copy)
  render from the changeset's recorded `Rename` op, and the two-sided
  form pairs a delete with a write of the identical blob at render
  time — the same rule `bed commit` records one by: same entry kind,
  destination absent on the old side (git apply refuses anything else).
  A moved-and-edited file
  still renders as a delete plus a write — no similarity below 100% is
  ever claimed (divergences §6 residual). Submodule pointers render
  git's `Subproject commit <sha>` hunks on add, delete, and retarget,
  with the real upstream SHA from the padded gitlink id — replacing the
  v0.1 `Binary files … differ` line (divergences §7 residual:
  gitlink↔regular typechanges still render mode lines only). Every
  rendered shape reapplies with `git apply` (gitlink entries via
  `--cached`). Verified by letter/byte parity with `git diff-tree -M`
  and `--submodule=short` on generated rename/gitlink histories, plus
  real-`git apply` tests (`crates/bed/tests/read_path.rs`,
  `crates/bed/tests/render_patch.rs`). The MCP `diff` tool is
  unchanged, keeping its `--no-renames` letter parity deliberately.

- **`bed` honors `.gitignore` for untracked-file discovery**: `bed
  status` and `bed commit` now read `.gitignore` files from the working
  copy (root plus one per directory walked) and hide matching untracked
  files — the dogfood gap where every cargo `target/` landed in status
  and rode into commits. Supported subset: blank lines, `#` comments,
  trailing `/` (dir-only), `*` `?` `**` globs, a leading `/` anchor, and
  bracket expressions (`[…]` character ranges like `*.[oa]` and POSIX
  `[:class:]` forms). Deliberately unsupported: `!` negation and
  global excludes. Git semantics are kept where they matter: ignore
  never un-tracks (tracked files keep reporting changes, and tracked
  content defeats directory pruning), an ignored directory with no
  tracked content is pruned wholesale (a fresh `target/` costs one
  stat), and untracked `.gitignore` files themselves still report.
  Matching uses `gix-glob` (already in the workspace lock via the
  `gardenbed-git` dependency). Verified against `git check-ignore`
  behavior (including bracket expressions) and by integration tests
  (`crates/bed/tests/gitignore.rs`) against a real server process.

- Use conventional commands: `commit`, `rebase`, `publish`, `sparse`, and
  `switch` replace `sow`, `graft`, `harvest`, `plot`, and `goto`. Old names
  are not aliases. `--state-dir` replaces `--bed-dir`; `clone --sparse`
  replaces `--plot`.
- Use server, repository, working copy, changeset, object store, metadata
  database, and derived data throughout current help, messages, and docs.
- Rename the Git bridge to `gardenbed-git` (`crates/git`) and the planned
  daemon package to `gardenbed-daemon` (`crates/daemon`). RPCs use `Rebase`
  and `Publish`.
- Repository storage is `objects/` and `metadata.sqlite3`; working-copy
  state uses `index.sqlite3` and a `sparse` configuration field. This is a
  breaking pre-release interface and layout change; existing repositories
  and working copies need to be recreated using the current version.
- No-argument `bed diff` is context-aware and always self-describing: a
  working copy dirty by modified or deleted tracked files diffs local
  bytes against the current changeset (untracked files excluded, git
  parity, so untracked-only dirt falls back to the committed diff;
  deleted files render as removal to `/dev/null`), while an otherwise
  clean one keeps the current-changeset-vs-first-parent diff. Every
  diff — empty ones included — prints a one-line header naming what is
  being diffed (`diff of working copy against <cs>` /
  `diff of <cs> against its first parent` / `diff of <a> against <b>`
  for non-parent pairs); explicit `bed diff A [B]` still means
  changesets, now with the same header line.


## [0.1.0] - 2026-10-16

First tagged release. Everything below landed between the workspace
scaffold and the M8 close-out plus two post-M8 hardening rounds; the
milestone plan with per-milestone verification criteria is
[docs/v0.1.md](docs/v0.1.md).

### Added

- **M0 — canonical model**: `docs/canonical-encoding.md` (the byte-level
  object spec) and `gardenbed-model` — `Id`, `Changeset`, `Change`, blob
  and tree types; encoder/decoder; golden fixtures; a Python cross-check
  that re-derives SHA-256 ids from the spec alone; decoder fuzz targets.
- **M1 — storage**: `loam` fs object store (GBLO1 envelope, 2-level
  sharding, tmp+rename atomic writes) and `seedbed` sqlite metadata (WAL,
  compare-and-swap refs, git-SHA mapping, derived cache), with in-memory
  impls and torn-write/verification tests.
- **M2 — compost engine**: tree derivation (only touched directories),
  tree diff, ancestry skip list, eager materialization; arboretum service
  layer + tonic server (17 RPCs) with differential git-oracle tests;
  `harness` crate (seeded generator, git materializer, comparators).
- **M3 — rootstock import**: gitoxide pack reads, topological ingestion,
  batched `git diff-tree` rename detection, incremental re-import and
  force-push handling, git-SHA bijection; `gardenbed-server import`
  subcommand; differential verification incl. the ripgrep corpus (100%
  sampled match); linux (torvalds/linux mirror, 1.7M commits, 8.6 GiB)
  pinned as flagship real corpus.
- **M4-prep — bed local state + protocol**: the clap CLI surface, `.bed`
  config/treestate/heads, plot matcher, offline status engine, loam-backed
  object cache; `gardenbed.proto` (17 RPCs) with protox+tonic codegen.
- **M4 — bed read path**: `clone`/`plot`/`goto`/`file`/`log`/`diff` over
  gRPC with a cache-first want-list fetcher (singleflight, bounded
  concurrency), resumability (zero duplicate fetches proven via server
  counters), byte fidelity vs `git show`.
- **M5 — write path**: `bed sow` (two-phase upload; blobs > 1 MiB stream
  via `PutBlob`, smaller ride inline), `graft`/`harvest` wiring; rootstock
  export via one `git fast-import` stream (SHA-exact roundtrip for
  unsigned histories); 16-way harvest race and crash-mid-sow proofs.
- **M6 — blame**: git-faithful first-parent lineage blame with rename
  following (xdiff port) and path-summary bloom segments for
  O(segments) walks — 11,352 differential cases vs
  `git blame --first-parent` at 100% agreement; skip-enabled 100k-commit
  blame at 17.3 µs; raw mode-aware tree-diff fallback so pre-canonical
  `100664` commits import cleanly (documented divergence).
- **M7 — MCP surface**: `gardenbed-mcp` stdio server (`log`, `diff`,
  `blame`, `file` over the in-process service layer), the zero-checkout
  contract proven, 51-question scripted QA at 100%.
- **M8 — hardening + release**: scripted §0 demo (`demo/run-v0.1.sh`, all
  seven steps, SHA roundtrip 100% @ 50k commits), SIGKILL chaos suite
  (10 RPC classes; 100 cycles/class verified extended), six fuzz targets,
  criterion benches (sow size-independence ratio 1.002, all budgets met),
  CI (fmt + clippy + tests + budgets + fuzz smoke + demo), quickstart and
  release docs.
- **Post-M8**: importer performance (8.5×, see below), the end-to-end QA
  pass (7 fixes), `verify_corpus` sampled content-fidelity sweep bin
  (repo-root convention, uniform sampling), kernel corpus verification
  (bijection 1,485,163 ↔ 1,485,163; 1,000-sample sweep 100%; incremental
  re-import proof 0 new objects in 0.03 s).
- **Post-M8 hardening round 2**: `bed clone --ref` (without it, clone
  probes `main` → `master` → `trunk` and errors actionably, listing the
  arboretum's actual refs); truncated changeset ids resolve as hex
  prefixes (≥ 8 chars; ref names win precedence; ambiguity errors name
  the match count) through `GetRef`, shared by `bed`, the MCP tools, and
  every id-taking RPC — an indexed store primitive answers in 19 µs on
  the 1.485M-row kernel store (188 ms naive).

### Fixed

- **Graft replay was over-strict**: a modify onto a file whose content
  matched the changeset's parent's copy was refused — unusable for the
  common rebase case. Replay now implements the three-way rule (a write
  is admitted when the new parent's copy is the untouched pre-image;
  genuine conflicts stay hard errors with every conflicting path listed).
- **Error rendering**: gRPC status codes render by name
  (`the arboretum refused (InvalidArgument): …`), not as a bare number;
  `sow` with an empty message says the fix
  (`the message is empty — sow with -m <message>`); `plot add` of a
  prefix with nothing under it at the current changeset says so
  (`… it fills as history lands`) instead of failing silently; `bed` run
  from a garden root instead of a `.bed` directory gets a pointing error
  (`… --bed-dir names the .bed directory itself, so pass --bed-dir …`).
  All pinned byte-exact by regression tests.
- **`bed clone` ref fallback**: without `--ref`, clone probes `main` →
  `master` → `trunk` (deterministic: `main` wins ties); a total miss
  names what was probed and what the arboretum actually grows
  (`no ref main (tried main, master, trunk); pass --ref`).
- **Short-id revspecs**: an 8-to-63-hex token now resolves server-side;
  ambiguity errors name the match count
  (`prefix <p> is ambiguous (N changesets)`), too-short tokens get the
  minimum-length hint, and unmatched tokens say they matched neither a
  ref nor a prefix.
- **Diff rendering is git-apply-safe**: binary blobs render git's
  one-liner (`Binary files a/<p> and b/<p> differ`, `/dev/null` on the
  absent side) instead of lossy U+FFFD text hunks; submodule gitlinks
  render mode lines only — never hunks, never a synthetic
  `Subproject commit` line; a payload-free header can no longer leak its
  path into the next entry's parse. Plain, mode-only, empty-file, and
  unicode-path diffs render byte-identically to before; binary entries
  are refused by `git apply` exactly as they are in git's own plain
  patches (full-index binary patches are out of scope for v0.1).
- **`plot add` cannot touch bed's own state**: a prefix at or inside
  `.bed` is refused before any connection or state write
  (`refusing to plot .bed — bed tends its own state there`); broader
  plans exclude `.bed`-component paths with a printed note, so a repo
  that ships `.bed/` files can neither hijack nor corrupt bed state, and
  the plot summary stays truthful about what was materialized.
- **SIGPIPE**: `bed log | head` and friends die silently on a closed
  pipe (signal 13) like git and rg, instead of panicking with
  `failed printing to stdout: Broken pipe`.

### Performance

- **Importer 8.5×** (profiled on the live kernel import — 86% of wall was
  blocked in ext4 journal commits, not CPU). Three root causes fixed:
  the per-object directory fsync now honors `GARDENBED_FSYNC=off` like
  every other fsync; the quadratic `repair_dir_to_file` scan became
  O(C log C); each commit's ~5–6 sqlite transactions (~95 file locks)
  collapsed into one `BEGIN IMMEDIATE` transaction per commit. Measured:
  ripgrep 34→395/s, git.git 52→336/s, kernel segment 30→278/s;
  kill+resume proven byte-identical. (docs/release-0.1.md)
- **Blame transport + walk**: the 53× CLI gap vs `git blame
  --first-parent` (28.7 s vs 0.54 s on a 670-step walk) was Nagle
  buffering, not the walk — the server's serve path now sets
  `TCP_NODELAY` (28.4 s → 0.66 s cold, 0.07 s warm). Derive-side walk
  work: per-walk blob cache exploiting the consecutive-toucher invariant
  (blob fetches 3001 → 1497 on the measured walk), an O(1) termination
  counter, FNV-1a record hashing, and an engine-level blame memo; output
  byte-identical. (commit `f393f8c`, `crates/server/src/main.rs`)
- **Prefix revspec resolution**: an indexed id-range seek over the
  seedbed's 32-byte ids — 19 µs at 1.485M rows vs 188 ms naive.

### Security

- **Fuzz**: six cargo-fuzz targets (`decode_changeset`, `decode_tree`,
  `fuzz_tree_apply`, `fuzz_diff`, `fuzz_difftree_parse`, `fuzz_mcp_line`)
  with corpora committed; the varint-strictness and diff-reapply findings
  hardened the canonical decoder and the tree applier (crash inputs kept
  as rejection vectors under `fuzz/artifacts/`).
- **Chaos**: SIGKILL under load for every RPC class; every acknowledged
  answer re-fetched byte-exactly after restart, then loam checksum sweep,
  sqlite `integrity_check`, and derived-data rebuild determinism.
- **Durability semantics**: object writes are fully fsynced by default
  (tmp + fsync + rename + directory fsyncs). `GARDENBED_FSYNC=off`
  (`off`/`0`/`false`) relaxes durability to atomicity-only — tmp+rename
  is kept, fsyncs are skipped — for rebuildable bulk loads (corpus
  imports, demo seeds) where a crash costs a re-import, not data. Real
  repositories must run with durability on; the torn-write torture tests
  always do.

### Divergences

v0.1 keeps **zero silent divergences** from git: every deviation is
documented with a repro in [docs/divergences.md](docs/divergences.md) —
signed commits export unsigned, non-UTF-8 commit bytes transcode lossily,
unmodeled headers are dropped, pre-canonical `100664` tree modes
canonicalize, blame attributes blank hunk-edge lines per our own diff
rather than xdiff's, and `bed diff` renders renames as delete + add
(divergences.md §6). Everything else round-trips byte-exactly.

