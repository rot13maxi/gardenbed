# feat/merge landing procedure (pre-staged; RE-CHECK APPROVED, holds for p5)

p8 approved the re-check at 24d4a3d9babfbaf1 on 2026-10-17 (verified in the
9419 clone: RebaseConflict mapping, merge handler, both must-fix repros, all
four LOW fixes, 4-arg publish union, stack integrity a22af3d -> 6 cs -> tip).
Cleared for everything EXCEPT the main publish — that waits on p5's
multi-repo landing + p8's ping.

State: git branch `wip/merge-rebase-onto-dc49e06` @ 5aca8c7 (pushed to origin).
Bed scratch import root: /tmp/gb-merge-rebase-bed (in flight).
Live lane ref feat/merge = f19510ee (old parents, do not touch until landing).

## When p8 sends "main is ready, land" with changeset id CS:

1. If main moved past 2d58e6ea (multi-repo lane p5 will: server/repos.rs,
   sqlite.rs, docs):
   - re-run the union over the new delta (base 2d58e6ea, ours = 5aca8c7 tree,
     theirs = CS). Expected small: p5's files are disjoint from the lane's.
   - re-gate: fmt, clippy 7 crates, bed/server/derive/git merge suites +
     differential. Then amend the union commit.
2. Import the rebased branch into the LIVE server (same import command as
   scratch, --ref refs/heads/wip/merge-rebase-onto-dc49e06; the dogfood
   server on 9418 owns the lane refs).
3. Move the lane ref: bed publish --ref feat/merge --force from a clone at
   the rebased tip (non-FF is the point; --force is the deliberate form).
4. bed publish (main, plain form) from the same tip. p8 re-checks before
   this step per the plan ("one focused landing re-check by me").
5. git side: push wip branch commits onto feat/merge (ff or force-with-lease)
   so git and bed agree; delete the wip branch; redeploy binaries in the
   same motion (bed + gardenbed-server).
6. Send p8 the main changeset id; p9's export+push rides the batch.
7. merge3-zealous follow-up (77cedc2 semantics) queues AFTER landing —
   separate review cycle, p4's branch is the vehicle.

Gate evidence for the current tip (5aca8c7): fmt clean; clippy 0 on
derive/server/bed/protocol/git/harness/mcp; bed merge 5/5; server merge 8/8;
git roundtrip 1/1; derive merge 17/17; differential 1000 pairs green (877s);
bed cli 24 / read_path 13 / write_path 14 / gitignore 3 / switch_prune 3 /
render_patch 2; mcp 7; server smoke 4 / chaos 1 / integrity 9.
