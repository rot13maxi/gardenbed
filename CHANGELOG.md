# Changelog

All notable changes to gardenbed are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions are git
tags, and the verification record for a release lives in
[docs/release-0.1.md](docs/release-0.1.md).

## Unreleased

### Added

- Rename-aware merges across directories, recursive multi-base resolution,
  and native octopus merges with `bed merge <TARGET>...`. Conflicts refuse
  without publishing a partial merge. [Behavior and limits](docs/merge-rename-octopus.md)
  document scoring, parent order, and older-server refusal.

- CI now includes blocking bounded fuzz smoke and Playwright, plus
  nightly/manual PostgreSQL/S3 schema, parity, and replica checks.
  FUSE verification is explicitly enabled on a provisioned self-hosted
  runner; a separate manual workflow supports the pinned kernel corpus.
  [Verification coverage](docs/ci-verification.md) records prerequisites
  and guards against skipped or zero-test passes.

- **`gardenbed-ab suite`: a repeatable Gardenbed-vs-Git benchmark**.
  `gardenbed-ab suite --source <repo> --out <dir>` slices the newest N
  commits with a **real root** (a graft at the cut point, so the slice is
  complete history both systems accept), builds a byte-identical Git
  fixture and Gardenbed store, starts both servers on free ports, and
  runs six scenarios: `provision`, `query`, `history`, `refresh`
  (small + large selection), `land`, `contend`. One command, no manual
  server wrangling, no state carried between runs (`--out` must be
  empty). `gardenbed-ab fixture` builds the pair alone and prints the
  URLs. `bed`/`gardenbed-server` resolve to siblings of the harness
  binary (with `$BED_BIN`/`$BED_SERVER_BIN` overrides) and every run
  prints them — benchmarking a stale installed `bed` from PATH produced a
  141 ms number where the rebuilt binary measured 10 ms.

- **Checkout updates cost the change, not the selection.** `bed switch`
  (and `rebase`/`merge`, which move the working copy the same way)
  re-verified the entire sparse selection: two read-and-hash passes over
  every selected file plus one SQLite lookup and one upsert per file. On
  a 3,222-file selection a warm `switch` cost ~140 ms and ~16k
  `statx`/32k `fcntl` calls. Three changes:
  `CheckoutFs::entry_verified` settles a path from `stat` alone when its
  `HINT_VERIFIED` row still matches on size, mtime, and mode (the same
  trust `bed status` already extends to hints); `sync_sparse` skips paths
  whose target already equals their verified row entirely (nothing to
  write, nothing that could be refused); and `Treestate::all_entries` /
  `upsert_all` / `remove_all` replace the per-path query and statement
  round-trips. Result: **19.5 ms** at 3,222 files and **5.1 ms** at 10 on
  the same fixture (Git: 170 ms and 149 ms). Safety is unchanged and
  covered: a verified row that no longer matches is read and refused, an
  unverified row is always read, and a skipped path is never rewritten.

- **The pre-rename storage layout opens in place.** A root holding
  `seedbed.sqlite3` + `loam/objects` is the same store format under the
  historical directory names (identical schema, identical object
  envelopes), so `Repo::open` resolves it instead of refusing it — the
  1.48M-commit kernel store now serves directly. A root with neither
  layout but with foreign content is still refused rather than
  silently initialized as an empty repository beside the user's data.

- **`gardenbed-ab`: a paired Gardenbed-vs-Git benchmark harness**
  (`crates/bench/src/bin/ab.rs`). Six scenarios — `provision`, `query`,
  `land`, `contend`, `refresh`, `history` — run both systems against one
  history and record wall/CPU/max-RSS per process (`wait4(2)`), network
  bytes in both directions (a transparent TCP relay), and worker disk.
  Git is measured in three honest configurations: `git-partial`
  (blobless + sparse), `git-full`, and `git-worktree` (a shared clone
  with per-worker worktrees — the realistic "many agents on one host"
  setup, and the baseline any disk claim must beat). The harness
  **fails** a `git-partial` run when the server ignored `--filter`, and
  refuses to start with under 2 GiB free, because both cases silently
  produce flattering nonsense. Query results are checked for equivalence
  against a reference copy. See [docs/benchmarking.md](docs/benchmarking.md)
  for the dimension map,
  the methodology traps (Nagle stalls in a relay; `git daemon` ignoring
  filters; local-path clones ignoring filters; a stale `bed` on PATH),
  and the recorded results.

