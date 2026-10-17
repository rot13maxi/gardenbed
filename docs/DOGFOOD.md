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
- `bed diff` (no args) — today: committed diff of current changeset vs its first parent (NOT working-copy changes); `bed diff <id>` — review a changeset. Fix in flight (fix/bed-switch-prune-diffux): no-arg will diff working copy when local mods exist + header line naming the sides
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
