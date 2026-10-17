# Divergences from git — v0.2

Definition of done §10.2 requires **zero silent divergences** from git on
covered semantics: every place the bridge differs from git must be
documented here with a repro. Silent disagreement is a release blocker.

Each entry states what happens, why, what it costs, and how to reproduce
it. Anything *not* on this list must round-trip exactly: imported git
history — signed, extra-header, and non-UTF-8-identity commits included —
exports back with byte-identical commit SHAs (verified in
`crates/git/tests/export.rs`).

---

## 1. (Retired in v0.2) Signed commits are exported unsigned

**Retired by raw-commit capture (v0.2 D21).** The importer byte-compares
each commit's original object bytes with what the model can reconstruct;
a signature makes them differ, so the original bytes are captured verbatim
in a sparse side table (`git_raw_commit`, keyed by changeset id) inside
the ingest transaction, and the export emits them as-is. The SHA is the
original by definition — no re-signing, no id forking.

**Repro (now a fidelity proof):**
`signed_commits_roundtrip_exactly` in `crates/git/tests/export.rs` —
import a hand-signed commit, export it back, and the exported commit's
SHA equals the original, `signed_dropped == 0`, and `git cat-file`
returns the original object bytes.

**Residual:** v0.1 stores carry no raw rows; exporting their signed
changesets REFUSES loudly (the SHA assertion fires before anything
ships — `signed_dropped` is 0 on every successful export). Re-importing
the git history into a fresh store retires the divergence with stable
changeset ids (canonical bytes are unchanged).

## 2. Non-UTF-8 commit bytes are transcoded lossily (narrowed to paths in v0.2)

**What remains:** gardenbed stores `String`s; **path names** that are not
valid UTF-8 are decoded with `String::from_utf8_lossy` at import (each
invalid byte becomes U+FFFD). This is tree-level divergence: the derived
tree differs from git's raw tree, so such commits cannot round-trip, and
the export now **refuses loudly** (`ExportError::Changeset`: the raw
commit bytes do not reference the derived tree) rather than emitting a
transcoded commit with a silently different SHA.

**What was fixed (v0.2 D21):** non-UTF-8 **identity** bytes (author/
committer names, emails) and **messages**. The import capture predicate
byte-compares the reconstruction with the original object; any lossy
decode makes them differ, so the original bytes are captured and the
export emits them verbatim — SHA-exact.

**Repro (fixed half):** `non_utf8_author_roundtrips_exactly` in
`crates/git/tests/export.rs` — a latin-1 author name roundtrips SHA-exact.

**Repro (remaining half):** import a repository containing a non-UTF-8
path name and export it: the export fails with
`the raw commit bytes do not reference the derived tree`. The
**raw-tree capture** that would retire this half is gated on the kernel
import throughput gate (§4 below).

## 3. (Retired in v0.2) Extra commit headers are not modeled

**Retired by raw-commit capture (v0.2 D21).** `encoding`, `mergetag`,
`gpgsig-sha256` — any header the model does not carry — makes the
byte-compare at import fail, so the original bytes are captured raw and
the export emits them verbatim, SHA-exact.

**Repro (now a fidelity proof):** `extra_header_commits_roundtrip_exactly`
in `crates/git/tests/export.rs` — a commit with `encoding` and `mergetag`
headers roundtrips SHA-exact, and the exported object bytes equal the
original bytes.

## 4. Pre-canonical git tree modes (`100664`) import canonicalized — export half retired

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

