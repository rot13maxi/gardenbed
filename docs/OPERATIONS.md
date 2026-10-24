# Operations — landing chain, gates, and review cycle

This is the runbook the release manager and the chain operator follow between a
reviewed lane tip and a deployed `main`. It encodes the rules the 2026-10 dogfood
sweep learned the hard way; the underlying findings (each numbered) live in
[docs/DOGFOOD.md](DOGFOOD.md), and command spellings are in
[docs/cli.md](cli.md).

Roles, not panes: **release manager** approves a reviewed tip and re-checks the
landing; **chain operator** runs the steps below and owns the mirrors and the
server; **reviewer** is a fresh agent per lane; **lane owner** commits on its own
checkout and publishes once, at the end.

## 1. The landing chain

Steps in order. Each one guards a specific failure; skipping or piping away its
evidence reintroduces that failure silently.

### 1.1 `import --ref` — git-side changeset → live bed server

Run the chain from the chain checkout, never from a lane checkout. This step is
the git→bed direction: it brings the git-side landing branch (a replayed stack,
or a re-union prepared in git) into the canonical server and moves the lane ref
the publish will use. A bed-native lane tip needs no import — its changesets and
lane ref already live on the server — but the step is mandatory whenever the
reviewed landing exists as git commits, which is the normal case for a replayed
span.

```sh
gardenbed-server import --from /home/user/code/gardenbed \
    --root /home/user/gardenbed/arb --repo default \
    --ref refs/heads/wip/<landing-branch>
# 2 commits (2 new, 0 skipped), 9 objects, 1 ref(s) moved, 0.24s (8 commits/s)
```

Import reads git, never a bed working copy, and it moves only the refs named by
`--ref`; without that selection every branch in the source repository is
imported.

