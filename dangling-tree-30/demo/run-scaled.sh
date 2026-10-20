#!/usr/bin/env bash
# demo/run-scaled.sh — the scale-out demo (docs/scale-out-design.md).
#
# Boots the compose services from tools/scale/compose.yaml (or reuses
# already-running ones), imports a small git repository against the
# Postgres + S3 backends, then starts TWO local gardenbed-server
# replicas on the same storage config and proves cross-replica
# visibility end to end:
#
#   1. up      — postgres + minio (compose), bucket created;
#   2. import  — a small git history via `gardenbed-server import
#                --storage` (the same path run-v0.1.sh exercises, on a
#                small fixture instead of the 50k-commit corpus);
#   3. serve   — two replicas, ephemeral ports, one storage config;
#   4. clone + sparse add + commit + publish through replica 1;
#   5. fetch + log through replica 2 — the commit made through
#      replica 1 must be visible and byte-identical there;
#   6. down    — compose stack down (volumes kept unless --keep-stack
#                asks to leave it up).
#
# Usage:
#   demo/run-scaled.sh [--workdir DIR] [--keep-work] [--keep-stack]
#
# Exits non-zero on the first failed step. Requires docker (with the
# compose plugin), git, cargo, and network access to crates.io on first
# build. Re-runs are safe: a fresh compose project name is used per
# workdir, and the compose stack is torn down on exit unless asked to
# stay.

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
COMPOSE_FILE="$REPO_ROOT/tools/scale/compose.yaml"
COMPOSE="docker compose -f $COMPOSE_FILE"

# Ports the compose stack publishes on the host (compose.yaml).
PG_PORT=55432
S3_PORT=55900
# Compose-internal credentials (compose.yaml / storage.toml).
PG_URL="postgres://gardenbed:gardenbed@127.0.0.1:$PG_PORT/gardenbed"
S3_ENDPOINT="http://127.0.0.1:$S3_PORT"
S3_ACCESS_KEY=gardenbed
S3_SECRET_KEY=gardenbed-secret

KEEP_WORK=0
KEEP_STACK=0
WORKDIR=""

usage() {
    sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --workdir) WORKDIR=$2; shift 2 ;;
        --keep-work) KEEP_WORK=1; shift ;;
        --keep-stack) KEEP_STACK=1; shift ;;
        -h|--help) usage ;;
        *) echo "unknown flag: $1" >&2; usage ;;
    esac
done

for tool in git cargo docker; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "missing required tool: $tool" >&2
        exit 1
    }
done
docker compose version >/dev/null 2>&1 || {
    echo "docker compose plugin is required" >&2
    exit 1
}

ok()   { printf '  ✓ %s\n' "$*"; }
bad()  { printf '  ✗ %s\n' "$*" >&2; }

# Workdir: tmpfs when available (same choice as run-v0.1.sh).
if [[ -z $WORKDIR ]]; then
    if [[ -d /dev/shm && -w /dev/shm ]]; then
        WORKDIR=$(mktemp -d /dev/shm/gardenbed-scaled.XXXXXX)
    else
        WORKDIR=$(mktemp -d /tmp/gardenbed-scaled.XXXXXX)
    fi
fi
mkdir -p "$WORKDIR"
W=$WORKDIR

SERVERS=()
cleanup() {
    local pid
    for pid in "${SERVERS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    if [[ $KEEP_STACK -eq 0 ]]; then
        $COMPOSE down --remove-orphans >/dev/null 2>&1 || true
    fi
    if [[ $KEEP_WORK -eq 0 ]]; then
        rm -rf "$W"
    fi
}
trap cleanup EXIT

# ---------------------------------------------------------------- 1. up

