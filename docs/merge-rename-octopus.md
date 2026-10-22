# Rename-aware, recursive, octopus merges (v0.3 merge lane)

This is the task doc for the `todo/merges` lane. It documents the
merge semantics the lane introduces, their bounds, the git parity
evidence, and the DOGFOOD #30 fix. Shared docs (divergences.md,
cli.md, status.md) carry only what this lane proposes for them in its
final report; retirement text for divergences §8/§9 is proposed there,
not applied here.

## What changed

Three engine capabilities, one server capability, one bug fix:

1. **Rename-aware tree merge** — `crates/derive/src/merge.rs`. The
   merge walk detects renames on each side and participates them as
   moves instead of delete+add pairs.
2. **Recursive multi-base resolution** — criss-cross merges fold all
   maximal common ancestors into one virtual base before the walk
   (git's `merge-recursive`/`merge-ort` shape).
3. **Octopus merge** — `bed merge t1 t2 …` folds any number of
   targets sequentially, following `git-merge-octopus` exactly.
4. **DOGFOOD #30** — the direct merge-base APIs pick from the MAXIMAL
   common ancestors (DAG ancestry dominates timestamps).

## Rename detection (whole tree)

`detect_side_renames` pairs each side's (deleted, added) file paths
over **flattened whole-tree listings** — git's `diffcore_rename`
scope. Whole-tree pairing is what makes cross-directory moves
detectable: `dirA/f` deleted and `dirB/g` added pair regardless of
which tree-walk level each endpoint surfaces at.

- **Pass 1, exact renames:** destination blob id equals the source's.
  Each source pairs with the lexicographically first exact
  destination; ties pick the first source in path order.
- **Pass 2, scored renames:** leftovers scored by BYTE-weighted span
  similarity (`SpanProfile`, git's `diffcore_rename` shape): content
  is chunked at newline boundaries to spans of at most 64 bytes,
  hashed (FNV-1a), and matched as a byte-weighted MULTISET — the
  score is matched-chunk bytes over the larger side's total bytes.
  Common BYTES, not common line counts: three shared short lines
  cannot outvote two divergent megabyte lines. Greedy best-first
  pairing; a pair must clear `MIN_RENAME_SCORE = 50` (git's
  `-M50%`). Candidate span data is memoized per blob id per detection
  pass — the quadratic pairing loop never re-fetches or re-hashes a
  blob.
- **Bounds:** `MAX_RENAME_CANDIDATES = 1000` per side over the whole
  tree (git's `diff.renameLimit` default). Beyond it, candidates stay
  unpaired and the merge degrades to rename-blind for them — the same
  fallback shape as git's rename limit.
- Mode class must agree on both ends (Regular/Exec both count as
  files; symlink/gitlink are not rename candidates).

The detection runs once per merge (`Engine::side_renames`), and the
per-level walk filters each level's renames by endpoint directory.
Two consequences needed whole-tree screening in `merge_tree_ids`
because the per-level trivial rules (`ours == theirs` at the source's
directory — each side's move or delete produces the same subtree —
or `theirs == base` at the destination's) short-circuit the level
before any rename rule runs:

- **rename/delete** (one side renamed, the other deleted the source):
  `screen_rename_deletes` names both endpoints.
- **rename/rename 1-to-2** (both sides renamed the same source to
  different destinations): `screen_rename_rename_1to2` names source
  and both destinations.

### The rename rules

| shape | outcome |
|---|---|
| rename vs untouched | clean — the move stands |
| rename vs edit at source | clean — the edit merges INTO the destination (3-way over base-content, mover's dst, editor's src) |
| rename + edit vs untouched | clean — dst content is the mover's edited content |
| same source → same dst, different edits | clean — 3-way at dst anchored on the source's base entry |
| rename/rename 1-to-2 | conflict, source + both destinations |
| rename/delete | conflict, both endpoints |
| rename/rename 2-to-1 | conflict at the destination |
| rename onto occupied path | conflict at the destination |
| rename with mode flip | clean — the mode rides the rename |
| both sides rename same source into the SAME directory | clean — recursion into the directory merges every descendant (extras preserved) |

The reverse direction (theirs renames, ours edits the source) emits
the merged content at the destination AND the delete of the source —
the change list is vs OURS, and ours' tree still holds the edited
source.

### File `A` vs directory `A` (the path-mask trap)

A rename may move a file INTO a directory named like the file itself
(`A` → `A/sub`): the source FILE's path then equals the destination
directory's path. The rename-vs-edit rule fires only when the other
side's entry at the source name is a **file** (a real edit). When
both sides renamed the same source into the same directory, the
source name holds a **directory** on both sides — the rule stands
down and the walk recurses into that directory over the (absent) base
subtree, merging every descendant file (`A/sub` plus each side's
extras). This distinguishes the consumed FILE entry `A` from the
directory CONTAINER `A`; a single whole-tree rename map keyed by
exact paths handles both because `Engine::entry_at` returns the entry
at a path regardless of kind.

## Recursive multi-base resolution

`Engine::merge_tree_changes_recursive(ours, theirs)` computes every
maximal common ancestor (`merge_bases_all`, git's `merge-base --all`
semantics), and:

- **0 bases:** empty-tree merge (the caller refuses unrelated
  histories first).
- **1 base:** the ordinary single-base merge, byte-identical to
  `merge_tree_changes` (the v0.2 contract preserved).
- **≥2 bases:** `fold_virtual_base` folds them left-to-right —
  `acc = merge(acc, bases[i])` anchored on the MAXIMAL common
  ancestors of the accumulated references and `bases[i]` — into a
  **virtual base tree**. The accumulator tracks the real folded
  changesets (`acc_refs`) as a VIRTUAL ancestry overlay: the common
  ancestors of `(∪ acc_refs, b)` are the union of
  `merge_bases_all(ref, b)` over every folded reference, filtered to
  the maximal set by `is_ancestor` — the same shape as the server's
  `octopus_bases`. NOTHING is stored: no synthetic or authored
  changeset ever enters the metadata store (a merge that conflicts
  leaves `recent_changesets` untouched; the fold's intermediate trees
  are derived, content-addressed, and orphan-safe). When a round's
  inner bases are multiple, the inner anchor is itself a RECURSIVE
  fold (depth +1) — all inner maximal bases participate. A
  conflicting inner fold DROPS the conflicting paths from the virtual
  base (`virtual_base_dropping`): the outer merge then sees those
  paths as base-absent, surfacing the real disagreement instead of
  hiding it behind an arbitrary pick. `MAX_RECURSIVE_BASE_DEPTH = 16`
  bounds stacked criss-crosses; beyond it the merge REFUSES with a
  named error — never a silent anchor to an arbitrary base
  (documented limit).

`Repo::merge` uses the recursive path; `Engine::merge_tree_changes`
(single explicit base) remains for callers that want exactly that.

## Octopus merge

`Repo::merge_octopus(ours, targets, message, author, committer)`
folds `targets` into `ours` sequentially, following
`git-merge-octopus` (verified against git 2.43 source and behavior):

- **Round i:** merge the accumulated reference with `targets[i]` over
  `octopus_bases` — the intersection of pairwise `merge_bases_all`
  over the accumulated references (git: `git merge-base --all SHA
  $MRC` over the accumulated list).
- **Up-to-date target** (an ancestor of the accumulated reference):
  skipped, NOT recorded as a parent.
- **First-round fast-forward** (the first target's tree equals the
  merge result): the target replaces ours as the first parent.
- **Any conflict in any round:** refused —
  `ServerError::MergeConflict` with
  `octopus merge refused (round {n}[, last]): {paths}`, round number
  and conflicting paths named, NOTHING stored (the D11 contract: no
  partial publication, no marker trees).

CLI: `bed merge <T1> <T2> …` — one target is the ordinary two-way
merge; two or more fold octopus-style (default message
`merge {names}`). RPC `MergeOctopus` requires ≥2 targets
(`invalid_argument` otherwise; one target goes through `Merge`).
The daemon proxy forwards `MergeOctopus`.

## DOGFOOD #30: merge-base picks from the maximal set

`merge_base_naive_impl` returns the deterministic first of
`merge_bases_all`'s MAXIMAL set — the maximal filter runs BEFORE the
`(committer secs desc, id bytes desc)` order, not after. The old
rationale ("the maximal filter is dead work — newest-first order
already surfaces the best ancestor") was wrong twice:

- **Same-second histories:** the raw order tie-breaks by id bytes,
  which can surface a dominated ancestor (the classic criss-cross
  built in one committer second).
- **Skewed clocks:** a dominated ancestor stamped NEWER than its own
  descendant (e.g. a clock jumped ahead and recovered) sorts first.

Merging over a non-maximal base mis-anchors the 3-way walk: both
sides' edits can look changed against a base that predates them,
producing spurious conflicts on disjoint-line edits.

All consumers inherit the fix: `Engine::merge_base`,
`Engine::merge_base_naive`, the `MergeBase` RPC, and rebase /
rebase-stack. Unique-base histories (the v0.1 linear mainline) are
unchanged — the maximal set there is a single changeset.

## Git parity evidence

All differential evidence runs the SAME fixture content through real
git repositories (`git merge-tree --write-tree`, `git merge-base`)
and compares classification (clean/conflict, conflict paths) plus
every clean path's merged content and the full merged path set.

- `crates/derive/tests/merge_rename.rs` — 21-case matrix: the eight
  same-directory shapes, cross-directory rename-vs-edit (clean, edit
  lands at `dirB/g`, source gone), cross-directory rename/delete
  (refused, both endpoints), cross-directory rename/rename 1-to-2
  (refused, source + both destinations), the reversed
  edit-vs-rename (clean, edit lands at dst, source gone), and the two
  saved merge-path oracle shapes: `A` → `A/sub` vs an edit of `A`
  (clean; the edit lands at `A/sub` and nothing else survives), and
  both sides renaming `A` → `A/sub` with distinct extras under `A/`
  (clean; all three files survive). Both oracle cases run the same
  fixture content through real git inside the test. Seven
  byte-span-estimator cases: the random-long-line no-pair shape (both
  directions — git's modify/delete conflict, not a false rename), a
  REAL rename of a mostly-copied long line, binary-with-NUL rename
  (detection proven by the conflict landing at the destination — git
  refuses binary content merges), exact-duplicate blobs with one
  mover, an equal-score two-pair tie, and a huge-blobs /
  short-common-lines false-positive guard.
- `crates/derive/tests/merge_recursive.rs` — 8 tests: `merge_bases_all`
  parity with `git merge-base --all` on criss-crosses; clean-fold
  content equality with git; classification parity (single-base
  tie-break conflicts where git+recursive merges cleanly); unique-base
  byte-identity with the single-base path; determinism across runs;
  and TWO three-maximal-base regressions (disjoint tips' edits and a
  shared edit) over the git-verified shape
  `base→{x,y,z} siblings, p1=(x,y), p2=(y,x), ours=(p1,z)+edit,
  theirs=(p2,z)+edit` — asserting `git merge-base --all` sees exactly
  three maximal bases and the fold reproduces git's clean merge
  content — and a no-persistence regression: through BOTH the
  conflicting and the clean multi-base fold, `recent_changesets` is
  byte-identical before and after (the virtual ancestry overlay
  stores nothing).
- `crates/derive/tests/merge_base_maximal.rs` — 5 tests: same-second
  criss-cross (both direct APIs pick a maximal base; `merge_bases_all`
  equals the maximal pair; merging over the dominated pick conflicts
  while the recursive fold is clean); skewed-clock DAG (all APIs pick
  the true base over the skewed dominated ancestor); merge outcome
  cleanliness; `git merge-base` oracle parity on the skewed shape.
- `crates/server/tests/merge_octopus.rs` — 6 tests: fold shape +
  parents; up-to-date skip (parents exclude it); first-round
  fast-forward (ours not a parent); conflict refusal (round + paths,
  FAILED_PRECONDITION, nothing stored — verified via the log stream);
  16 racing identical merges converge on one id; merged tree equals
  git's (path set + contents).

## Bounds and limits

| bound | value | git analogue | beyond it |
|---|---|---|---|
| `MIN_RENAME_SCORE` | 50 | `-M50%` | pair stays delete+add |
| `MAX_RENAME_CANDIDATES` | 1000/side/tree | `diff.renameLimit` | rename-blind for the surplus |
| `MAX_RECURSIVE_BASE_DEPTH` | 16 | (none) | merge refuses loudly (named error) |
| `SPAN_CHUNK_MAX` | 64 bytes | span-hash size | chunks beyond 64B split |

Octopus refusal is refusal: there is no "last round is softer"
behavior (git's exit-2 vs exit-1 distinction is a script protocol
detail; both leave no commit).

First-round fast-forward drops ours from the parent list: when the
first target's base IS ours, the target replaces ours as the
accumulated head (`git-merge-octopus`'s "Fast-forwarding to:"), so
the final changeset's parents are `[t1, t2, …]` — ours is NOT a
parent, exactly like git.
