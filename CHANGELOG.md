# Changelog

All notable changes to gardenbed are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions are git
tags, and the verification record for a release lives in
[docs/release-0.1.md](docs/release-0.1.md).

## Unreleased

### Added

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
  links and images resolve to the viewed revision's own files). See
  [docs/web.md](docs/web.md). Verified by in-memory router
  integration tests (`crates/web/tests/api.rs`) and a Playwright
  end-to-end suite (`e2e/`) that drives a real browser through every
  page against the seeded repository.

### Fixed

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

- **`bed` honors `.gitignore` for untracked-file discovery**: `bed
  status` and `bed commit` now read `.gitignore` files from the working
  copy (root plus one per directory walked) and hide matching untracked
  files — the dogfood gap where every cargo `target/` landed in status
  and rode into commits. Supported subset: blank lines, `#` comments,
  trailing `/` (dir-only), `*` `?` `**` globs, and a leading `/` anchor.
  Deliberately unsupported: `!` negation, character classes, and
  global excludes. Git semantics are kept where they matter: ignore
  never un-tracks (tracked files keep reporting changes, and tracked
  content defeats directory pruning), an ignored directory with no
  tracked content is pruned wholesale (a fresh `target/` costs one
  stat), and untracked `.gitignore` files themselves still report.
  Matching uses `gix-glob` (already in the workspace lock via the
  `gardenbed-git` dependency). Verified against `git check-ignore`
  behavior and by integration tests (`crates/bed/tests/gitignore.rs`)
  against a real server process.

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
  dirty working copy diffs local bytes against the current changeset
  (untracked files excluded, git parity; deleted files render as removal
  to `/dev/null`), while a clean one keeps the current-changeset-vs-
  first-parent diff. Both modes print a one-line header naming what is
  being diffed (`diff of working copy against <cs>` /
  `diff of <cs> against its first parent`); explicit `bed diff A [B]`
  still means changesets, now with the same header line.


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

