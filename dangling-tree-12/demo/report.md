# gardenbed v0.1 — north-star demo report

- date: 2026-10-17T03:39:22Z
- host: Linux 6.8.0-146-generic x86_64, 16 cores
- git: git version 2.43.0, sqlite3: 3.45.1
- workdir: /dev/shm/gardenbed-demo-v0.1.QxCQbn

Preconditions: cargo, git, sqlite3, jq on PATH; binaries built release.

---

## 0. build (cargo build --release)

  ✓ release binaries built in 1075 ms
---

## 1. import — history into the server

  ✓ corpus reused: /tmp/mr-demo-corpus
  ✓ history has 50100 commits (§0 wants ≥ 50000)
  (deviation: synthetic corpus — 5106 objects, 23272 kilobytes size bar skipped)
  ✓ git_map ↔ git bijection: 50100 changesets for 50100 commits

---

## 2. serve (gardenbed-server, fixed port)

  ✓ server listening at http://127.0.0.1:19410

---

## 3. bed clone (sparse bed, materializing nothing)

  ✓ clone in 8 ms (§0 wants < 5000)
  ✓ zero working files outside .bed/ (nothing materialized)
  ✓ server stopped (moves are offline operations)
  ✓ repository garden moved to orchard (WAL checkpointed, files renamed)
  ✓ URL clone of http://127.0.0.1:19410/orchard with no --repo flag
  ✓ the vacated name 404s naming the served repositories
  ✓ server stopped (moving orchard back to garden)
  ✓ repository orchard moved back to garden

---

## 4. bed sparse add big (fetch + materialize, byte-verified)

  ✓ sparse add: 1021 files in the selected paths
  ✓ byte-identical to git show for all 1011 materialized files
  ✓ all 10 symlinks materialize as symlinks with exact targets
  ✓ all 10 executable files kept their mode bit
  ✓ bed status: working copy is clean

---

## 5. bed commit (write path: edit → commit → query < 1 s)

  ✓ commit accepted: created changeset 7f6e2aeafd8ee297: 3 change(s) (1 added, 2 modified)
  ✓ log --limit 1 answered in 4 ms (§0 wants < 1000)
  ✓ the created changeset is the newest history entry
  ✓ bed status after commit: working copy is clean

---

## 6. export — back to git (fsck + push --dry-run + SHA roundtrip)

  ✓ export stream written (50100 commits mapped)
  ✓ git fsck --strict on the exported repository: clean
  ✓ git push --dry-run to a fresh local mirror succeeds
  ✓ SHA roundtrip: all 50100 exported SHAs equal the synthetic corpus's own SHAs (100%)

---

## 7. MCP — log/diff/file/blame with no working copy

  ✓ initialize echoes the protocol version (2025-06-18)
  ✓ tools/list advertises exactly the four tools (blame,diff,file,log)
  ✓ log(limit 3): newest entry is the head changeset with git's own subject
  ✓ diff parent→head matches git diff-tree --name-status (2 paths)
  ✓ file story.md: byte-identical to git show (1831 bytes)
  ✓ blame story.md: 39 ranges, every attribution == git blame --first-parent
  ✓ no .bed anywhere under the MCP session's home — the server answered alone

## Timings

| phase | ms |
|---|---|
| import | 74744 |
| bed clone | 8 |
| bed sparse add big | 126 |
| bed commit | 30 |
| bed log --limit 1 | 4 |
| export | 17860 |

## Verdict

**PASS** — all seven §0 steps asserted clean.