# Reuse a running stack when one is already healthy on the published
# ports; otherwise bring the compose stack up.
stack_up() {
    if $COMPOSE ps --status running 2>/dev/null | grep -q postgres \
        && $COMPOSE ps --status running 2>/dev/null | grep -q minio; then
        ok "compose stack already running (reused)"
        return 0
    fi
    # `up --wait` names only the long-running services: a one-shot
    # service that exits 0 makes `--wait` return 1 (docker/compose
    # #10596), even though the bucket-creator ran to completion.
    # createbuckets is started by postgres/minio's depends_on ordering
    # and is verified below by the bucket's existence.
    if ! $COMPOSE up -d --wait postgres minio >"$W/compose.log" 2>&1; then
        bad "compose up failed (see $W/compose.log — kept with --keep-work)"
        return 1
    fi
    ok "postgres + minio up (ports $PG_PORT / $S3_PORT)"
}

stack_up

# The bucket-creator is a one-shot sidecar (it may not have run yet on a
# fresh stack): ensure the demo bucket exists, then prove it is readable
# with a *signed* check. MinIO answers plain bucket-root requests with
# 403 AccessDenied even when the bucket exists (anonymous S3 has no list
# permission), so the probe must authenticate like a real client —
# `mc stat` with the same endpoint/credentials the storage config uses.
ensure_bucket() {
    if ! docker compose -f "$COMPOSE_FILE" run --rm createbuckets \
        >"$W/bucket.log" 2>&1; then
        bad "bucket creation failed (see $W/bucket.log — kept with --keep-work)"
        return 1
    fi
    local probe
    probe=$(docker compose -f "$COMPOSE_FILE" run --rm -T \
        --entrypoint sh minio -c \
        "mc alias set probe http://minio:9000 $S3_ACCESS_KEY \
         $S3_SECRET_KEY >/dev/null && mc stat probe/gardenbed" \
        2>/dev/null | grep -c . || true)
    if [[ "$probe" -lt 1 ]]; then
        bad "bucket gardenbed not reachable (mc stat probe failed)"
        return 1
    fi
    ok "bucket gardenbed ready"
}
ensure_bucket

# Host-side storage config: same backends as tools/scale/storage.toml,
# but 127.0.0.1 endpoints (the compose-internal hostnames only resolve
# inside the compose network; the demo's replicas run on the host).
STORAGE_TOML=$W/storage.toml
cat >"$STORAGE_TOML" <<EOF
[metadata]
backend = "postgres"
url = "$PG_URL"

[objects]
backend = "s3"
endpoint = "$S3_ENDPOINT"
bucket = "gardenbed"
region = "us-east-1"
access_key = "$S3_ACCESS_KEY"
secret_key = "$S3_SECRET_KEY"
path_style = true
EOF

# ------------------------------------------------------------- 2. build

if ! cargo build --release -q -p gardenbed-server -p gardenbed \
        >>"$W/build.log" 2>&1; then
    bad "cargo build --release failed (see $W/build.log)"
    exit 1
