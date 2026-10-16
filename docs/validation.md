# Durable publication validation

Native sow and Git import validate changeset shape, referenced blob availability,
and tree application before publishing metadata. Every parent must already be
validated, including parents beyond the first. Tree replay uses the first
parent's verified root directly; mutable derived cache entries cannot establish
validity.

SQLite records this proof in `validated_changesets_v1`, separately from the
`derived` table. Each immutable certificate associates the canonical changeset
ID with its verified root tree ID (an empty payload means an empty tree). The
version names the validation rules, allowing a future stricter validator to use
a new certificate version. Native writes atomically store changeset,
certificate, and derived rows; imported writes add the Git mapping in the same
transaction. The store enforces certified parents and refuses to change an
existing certificate's root. Ordinary `put_changeset`, `ingest_changeset`, and
`derived_put` operations never grant a certificate. The explicit
`put_validated_changeset` API is reserved for trusted validators that have
performed replay and blob validation; it is not a public RPC.

On CAS, harvest, or selection of an import frontier, a certified tip needs one
indexed certificate lookup to establish history validity. A new changeset only
checks its parents' certificates and replays its own delta. Certificates survive
server restarts; the service retains no history-sized validation set. Tree reads
prefer the certified root to a generic derived tree entry, while object reads
still verify content addressing.

Existing databases acquire the table automatically when opened. Uncertified
legacy history undergoes one bottom-up validation walk on its first publication
or incremental re-import. This includes all parents and verifies canonical IDs,
blob existence, and tree application even when generic tree caches claim a
valid result. Successfully checked ancestors are certified as the walk proceeds,
so a failed request or interrupted migration can resume at that durable
frontier. Invalid history stays uncertified and cannot move a ref. This first
migration costs O(uncertified reachable history); subsequent restarts do not
repeat it. Deleting derived caches does not delete validation proofs. This proof
records availability when validated, rather than promising that objects cannot
later be damaged or manually removed from disk.

## Regression and performance evidence

`crates/server/tests/integrity.rs` seeds 10,000 native changesets, reopens the
stores three times, and counts backend operations: publishing the certified tip
loads zero changesets and performs exactly one certificate lookup. New sow reads
fewer than 16 changesets/certificates. Removing certificates to simulate an old
database validates each ancestor once, and another restart again loads zero
changesets for publication. Additional tests reject forged generic tree caches,
malformed non-first-parent history, and canonical bytes under the wrong ID.
Store tests verify certificate immutability, certified-parent enforcement, and
transaction rollback; importer tests verify certificate persistence and legacy
frontier migration.

A local debug-build benchmark with 10,000 empty changesets measured repeated
reopen-and-create requests at roughly 278–282 ms before durable certificates.
After the change, an existing uncertified database took 1.215 s for its one-time
migration and about 4.3 ms on later restarts. A fresh certified database took
about 4.7–11.6 ms per reopened request. Seeding the fresh history took 4.32 s,
compared with 3.96 s before the change. These are synthetic local timings, not a
kernel-corpus throughput claim; the counted restart regression establishes that
history validation work no longer grows with already-certified history.
