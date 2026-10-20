# gardenbed v0.2 — acceptance demo report (docs/v0.2.md §0)

- date: 2026-10-19T22:50:11Z
- host: Linux 6.8.0-147-generic x86_64, 16 cores
- git: git version 2.43.0, sqlite3: 3.45.1
- workdir: /dev/shm/gardenbed-demo-v0.2.mdaY51

Preconditions: cargo, git, sqlite3, jq, python3 on PATH; binaries built release.

---

## 0. build (cargo build --release)

  ✓ release binaries built in 189 ms

---

## 1. import — two named repositories, one server process

  ✓ synthetic corpus generated: 63 commits
  ✓ fixture carries a hand-built signed commit (0362d2f13cbbb6d1c33b586412b79bbfb325af8f), fsck clean
  ✓ history has 64 commits on main (branches: main side )
  ✓ alpha imported: 64 commits (64 new, 0 skipped), 577 objects, 2 ref(s) moved, 0.03s (1930 commits/s)
  ✓ gamma imported for the gc step (a 4-commit slice): 4 commits (4 new, 0 skipped), 283 objects, 2 ref(s) moved, 0.01s (323 commits/s)
  ✓ alpha git_map ↔ git bijection: 64 rows for 64 commits

---

## 2. serve (one gardenbed-server process, two repositories)

  ✓ server listening at http://127.0.0.1:19410 (pid 4022101)
  ✓ beta created during serving: 6 commits (6 new, 0 skipped), 293 objects, 2 ref(s) moved, 0.01s (459 commits/s)
  ✓ the server survived the import
  ✓ URL clone of http://127.0.0.1:19410/beta
  ✓ URL clone of http://127.0.0.1:19410/alpha (same server process, pid 4022101)
  ✓ the lone server process never restarted between the two clones
  ✓ beta is a separate store with its own 6-changeset slice
  ✓ beta's main is its own tip (a41504baf718df5c ≠ alpha 888f1534f60763a6)
  ✓ an unknown repository name 404s naming the served repositories (alpha, beta, gamma)

---

