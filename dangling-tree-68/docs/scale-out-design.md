# Horizontal scale-out design — off-box metadata DB + off-box object storage

Status: implemented (v0.2 track). Everything below builds on the two-trait
storage seam (`ObjectStore` / `MetadataStore`,
`crates/store/src/traits.rs`) that v0.1 already put behind every surface
(server, mcp, git import, bench).

## Problem

`gardenbed-server` binds one process to one machine: `Repo::open(root)`
hardwires `FsObjectStore` + `SqliteMetadataStore` on local paths. Two
store assumptions break horizontal scale-out:

1. `FsObjectStore` writes are tmp-file + fsync + rename on local disk —
   there is no object store where many server replicas can write.
2. `SqliteMetadataStore` serializes all access on one process-local
   `Mutex<Connection>`; CAS and multi-row transactions are single-node
   by construction.

The RPC surface itself is already server-authoritative and stateless
(v0.1.md §5: every request is `repo` + content-addressed ids, refs are
CAS rows, want-list fetch) — the protocol needs no changes to scale the
tier. Only the storage seams do.

## Decisions

- **D1 — Postgres for the metadata, S3 for objects.** Matches the
  project plan of record ("blobs in the object store; both behind traits
  (fs/sqlite first, s3/postgres later)", DESIGN.md) and the upstream
  Scion architecture it follows. One new backend per trait.
- **D2 — Reuse the trait contracts unchanged.** The scale-out backends
  implement `MetadataStore` and `ObjectStore` as-is: `cas_ref` stays a
  boolean CAS, `put_validated_changeset` / `ingest_changeset` stay the
  atomic multi-row writes, `changeset_ids_in_id_range` stays the
  prefix-resolution seek. Backend-independent tests apply to the new
  backends by construction. Verification is backend-parity tests, not
- **D3 — Server picks backends from one `--storage` flag.**
  `gardenbed-server serve --storage <config>` plus the `GARDENBED_STORAGE`
  env-var fallback (a path to the same TOML file) — one config surface
  for serve, import, and export. `--root` remains the local mode and the
  default; nothing existing changes. Resolution order: `--storage` flag
  → `GARDENBED_STORAGE` → `--root` local mode. (Individual
  `GARDENBED_PG_URL` / `GARDENBED_S3_*` overrides were dropped during
  implementation — the config file already carries every backend
  parameter, so per-backend env vars were a second surface with no
  caller; the test-suite env vars `GARDENBED_TEST_*` are a separate,
  test-only contract.)
- **D4 — Deployment packaging via docker compose, tests against the
  same services.** `tools/scale/compose.yaml` runs postgres + minio (the
  S3 endpoint) + N server replicas; the live-service test suites point
  at the compose stack's published ports (Postgres 55432, MinIO 55900),
  so tests exercise the same backing services the deployment ships.
  MinIO is pulled from the `pgsty/minio` community image because
  minio/minio left Docker Hub and quay (Sept 2026); the S3 API is
  identical, and the same image ships `mc` (bucket creator,
  healthcheck).
- **D5 — Postgres schema mirrors the SQLite schema one-to-one.** Same
  five tables, same columns, BLOB→`bytea`, PK/UNIQUE preserved. Keeping
  the two in lockstep makes parity bugs visible in backend-parity
  tests instead of in production.
- **D6 — CAS in Postgres is one `UPDATE ... WHERE` — no advisory locks.**
  An existing ref: `UPDATE refs SET cs = $new WHERE name = $name AND
  cs = $expected` — rowcount 1 ⇔ `RefCas::Won`. A lost race re-reads the
  current value for the Conflict report. Creating a ref
  (`expected = None`) is `INSERT ... ON CONFLICT DO NOTHING` + re-read
  on conflict. Single-statement atomic, no lock manager.
- **D7 — The import/export paths pick backends the same way (D3).**
  v0.1 code already funnels them through explicit store construction;
  the change is one call site each in `crates/server/src/main.rs`.

## Architecture

```text
bed ──gRPC── gardenbed-server replica 1 ─┐
              gardenbed-server replica 2 ─┼── Postgres (metadata)
              gardenbed-server replica N ─┘         │
                                              S3 / MinIO (objects)
```

Every replica is identical: stateless gRPC front, same RPCs, no leader
election, no sharding. Ref writes are CAS rows in Postgres (exactly one
replica wins a race); object writes are immutable content-addressed
writes to S3 (re-racing is the same write). Clients see one logical
repository, consistent because all state lives off-box.

### Storage configuration

```toml
# gardenbed-storage.toml — same file for serve/import/export
[metadata]
backend = "postgres"
url = "postgres://user:pass@host:5432/gardenbed"
# backend = "sqlite" points at an existing local repository instead:
#   root = "/path/to/repo"   (metadata at <root>/metadata.sqlite3)

[objects]
backend = "s3"
endpoint = "http://minio:9000"
bucket = "gardenbed"
region = "us-east-1"
access_key = "..."
secret_key = "..."
path_style = true

# backend = "fs" likewise:
#   root = "/path/to/repo"   (objects at <root>/objects)
```

```bash
# Local (unchanged, the default)
gardenbed-server --root ./repo serve

# Scaled: N replicas over Postgres + S3
gardenbed-server --storage config.toml serve
GARDENBED_STORAGE=/etc/gardenbed/storage.toml gardenbed-server serve
```

`--storage` is additive; `--root` keeps working exactly as before.
Migration: existing single-machine repositories move by re-import — the
git import path already writes every changeset + git mapping atomically
against whatever backends it is given, so
`gardenbed-server import --from <repo> --storage config.toml` is the
migration path, same as any repository seeding (URL `--from` requires
local mode — clone the repository locally and import the path).

## Backends

### Postgres metadata store

`crates/store/src/postgres.rs` — `PostgresMetadataStore` implements
`MetadataStore`.

- **Connection pooling:** `deadpool-postgres`, pool sized to the CPU
  count; TLS is not negotiated (`NoTls`) — Postgres is expected on a
  trusted network or a local compose network. All trait methods are
  sync, so the store owns a private single-threaded tokio runtime in a
  dedicated driver thread (`crates/store/src/driver.rs`); every trait
  call *spawns* its future onto that runtime and parks the caller on a
  std channel. No tokio blocking primitive runs on the calling thread,
  so calls are panic-free from inside any tokio context (a tonic
  handler under `#[tokio::main]`, say).
- **Schema (D5):** same five tables (`changesets`,
  `validated_changesets_v1`, `refs`, `git_map`, `derived`); `id`/`cs`/
  `root_tree` `bytea`, `git_sha` `text`, `canonical`/`bytes` `bytea`.
  `refs.cs` carries no FK to changesets — SQLite has none either (a CAS
  may point a ref at an id before its changeset row lands).
- **CAS (D6):** see the decision. The guarded statement and the
  Conflict re-read run in ONE transaction, so the re-read cannot observe
  a value newer than the one that blocked the write; a vanished ref on
  the existing path is `NotFound`, not a Conflict.
- **`put_changeset` / `put_validated_changeset` / `ingest_changeset`:**
  one transaction (deadpool client object transaction) per call, all
  rows in it — the Postgres twin of SQLite's `in_tx`. Changeset inserts
  are `INSERT ... ON CONFLICT (id) DO NOTHING` + byte-compare on the
  existing row: same bytes is the idempotent path, different bytes is
  `ImmutabilityViolation`.
- **`changeset_ids_in_id_range`:** `SELECT id FROM changesets WHERE id
  >= $lo` (+ `AND id < $hi` when a bound is given) `ORDER BY id` — same
  PK range seek as SQLite, as two statement shapes (Postgres binds
  optional `bytea` bounds more cleanly that way than a unified
  `$hi IS NULL` plan).
- **Schema versioning:** `CREATE TABLE IF NOT EXISTS` on every open,
  same as SQLite. First schema cut; no migrations yet.

### S3 object store

`crates/store/src/s3.rs` — `S3ObjectStore` implements `ObjectStore`.

- **Client:** the `object_store` crate (vendor-neutral URL config,
  static creds, custom endpoint, path-style via
  `with_virtual_hosted_style_request(!path_style)`, conditional put
  `S3ConditionalPut::ETagMatch`). Same driver-thread pattern for sync
  trait calls. Opening HEADs a probe key to fail fast on unreachable
  endpoints or missing buckets.
- **Key layout:** identical 2-level sharding: S3 key `<2hex>/<62hex>`.
  Same envelope (GBLO1 magic + raw_len + one zstd frame) for blobs and
  bare canonical bytes for trees — byte-identical payloads to what
  FsObjectStore writes, so a repository can move between fs and S3.
- **`put` / immutability:** conditional put (`If-None-Match: *`,
  `PutMode::Create` — supported by both S3 and MinIO); on precondition
  failure, read back and verify — same content is an idempotent no-op,
  different content is `ImmutabilityViolation`. A structurally corrupt
  blob envelope heals with the caller's verified payload (unconditional
  put), exactly like the fs backend: the caller holds content that
  hashes to the id, so overwriting restores the invariant.