**Cost (import half — unchanged):** for pre-canonical history,
Gardenbed's derived tree ids differ from git's raw tree SHAs **by
construction** — ours are canonical, git's carry the historical mode
bytes. Tree-level comparisons for such commits (`git cat-file` on tree
objects, non-recursive raw diffs) must normalize `100664` ≡ `100644`
first. Path/mode/content comparisons via `git ls-tree -r` are
unaffected: ls-tree displays canonicalized modes. This **will** matter
for the linux verification sweep: the early history contains such
commits (e.g. `ce1dc02f7643`, "Fix up some file mode differences due
to the new git world order.", whose tree differs from its first
parent's only by three `100664` entries). The same collapse covers
permission-bit variants of every other entry kind (`100775` ≡ `100755`,
`120777` ≡ `120000`, `160123` ≡ `160000`, dir `040750` ≡ `040000`);
exotic variants outside the file-mode table are rejected loudly by the
tree parser, as before.

**Export half (retired in v0.2):** the old export silently re-rendered
the canonical tree, producing a commit with a **different SHA** than the
mapping recorded. The native export asserts every computed SHA against
the git-mapping table (D20): for such commits the raw bytes' tree header
disagrees with the derived tree, and the export **fails loudly**
(`ExportError::Changeset`) instead of diverging silently. The
**raw-tree capture** that would make these exports verbatim is gated on
the kernel import throughput gate: it is implemented only if capturing
sparse raw tree bytes keeps a kernel-segment import within ±20% of the
recorded 278 commits/s. Until then the behavior is: loud refusal on
export, exact import.

**Why not fixed outright:** re-canonicalizing historical trees would
rewrite git object identity (the raw tree SHAs are what corpus pins
record), and carrying a non-canonical mode column through the changeset
model would change the wire format for a mode that disappeared from git
twenty years ago. Documented and handled explicitly (fallback, never a
silent skip).

**Repro (import half):** `pre_canonical_100664_mode_fixups_import_via_raw_fallback`
in `crates/git/tests/edgecases.rs` — hand-crafted trees via
`git hash-object -t tree -w --stdin --literally` (modern git fsck-checks
fresh tree objects and refuses `100664` without the flag; objects that
already contain the mode are accepted by plain `git fsck`, warning only).

**Repro (export refusal):** import such a repository, export it — the
export names the changeset whose raw tree diverges.

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

## 6. Renames below 100% similarity render as delete + add (retired v0.2)

**Status:** retired by D22 (v0.2 M19). Pure renames — a moved file whose
blob is unchanged — render git's `R100` form (`similarity index 100%` /
`rename from` / `rename to`) in every `bed diff` form. The scoped forms
(`bed diff <cs>`, and the no-argument form on a clean copy) render from
the changeset's recorded `Op::Rename`; the two-argument form pairs a
delete with a write carrying an identical blob at render time — the same
rule `bed commit` uses to record a rename, so no second pairing
convention exists. Letter parity with `git diff-tree -M` and byte parity
of pure-rename changeset bodies (`git diff-tree -M -p`) hold on
generated rename histories (`pure_renames_render_git_m_parity_and_byte_parity`
in `crates/bed/tests/read_path.rs`), and rendered rename blocks reapply
with `git apply` (`git_apply_accepts_every_rendered_shape` in
`crates/bed/tests/render_patch.rs`). The MCP `diff` tool deliberately
keeps its v0.1 letter parity (`git diff-tree --no-renames`); that is a
deliberate scope decision recorded with D22, not an oversight.

**Residual — moved-and-edited files:** a file that moved *and* changed
content renders as a delete plus a write, never git's
`R<similarity<100>` form. Gardenbed claims no similarity score below
100%, because it has none: the model records rename facts (identical
blob) and the deterministic delete+write expansion otherwise. git's
`-M` heuristic would print one `R` block with a similarity index for
the same change, so review tooling diffing outputs still sees two
blocks where git prints one. `commit` records these as a delete+add
pair (never a `Rename`), so the residual matches what the changeset
records — one model fact, one rendering. Counted from recorded facts
on generated histories: the 25%-edit rename histories render
`delete`/`write` pairs exactly for edited moves
(`moved_and_edited_files_render_delete_plus_write`), never a spurious
rename block.