**The report line prints ONLY on success** (dogfood #14): a missing line means
the import failed, even with a zero exit that a pipeline hid. Run it bare or
redirect to a file, then check both the report line and `$?`. NEVER pipe it
through `tail`, `head`, or an `&&`-chain that swallows the exit code. A
non-descendant ref move is refused unless `--allow-rewind`, and the error names
the ref, both tips, and how many changesets would fall off — see
[docs/cli.md](cli.md) `gardenbed-server import`.

### 1.1b Check git-side divergence — before anything moves

Bed and git are two views of one history, and each can advance without the
other: a bed publish is invisible to git until export, and a git-native push is
invisible to bed until import. Both are legitimate, so the chain checks instead
of assuming (dogfood #26). Run this BEFORE the publish in 1.2: a divergence
caught here costs nothing, while the same divergence caught at push time means
the bed publish and export already happened and the push is rejected.

What can be checked mechanically, and what cannot:

- Import records `git_map` rows. Export is mapping-read-only by default;
  `--record-mappings` records accepted pairs after the Git output completes.
  A native tip refused as a representation twin can still have no row; keep
  the exported mapping TSV and landing record as the fallback.
- The GitHub remote belongs to the Git consumer `/home/user/code/gardenbed`,
  not to the chain checkout (`/home/user/bed-land` is a bed working copy with
  no `.git`), so every git command here runs with `-C <mirror>`.

So the chain keeps a **recorded landing tip**: `LANDED_BED_TIP` and
`LANDED_GIT_SHA`, taken from §1.3's export report and mapping row at the end of
the previous landing (the landing note is the record; the mapping TSV is the
machine copy). The check compares `origin/main` against that recorded SHA. A
mapping row for bed `main` can come from import or an accepted
`--record-mappings` export; when present, that row is the authority.

```sh
ROOT=/home/user/gardenbed/arb            # canonical bed root (repo "default")
MIRROR=/home/user/code/gardenbed         # Git consumer; origin lives here

# The mapped twin of bed main (import or accepted export recording).
# Resolve modern/default and legacy layouts without creating a missing DB.
DB="$ROOT/default/metadata.sqlite3"
test -f "$DB" || DB="$ROOT/metadata.sqlite3"
test -f "$DB" || { echo "FAIL: metadata DB not found"; exit 1; }
MAIN_TWIN=$(sqlite3 -readonly "$DB" \
    "select g.git_sha from refs r join git_map g on g.cs = r.cs
      where r.name = 'refs/heads/main';")

# Otherwise fall back to the previous landing's record (see the note above).
# Leaving it empty makes the check report "cannot verify" instead of guessing.
: "${LANDED_GIT_SHA:=}"

EXPECTED=${MAIN_TWIN:-$LANDED_GIT_SHA}
REMOTE=$(git -C "$MIRROR" ls-remote origin refs/heads/main | awk '{print $1}')

test -n "$EXPECTED" || {
    echo "FAIL: cannot verify: bed main has no git_map row and no recorded"
    echo "      landing SHA. Set LANDED_GIT_SHA from the landing note (the"
    echo "      mapping row recorded by the last export), or import the mirror"
    echo "      so main gains a row, then re-run."
    exit 1
}
test "$EXPECTED" = "$REMOTE" || {
    echo "FAIL: origin/main $REMOTE is not the expected tip $EXPECTED"
    echo "      git-side commits arrived without import (or bed moved without"
    echo "      export/push). Resolve before publishing: import the git commits"
    echo "      and re-union, or export and push the bed ones."
    exit 1
}
echo "OK: origin/main is the expected tip ($REMOTE)"
```

A failure means the two sides diverged: import the git commits (or export and
push the bed ones), hand-re-union if both moved, and only then continue.
`ls-remote` is the authority, not a stale local ref: the mirror's `main` is only
as fresh as its last fetch.

**Record the landing tip at the end of every chain** (§1.3 exports it, §1.4
verifies the mapping row against the mirror): storing `(bed tip, git sha)` in
the landing note is what makes the next pre-publish check possible without
importing the mirror back. Accepted export recording also persists the pair.
If neither a row nor a landing record is available, reconstruct the SHA from
the exported repository with `git rev-parse main`.

### 1.2 Advance bed `main` — `bed publish`, plain CAS form

Publish is the normal way bed `main` advances. Import (§1.1) moves only the refs
named by `--ref` — it moves `main` too if you hand it `refs/heads/main` (that is
the deliberate rewind route, *When main goes red*). Export reads `main` and the
push carries it, so publish BEFORE export and before the push:

```sh
# in the chain checkout (a bed working copy on the canonical server)
bed switch wip/<landing-branch>     # the ref import just moved
bed publish                          # plain form: main advances to the current changeset
# published e5a76c025d566425 — `main` now points to it
bed switch main                      # leave the chain checkout on main
```
(Sample ids in this section are from the scratch server run that verified the
sequence; they are illustrative, not live ids.)

`bed switch <ref>` resolves the imported lane ref server-side and moves the
chain checkout's current changeset there; `bed publish` then fast-forwards
**the working copy's current changeset** onto `main`. A lane-tip clone is the
equivalent shape: `bed switch <lane> && bed publish`. This sequence was
exercised end to end against a scratch server (fresh root, ephemeral port):
lane publish → `bed switch lane/test` → `bed publish` moved
`refs/heads/main`; the import variant (`import --ref refs/heads/wip/land` →
`bed switch wip/land` → `bed publish`) moved main to the imported tip.

The landing tip must be a descendant of current `main`; a re-union or a
replayed stack is a fast-forward by construction. Publishing is CAS: if `main`
moved under you, the command fails and names the new tip —

```
bed: cannot publish 3f6381f12d454521: it does not fast-forward `main`, whose current tip is 64eba585b29e5040
  rebase this changeset onto the new main (`bed rebase main`) and publish again
```

— and the retry depends on how many changesets you are carrying:

- **One changeset** (a plain lane commit): `bed rebase main`, then
  `bed publish`. `bed rebase` replays the working copy's *current changeset*
  only.
- **A stack / multi-changeset span**: `bed rebase --stack main`, then
  `bed publish`. The plain form would replay just the tip, dropping changesets
  1..N-1 from the retried lineage. `--stack` walks the first-parent chain from
  the merge base up to the current changeset, oldest first, and replays every
  link; a conflict anywhere aborts the whole stack before the working copy
  moves (`crates/bed/src/ops.rs::rebase_stack`).

Both were exercised on a scratch server: single-changeset CAS loss → `bed
rebase main` → publish green; a 3-changeset span that lost the CAS → `bed
rebase --stack main` replayed all three as a chain and the publish went green.
After a `--stack` replay the publish is a fast-forward by construction.

**`--stack` on an up-to-date checkout answers instead of panicking.** Before
the twin/up-to-date work in this sweep (dogfood #22), `bed rebase --stack
<ancestor>` — a fresh clone at `main` running `bed rebase --stack main` —
panicked on the empty replay chain (`index out of bounds`, exit 101). Both
forms now answer `already up to date — <cs> is an ancestor of <parent>; nothing
to rebase` and leave the working copy put; if you are on an older binary, check
`bed log --cs refs/heads/main --limit 1` against the checkout first and
`bed switch main` (publishing from a strict ancestor is refused by the CAS —
`cannot publish <cs>: it does not fast-forward main`).

**When `main` already carries the lane's content (the hand-union / twin case).**
What the replay does is decided per written path by `check_write`
(`crates/server/src/lib.rs`): a path whose desired (mode, blob)
already equals the new parent's entry returns early and is accepted; a path
whose (mode, blob) differs between the new parent and the pre-image is refused
(`"<path>" was changed or deleted`); a path the element deletes or renames is
refused by `check_removal` (same file) when the new parent's copy is missing
or differs from the pre-image. `rebase --stack` aborts
before any publish if a link refuses. Three shapes:

1. **Prefix twin** — `main`'s tip is the lane's earlier state (same tree, same
   message). The suffix replays, the first replayed changeset is parented on
   `main`'s tip, and `bed publish` advances `main`.
2. **Third-value or delete/rename overlap** — some path sits at a value that
   differs from *both* the pre-image and the element's post-image (main edited
   it independently), or the element deletes/renames a path `main` changed.
   The replay refuses and the stack aborts; nothing is published. The exact
   text depends on the change kind: a write refuses with `"<path>" was changed
   or deleted` (`check_write`), while a delete or a rename source refuses with
   `removal of missing or changed path "<path>"` (`check_removal`) — both in
   `crates/server/src/lib.rs`. This is the layered same-path shape the
   lanes hit (the finding recorded as DOGFOOD #23). A merely *partial* overlap
   is not this case: if the paths main did not touch are unchanged since the
   merge base, their writes pass (verified: a two-path element whose first path
   main already matched, second untouched, replayed and published).

   Twin recognition does **not** rescue this shape: the twin scan matches an
   element's exact post-image tree, and a path at a third value means main's
   tree cannot equal it. Hand-union deliberately — rebase the element onto
   main's tip by hand, resolving the path in favor of whichever content you
   actually want — and record the resolution in the landing note.
3. **Whole-span twin** — every element is already on `main` as a tree-equal,
   same-message changeset. Today this splits:
   - (3a) **exact-content twin** (add/modify elements whose post-image blob and
     mode equal `main`'s): `check_write`'s early return accepts the write, so
     the replay **succeeds** and `bed publish` advances `main` by a
     tree-equal changeset. Verified: the replay produced a new changeset whose
     diff against the previous `main` is empty, and the publish moved `main`.
     This is the empty move to accept deliberately or skip — not a refusal.
   - (3b) **same-path layered twin** (main carries the path at a different blob
     — the #23 shape) or a delete/rename twin: refused as in case 2. Verified:
     `bed rebase --stack` → `the server refused (FailedPrecondition): rebase
     conflicts on: "f.txt" was changed or deleted`.

   With twin recognition (`crates/bed/src/ops.rs`; documented under `bed
   rebase` in [docs/cli.md](cli.md)), **(3a)** — an exact-content twin whose
   paths main has not touched since — replays nothing, prints
   `stack already applied`, moves the working copy to the new parent, and a
   follow-up `bed publish` is a no-op fast-forward at the same tip. **(3b)** is
   not a twin: main carries the path at a different blob, so the post-image
   trees differ and the element still replays into the ordinary conflict (the
   stale-twin rule below). On a binary older than this sweep, (3a) advances
   main by an empty changeset and (3b) refuses — check the deployed generation
   before interpreting the outcome (#13).

Operator guidance for a whole-span twin today: accepting the empty move is
harmless for content but creates a tree-equal changeset on `main` (decide and
record it); otherwise hand-union is unnecessary — the content is already there,
so close the lane after recording that its tree is on `main`. Twin recognition
makes the no-op explicit.

A twin later rewritten or reverted by `main` is not skipped even after twin
recognition: the element replays and the ordinary conflict rule decides.

`--force` is not available for `main` and must not be: naming `main` in the
`ref` field is refused by the server (`ref "main" must use the plain form`).

Record `main`'s id before publishing (`bed log --cs refs/heads/main --limit 1`)
and re-read it after: that read-back is both the CAS confirmation and the #13
skew check (the upgrade note in [docs/cli.md](cli.md) `bed publish` — an old
server can also move `main` on a `--ref` publish).

### 1.3 `export` — bed metadata → git

```sh
set -eu
MIRROR=/home/user/code/gardenbed
EXPORT="$(mktemp -d)/repo.git"          # a fresh bare export target
mkdir -p /home/user/land-mappings
gardenbed-server export --root /home/user/gardenbed/arb --repo default \
    --into "$EXPORT" --ref refs/heads/main --record-mappings \
    --mapping /home/user/land-mappings/landing.map
git -C "$EXPORT" fsck --strict
git -C "$EXPORT" merge-base --is-ancestor \
    "$(git -C "$MIRROR" rev-parse main)" refs/heads/main
if test "$(git -C "$MIRROR" rev-parse --is-bare-repository)" = true; then
    git -C "$MIRROR" fetch "$EXPORT" refs/heads/main:refs/heads/main
else
    test "$(git -C "$MIRROR" branch --show-current)" = main
    git -C "$MIRROR" fetch "$EXPORT" refs/heads/main
    git -C "$MIRROR" merge --ff-only FETCH_HEAD
fi
```


Export writes every ref unless `--ref` narrows it; the example exports main
only. Publish first so the exported `refs/heads/main` is the just-published
bed tip. The fresh bare target isolates Git output from any consumer checkout;
the ancestry check and fast-forward update preserve its existing history.
`--record-mappings` writes accepted metadata pairs; inspect conflict warnings,
and retain the TSV/landing record for refused native representation twins.

Same #14 rule: the report line is the success signal, and export must not be
piped through exit-swallowing filters. The mapping TSV is the changeset↔SHA
record; keep it with the landing note.

Export **writes the pack and advances the ref; it does not touch a checkout's
index or working tree** (#11). Into a non-bare target that left the prior
checkout staged as deletions (226 files/361k lines observed). Main already
refuses an existing non-bare target in `prepare_target` (`the target repository
is not bare (worktree at …)`); this sweep adds the remedy text
(`use a bare target (git init --bare)`) and the regression test
`export_into_a_non_bare_worktree_is_refused_untouched`. Verified: an export
into a fresh non-bare clone exits 1 with the refusal and leaves the worktree
untouched (pack, refs, index and status all unchanged). So:

- A bare Git consumer can fetch the exported main ref directly, without
  forcing. It has no working tree to reset.
- A non-bare consumer must fetch and fast-forward its checked-out main as in
  §1.3. Direct export into it is refused. A conflicting dirty file makes the
  fast-forward refuse; preserve that work and resolve the conflict rather than
  resetting or cleaning it away.
- In either case compare the TSV's Git SHA to the consumer's main ref (§1.4).
  A 64-hex bed id and a 40-hex Git SHA are different identifiers.

### 1.4 Verify the mirror's ref

The GitHub consumer is `/home/user/code/gardenbed`
(`origin = git@github.com:rot13maxi/gardenbed.git`). It may be bare or have a
working tree; check `rev-parse --is-bare-repository` rather than assuming.
Direct export into a non-bare consumer is refused. The scratch-export and
fast-forward procedure above updates its index/worktree coherently while
preserving unrelated dirty files. Verify its main ref against the exported
mapping in either layout:

```sh
# main's bed changeset id as published in §1.2 (short id is enough: prefix match)
TIP=<64-hex-or-8+-hex id of main, from `bed log --cs refs/heads/main --limit 1 | head -n 1`>
test -n "$TIP" || { echo "FAIL: TIP is empty (no bed tip to check)"; exit 1; }

# the mapping writes "<changeset id>\t<git sha>" per exported commit; set MAP
# to the same file you passed to export in 1.3 (the chain uses
# /home/user/land-mappings/<landing>.map)
MAP=/home/user/land-mappings/landing.map
ROW=$(awk -v t="$TIP" 'index($1,t)==1 {print $2}' "$MAP")

if [ -z "$ROW" ]; then
    echo "FAIL: no mapping row for $TIP (export did not reach bed main?)"
    exit 1
elif [ "$(printf '%s\n' "$ROW" | wc -l)" -ne 1 ]; then
    echo "FAIL: $TIP matches $(printf '%s\n' "$ROW" | wc -l) mapping rows; use the full id"
    exit 1
elif [ "$ROW" != "$(git -C /home/user/code/gardenbed rev-parse main)" ]; then
    echo "FAIL: mirror main $(git -C /home/user/code/gardenbed rev-parse main) != bed tip row $ROW"
    exit 1
fi
echo "OK: mirror main matches bed main ($ROW)"
git -C /home/user/code/gardenbed fsck --strict || { echo "FAIL: fsck"; exit 1; }
```

The comparison is mapping-row git SHA vs `rev-parse main` (40-hex vs 40-hex) —
never the 64-hex changeset id against the mirror's ref. The check prints an
explicit `OK:` line only on success; a missing line is failure (the same #14
rule as the import/export report lines), and each failure branch names which
precondition broke. Verified on a scratch export: the row for
`8ffcaa573aa90b48` gave `202363bf…` and `rev-parse main` reported the same SHA;
`fsck --strict` clean.

`git status --porcelain` is only meaningful in a non-bare consumer checkout
(git exits 128, "this operation must be run in a work tree", in a bare
repository); use the reset/clean in §1.3 there.

### 1.5 Push from the mirror — never from a lane

```sh
git -C /home/user/code/gardenbed push origin main
```

Pushes leave from the mirror only. A lane checkout is a bed working copy
(`.bed`, no `.git`); pushing "from a lane" means pushing a tree that never went
through import/export, so git and bed disagree about the tip. GitHub is a
downstream mirror of bed `main`; the canonical tip is on 9418 (lane refs live
there and are not published to GitHub).

### 1.6 Redeploy — workspace-wide, `--bins`, release

```sh
/home/user/bed-lanes/cargo-gate cargo build --workspace --bins --release
install -m 755 target/release/{bed,gardenbed-server,gardenbed-daemon,gardenbed-web,gardenbed-mcp,gardenbed-ab} \
    ~/.local/bin/
```

**Enumerated `-p` lists drift** (dogfood #17 deploy half): the package list
`-p gardenbed -p gardenbed-server -p gardenbed-web` missed `gardenbed-daemon`,
and the first canary went RED with 14 "binary not found" failures. Always
`--workspace --bins`. The deployed binaries are what the canary and the agents
on 9418 actually execute; a rebuilt `target/` that was never installed proves
nothing.

### 1.7 Watchdog bounce

```sh
systemctl --user restart gardenbed-dogfood     # the unit runs from ~/.local/bin
ss -ltn | grep '127.0.0.1:9418'                # port is back
```

`/home/user/gardenbed/server-watchdog.sh` is a session-independent fallback that
starts the server when 9418 is down; if both supervisors are alive they race for
the port, so keep one in charge (the script's own note). Bounce the supervisor
that is actually serving before any canary: a pre-`fd29b3d` server silently turns
`bed publish --ref <lane>` into a `main` publish (#13; see the upgrade note in
[docs/cli.md](cli.md) `bed publish`).

### 1.8 Canary — through the DEPLOYED pair

The canary exercises the binaries in `~/.local/bin` (the harness spawns them for
its own fixture roots), then probes the live server. The probe includes the
#13 skew check on both refs — the lane/main read-back in §1.2:

```sh
env -u GARDENBED_AUTHOR -u GARDENBED_FSYNC \
    BED_SERVER_BIN="$HOME/.local/bin/gardenbed-server" \
    BED_DAEMON_BIN="$HOME/.local/bin/gardenbed-daemon" \
    /home/user/bed-lanes/cargo-gate cargo test -p gardenbed --no-fail-fast

# live probe against the deployed server on 9418, from a throwaway clone
mkdir -p /tmp/canary-$$ && cd /tmp/canary-$$ && bed clone http://127.0.0.1:9418
bed switch main && bed log --limit 1 && bed diff
```

`BED_SERVER_BIN`/`BED_DAEMON_BIN` are trusted as-is by the bed harness (its
staleness oracles are skipped), so that suite spawns the installed server and
daemon. The chaos suite (`-p gardenbed-server --test chaos`, and `gc_chaos`)
spawns `env!("CARGO_BIN_EXE_gardenbed-server")` — `target/<profile>/gardenbed-server`,
which is byte-identical to the installed artifact as long as nothing was rebuilt
after `install`. Run the canary with no rebuild in between, and the pair under
test is the deployed one:

```sh
env -u GARDENBED_FSYNC /home/user/bed-lanes/cargo-gate cargo test -p gardenbed-server --test chaos --release
env -u GARDENBED_FSYNC /home/user/bed-lanes/cargo-gate cargo test -p gardenbed-server --test gc_chaos --release
```

The release-mode `gc_chaos` run is a required half of the canary when the span
touches gc or any other timing-sensitive class (#19, §4.4).

## 2. One-landing-deep

Landings serialize: exactly one landing is in flight at a time. Lanes develop in
parallel, but only one tip is imported, exported, pushed, deployed, and canaried
before the next starts.

A span based on an old `main` never lands wholesale. At landing time, re-union
it onto **current** `main`:

- lane-only files (new tests, new docs, lane-exclusive sources): take the lane's
  version wholesale;
- files `main` moved while the lane was out (stale carries): `main` wins, and
  the lane's intended delta is re-applied by hand on top;
- code-dependent files (`Cargo.lock`, generated docs): take the lane's version
  only when the lane introduced the dependency, otherwise regenerate after the
  union.

A hand-union is a real changeset and gets the same review and gates as a lane
tip; record its composition in the commit message (which files came from which
side).

## 3. Rewind guard and intent on record

Ref moves are guarded by default; the escape hatches are separate, deliberate
forms:

- **Lane refs** (non-`main`): a non-fast-forward move requires
  `bed publish --ref <lane> --force`. `--force` is the intent-on-record form.
  The client's refusal prints what it knows — the changeset, the ref, and the
  ref's current tip:

  ```
  bed: cannot publish da11d921f6e9081b: it does not fast-forward `lane/test`, whose current tip is 9646d88a54052620
    rebase this changeset onto the new lane/test (`bed rebase lane/test`) and publish again
  ```

  The server-side refusal (`ServerError::PublishRewind`, built by
  `Repo::publish` in `crates/server/src/lib.rs`) also counts the changesets
  that would fall off, but `bed publish` maps every `FailedPrecondition` to the
  fast-forward message above (the `FailedPrecondition` arm of `publish` in
  `crates/bed/src/ops.rs`), so the operator never sees the count. There is no log route today: the server logs only
  `log::debug!("request rpc={rpc} repo={repo}")` per RPC
  (`Repo::repo_guard`, `crates/server/src/lib.rs`), not refusal payloads. The observable
  analogue is `import --allow-rewind`'s warning, which does print the count.
  Use `--force` only for a deliberate rewrite (replayed stack, re-union), and
  say why in the landing note.
- **Import's parallel hatch**: `--allow-rewind` moves a git ref backward and
  prints a loud warning naming the ref, both tips, and the uncounted/reachable
  changesets (verified output: `warning: import moved refs/heads/main backward:
  9646d88a54052620 -> 1e06643dc3e90498 (2 changeset(s) may no longer be
  reachable from the ref)`).
- **`main`**: plain CAS form only, never `--force`:

  ```sh
  bed rebase main && bed publish            # one changeset
  bed rebase --stack main && bed publish    # a multi-changeset span
  ```
  (`--stack` is the stack form: the plain `rebase` replays only the current
  changeset; see §1.2.)

  A lost race names the new tip; rebase onto it and publish again. That is
  normal, not failure. `main` must use the plain form: the server rejects a
  publish whose `ref` field names `main` (`ref "main" must use the plain
  form`).
- **Messages are a shell boundary.** `bed commit -m "..."` receives its text
  after the shell has expanded it: an unquoted backtick pair or `$(...)` inside
  a double-quoted message executes before bed runs. That happened during this
  sweep — the substituted text ran `bed switch` and `bed publish`, moving
  `main` to an unapproved lane tip (dogfood #24). Single-quote commit messages
  (or use a quoted heredoc / a message file), and treat every message-taking
  command as dangerous. Recovery from an unintended `main` move is the
  `import --allow-rewind` route below, with the rewind recorded and reported.

## 4. Review-cycle rules

### 4.1 Fresh reviewer per lane

Every lane gets a reviewer that did not write it. The reviewer reads the
**whole span**, not the author's summary: `bed diff <main-tip> <lane-tip>` for
the cumulative delta, plus `bed log --limit N` and per-changeset
`bed diff <cs>` where the per-commit shape matters. A single `bed diff <tip>`
is tip-vs-first-parent only — on a multi-commit lane it shows just the last
changeset's delta (verified: `bed diff A B` listed all three lane files;
`bed diff <tip>` showed only the final one).

### 4.2 Delta reviews after fixes

After a fix round, review the **delta** — `bed diff <old-tip> <new-tip>` —
plus the fix's interaction with the original findings; do not re-litigate the
whole lane. A fix round that touches a shared surface reopens the full surface
gate.

### 4.3 Mutation proofs for guards and oracles

A guard or oracle is reviewed together with the evidence that it fails when the
property is removed: gut the guard (verify → no-op), show the named test goes
red, restore, show green. The M13 gc round-4 write-set handshake carries this
shape (`p2_1_missing_intermediate_tree_fails_the_commit`), and the same
expectation applies to new tests whose only job is to refuse something. A test
that has never been observed red is not evidence.

### 4.4 The #19 release bar

Debug-green is not evidence for timing or assert-sensitive classes
([DOGFOOD.md](DOGFOOD.md) #19). Lane gates for such code MUST include a
release-mode run of the affected suites, and the release canary through the
deployed pair is a first-class gate:

- release-only timing reds (commit p50 proportional to repository size) pass
  every debug gate;
- `debug_assert!`/`debug_assert_eq!` operands are not evaluated in release
  builds, so a required side effect inside an assertion disappears — evaluate
  required effects before the assertion;
- release suites legitimately take 40–60+ minutes (§5).

The release-bar CI job (Group B) makes this structural rather than tribal: the
release-mode affected suites run on a nightly/landing trigger, not only on a
human's memory.

## 5. Test environment and binary staleness

```sh
# bulk / functional suites
env -u GARDENBED_AUTHOR GARDENBED_FSYNC=off <cargo-gated command>
# durability suites (chaos, torture, gc_chaos)
env -u GARDENBED_AUTHOR -u GARDENBED_FSYNC <cargo-gated command>
```

- `GARDENBED_FSYNC=off` for bulk/CI-style suites (functional tests, demo,
  import). The durability suites depend on fsync being on and handle it per
  suite:
  `crates/server/tests/chaos.rs` **removes** the variable before opening any
  store, so it is durable whether or not your shell exports the toggle;
  `crates/store/tests/torture.rs` does **not** touch it, so run it with the
  toggle unset (`env -u GARDENBED_FSYNC`) — under `off` it would torture an
  atomicity-only store that promises nothing about torn writes.
  `crates/server/tests/gc_chaos.rs` spawns the `gardenbed-server` binary, which
  inherits the environment, so it too runs with the toggle unset.
- `env -u GARDENBED_AUTHOR` — a stray author changes recorded identities and
  breaks author-filtered reviews.
- Build the sibling binaries before bed suites: `cargo build -p gardenbed
  -p gardenbed-server` and, when daemon tests run, `-p gardenbed-daemon`
  (#12, #17 test half). `bed` tests spawn `target/{debug,release}/gardenbed-{server,daemon}`
  and never rebuild them; a stale server fails protocol tests in ways that look
  like regressions.
- Full suites run `--no-fail-fast` (#15 rule 1b): fail-fast masked 3 of 4 reds
  in one landing.
- Cap concurrent cargo jobs at two box-wide; this box is memory-constrained and
  a compile storm also flakes load-sensitive tests (#16).
- Release suites are slow by design (40–60+ min). Do not "speed them up" by
  dropping to debug; that is the failure #19 names.

## 6. Where things live

| thing | path |
|---|---|
| Canonical bed server | `http://127.0.0.1:9418`, root `/home/user/gardenbed/arb` |
| Bed repository name | `default` (v0.1 root layout: one repository at the root) |
| Git mirror (GitHub downstream) | `/home/user/code/gardenbed` (Git consumer; inspect bare/worktree layout; `origin` = `git@github.com:rot13maxi/gardenbed.git`) |
| Chain checkout | `/home/user/bed-land` (stable path, outside `/tmp` — #18) |
| Lane checkouts | `/home/user/bed-lanes/<lane>`, one per lane owner |
| Deployed binaries | `~/.local/bin/{bed,gardenbed-server,gardenbed-daemon,gardenbed-web,gardenbed-mcp}` |
| Server supervisor | `systemctl --user gardenbed-dogfood`; fallback `/home/user/gardenbed/server-watchdog.sh` |
| Durable registry | `docs/DOGFOOD.md` in this repository (in-repo is the only durable home) |

Lane and chain checkouts MUST live outside `/tmp` **and** outside the shared
mirror's worktree path: `/tmp` cleanup deleted a landing checkout mid-chain
(#18), and the shared worktree path disappeared mid-session in the backlog sweep
(`/home/user/code/gardenbed-worktree` was a git worktree of the bare mirror;
removing it took the coordinator's checkout with it). Use
`/home/user/bed-lanes/<lane>`, and verify the path exists and
`.bed/config.toml` names the right server before starting a lane.

## Pre-landing checklist

- [ ] Reviewed tip is a single lane ref; the reviewer signed off on the final
      delta; fixes were re-reviewed.
- [ ] Guards/oracles carry a mutation proof (observed red without the property).
- [ ] Lane gates: `cargo fmt --all -- --check`; `cargo clippy -p <touched>
      --all-targets -- -D warnings`; affected crate suites `--no-fail-fast`; the
      `-p gardenbed` suite whenever the span touches `crates/bed` sources or the
      shared test harness (#15 rule 1).
- [ ] Timing/assert-sensitive code: release-mode run of the affected suites
      (#19).
- [ ] Span re-unioned onto current `main`; main-wins on stale carries; the union
      commit is documented.
- [ ] Import report line present (if the landing exists as git commits); bed
      `main` advanced by the plain `bed publish` (§1.2) and read back at the
      published tip — for a single-changeset retry `bed rebase main`, for a
      stack `bed rebase --stack main`.
- [ ] Export report line present; `git -C <mirror> rev-parse main` equals the
      mapping row's git SHA for the published bed tip; `git fsck --strict`
      clean. (`git status --porcelain` applies only in a non-bare consumer
      checkout — a bare mirror exits 128.)
- [ ] `cargo build --workspace --bins --release` and the deployed binaries
      installed.
- [ ] Server bounced and answering on 9418; the deployed server is at least as
      new as the span (`bed publish --ref` needs the `fd29b3d` protocol, #13).
- [ ] Canary through the deployed pair green; live `bed log`/`bed diff` probe
      green.
- [ ] One landing in flight — nothing else is queued behind this step.

## When main goes red

- The red is fixed **on `main`** by the lane that finds it (or by the release
  manager as fix-secretary); it is never carried by the next lander
  (#15 rule 2). A landing does not proceed past a known red.
- Identify whether the red is debug-only or release-only before deciding the
  span is at fault: re-run the failing suite in release through the deployed
  pair. Debug-green/release-red and release-green/debug-red are different
  diagnoses (#19).
- If the red is attributable to the latest landing and the fix is not
  immediate, the preferred route is a **forward fix**: land a small commit on
  top of `main` (same chain, plain publish). A rewind is the second route and
  is deliberately awkward: `bed publish` refuses a non-ancestor `main` move and
  `--ref main` is reserved, so the only executable path is
  `gardenbed-server import --from <git> --root <root> --repo <repo>
  --ref refs/heads/main --allow-rewind`, which prints the loud backward-move
  warning (verified: the ref moved and the warning named both tips and the
  unreachable count). Treat a rewind as a recorded operation — put the
  replacement tip, the reverted ids, and the evidence path in the landing note
  and the registry — then re-land the span only with the fix.
- Record the finding in [docs/DOGFOOD.md](DOGFOOD.md) with the exact command and
  error; the registry is the durable home (#18).