## 3. merge — two workers, one conflict, one two-parent changeset

  · two workers cloned alpha and materialized story.md
  · worker 1 committed: created changeset 85aaf9a3429d3283: 1 change(s) (1 modified)
  · worker 1 published: published 85aaf9a3429d3283 — `main` now points to it
  · worker 2 committed: created changeset 2fd7857a839cde28: 1 change(s) (1 modified)
  ✓ bed merge main listed the conflict (story.md) and stored nothing
  ✓ the working copy did not move on the refused merge
  · resolution committed: created changeset 877998adad8a8eee: 1 change(s) (1 modified)
  · re-merge: merged 85aaf9a3429d3283 into 877998adad8a8eee as 4fb32e4f6253dab3
  ✓ merge published; main's server-side tip is the merge changeset (4fb32e4f6253dab3)
  ✓ the landed changeset has TWO parents (877998adad8a8eee 85aaf9a3429d3283)
  ✓ parent[0] is the resolution commit (877998adad8a8eee)
  ✓ parent[1] is worker one's published tip (85aaf9a3429d3283)
  ✓ the landed content is the resolved one (main's line 1 + the resolution note)

---

## 4. offline — three queued changesets, sync, publish

  · worker 3 cloned alpha
  ✓ server stopped and reaped (offline commits require the server down)
  ✓ offline commit 1 queued as 7678ff6d442417b6 (1 pending)
  ✓ offline commit 2 queued as 7be5c4f268cf8934 (2 pending)
  ✓ offline commit 3 queued as 21a0304f927356a7 (3 pending)
  ✓ bed status reports 3 pending commits while offline
  · server restarted at http://127.0.0.1:19410
  ✓ bed sync drained the queue (3 synced)
  ✓ all three server ids equal the locally computed ids
  ✓ status after sync: clean, nothing pending
  · published: published 21a0304f927356a7 — `main` now points to it
  ✓ main's server-side tip is the third offline changeset (21a0304f927356a7)

---

## 5. gc — dry-run reports the planted orphans, gc deletes them, second run is empty

  ✓ server stopped and reaped (offline gc needs the exclusive repo.lock)
  ✓ baseline dry-run on the untouched store reports no garbage
  · planted 2 orphans (blob 931b986047098bc7, tree 39e6e18fe8ca9a57); 285 objects on disk
  ✓ gc --dry-run reports exactly the two planted orphans: gc: 285 object(s) scanned, 283 live, 1 blob(s) and 1 tree(s) deleted, 0 tmp file(s) reaped, 37 byte(s) reclaimed [mark 0.002s, sweep 0.003s]
  ✓ dry-run deleted nothing (285 objects still on disk)
  ✓ gc deleted exactly the planted orphans: gc: 285 object(s) scanned, 283 live, 1 blob(s) and 1 tree(s) deleted, 0 tmp file(s) reaped, 37 byte(s) reclaimed [mark 0.002s, sweep 0.003s]
  ✓ object store shrank by exactly 2 (283 remain)
  ✓ both planted object files are gone by name
  ✓ a second gc over the settled store deletes nothing
  ✓ the settled store still holds exactly 283 objects
  · server restarted after gc

---

## 6. > 1 MiB blob — commit, clone, export with the id unchanged

  · source blob: 1060921 bytes, sha256 fc28bbffe15c0a79
  ✓ committed the > 1 MiB blob: created changeset af75756761c9a448: 1 change(s) (1 added)
  · published: published af75756761c9a448 — `main` now points to it
  ✓ stored object fc28bbffe15c0a79 is a GBLO2 chunked envelope
  ✓ fresh clone materialized the blob byte-identically
  ✓ bed file returns the same content address (fc28bbffe15c0a79)

---

## 7. export — fsck clean, 100% changeset↔git roundtrip, verbatim signed commit

  · exported: 72 commits (0 signed dropped), 395 blobs, 2 ref(s), 0.03s
  ✓ 72 changeset→git-SHA mappings written (the export report names 72 commits)
  ✓ git fsck --strict on the exported repository: clean
  ✓ every one of the 72 mapped git SHAs exists in the export
  ✓ export→import→export is idempotent (72/72 ids reproduced byte-for-byte)
  ✓ every source commit exported with its own git SHA (64/64, 100%)
  · excluded 8 bed-only changeset(s) with no git origin (workers, merge, offline, big blob)
  ✓ every corpus commit's exported SHA carries its original changeset id (0 mismatches)
  ✓ the signed commit exported with its original SHA (0362d2f13cbbb6d1c33b586412b79bbfb325af8f)
  ✓ the exported commit still carries the gpgsig header
  ✓ the exported commit object is byte-identical to the imported one
  ✓ the re-imported changeset references the same > 1 MiB blob id (fc28bbffe15c0a79)
  ✓ the big-blob changeset id itself survived the roundtrip (af75756761c9a448)

---

## 8. daemon — a warm second working copy makes zero server-side object requests

  · daemon 1 serving http://127.0.0.1:19410 with a cold shared cache
  · working copy A cloned + materialized through the daemon (cache warm)
  ✓ B's warm fetch added ZERO server-side object requests (GetTree 35→35, GetBlob 759→759)
  ✓ both working copies hold identical bytes
  ✓ negative control: the cold daemon did add requests (GetTree +10, GetBlob +252)
  ✓ the cold run produced the same bytes
  ✓ the shared cache holds objects (262 files)

---

## 9. MCP — log/diff/file/blame with no working copy and no server

  ✓ server stopped and reaped (MCP answers in process; the server is not needed)
  · MCP oracle: main = af75756761c9a448, story.md = 44 lines
  ✓ initialize echoes the protocol version (2025-06-18)
  ✓ tools/list advertises log/diff/file/blame (+copies): blame,copies,diff,file,log
  ✓ log(limit 1): the main tip with the expected subject
  ✓ file story.md: byte-identical to the exported git content (1789 bytes)
  ✓ diff merge→tip lists exactly the landed changes (A big/roundtrip.bin, M story.md)
  ✓ blame covers all 44 lines contiguously and attributes the resolution to 877998adad8a8eee
  ✓ a second stdio session in a pristine HOME gives byte-identical answers
  ✓ no .bed anywhere under the MCP sessions' homes — answers came from the store alone

---

## 10. auth — anonymous denied, token allowed (clone/commit/publish)

  · server restarted with --auth (/dev/shm/gardenbed-demo-v0.2.mdaY51/auth.toml)
  ✓ anonymous clone denied with the actionable message (names GARDENBED_TOKEN)
  ✓ a wrong token is denied with the token-role message
  · tokened commit accepted: created changeset d9004d04eb6bf53d: 1 change(s) (1 modified)
  · tokened publish: published d9004d04eb6bf53d — `main` now points to it
  ✓ main's server-side tip is the tokened changeset

## Timings

| phase | ms |
|---|---|
| corpus generation | 37 |
| import alpha | 40 |
| import gamma | 18 |
| export | 33 |

## Verdict

**PASS** — 73 ✓ checks, 0 ✗; every step green.