**Residual — pairing-rule corners:** pairing matches `commit`'s
recorded-rename rule — a delete plus a *new* path of the identical blob
and the same entry kind — where git's `-M` heuristic scores content
similarity across arbitrary adds and modifies. Two letter divergences
follow, both counted: git may pair an *edited* rename bed cannot (a
`R<similarity<100>` entry where bed prints delete + write, the residual
above), and git may pair a *cross-kind* move as one entry where bed
prints `D` plus `A` (bed never pairs across entry kinds — git apply
refuses a cross-kind `R100` with "new mode does not match"). Both
corners need hand-crafted content (duplicate blob bytes across paths, or
a symlink whose target equals a deleted file's content); generated
histories mint globally unique blob content per write, so neither occurs
there. Pre-fix, pairing also matched *rewrites of pre-existing paths*,
rendering an `R100` git apply refuses ("already exists in working
directory"); that is fixed — pairing requires the destination to have
been absent on the old side (`guarded_corners_render_delete_plus_write_and_apply`).

**Repro:** commit a moved-and-edited file (`git mv a b && edit b && bed
commit`) and run `bed diff <cs>`: `a` renders a `deleted file mode`
block and `b` a `new file mode` block; `git diff-tree -M
--name-status` prints one `R` entry. A pure move in the same changeset
renders one rename block, matching git.

## 7. Gitlink↔regular typechanges render mode lines only (retired v0.2)

**Status:** retired by D23 (v0.2 M19) for the gitlink add, delete, and
pointer-change cases. All three render git's `Subproject commit <sha>`
form — the real upstream SHA read from the gitlink's padded blob id
(twelve zero bytes + SHA-1, `docs/canonical-encoding.md`) — as `new
file mode 160000` + a one-line add hunk, `deleted file mode 160000` +
`--- a/<p>` + a one-line delete hunk, and `--- a/<p>` / `+++ b/<p>` +
a one-line swap respectively. The retired v0.1 shape (`Binary files …
differ`) is gone: `git apply --cached` accepts the add and pointer
forms (`git_apply_accepts_every_rendered_shape` in
`crates/bed/tests/render_patch.rs`), and rendered SHAs match `git
diff-tree --submodule=short` on the same changesets
(`gitlink_pointer_changes_render_subproject_hunks`). One formal byte
difference stays: git prints an `index` line on gitlink entries; bed
omits it, as on every entry (bed does not abbreviate ids dynamically),
and `git apply` accepts both.

**Residual — gitlink↔regular typechange:** a path that was a regular
file and becomes a gitlink (or back) renders its `old mode`/`new mode`
lines only — no content hunk of either shape — the v0.1 typechange
form. git prints the mode lines plus a content/`Subproject commit`
hybrid. The renderer treats a typechange between entry kinds as
shapeless (neither bytes nor a pointer to render); rendering both
sides' shapes in one block is a v0.2+ candidate.

**Repro:** in a history with submodules, `bed diff <cs>` on a changeset
that retargets one prints the `-Subproject commit <old>` /
`+Subproject commit <new>` hunk; a path switching from a regular file
to a gitlink prints only `old mode`/`new mode`.

## 8. Merge is rename-blind (v0.2.md D9)

**What:** the merge walk pairs entries by path only. A rename on one
side is indistinguishable from a delete+add pair, so rename/rename
(both sides rename the same file to different destinations) merges as
two independent add/add pairs — each new path conflicts only when the
file's content also changed on both sides; rename/delete merges as
delete (one side) vs modify (the other, at the old path), a
modify/delete conflict naming the *original* path. Git's
`merge-ort` detects the renames and reports rename/rename or
rename/delete conflicts at both endpoints.

**Repro:**

```sh
git init rr && cd rr && git config user.email a@b.c && git config user.name A
echo x > f && git add f && git commit -qm base
git checkout -qb o && git mv f one && git commit -qm "ours renames f->one"
git checkout -q main && git checkout -qb t && git mv f two && git commit -qm "theirs renames f->two"
git merge-tree --write-tree o t   # git: rename/rename conflict at one, two, f
```

Gardenbed merges this cleanly (`one` and `two` both appear; the two
adds touch different paths), where git refuses.

**Cost:** a rename/rename or rename/delete merge can silently succeed
where git would stop for review; blame stays correct either way —
renames are recorded as `Op::Rename` with `src_path`, and blame follows
`src_path` down the first-parent chain regardless of how the merge was
classified. The content merge itself never mixes up files: it only ever
pairs a path with itself across the three trees.

**Why not fixed:** rename detection at merge time needs a similarity
heuristic over candidate (deleted, added) pairs on each side — the same
machinery git's `-M` uses, deliberately absent from the model so far
(v0.1.md D8 imports renames only because git reports them). The
divergence is documented as the v0.2 boundary: renames must be resolved
by hand (rename on one side + edit on the other merges correctly as
modify at the surviving path).

## 9. Criss-cross merge bases pick a deterministic tie-break, not git's

**What:** when the two merge candidates have multiple equally-good
merge bases (a criss-cross history), git's `merge-base` picks one
implementation-defined candidate; gardenbed's `merge_base` resolves the
tie deterministically — newest committer timestamp first, then
descending id bytes — and the merge then runs against exactly that base.

**Repro:**

```sh
git init cc && cd cc && git config user.email a@b.c && git config user.name A
echo base > f && git add f && git commit -qm base
git checkout -qb a && echo A > f && git commit -qam a
git checkout -qb b main && echo B > f && git commit -qam b
git checkout -q a && git merge -q -m ab b 2>/dev/null || true
git checkout -q b && git merge -q -m ba a 2>/dev/null || true
# `a` and `b` now have two merge bases (the two cross-merges)
git merge-base a b   # git picks one; gardenbed may pick the other
```

**Cost:** a criss-cross merge can differ from git's in *which* hunks
count as "ours changed" vs "theirs changed" around the differing base
region — occasionally flipping a clean merge to a conflict (or the
reverse) relative to git's choice. Both answers are defensible; only
one is ours, and it is stable across runs, replicas and rebuilds.

**Why not fixed:** "recursive merge" (merging the bases first, as
`merge-ort` does) multiplies conflict surfaces and can invent
intermediate trees that exist in neither history; the v0.2 spec (D10)
chose the deterministic single-base rule knowingly. The tie-break
order is part of the merge contract and tested for stability.

## 8. Merge is rename-blind (v0.2.md D9)

**What:** the merge walk pairs entries by path only. A rename on one
side is indistinguishable from a delete+add pair, so rename/rename
(both sides rename the same file to different destinations) merges as
two independent add/add pairs — each new path conflicts only when the
file's content also changed on both sides; rename/delete merges as
delete (one side) vs modify (the other, at the old path), a
modify/delete conflict naming the *original* path. Git's
`merge-ort` detects the renames and reports rename/rename or
rename/delete conflicts at both endpoints.

**Repro:**

```sh
git init rr && cd rr && git config user.email a@b.c && git config user.name A
echo x > f && git add f && git commit -qm base
git checkout -qb o && git mv f one && git commit -qm "ours renames f->one"
git checkout -q main && git checkout -qb t && git mv f two && git commit -qm "theirs renames f->two"
git merge-tree --write-tree o t   # git: rename/rename conflict at one, two, f
```

Gardenbed merges this cleanly (`one` and `two` both appear; the two
adds touch different paths), where git refuses.

**Cost:** a rename/rename or rename/delete merge can silently succeed
where git would stop for review; blame stays correct either way —
renames are recorded as `Op::Rename` with `src_path`, and blame follows
`src_path` down the first-parent chain regardless of how the merge was
classified. The content merge itself never mixes up files: it only ever
pairs a path with itself across the three trees.

**Why not fixed:** rename detection at merge time needs a similarity
heuristic over candidate (deleted, added) pairs on each side — the same
machinery git's `-M` uses, deliberately absent from the model so far
(v0.1.md D8 imports renames only because git reports them). The
divergence is documented as the v0.2 boundary: renames must be resolved
by hand (rename on one side + edit on the other merges correctly as
modify at the surviving path).

## 9. Criss-cross merge bases pick a deterministic tie-break, not git's

**What:** when the two merge candidates have multiple equally-good
merge bases (a criss-cross history), git's `merge-base` picks one
implementation-defined candidate; gardenbed's `merge_base` resolves the
tie deterministically — newest committer timestamp first, then
descending id bytes — and the merge then runs against exactly that base.

**Repro:**

```sh
git init cc && cd cc && git config user.email a@b.c && git config user.name A
echo base > f && git add f && git commit -qm base
git checkout -qb a && echo A > f && git commit -qam a
git checkout -qb b main && echo B > f && git commit -qam b
git checkout -q a && git merge -q -m ab b 2>/dev/null || true
git checkout -q b && git merge -q -m ba a 2>/dev/null || true
# `a` and `b` now have two merge bases (the two cross-merges)
git merge-base a b   # git picks one; gardenbed may pick the other
```

**Cost:** a criss-cross merge can differ from git's in *which* hunks
count as "ours changed" vs "theirs changed" around the differing base
region — occasionally flipping a clean merge to a conflict (or the
reverse) relative to git's choice. Both answers are defensible; only
one is ours, and it is stable across runs, replicas and rebuilds.

**Why not fixed:** "recursive merge" (merging the bases first, as
`merge-ort` does) multiplies conflict surfaces and can invent
intermediate trees that exist in neither history; the v0.2 spec (D10)
chose the deterministic single-base rule knowingly. The tie-break
order is part of the merge contract and tested for stability.

## Explicitly *not* divergences

These shapes differ from git's most literal rendering but produce
byte-identical trees **and identical commit SHAs** — commit objects
encode no diff information, so the change shape below a commit is
invisible to git's identity:

* **Rename vs. delete+add.** The generator commits pure renames (identical
  blob id) as one `Rename`; moved-and-edited files commit as a delete+add
  pair (no similarity heuristic). Import keeps git's `-M` renames; the
  native pack export writes git tree objects, which carry no rename
  information at all — every shape builds the same tree and the same
  commit SHA.
* **Gitlink padding.** A submodule pointer's gardenbed blob id is the
  upstream git SHA-1 left-padded with twelve zero bytes; export strips
  the padding and writes `M 160000 <sha> <path>`. Byte-identical to the
  original entry.
* **Empty identity fields.** A commit with an empty name (` <email>`) or
  empty email (`<>`) is stored and re-exported as-is; `git fsck --strict`
  accepts both, and the identity line is reproduced byte-for-byte.
* **Raw timezone offsets.** Identity timestamps are stored as epoch
  seconds plus the original `±hhmm` offset and are re-emitted raw — never
  normalized to UTC — so dates round-trip exactly.
