# Divergences from git — v0.1

Definition of done §10.2 requires **zero silent divergences** from git on
covered semantics: every place the bridge differs from git must be
documented here with a repro. Silent disagreement is a release blocker.

Each entry states what happens, why, what it costs, and how to reproduce
it. Anything *not* on this list must round-trip exactly: unsigned git
history imports, exports, and re-imports with byte-identical commit SHAs
(verified in `crates/git/tests/export.rs`).

---

## 1. Signed commits are exported unsigned

**What:** a gardenbed changeset carries an optional `signature`
(the commit's `gpgsig` header, decoded). `git fast-import` has no grammar
for attaching extra headers to a commit, so export cannot re-emit a
signature. Signed changesets are exported **unsigned**, and therefore
receive a **different git SHA** than the original signed commit. Unsigned
histories — everything gardenbed generates itself — are unaffected and
reproduce their SHAs exactly.

**Cost:** `ExportReport::signed_dropped` counts the affected commits.
Their *trees* still match bit-for-bit; only the commit objects differ
(a signature is part of the commit payload, hence part of its SHA).
A re-import of an exported signed history produces new changeset ids for
those commits — the git↔changeset mapping for them is not a bijection.

**Why not fixed:** forging the original bytes would require re-signing
(with the original key) or carrying a non-canonical commit writer — both
out of v0.1 scope. Documented, counted, loud.

**Repro** (fake signature — no gpg needed; any `gpgsig` header behaves
the same):

```sh
git init -q --bare sigrepo
TREE=$(git -C sigrepo hash-object -w -t tree --stdin </dev/null)
BODY="tree $TREE
author Gopher <g@arden> 1700000000 +0000
committer Gopher <g@arden> 1700000000 +0000
gpgsig -----BEGIN PGP SIGNATURE-----
 fake: this header cannot survive a fast-import stream
 -----END PGP SIGNATURE-----

signed!
"
SHA=$(printf '%s' "$BODY" | git -C sigrepo hash-object -t commit -w --stdin)
git -C sigrepo update-ref refs/heads/main "$SHA"
```

Import `sigrepo` (the changeset's `signature` is captured), export it back
to git: `signed_dropped == 1`, the exported commit's SHA differs from
`$SHA`, and `git cat-file` on it shows no `gpgsig` header while the tree
matches. Covered by `signed_commits_drop_their_signature` in
`crates/git/tests/export.rs`.

## 2. Non-UTF-8 commit bytes are transcoded lossily

**What:** git commit objects are byte sequences; gardenbed stores
`String`s. Names, emails, messages, and path names that are not valid
UTF-8 are decoded with `String::from_utf8_lossy` at **import** (each
invalid byte becomes U+FFFD). Export writes those `String`s verbatim, so
such commits do **not** round-trip: the exported commit has the
transcoded bytes and a different SHA.

**Cost:** real-world corpora are UTF-8; git itself treats unknown
encodings as raw bytes only for display. The lossy decode is visible and
deterministic (same bytes → same replacement chars → same changeset id).

**Repro:** create a commit whose author name contains an invalid UTF-8
byte (`printf 'bad \xff name'` via `git commit-tree` env plumbing), import
it, and observe the stored name contains U+FFFD. An export of that
changeset writes U+FFFD — never the original `\xff`.

## 3. Extra commit headers are not modeled

**What:** headers gardenbed does not model — `encoding`,
`mergetag`, `gpgsig-sha256` — are dropped on import (their continuation
lines are consumed but discarded) and never re-emitted on export. Like
§1 this changes commit bytes, hence SHAs, for commits carrying them.

**Cost:** same shape as §1 — trees are exact; commit objects containing
those headers differ. `gpgsig-sha256` dropping is *not* counted in
`signed_dropped` (v0.1 only recognizes the SHA-1 `gpgsig` header); a
commit carrying both headers is counted once for its SHA-1 signature.

**Repro:** hand-write a commit with an `encoding ISO-8859-1` header via
`git hash-object -t commit -w --stdin`, import, export: the exported
commit has no `encoding` header.

## 4. Pre-canonical git tree modes (`100664`) import canonicalized

**What:** very old git (April 2005, around the "new git world order"
re-import of the linux history) wrote regular-file tree entries with the
group-writable mode `100664`. Gardenbed's mode table has no `664`: the
importer canonicalizes any `100xxxx` blob entry by its exec bit —
`100664` and `100644` both import as `Mode::Regular` (the rule the tree
parser has always applied). Commits that **only** normalize such modes
(identical blob ids, differing raw tree bytes) are invisible to git's
own diff machinery: `canon_mode` collapses `100664` ≡ `100644` before
comparing, so `git diff-tree -r --name-status` — and even `--raw` —
prints nothing for them. The importer therefore classifies these commits
with a raw, mode-aware tree comparison (`git`'s
`raw_first_parent_diff`): a pure mode fixup becomes a **zero-change
changeset** inheriting the parent tree; content changes under those
modes and structural changes come out exactly as git would shape them;
no renames are invented.

**Cost:** for pre-canonical history, Gardenbed's derived tree ids differ
from git's raw tree SHAs **by construction** — ours are canonical, git's
carry the historical mode bytes. Tree-level comparisons for such commits
(`git cat-file` on tree objects, non-recursive raw diffs) must normalize
`100664` ≡ `100644` first. Path/mode/content comparisons via
`git ls-tree -r` are unaffected: ls-tree displays canonicalized modes.
This **will** matter for the linux verification sweep: the early history
contains such commits (e.g. `ce1dc02f7643`, "Fix up some file mode
differences due to the new git world order.", whose tree differs from
its first parent's only by three `100664` entries). The same collapse
covers permission-bit variants of every other entry kind (`100775` ≡
`100755`, `120777` ≡ `120000`, `160123` ≡ `160000`, dir `040750` ≡
`040000`); exotic variants outside the file-mode table are rejected
loudly by the tree parser, as before.

**Why not fixed:** re-canonicalizing historical trees would rewrite git
object identity (the raw tree SHAs are what corpus pins record), and
carrying a non-canonical mode column through the changeset model would
change the wire format for a mode that disappeared from git twenty years
ago. Documented and handled explicitly (fallback, never a silent skip).

**Repro:** `pre_canonical_100664_mode_fixups_import_via_raw_fallback` in
`crates/git/tests/edgecases.rs` — hand-crafted trees via
`git hash-object -t tree -w --stdin --literally` (modern git fsck-checks
fresh tree objects and refuses `100664` without the flag; objects that
already contain the mode are accepted by plain `git fsck`, warning only).

---

## 5. Blame attributes blank context lines per our own diff, not xdiff's

**What:** `bed blame` (and the MCP `blame` tool) attribute line ranges by
diffing each first-parent step with the store's own minimal diff. git's
xdiff applies additional hunk-placement heuristics (indent heuristic,
compaction). When a commit inserts a block next to a blank line, the two
diffs are both minimal but place the blank on opposite sides of the hunk:
xdiff appends the inserted blank *after* the block, Gardenbed's diff keeps
the pre-existing blank where it was and inserts the block before it. For
the file's *content* the two encodings are identical; for *attribution*,
an identical blank line may be blamed on a different (equally valid)
commit — usually a merge on either side of the insertion.

**Observed:** across ~11,000 blamed lines of git.git (builtin/checkout.c,
builtin/remote.c, Makefile, builtin/commit.c, builtin/add.c, and others)
compared per line against `git blame --first-parent --porcelain`, 4 lines
diverged — all blank lines adjacent to insertion hunks (e.g.
builtin/checkout.c lines 328 and 661, builtin/remote.c lines 872 and 875).

**Why not fixed:** the choice is inherent to diff ambiguity, not a
correctness bug: both diffs are minimal, the trees are identical, and
every non-blank line in the corpus attributed identically. Matching git
line-for-line would require reimplementing xdiff's heuristics in the
derive diff; if exact parity ever matters, that is the fix.

## 6. `bed diff` renders renames as delete + add

**What:** `bed diff` never pairs renames at the render level: a rename
changeset renders as a delete + an add — two `diff --git` blocks (one
`deleted file mode`, one `new file mode`) — never git's
`similarity index 100%` / `rename from` / `rename to` form. The renderer
consumes the server's tree diff, which runs with rename detection off
(`Add`/`Modify`/`Delete` per path), and classifies each path by presence
alone — `DiffOp` has no rename classification — so no rename is ever
available to pair; the renderer's documented parity target is
`git diff-tree -r --no-renames` (`crates/bed/src/render.rs`). The MCP
`diff` tool answers from the same tree diffs, so a rename shows as `D`
and `A` letters there too.

**Cost:** purely output fidelity — review parity. Compared with
`git diff-tree -M` on the QA regression pass's rename pairs, entry
counts differ (1,318 vs 1,321 and 4,794 vs 5,128 entries), and
`grep '^rename from '` over `bed diff` output finds 0 hits, so review
tooling (or a human) diffing working copy's output against rename-aware git sees
a different shape for every moved file. What is *not* affected: the
trees and the changesets — imported changesets record renames as git's
own `-M` detection reported them (the D8 import mechanism,
`docs/v0.1.md` §3), `commit` pairs a pure rename — a deleted path and an
added path with the identical blob id — into one canonical `Op::Rename`
carrying `src_path` (op `3 rename` in the canonical encoding spec's
`change` table, `docs/canonical-encoding.md`), and patches built from
the delete + add rendering reapply cleanly with `git apply` (verified in
the same pass). The identity half of this shape is already catalogued
under "Explicitly *not* divergences" below: rename vs. delete+add builds
the same tree and the same commit SHA. Only the rendered diff differs.

**Why not fixed:** render-level similarity pairing is not implemented in
v0.1 — the diff parity contract was written against `--no-renames` from
the start. Import-level rename detection is a separate mechanism (D8)
and does exist; only the render level lacks it. The fix is render-local:
pair identical-blob delete + add entries at render time (the
deterministic rule `commit` already applies), or reuse the changeset's
recorded `Rename` ops for changeset-scoped diffs instead of re-deriving
from trees — a v0.2 candidate.

**Repro:** commit a pure rename (or import a history containing one) and
run `bed diff <cs>` — that changeset against its first parent. The old
path renders as a `deleted file mode` block and the new path as a
`new file mode` block, while `git diff-tree -M --name-status` on the
same pair shows the two paths as a single rename entry.

## 7. Gitlink pointer changes render as a binary-differ line

**What:** a submodule pointer change in `bed diff` renders the gitlink's
mode lines (`new file mode 160000` / `deleted file mode 160000` /
`old mode` + `new mode` for a typechange) plus git's binary one-liner
`Binary files a/<p> and b/<p> differ` — not git's
`@@ -1 +1 @@` hunk with `-Subproject commit <sha>` / `+Subproject commit
<sha>`. Gitlink blobs in the canonical model are synthetic padded ids
(the import convention, `docs/canonical-encoding.md`), not git's
submodule SHAs, so emitting a `Subproject commit` line would write bytes
git would then apply as a *wrong* pointer target.

**Cost:** review parity, and one patch consequence: `git apply` refuses
the pointer-change entry (`cannot apply binary patch … without full
index line`) where git's own `Subproject commit` form applies. The
refusal is per-entry and parse-safe — other entries in the same patch
still apply (the pre-fix renderer leaked the gitlink's path into the
next entry's parse and poisoned the whole patch). Gitlink
additions/deletions render mode lines only and apply cleanly. The
faithful oracle for pointer targets remains `git diff --submodule` /
`git submodule status`.

**Why not fixed:** an honest `Subproject commit` line needs git's real
submodule SHAs imported and stored as first-class ids — model work, not
render work. A v0.2 candidate alongside rename pairing (§6).

## Explicitly *not* divergences

These shapes differ from git's most literal rendering but produce
byte-identical trees **and identical commit SHAs** — commit objects
encode no diff information, so the change shape below a commit is
invisible to git's identity:

* **Rename vs. delete+add.** The generator commits pure renames (identical
  blob id) as one `Rename`; moved-and-edited files commit as a delete+add
  pair (no similarity heuristic). Import keeps git's `-M` renames;
  export emits fast-import `M` + `D` for a rename, never `R`. All three
  shapes build the same tree and the same commit SHA.
* **Gitlink padding.** A submodule pointer's gardenbed blob id is the
  upstream git SHA-1 left-padded with twelve zero bytes; export strips
  the padding and writes `M 160000 <sha> <path>`. Byte-identical to the
  original entry.
* **Empty identity fields.** A commit with an empty name (` <email>`) or
  empty email (`<>`) is stored and re-exported as-is; `git fsck --strict`
  accepts both, and the identity line is reproduced byte-for-byte.
* **Raw timezone offsets.** Identity timestamps are stored as epoch
  seconds plus the original `±hhmm` offset and re-emitted raw — never
  normalized to UTC — so dates round-trip exactly.
* **Marks do not persist.** Each export is one `git fast-import` run
  with one mark namespace; marks are re-issued deterministically per run
  (blobs sorted by id, commits in topological order) and the final
  `--export-marks` file is left in place (path in
  `ExportReport::marks_path`).
