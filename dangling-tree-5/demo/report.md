# gardenbed v0.1 — north-star demo report

- date: 2026-10-17T06:00:48Z
- host: Linux 6.8.0-146-generic x86_64, 16 cores
- git: git version 2.43.0, sqlite3: 3.45.1
- workdir: /tmp/mrdemo-w2

Preconditions: cargo, git, sqlite3, jq on PATH; binaries built release.

---

## 0. build (cargo build --release)

  ✓ release binaries built in 2491 ms
---

## 1. import — history into the server

  ✓ corpus reused: /dev/shm/gardenbed-demo-corpus
  ✓ history has 50100 commits (§0 wants ≥ 50000)
  (deviation: synthetic corpus —  size bar skipped)
