# Daemon cache isolation (`todo/daemon-isolation`)

The daemon's shared object cache is partitioned by **(upstream origin,
repository, incarnation)**. This note records the leaks that motivated
the change, the layout, the identity contract, and the tests that pin
it all.

## The leaks

**Cross-repository.** Before the partition, the daemon's `ProxyService`
kept one `FsObjectStore` for its whole cache root, keyed by content id
only. `authorize_hit` — the per-hit upstream probe — consults
`ListRefs(repo)`, which proves the *caller* may read `repo`, but says
nothing about which objects `repo` holds. So a warm object fetched
under repo `alpha`'s authorization was served for `GetBlob(beta, X)` /
`GetTree(beta, X)` while the origin answered NotFound: **beta's client
received alpha's bytes**, fetched under another principal's credential.

**Same-name replacement (cross-incarnation).** The (origin, repo)
partition alone cannot distinguish a repository *replaced* under the
same name (delete + re-init: a new metadata database) from its
predecessor. A caller authorized only for the replacement repository
could read the predecessor's warm objects through the daemon — bytes
the replacement repository has never possessed and the origin says are
gone. Unlike a working copy's own local cache (where the possessor
fetched the bytes itself), the daemon's caller never possessed them:
serving them is a cross-principal leak. Both leaks are fixed by keying
cache membership on the **server-published repository identity**.

Regression tests: `daemon_cache_does_not_leak_objects_across_repos`
(cross-repository) and
`daemon_scoped_cache_denies_replacement_repo_the_prior_incarnation_objects`
(cross-incarnation), both in `crates/bed/tests/auth.rs`. Each was
proven failing against the unfixed daemon before its fix landed.

## Repository incarnation identity

`MetadataStore::repo_incarnation()` — implemented by the SQLite,
Postgres, and Memory metadata backends — answers 16 random opaque
bytes from the operating system random generator, minted **atomically
on first use** and stored with the repository metadata:

* **SQLite**: a `repo_incarnation` singleton table; read-then-insert
  in one `BEGIN IMMEDIATE` transaction (the write lock serializes
  racing first callers, including other processes on the same
  database).
* **Postgres**: the same table, `INSERT ... ON CONFLICT DO NOTHING`
  followed by `SELECT` in one transaction — racing replicas converge on
  the single stored row. The startup advisory-lock DDL transaction and
  the strict schema table set are preserved (`repo_incarnation` joined
  the expected table union).
* **Memory**: a lazy field under the store's write lock, stable for that
  store instance. A fresh in-memory store gets a new identity; it has no
  durable state to reopen.

The contract:

* **Same identity on durable-store restart**: SQLite and Postgres keep
  the value in the metadata database; another handle, process, or replica
  reads the same bytes.
* **New identity on new database**: a re-initialized or replaced
  repository (new metadata) mints a fresh identity — never a
  predecessor's, even when both histories are empty or identical
  (randomness, not derivation, is what makes this true).
* **Existing repositories gain identity without rewriting**: the
  mint is a lazy one-row insert; no existing ids change.
* **Identity is not derivable from content or ref topology** — those
  cannot distinguish replacement from coincidence; only the persisted
  random row can.

The identity is exposed through the additive `RepoInfo` RPC
(read-role authorized, so a successful answer doubles as proof the
caller may read the repository). Old servers predate the RPC and
answer `UNIMPLEMENTED`.

## The layout

One `FsObjectStore` per scope:

```text
<cache_root>/scopes/<sha256(upstream endpoint ⊕ NUL ⊕ repo name ⊕ NUL ⊕ incarnation)>/objects/<xx>/<62>
```

* **A digest, not the raw repo string**: repository names are protocol
  strings, never filesystem path components.
* **The endpoint in the key**: a `--cache` root reused for a different
  `--upstream` starts cold (that upstream's membership is unproven).
* **The incarnation in the key**: a replaced repository resolves a
  fresh identity on its next request, so its scope directory is cold —
  the predecessor's warm objects are unreachable by name. No deletion
  of the old scope directory is performed (concurrent requests may
  still legitimately read the old incarnation's scope; a new namespace
  simply coexists).
* **Membership, per scope**: an object lands in a scope only after
  that scope's own origin fetch succeeded.
* **Authorization is unchanged**: every cache hit still pays the
  `ListRefs` consult (`authorize_hit`), and the per-request scope
  resolution (`RepoInfo`) is itself read-role authorized with the
  caller's credentials.
* **Singleflight is scoped** per (scope directory, id).
* **Bounded maps**: the scopes and tree-lock maps evict arbitrary
  entries past fixed caps; eviction only costs dedup, never
  correctness (the stores are persistent and reopen lazily).

## Old servers (no identity RPC)

An upstream that answers `RepoInfo` with `UNIMPLEMENTED` cannot prove
any membership. The daemon sticks a per-process flag and **bypasses
the cache entirely** for that upstream: every `GetTree`/`GetBlob`
reads the origin, and nothing is written through. Legacy pre-scope
cache bytes (`<cache_root>/objects/`) are likewise never read. A miss
plus one origin fetch is always the safe re-warm; unproven bytes are
never served.

## The lifecycle contract

* **Restart**: same database ⇒ same identity ⇒ same scope ⇒ warm
  objects serve with zero new origin fetches.
* **Move** (`gardenbed-server move`): same database (moved directory)
  ⇒ same identity. The namespace also includes the repository name, so
  the new name starts cold; identity continuity does not reuse the old
  name's cache scope.
* **Replacement** (delete + re-init, new database): fresh identity ⇒
  cold scope ⇒ the replacement's callers get the replacement origin's
  own answers, never the predecessor's cached bytes.

## Tests

`crates/bed/tests/auth.rs`:

* `daemon_cache_does_not_leak_objects_across_repos` — cross-repo
  leak: warm alpha objects, beta-scoped token, blob + tree NotFound;
  alpha control still serves. (RED-first pre-fix.)
* `daemon_cache_hit_still_requires_token` /
  `daemon_cache_hit_respects_repo_scope` — per-hit authorization
  consult still gates warm hits (pre-existing, kept green).
* `daemon_scoped_cache_persists_across_restart` — daemon restart over
  the same cache root and endpoint: zero new origin fetches.
* `daemon_scoped_cache_isolates_a_reused_cache_root_from_other_upstreams`
  — same repo name, different upstream, reused cache root: NotFound,
  not the other origin's bytes; that origin's own content still
  serves.
* `daemon_scoped_cache_denies_replacement_repo_the_prior_incarnation_objects`
  — the replacement leak: warm first-incarnation objects fetched by
  `first-token`; after delete + re-init under the same name, a
  `replacement-token` caller gets NotFound for the predecessor's blob
  AND tree while the daemon stays alive across replacement; the
  replacement's own content serves. (RED-first pre-fix: the
  (origin, repo)-only partition served the warm bytes.)

* `daemon_restarted_over_replaced_repo_still_denies_prior_incarnation`
  checks the persistent-cache variant after restarting the daemon.

`crates/bed/tests/daemon.rs` (pre-existing, kept green):

* `cross_working_copy_dedup_hits_the_shared_cache` — same-repo warm
  dedup still produces zero new server fetches.
* `two_repositories_through_one_daemon` — two repos through one daemon
  both clone and materialize correctly.

`crates/store/tests/postgres_schema.rs` (live-PG fixture, gated on
`GARDENBED_TEST_PG_URL`):

* `repo_incarnation_is_atomic_and_stable_across_handles` — 16 racing
  independent handles mint one identity; a later reopen reads the same
  bytes; a fresh schema gets a different identity.