- **`has`** → HEAD; **`get_blob`/`get_tree`** → GET, then the same
  envelope + length + SHA-256 verification against the addressed id as
  the fs backend: a torn or tampered object is never handed back as
  correct bytes.
- **Durability:** an acknowledged S3 put is durable by S3 semantics;
  `GARDENBED_FSYNC` stays fs-only.
- **Races:** two replicas putting the same id with the same bytes both
  succeed (verified no-op on the precondition loser); different bytes
  at one id is the immutability violation and never lands.

### Repo wiring

`Repo::open_stores(objects: Arc<dyn ObjectStore>, metadata:
Arc<dyn MetadataStore>)` — the existing `Repo::new`, now first-class
(explicit stores are a supported way to open, not a test hook).
`StorageConfig::from_path` + `.open()` → `(Arc<dyn ObjectStore>,
Arc<dyn MetadataStore>)`. `serve`, `import`, and `export` route through
it when `--storage` is given; `mcp` stays local-mode (`--root`) for now.
Every existing caller of `Repo::open(root)` (tests, benches, mcp, demo)
keeps working unchanged. One wrinkle: URL imports (`--from <url>`)
clone a mirror under `<root>/git-mirror.git`, so they require local
mode — in scaled mode, clone the repository locally first and import
the path.

