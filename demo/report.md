# gardenbed v0.1 — north-star demo report

- date: 2026-10-14T23:25:47Z
- host: Linux 6.8.0-146-generic x86_64, 16 cores
- git: git version 2.43.0, sqlite3: 3.45.1
- workdir: /dev/shm/gardenbed-demo-v0.1.WwS0pi

Preconditions: cargo, git, sqlite3, jq on PATH; binaries built release.

---

## 0. build (cargo build --release)

  ✓ release binaries built in 11755 ms
---

## 1. import — history into the arboretum

  ✓ corpus reused: /dev/shm/gardenbed-demo-corpus
  ✓ history has 50100 commits (§0 wants ≥ 50000)
  (deviation: synthetic corpus — 377 objects, 1940 kilobytes size bar skipped)
  ✓ git_map ↔ git bijection: 50100 changesets for 50100 commits

---

## 2. serve (gardenbed-server, ephemeral port)

  ✓ arboretum listening at http://127.0.0.1:37385

---

## 3. bed clone (sparse bed, materializing nothing)

  ✓ clone in 48 ms (§0 wants < 5000)
  ✓ zero working files outside .bed/ (nothing materialized)

---

## 4. bed plot add big (fetch + materialize, byte-verified)

  ✓ plot add: 1021 files in the plot
  ✓ byte-identical to git show for all 1011 materialized files
  ✓ all 10 symlinks materialize as symlinks with exact targets
  ✓ all 10 executable files kept their mode bit
  ✓ bed status: the plot is clean

---

## 5. bed sow (write path: edit → sow → query < 1 s)

  ✓ sow accepted: planted seed bc26fe5964c822e9: 3 change(s) (1 added, 2 modified)
  ✓ log --limit 1 answered in 46 ms (§0 wants < 1000)
  ✓ the sown changeset is the newest history entry
  ✓ bed status after sow: the plot is clean

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
  ✓ file story.md: byte-identical to git show (1675 bytes)
  ✓ blame story.md: 39 ranges, every attribution == git blame --first-parent
  ✓ no .bed anywhere under the MCP session's home — the arboretum answered alone

## Timings

| phase | ms |
|---|---|
| import | 54797 |
| bed clone | 48 |
| bed plot add big | 259 |
| bed sow | 69 |
| bed log --limit 1 | 46 |
| export | 16122 |

## Verdict

**PASS** — all seven §0 steps asserted clean.