fi
SERVER_BIN=${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release/gardenbed-server
BED_BIN=${CARGO_TARGET_DIR:-$REPO_ROOT/target}/release/bed
ok "release binaries built"

# ------------------------------------------------------------- 3. fixture + import

# A small deterministic git history — the same import path run-v0.1.sh
# exercises (gardenbed-server import --from <path>), on a fixture small
# enough to keep the demo quick.
FIXTURE=$W/fixture.git
git init -q --bare "$FIXTURE"
git init -q "$W/fixture-work"
(
    cd "$W/fixture-work"
    git config user.name "Scaled Demo"
    git config user.email "scaled-demo@example.com"
    mkdir -p src docs
    printf 'fn main() { println!("scaled"); }\n' >src/main.rs
    printf '# demo\n\nfirst commit\n' >docs/README.md
    git add -A && git commit -qm "first"
    printf 'fn main() { println!("scaled, twice"); }\n' >src/main.rs
    git add -A && git commit -qm "second"
    git push -q "$FIXTURE" HEAD:refs/heads/main
)

if ! "$SERVER_BIN" import --from "$FIXTURE" --storage "$STORAGE_TOML" \
        >"$W/import.log" 2>&1; then
    bad "import --storage failed: $(tail -2 "$W/import.log")"
    exit 1
fi
ok "imported $(git -C "$FIXTURE" rev-list --count main) commits into Postgres + S3"

# ------------------------------------------------------------- 4. two replicas

start_replica() { # <name>
    local name="$1" log="$W/$1.log" err="$W/$1.err"
    "$SERVER_BIN" serve --addr 127.0.0.1:0 --storage "$STORAGE_TOML" \
        >"$log" 2>"$err" &
    local pid=$!
    SERVERS+=("$pid")
    local addr=""
    local _
    for _ in $(seq 1 300); do
        addr=$(head -1 "$log" 2>/dev/null || true)
        [[ $addr == "listening on "* ]] && break
        kill -0 "$pid" 2>/dev/null || {
            bad "$name died before announcing: $(tail -2 "$err")"
            return 1
        }
        sleep 0.1
    done
    if [[ $addr != "listening on "* ]]; then
        bad "$name never announced a listening address"
        return 1
    fi
    eval "REPLICA_${name^^}_URL=http://${addr#listening on }"
    ok "$name at $(eval echo "\$REPLICA_${name^^}_URL")"
}

start_replica one || exit 1
start_replica two || exit 1

# ------------------------------------------------------------- 5. cross-replica flow

# Clone through replica 1 (read path across the S3/Postgres seam).
GARDEN=$W/garden
mkdir -p "$GARDEN"
out=$(cd "$GARDEN" && "$BED_BIN" clone "$REPLICA_ONE_URL" 2>&1) || {
    bad "bed clone via replica one failed: $out"
    exit 1
}
ok "clone via replica one: $(printf '%s\n' "$out" | head -1)"

out=$(cd "$GARDEN" && "$BED_BIN" sparse add src docs 2>&1) || {
    bad "bed sparse add failed: $out"
    exit 1
}
if cmp -s "$GARDEN/src/main.rs" "$W/fixture-work/src/main.rs"; then
    ok "materialized file byte-identical to the git fixture"
else
    bad "materialized file differs from the git fixture"
    exit 1
fi

# Commit through replica 1 (write path).
printf 'a scaled-demo change\n' >>"$GARDEN/docs/README.md"
out=$(cd "$GARDEN" && GARDENBED_AUTHOR="Scaled Demo <scaled-demo@example.com>" \
    "$BED_BIN" commit -m "scaled demo commit" 2>&1) || {
    bad "bed commit via replica one failed: $out"
    exit 1
}
ok "commit via replica one: $(printf '%s\n' "$out" | head -1)"
out=$(cd "$GARDEN" && "$BED_BIN" publish 2>&1) || {
    bad "bed publish via replica one failed: $out"
    exit 1
}
ok "publish via replica one: $(printf '%s\n' "$out" | head -1)"

# Read the new tip through replica 2 — the same logical repository, a
# different process, the same off-box state.
mkdir -p "$W/garden2"
out=$(cd "$W" && "$BED_BIN" --state-dir "$W/garden2" clone "$REPLICA_TWO_URL" 2>&1) || {
    bad "bed clone via replica two failed: $out"
    exit 1
}
ok "clone via replica two: $(printf '%s\n' "$out" | head -1)"
out=$(cd "$W" && "$BED_BIN" --state-dir "$W/garden2" log --limit 1 2>&1) || {
    bad "bed log via replica two failed: $out"
    exit 1
}
if printf '%s' "$out" | grep -q "scaled demo commit"; then
    ok "commit made via replica one is the newest history entry via replica two"
else
    bad "replica two does not see the commit made through replica one: $(printf '%s' "$out" | head -3)"
    exit 1
fi
out=$(cd "$W" && "$BED_BIN" --state-dir "$W/garden2" file docs/README.md 2>&1) || {
    bad "bed file via replica two failed: $out"
    exit 1
}
if printf '%s' "$out" | grep -q "a scaled-demo change"; then
    ok "blob written via replica one fetched byte-verified via replica two"
else
    bad "replica two returned stale content: $out"
    exit 1
fi

printf 'scaled demo OK — 2 replicas, one Postgres, one S3 bucket\n'