## Deployment

`tools/scale/compose.yaml`:

```yaml
services:
  postgres:
    image: postgres:17-alpine
    environment: [POSTGRES_USER=gardenbed, POSTGRES_PASSWORD=gardenbed,
                  POSTGRES_DB=gardenbed]
    healthcheck: pg_isready
  minio:
    image: pgsty/minio:latest   # minio/minio left Docker Hub + quay (2026-09)
    command: server /data --console-address ":9001"
    environment: [MINIO_ROOT_USER=gardenbed, MINIO_ROOT_PASSWORD=gardenbed-secret]
  createbuckets:
    image: pgsty/minio:latest   # the image ships mc; one-shot bucket creator
    depends_on: [minio]
  server:
    build: server image from workspace
    depends_on: [postgres, minio]
    command: serve --storage /etc/gardenbed/storage.toml
    deploy: replicas: 3
```

`tools/scale/README.md` documents bring-up, env-var overrides, and the
replica-count knob.

## Verification

1. **Backend parity (store crate):** `tests/parity.rs` runs the full
   backend-independent matrix (every test that runs against
   `MemoryStore`/`SqliteMetadataStore` today) against Postgres +
   S3-backed stores, gated on live services (`GARDENBED_TEST_PG_URL`,
   `GARDENBED_TEST_S3_*` env vars; skip when unset — no developer
   without Docker pays for it).
2. **Replica correctness (server crate):** `tests/scale.rs` points at
   externally provided services (the compose stack, or any
   Postgres+S3), gated on `GARDENBED_SCALE_TEST=1` plus the same
   `GARDENBED_TEST_*` env vars as parity. It exercises both the
   in-process seam (read-your-writes across two `Repo` handles on the
   same backing stores, single-winner concurrent CAS on one ref,
   idempotent identical creates from two handles, object fetch through
   the handle that did not write it) and a two-process gRPC pass
   (create via replica 1, publish via replica 2, fetch back through
   replica 1, cross-process CAS race).
3. **Smoke:** `demo/run-scaled.sh` runs a corpus import + client flow
   against the compose services, with `bed` split across two replicas
   to prove cross-replica visibility.
4. **Regression:** the full existing suite (sqlite/fs backends) must
   stay green — the local mode is not deprecated, it is the default.

## Out of scope

- Multi-repo sharding inside one server process (v0.2's multi-repo
  track; the `repo` field is already on the wire).
- Delta compaction / storage economics (existing open question).
- Postgres connection routing per-repo (one database, one pool).
- Client-side changes: `bed` still talks to one server address; a
  load balancer in front of N replicas is a deployment concern, not a
  client one.
- Cross-region replication (a S3/asynchronous Postgres concern; one
  region assumed).