- **Offline commit queue** (v0.2.md §6, M15): when the server is
  unreachable at connect time — and only then; a server that answers,
  even to refuse, is online — `bed commit` completes locally: the
  changeset is built and content-addressed from the model, every changed
  blob is staged into `.bed/objects`, and `pending`/`pending_files`
  rows in `index.sqlite3` (created `IF NOT EXISTS`, so v0.1 files load
  unchanged) record the replay. The working copy moves exactly as an
  online commit's would. `bed sync` drains the queue oldest-first as an
  idempotent re-upload of content-addressed facts — a crash mid-sync
  costs a re-upload, never a divergence — then replays each offline
  commit's state move. `switch`, `rebase`, and `publish` refuse while
  commits are pending (naming `bed sync` and the count); `status`
  prints the pending line offline. Airplane-test coverage in
  `crates/bed/tests/offline.rs` (chained commits, SIGKILL in both
  crash windows, replay idempotence, offline refusals).

- **Blob chunking (GBLO2)** (v0.2.md §7, M14): blobs above 1 MiB are
  stored in a new content-defined chunked envelope — `"GBLO2"` magic ·
  `raw_len: u64` LE · `chunk_count: u32` LE · per-chunk
  `(raw_len, comp_len)` table · per-chunk zstd frames — with FastCDC
  boundaries (min 64 KiB / avg 256 KiB / max 1 MiB, Gear hash).
  Content-defined boundaries make the chunk table deterministic for
  identical content and stable under one-byte tail appends; identical
  prefix content shares chunks. Blobs ≤ 1 MiB keep the GBLO1 envelope
  byte-identically. Chunking is invisible above the store: no trait,
  wire-protocol, `Repo`, client fetch, or export changes — the bed
  client cache and `git` export reassembly are envelope-blind
  (wire-invariance + SHA-exact roundtrip tests). Torn or tampered
  objects remain whole-blob SHA-256 gated: one corrupted chunk is
  `Corrupt`, never partial bytes (six damage classes tested). M14
  put/get benches (release, fsync on, shared box): 1 MiB GBLO1 put
  26 ms / get 3 ms; 8 MiB GBLO2 put 34–55 ms / get 6–16 ms; 64 MiB
  GBLO2 put 163–277 ms / get 104–151 ms. No ≤ 1 MiB regression: the
  GBLO1 encode/decode path is byte-identical pre/post change — dispatch
  adds one length check and a 5-byte magic compare. Mixed-version
  note: rebuild server and bed together — an older bed reading a newer
  working copy's cache fails cleanly on a GBLO2 entry
  (`corrupt object: bad envelope magic`, rc=1); deleting the cached
  entry under `.bed/objects/` recovers it (refetch over the wire,
  re-cache). See `docs/canonical-encoding.md` §6.

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

- **gc mark: batched temp-table writes (backlog C1/C2)**. On the 10.36M-object
  kernel corpus the mark opened one SQLite transaction per `insert_live` call
  (once per tree, once per file entry, once per parent/blob chunk per
  changeset) and one statement per claimed tree id — tens of millions of
  transactions. Those transactions are temp-database only (nothing touches
  `metadata.sqlite3` or its WAL), but each still costs a journal
  create/delete cycle plus a commit: the bulk of the ~3 h mark. `gc_live`
  ids now accumulate in a 4096-id buffer committed in one transaction per
  batch, and tree-closure claims likewise accumulate in a 4096-id buffer
  published in bounded batches — flushed mid-tree when a single tree is
  wide enough (one may hold up to 2^20 entries), otherwise spanning trees —
  through a temp batch table plus one `INSERT OR IGNORE ... RETURNING`
  (the returned rows are exactly the newly claimed children to enqueue,
  preserving the push-time claim invariant and the distinct-tree queue
  bound). The mark set is unchanged (D14, V1 oracle unmodified; the new
  V1b fixture crosses both batch boundaries and fails if a boundary id is
  dropped). Crash semantics are unchanged and now structural: every mark
  row lives in the mark connection's `temp` schema, so nothing survives the
  connection, and the sweep still starts only after the complete build,
  with every buffer flushed.
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

- Merge-base selection now discards nonmaximal ancestors before timestamp
  ordering, fixing incorrect bases when commits share a second or clocks
  are skewed. Recursive virtual bases do not persist synthetic changesets.

- PostgreSQL schema initialization now serializes concurrent first opens
  across replicas in one transaction, preventing duplicate catalog-key
  failures on an empty database.

