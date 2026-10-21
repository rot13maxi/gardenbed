# Daemon cache isolation (`todo/daemon-isolation`)

The daemon's shared object cache is partitioned by **(upstream origin,
repository)**. This note records the leak that motivated the change, the
layout, the lifecycle contract the partition has (and its one deliberate
edge), and the tests that pin it all.

## The leak

Before the partition, the daemon's `ProxyService` kept one
`FsObjectStore` for its whole cache root, keyed by content id only.
`authorize_hit` — the per-hit upstream probe — consults `ListRefs(repo)`,
which proves the *caller* may read `repo`, but says nothing about which
objects `repo` holds. So:

1. repo `alpha` holds object X; repo `beta` (same server) does not.
2. a wildcard token warms the daemon's cache with alpha's objects
   (a tokened clone through the daemon).
3. a beta-scoped token asks `GetBlob(beta, X)` / `GetTree(beta, X)`:
   `ListRefs(beta)` authorizes (the token is valid for beta), the origin
   read is NotFound — but the daemon answered from the cache: **beta's
   client received alpha's bytes**. A cross-repository object leak: the
   bytes were fetched under an alpha-authorized credential and served
   under a beta-only one.

Regression test: `daemon_cache_does_not_leak_objects_across_repos`
(`crates/bed/tests/auth.rs`). It was written first and proven failing on
the unpartitioned daemon (content-addressed caches dedup *bytes*, not
*membership*).

## The layout

One `FsObjectStore` per scope:

```text
<cache_root>/scopes/<sha256(upstream endpoint ⊕ NUL ⊕ repo name)>/objects/<xx>/<62>
```

* **A digest, not the raw repo string**: repository names are protocol
  strings, never filesystem path components — a hostile or merely
  unusual name cannot escape the cache root or collide with sibling
  directories.
* **The endpoint rides in the key**: a `--cache` root reused for a
  different `--upstream` starts cold. That upstream's membership is
  unproven; serving the old upstream's objects would be the same leak
  with a different axis. The regional cache tier (`crates/cache`) uses
  the identical convention, so the two tiers stay comparable and a
  shared root's semantics do not depend on which tier fronts it.
* **Membership, per scope**: an object lands in a scope only after that
  scope's own origin fetch succeeded. A repo that lacks an object is
  answered with the origin's own NotFound, never another scope's bytes.
* **Authorization is unchanged**: every cache hit still pays one
  `ListRefs` consult (the origin decides who may read). The scope fixes
  membership, not authorization; both are needed.
* **Singleflight is scoped too**: concurrent `GetTree` requests collapse
  per `(scope, id)` — the same tree id in two repos must not collapse
  into the wrong scope's fetch.
* **Persistence is scoped**: killing the daemon and restarting over the
  same cache root (same endpoint) re-opens the same scopes and serves
  warm objects with zero new origin fetches.

## Legacy cache roots

Pre-partition cache roots hold bytes directly under
`<cache_root>/objects/` with no membership proof. They are **never
read**: serving them would require independently establishing that the
(unknown) fetching repo held each object, which no record exists for. A
miss plus one origin fetch re-warms the object into its proper scope.
The old bytes simply age out as unreferenced; nothing migrates.

## The lifecycle contract (including its one edge)

Supported operations (`gardenbed-server move`, delete + re-ingest under
a reused name) can put a *different history* under the same
`(endpoint, repo)` identity. The partition cannot distinguish an
incarnation from its predecessor — by design, because the cache is a
performance tier, not a consistency oracle:

* Warm scope entries from the prior incarnation stay warm and serve
  objects the *replacement* repository lacks. The bytes are authentic
  (content-addressed, hash-verified) and only ever visible to callers
  the origin authorizes for that repo name. This matches what a working
  copy's own local cache does after the same replacement: `bed clone`'d
  history remains readable from `.bed/objects` regardless of what the
  server later holds.
* Pinned by `daemon_scoped_cache_serves_replacement_repo_under_same_name`
  (`crates/bed/tests/auth.rs`): direct origin answer NotFound, warm
  daemon answer Ok, for the same `(repo, id)`.

An incarnation protocol (repo-epoch ids, scope invalidation on
replacement) was **not** added: no concrete consumer-visible failure
mode exists beyond the above — which is identical to the working-copy
cache's long-standing behavior — and the daemon must not invent a second
truth about repository identity that the server does not publish. If
the server ever grows an incarnation/epoch signal, the daemon should key
scopes on it; that is the coordination point with the regional cache
tier, which has the same edge.

## Tests

`crates/bed/tests/auth.rs`:

* `daemon_cache_does_not_leak_objects_across_repos` — the leak: warm
  alpha objects, beta-scoped token, blob + tree must answer NotFound;
  alpha control still serves. (RED-first: proven failing pre-fix.)
* `daemon_cache_hit_still_requires_token` / `daemon_cache_hit_respects_repo_scope`
  — per-hit authorization consult still gates warm hits (pre-existing,
  kept green).
* `daemon_scoped_cache_persists_across_restart` — daemon restart over
  the same cache root and endpoint: zero new origin fetches.
* `daemon_scoped_cache_isolates_a_reused_cache_root_from_other_upstreams`
  — same repo name, different upstream, reused cache root: NotFound,
  not the other origin's bytes; that origin's own content still serves.
* `daemon_scoped_cache_serves_replacement_repo_under_same_name` — the
  replacement lifecycle contract above.

`crates/bed/tests/daemon.rs` (pre-existing, kept green):

* `cross_working_copy_dedup_hits_the_shared_cache` — same-repo warm
  dedup still produces zero new server fetches.
* `two_repositories_through_one_daemon` — two repos through one daemon
  both clone and materialize correctly.
