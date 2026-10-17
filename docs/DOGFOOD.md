# Gardenbed dogfood onboarding

We now develop gardenbed on gardenbed. This doc is the working agreement.

## Infrastructure (already running)
- Server: herdr tab `server` (workspace wG) — `gardenbed-server --root /home/user/gardenbed/arb --addr 127.0.0.1:9418`
- Client: `bed` on PATH (release build of main)
- `/home/user/code/gardenbed` stays the GitHub mirror; ping wG:p9 to re-import after git-side commits

## New working copy
```sh
mkdir -p ~/code/gb-<you> && cd ~/code/gb-<you>
bed clone http://127.0.0.1:9418
bed sparse add crates docs fuzz tools demo .github Cargo.toml Cargo.lock README.md DESIGN.md LICENSE CHANGELOG.md .gitignore
```
NEVER `bed sparse add .` or `/` — `target/` (cargo output) must stay OUTSIDE the sparse
selection or `bed status` drowns in untracked build files.

## Identity
`GARDENBED_AUTHOR="Your Name <you@dogfood.gardenbed.dev>"` — set it in your shell;
every commit records it and reviewers filter by author.

## Daily loop
- `bed status` — local changes (works offline)
- `bed commit -m "..."` — server-side changeset; cheap; commit early and often
- `bed log --limit 20` — note: `--limit N`, not `-N`
- `bed switch main` — sync to newest main (incremental fetch; warm cache is nearly free)
- `bed diff` (no args) — dirty working copy: working-copy changes vs current changeset (untracked excluded); clean tree: current changeset vs its first parent. One-line header names the sides. `bed diff <id>` — review a changeset
- `bed file <path> --cs <ref|id>` — any file at any changeset, no neighbor fetch

## Multi-agent collaboration
1. You commit; share the changeset id in your reports
2. Reviewer: `bed diff <id>`, or `bed switch <id>` then build/test
3. Land: `bed rebase main && bed publish`
   - publish is compare-and-swap on `main`. With ~10 writers you WILL lose races;
     the error names the new tip — `bed rebase main` and publish again. Normal, not failure.
4. After publish: `herdr agent prompt wG:p9 "landed <id>: <one-line>"` — wG:p9 reviews main

## Rules of the road
- Scope test runs: `cargo test -p <your-crate>`; full-workspace runs fight 10 agents for CPU
- `cargo test -p gardenbed` does NOT rebuild the workspace `gardenbed-server`
  binary its tests spawn from `target/{debug,release}`. After a change in
  `crates/server`, rebuild first (`cargo build -p gardenbed-server`) or a stale
  binary will fail protocol-dependent tests in ways that look like code
  regressions (same class as the e2e-vs-cli binary mismatch).
- **Dogfood #13 — protocol skew across server/client generations:** an
  OLD server binary (pre-`fd29b3d`, when `PublishReq` gained `ref`/`force`)
  plus a NEW `bed` makes `bed publish --ref <lane>` silently publish to
  `refs/heads/main`: proto3 unknown-field skipping means the old server
  ignores the new tag-3 `ref` field, treats the publish as the plain
  main form, and the CLIENT still prints the lane name (it echoes its
  own argv, never the server's ref choice). Reproduced: dc079f8-era
  server + fd29b3d-era client on a throwaway root — `refs/heads/main`
  moved to the published tip, no lane row. Wire-compat tests covered
  old-client/new-server only. **Upgrade note: restart servers before
  using `--ref`.** The test harness's server_bin() digest guard now
  refuses a binary built from different tree state, but a deliberately
  deployed old server is outside its reach — coordinate restarts with
  wG:p9 when protocol messages change.
- Before publishing: `cargo fmt --all && cargo clippy -p <pkg> --all-targets -- -D warnings`
- Hit a bed bug or papercut? Report to wG:p9 (`herdr agent prompt wG:p9 "..."`) with the
  exact command + error. Fixing bed is the point of this dogfood. Don't silently work around.
- A checkout belongs to ONE agent. Never run bed commands (switch/sparse/commit/publish) in
  another agent's checkout — a surprise current_cs move looks exactly like data loss to the
  owner. Coordinators verify in throwaway clones under /tmp, never in live checkouts.

## Git interop
- All 12 git branch refs are imported; `bed switch <branch>` works
- New commits made in git (not bed) need a server re-import — ping wG:p9
- GitHub pushes: export main via `gardenbed-server export`, then push from the git mirror

## Dogfood findings (2026-10-17)
- **#11 — Export leaves a non-bare mirror's index/worktree stale**: `gardenbed-server
  export` drives `git fast-import` (`crates/git/src/export.rs`), which advances the ref but
  never touches the index or working tree. Every export into `/home/user/code/gardenbed`
  (non-bare) therefore stages the prior checkout's content as deletions against the new
  HEAD (observed: 226 files/361k lines, 7 files, 1 file across three exports today).
  Coordinator mitigation: `git reset --hard main` in the mirror after every export.
  Exports themselves are unaffected (export reads server metadata). Product fix on backlog.
- **#12 — `cargo test -p gardenbed` spawns a stale workspace server binary**:
  `crates/bed/tests/common/mod.rs::server_bin()` starts `target/{debug,release}/gardenbed-server`
  without rebuilding it. After a `crates/server` change, protocol-dependent tests fail in
  ways that look like code regressions (real case: `publish_ref_advances` failed
  identically on a rebased tree AND pristine main until `cargo build -p gardenbed-server`
  ran). Rebuild the server package before re-running bed tests after server changes.

## Backlog — product fixes from dogfood findings
- **#11 product fix**: `export_to_path` should reset/checkout the target ref when the
  destination is a non-bare repo (or refuse/warn on non-bare targets). Owner: unassigned.
  Will bite any future non-bare mirror consumer.