- **`bed rebase` / `bed rebase --stack` recognize already-landed twins**
  (dogfood finding #23). When a lane's content was landed on `main` by
  hand as a NEW changeset — tree-equal and same-message, but a different
  id with no ancestry link — replaying the lane used to hit the server's
  own-content check (`"<path>" was changed or deleted`) and refuse a
  perfectly legitimate re-union; operators hand-unioned again instead.
  Both forms now walk the new parent's span above the merge base and
  match on **exactly** the message string and the post-image root tree.
  A match is treated as already applied: the element is not replayed,
  every resolution is printed with the skipped element and its twin, the
  last twinned element subsumes the prefix (its tree is the cumulative
  lane state) and only the suffix replays, onto the new parent's current
  tip. A whole-stack match replays nothing, prints `stack already
  applied`, and moves the working copy to the new parent — a follow-up
  `bed publish` is then a fast-forward no-op. A twin whose paths `main`
  rewrote or reverted afterwards is not skipped (the element replays and
  the ordinary conflict rule decides), partial file overlap stays a hard
  conflict, and same-message/different-tree stays a hard conflict. The
  scan visits exactly `ancestors(new parent) \ ancestors(merge base)` —
  frontier parents below the base are pruned with `IsAncestor`, so a
  merge-carrying `main` cannot burn the budget on ancient history — costs
  one `GetChangeset` per span changeset plus a `RootTree` per message
  match, an `IsAncestor` per matched candidate and a `Diff` per twin
  candidate, and is capped at 4096 span changesets
  (`GARDENBED_TWIN_SCAN_CAP` overrides it for tests); the cap note is
  printed on stderr as soon as truncation is observed, so it is present
  even when the cap-induced miss becomes the ordinary conflict — but only
  while the run still has something to replay, so a twin that was found
  and applied (or a fully resolved stack) raises no false alarm. Both
  forms answer `already up to date` when the current changeset is an
  ancestor of the new parent (the stack form previously indexed an empty
  chain; the single form stored a redundant replay). A stale candidate
  that the prefix skip subsumes is reported as subsumed, never as
  replayed. Regression tests:
  `crates/bed/tests/write_path.rs` (twin, whole-stack + no-op publish,
  prefix twin, reverted twin, same-message/different-tree refusal,
  single rebase, already-up-to-date).
- **gc round 4: two landing reds fixed — v3 chaos red and the commit-p50
  size regression** (M13, v0.2.md): (1) the gc_chaos v3 red was the
  test's own replant: it lived inside `debug_assert_eq!(repo.put_blob(
  ...), *id)`, and `debug_assert_eq!` does not evaluate its operands in
  release builds, so the priming gc's 40 orphan deletions (all correct;
  visible as plain `unlink` syscalls the strace filter did not capture,
  which read as "zero unlinkat") were never undone and the next
  iteration's backdate ENOENT'd. The replant is now a plain
  `expect` + `assert_eq!`. gc itself was sound. (2) The round-3
  commit-transaction verify walked the new root's full tree closure
  (one `get_tree` per distinct subtree) — O(repository size) per
  commit: 1M-file commits p50'd 2.9s vs 10k's 56ms (ratio ~51 vs
  budget 2). The writer-side handshake now verifies the derivation's
  write set instead (every tree the replay stored, idempotent re-puts
  included) via flat `has()` checks — O(touched directories). Complete
  by V1/D14: trees the replay did not store are untouched subtrees of
  certified rows, which the gc mark never collects; the raced trees are
  exactly the replay's `put_tree` set (the P2-1 deterministic-replay
  orphan shape). Both server `CreateChangeset` and git import share the
  new `validated_tree_for_new_with_writes`. p50 after: 26.9ms vs
  25.9ms, ratio 1.04. Mutation re-evidence: verify gutted to no-op →
  `p2_1_missing_intermediate_tree_fails_the_commit` red; restored →
  green. Release gc_chaos 4/4 (the round-3 gap: v3 was debug-green /
  release-red and debug-only gates missed it).

- **Two merge mismerges fixed** (round-1 review, exposed by the merge-core
  salvage): the derive walk treated a `None` root tree as a "deleted
  subtree" instead of the empty root, spurious-conflicting theirs'
  root-level adds after ours emptied the root (git merges clean);
  and the adopt-theirs arm fell through to `take()`, whose blob expect
  panicked when theirs added a directory where base and ours had
  nothing. Pinned by
  `deleted_directory_untouched_inside_lands` and
  `added_directory_arrives_wholesale`
  (`crates/derive/tests/merge.rs`). Plus four hardening items: the
  `MergeConflict` client error carries the promised next-step hint, and
  `bed merge` pre-checks working-copy cleanliness with the same offline
  scan `bed status` runs — a dirty-copy refusal can no longer leave a
  stored merge behind (pinned by
  `bed_merge_refuses_dirty_working_copy_without_storing`,
  `crates/bed/tests/merge.rs`).
- **Daemon review fixes**: cache hits on `GetTree`/`GetBlob` consult
  upstream auth before answering (an anonymous or wrong-repo-scope peer
  could previously read any object a warm cache held, while the same
  request direct to the server got `PERMISSION_DENIED`; the probe is one
  repo-scoped, `authorize()`-gated read forwarded with the caller's
  credentials); cached blobs re-chunk at 64 KiB matching the server's
  stream shape (a blob-sized single chunk failed tonic's 4 MiB
  per-message decode limit for every blob over 4 MiB); the mount's
  failed readiness probe no longer leaks a live FUSE mount and its
  serving thread, `reply_with` answers `EIO` instead of panicking during
  teardown races, and the sidecar watcher fails open — errors and
  inotify queue-overflow events bump the generation, so a stale "clean"
  over modified files is forbidden.
- **Perf corpus clone-URLs serve `/garden`** (dogfood #15 Rule 2):
  `crates/bed/tests/perf.rs` clone sites name the repository in the URL
  path, reduced to one span.
- **CI repair for the v0.2 wave**: the test job builds the workspace
  binaries before running (protocol-dependent tests failed on a stale
  binary otherwise); the perf corpus clones a local ripgrep mirror; and
  the dev-box-only `kernel_export_spot_check` and the child-entry-point
  `fastcdc_table_child_entry` are skipped in the perf job.

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

- **Export is native (v0.2 M17, D20/D21)**: `gardenbed-server export`
  and `export_to_path` write the pack + idx directly (new
  `crates/git/src/pack.rs`: full objects, no deltas, SHA-1, zlib level 1,
  per-entry CRC-32, v2 idx with fanout / sorted ids / large-offset
  table) instead of spawning `git fast-import`. Object names are
  computed, not observed: every commit SHA the export produces is
  asserted against the git-mapping row, and drift is a hard error that
  names the changeset — never a silent divergence. Marks and
  `ExportReport::marks_path` retire; `pack_path` lands in the report.
  The pack is written atomically (tmp + rename + fsync) as
  `objects/pack/pack-<sha>.{pack,idx}`; re-exports are idempotent
  (same content ⇒ same name, skipped when present). The reader side
  (`read_pack`/`read_idx`) is a full structural validator used as the
  decode oracle and the fuzz target (`fuzz_pack_reader`; 1.12M runs, 0
  crashes). The pack streams to disk as objects are produced (bounded
  memory, ~285 MB peak on the 50k-commit demo corpus vs multi-GB
  buffered), with blob hashing, tree decoding and zlib compression
  spread over all cores; demo-corpus export wall-clock is **12.5 s**
  (50,100 commits, 106,694 blobs; 16-core tmpfs, load-averaged box),
  recorded as the new CI baseline against the 14.4 s fast-import
  predecessor — the ≤17.3 s M17.2 budget (1.2×) holds with margin.

- **Imported history exports verbatim (v0.2 M18, D21)**: commits whose
  bytes are not provably reconstructable from the model — signatures,
  `encoding`/`mergetag`/`gpgsig-sha256` headers, non-UTF-8 identity
  bytes, header-order drift — are captured raw at import into a sparse
  `git_raw_commit` side table (same ingest transaction), and the export
  emits those bytes as-is: signed and extra-header commits roundtrip
  SHA-exact, `signed_dropped == 0` for post-M18 imports. Two loud
  failure modes replace silent divergence: a raw commit whose tree
  header disagrees with the derived tree (pre-canonical `100664`,
  non-UTF-8 paths — raw-tree capture stays gated on the kernel
  throughput gate), and a raw commit naming parents outside the export
  (limit-slice boundaries refuse rather than emit a dangling pack).
  `docs/divergences.md` §1/§3 retired, §2 narrowed to path names, §4's
  export half retired. v0.1 stores refuse loudly at export for signed
  commits (the SHA assertion fires before anything ships; re-import
  retires, ids stable). Kernel capture exercised on
  1,000/5,000-commit segments (19% of commits captured, 951 raw rows
  on the 5k segment). The pinned full kernel corpus (a v0.1 store) is
  verified to refuse loudly at its first pre-canonical `100664` tree
  entry (`include/asm-mips/vr41xx/pci.h`, commit `5dfa9c1b`) after a
  clean 35 GB blob phase — never silently diverging;
  `kernel_export_spot_check` pins that refusal.

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


### Added

- **Token auth and per-repository roles**: `gardenbed-server serve
  --auth <auth.toml>` maps bearer tokens to repositories (exact names or
  `*`) and roles (`read` < `write` < `admin`). Every RPC is checked
  centrally — after the repo field is read, before any store is touched:
  reads need `read`+, writes (`PutBlob`, `CreateChangeset`, `CasRef`,
  `Rebase`, `Publish`, `Merge`) need `write`+; unauthorized requests get
  `PERMISSION_DENIED` naming the repository and the next step, and
  repository existence is never leaked to unauthorized callers. `bed`
  sends its token from `GARDENBED_TOKEN`; the daemon forwards
  credentials end to end; `gardenbed-mcp --token` is accepted for
  forward compatibility (in-process access needs none). Without `--auth`
  the server is open (v0.1 back-compat, proven by the unchanged
  open-server suite).
- **Server-side two-parent merge** (v0.2.md D9–D11, M9/M10): `Repo::merge`
  builds one changeset with `parents = [ours, theirs]` whose changes are
  the diff `tree(ours) → merged tree`, stored through the ordinary
  create path — no new object types. Per-file content merges with
  `merge3` over the xdiff port (touching/overlapping regions conflict,
  matching git's `xdl_merge`; regions both sides changed identically
  collapse), a three-way tree walk prunes by content-addressed equality
  before diffing (a side equal to base adopts the other wholesale —
  O(changed), not O(tree)), and mode changes follow the same trivial
  rules. Conflicts are one `FAILED_PRECONDITION` listing every
  conflicting path at file granularity. `bed merge <target>` and the
  `Merge` RPC; `up_to_date` when theirs is already an ancestor.
  Verified against `git merge-tree --write-tree` as an oracle over
  1,000 generated divergent pairs (plus fixture matrices, a derivation
  invariant, determinism across wiped caches, and unchanged publish on
  merge changesets); rename-blindness and criss-cross tie-break are
  documented divergences (divergences.md §8, §9). `fuzz_merge3` runs 3.4
  M+ cases clean.
- **Local daemon** (v0.2.md D18–D19, M16): `gardenbed-daemon serve
  --upstream <url>` is an optional per-machine caching tier between
  `bed` and the server — a gRPC proxy on one unix socket
  (`$XDG_RUNTIME_DIR/gardenbed/daemon.sock`; `GARDENBED_DAEMON`
  overrides, empty disables). `GetTree`/`GetBlob` answer cache-first
  from a shared object store at `$XDG_CACHE_HOME/gardenbed`
  (write-through, same envelope and sharding as `.bed/objects`) so
  working copies dedup; write RPCs are peer-uid-gated via `SO_PEERCRED`;
  a sidecar (`Ping`/`Register`) watches working-copy roots (50 ms
  debounce, `.bed` excluded) and bumps a generation counter that `bed
  status` uses to reuse its cached report — any doubt runs the full
  scan, a stale "clean" is forbidden. `bed` resolves the daemon first
  and falls back to the embedded server endpoint (fetches retry once on
  daemon death; writes never blindly retried). M16 acceptance 1–7 pass
  end to end against real server + daemon + bed binaries.
- **Virtualized checkout** (`bed mount <dir>`): the current changeset as
  a read-only FUSE filesystem — the whole repository tree is visible and
  readable with no sparse selection, while `.bed/objects` grows only by
  what is actually read (lazy materialization through the ordinary
  cache-first fetch path; listing alone fetches only trees). Every
  mutation answers `EROFS`; metadata is deterministic (committer time,
  true file sizes); mounting needs no root (fusermount3); SIGINT/SIGTERM
  unmount cleanly and leave an ordinary removable directory. Snapshot
  semantics: the mount reflects the changeset at mount time; remount to
  switch.
- **Generic derive framework + copy detection**: derived-data kinds are
  now a registry (`DerivedKind`: one module plus one entry, no other
  wiring to miss; byte-compatibility with the pre-refactor eager writes
  proven on 3 histories), and the new `cuttings` kind records
  per-changeset exact-blob copy/rename facts against the first parent
  (rename = unchanged-blob move; copy = add duplicating an existing
  blob; source = smallest matching path). The `copies` MCP tool (fifth
  tool) answers without walking history; differential-verified against
  `git log -M -C --find-copies-harder`. Exact blob equality only — no
  similarity heuristic.

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

