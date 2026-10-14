# Divergences from git — v0.1

Definition of done §10.2 requires **zero silent divergences** from git on
covered semantics: every place the bridge differs from git must be
documented here with a repro. Silent disagreement is a release blocker.

Each entry states what happens, why, what it costs, and how to reproduce
it. Anything *not* on this list must round-trip exactly: unsigned git
history imports, exports, and re-imports with byte-identical commit SHAs
(verified in `crates/rootstock/tests/export.rs`).

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
`crates/rootstock/tests/export.rs`.

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

---

## Explicitly *not* divergences

These shapes differ from git's most literal rendering but produce
byte-identical trees **and identical commit SHAs** — commit objects
encode no diff information, so the change shape below a commit is
invisible to git's identity:

* **Rename vs. delete+add.** The generator sows pure renames (identical
  blob id) as one `Rename`; moved-and-edited files sow as a delete+add
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
