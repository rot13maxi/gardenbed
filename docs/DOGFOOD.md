# Gardenbed dogfood onboarding

This repository now develops on gardenbed ("bed") itself.

## Setup (already done on this machine)
- Server: `gardenbed-server --root /home/user/gardenbed/arb --addr 127.0.0.1:9418` (herdr tab "server", workspace wG)
- Client binary: `bed` on PATH (release build of main)

## Daily loop
1. `bed clone http://127.0.0.1:9418` (per working copy)
2. `bed sparse add <paths you touch>` — NEVER bare `.` or `/` (target/ and unrelated build output stay outside the scanner's selection)
3. Edit files; `bed status`
4. `GARDENBED_AUTHOR="Name <email>" bed commit -m "..."`
5. Share your changeset id; a reviewer does `bed switch <id>` + `bed diff`
6. `bed rebase main` then `bed publish` — publish is CAS on `main`; on race, rebase and retry
7. To sync newest main: `bed switch main` (incremental fetch)
