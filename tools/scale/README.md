# Scaled deployment: Postgres metadata + S3 objects + N server replicas

This is the horizontal scale-out tier from
[docs/scale-out-design.md](../../docs/scale-out-design.md): every
`gardenbed-server` replica is stateless, metadata lives in Postgres, and
objects live in S3 (MinIO here). There is no leader, no sharding, and no
client change — `bed` still talks to one server address; putting a load
balancer in front of N replicas is a deployment concern.

## Bring-up

```sh
docker compose -f tools/scale/compose.yaml up --build
```

That starts:

| service         | image                | role                                   |
|-----------------|----------------------|----------------------------------------|
| `postgres`      | `postgres:17-alpine` | metadata DB (`gardenbed/gardenbed`)    |
| `minio`         | `pgsty/minio:latest` | S3 endpoint (bucket `gardenbed`)       |
| `createbuckets` | `pgsty/minio:latest` | one-shot bucket creator (`mc mb`)      |
| `server` ×3     | built locally        | `gardenbed-server serve --storage …`   |

`docker compose … ps` then shows three `server` replicas. Each replica
loads `/etc/gardenbed/storage.toml` (bind-mounted from
`tools/scale/storage.toml`) which points at the compose-internal
hostnames `postgres:5432` and `minio:9000`.

Note on images: MinIO is pulled from the `pgsty/minio` community mirror
because `minio/minio` left Docker Hub and quay in Sept 2026. The S3 API
is identical, and the same image ships `mc`, which is what the bucket
creator and the minio healthcheck use.

## Pointing `bed` at a replica

The replicas publish host ports `9418-9420` (the service's internal
port is always 9418; compose assigns one distinct host port per
replica). Any one of them serves the whole logical repository, so pick
a published port and clone from it (the repository name is a
`--repo` flag, `default` if omitted):

```sh
bed clone http://127.0.0.1:9418
```

To import an existing git repository directly against the scaled
backends (this is also the migration path from a single-machine
repository — see the design doc):

```sh
docker compose -f tools/scale/compose.yaml run --rm server \
    import --from /path/to/local/repo.git \
    --storage /etc/gardenbed/storage.toml
```

Scaled imports require a local Git path accessible inside the container
(bind-mount it for the command above); URL imports require local storage mode.

## Scaling the replica count

`deploy.replicas: 3` is the default in `compose.yaml`. Override it
without editing the file:

```sh
docker compose -f tools/scale/compose.yaml up -d --scale server=5
```

or edit `deploy.replicas` and `docker compose up -d` again. The host
port range `9418-9420` covers three replicas; widen it (e.g.
`9418-9430`) when scaling past three so every replica can publish a
port.

## Environment overrides

The services read plain compose environment defaults, so override them
with `docker compose` flags or an `.env` file next to `compose.yaml`:

```sh
POSTGRES_USER=… POSTGRES_PASSWORD=… MINIO_ROOT_USER=… MINIO_ROOT_PASSWORD=… \
    docker compose -f tools/scale/compose.yaml up
```

Credentials inside `storage.toml` must match the Postgres and MinIO
credentials — edit both together. For the test suite's env-var contract
(used by `crates/server/tests/scale.rs` and the store parity tests),
see the "Live-service tests" section of the design doc:

```sh
export GARDENBED_TEST_PG_URL=postgres://gardenbed:gardenbed@127.0.0.1:55432/gardenbed
export GARDENBED_TEST_S3_ENDPOINT=http://127.0.0.1:55900
export GARDENBED_TEST_S3_BUCKET=gardenbed
export GARDENBED_TEST_S3_ACCESS_KEY=gardenbed
export GARDENBED_TEST_S3_SECRET_KEY=gardenbed-secret
export GARDENBED_TEST_S3_REGION=us-east-1
export GARDENBED_TEST_S3_PATH_STYLE=1
```

(The compose file publishes Postgres on host port 55432 and MinIO on
55900, so the same running compose stack serves those URLs.)

## Build context

`compose.yaml` resolves `build.context: ../..` relative to the compose file,
so it builds the repository checkout containing `tools/scale/`. Run the
commands above from that checkout's root. `demo/run-scaled.sh` likewise
resolves the compose file from its own location.

The original scale-out lane used `/home/user/code/gardenbed-scale` during
landing. That path and the `feat/scale-out` mirror instructions were staging
notes, not deployment prerequisites. For the current dogfood deployment and
Git/bed synchronization procedure, use [OPERATIONS.md](../../docs/OPERATIONS.md).

## Teardown

```sh
docker compose -f tools/scale/compose.yaml down -v   # -v drops pg/minio data
```
