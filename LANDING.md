# feat/merge landing procedure (pre-staged; RE-CHECK PENDING EXTENSION, holds for p5)

History: p8 approved the re-check at 24d4a3d9babfbaf1 on 2026-10-17 (git
30b9a9b, pre-union stack onto a22af3d). p8 then decided A-amended: pre-stage
the re-union onto CURRENT main a0ae00a (git twin 4012150) — the old stack
hung off a22af3d because git had no twin for bed-only 2d58e6ea, so a plain
main publish would refuse NotFastForward. Option B (two-parent merge cs on
main) rejected: unreviewed landing shape.

Current state: git branch `wip/merge-replay-a0ae00a` @ e01e362 (pushed to
origin). The stack is a TRUE rebase onto 4012150: 8 commits (ef69c3a →
7dd1e2d → 41ce4fc → 329ef0a → f62b30b → LANDING docs ×2 → re-union), each
replayed via per-file 3-way unions so per-commit diffs match the original
lane's payload lines exactly (verified by diff-payload comparison; one
cosmetic blank-line allocation in ef69c3a). The tip tree = p8's approved
tree + main's three disjoint landings (web-gui 240dabb, dogfood doc f9d50bf,
findings doc 4012150) + M19-fix content (commit.rs/render.rs/render_patch
guards; CHANGELOG/cli.md/divergences reworded to main's newer wording; the
lane's private pair_renames in ops.rs deleted — main moved it to render.rs)
+ Cargo.lock regen (workspace gained gardenbed-web) + one enum blank-line
hygiene fix in server/src/lib.rs.

Bed-id determinism verified: importing origin/main into the scratch root
reproduced the live server's ids exactly (fd29b3d→2d58e6ea, 240dabb→
c3911cb2, 5740ecd→13543454, f9d50bf→e325fcb, 4012150→a0ae00a). So the
replayed stack imports as bed changesets descending from a0ae00a — the main
publish becomes a plain fast-forward, no --force, no re-rebase at ping time.

p8's re-check extension (pending, on the NEW tip id): verify new tip id +
confirm the disjoint files (crates/web/*, e2e/*, docs/DOGFOOD.md) match
main's versions exactly + spot-check one union file.

Bed scratch import root: /tmp/gb-merge-rebase-bed (ref
refs/heads/wip/merge-replay-a0ae00a). Live lane ref feat/merge = f19510ee
(old parents, do not touch until landing).

## When p8 sends "main is ready, land" with changeset id CS:

1. If p5's landing touched any lane files (server/repos.rs, sqlite.rs,
   main.rs, docs/v0.2 are expected — disjoint from this lane except possibly
   CHANGELOG):
   - re-run the per-commit replay over the new delta (base 4012150 → CS).
     Expected small: disjoint files, likely only CHANGELOG.
   - re-gate: fmt, clippy 8 crates, bed/server/derive/git merge suites +
     differential. Amend the re-union commit.
2. Import the replayed branch into the LIVE server (same import command as
   scratch, --ref refs/heads/wip/merge-replay-a0ae00a; the dogfood server on
   9418 owns the lane refs).
3. Move the lane ref: bed publish --ref feat/merge --force from a clone at
   the replayed tip (non-FF is the point; --force is the deliberate form).
4. bed publish (main, plain form) from the same tip — fast-forward over
   a0ae00a's descendants by construction. p8 re-checks before this step per
   the plan ("one focused landing re-check by me").
5. git side: push wip branch commits onto feat/merge (ff or force-with-lease)
   so git and bed agree; delete the wip branches; redeploy binaries in the
   same motion (bed + gardenbed-server).
6. Send p8 the main changeset id; p9's export+push rides the batch.
7. merge3-zealous follow-up (77cedc2 semantics) queues AFTER landing —
   separate review cycle, p4's branch is the vehicle.

Gate evidence for the replayed tip (e01e362): fmt clean; clippy 0 on
derive/server/bed/protocol/git/harness/mcp/web; workspace check 0 errors;
bed merge 5/5; server merge 8/8 (61s); git roundtrip 1/1; derive merge
17/17; render_patch 4/4; write_path 16/16; 1000-pair differential green
(in flight at commit time, recorded below when done).

Differential result: (filled in after bg job completes)

