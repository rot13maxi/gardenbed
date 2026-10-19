#!/usr/bin/env bash
# demo/run-v0.2.sh — the v0.2 acceptance test (docs/v0.2.md §0).
#
# An automated acceptance run of the whole v0.2 loop. Every §0 claim is
# asserted by the script; nothing is left to eyeball:
#
#   1. multi-repository — one real history imported into repository
#      `alpha`, a second repository `beta` (a smaller slice of the same
#      history) created and served by the SAME server process, both
#      addressed by URL path;
#   2. merge — two workers commit against `main` from the same
#      repository; one publishes, the other runs `bed merge main`,
#      resolves the listed conflict, commits the resolution, re-merges,
#      and publishes. The landed changeset has TWO parents (decoded from
#      the stored canonical bytes) and the file content is the resolved
#      one;
#   3. offline — the server stops; a third worker commits THREE changesets
#      offline (each queued with its locally computed id); the server
#      restarts; `bed sync` replays them (server ids equal the locally
#      computed ones, the queue drains) and `bed publish` fast-forwards;
#   4. gc — `gardenbed-server gc --dry-run` reports exactly the planted
#      orphans, `gc` deletes exactly those, a second run deletes nothing;
#   5. > 1 MiB blob — a chunked (GBLO2) blob round-trips through commit,
#      clone (fresh clone, byte compare), and export with its id
#      unchanged;
#   6. export — native export to git: `git fsck --strict` clean, the
#      changeset↔git-SHA mapping roundtrips 100% (re-import into a fresh
#      root reproduces the identical mapping), and a commit carrying a
#      literal `gpgsig` header exports with its original SHA and the
#      header intact;
#   7. daemon — two working copies through one local daemon: the second
#      copy's warm fetch adds ZERO server-side object requests (counted
#      from the server's own `request rpc=<name>` debug lines), while a
#      cold daemon over the same server does add requests (the negative
#      control that proves the counter can move);
#   8. MCP — `gardenbed-mcp` answers `log`/`diff`/`file`/`blame` with no
#      working copy anywhere and with the server STOPPED, from three
#      stdio sessions in a pristine HOME.
#
# Plus the auth-token seam exercised in the brief: a server started with
# `--auth <auth.toml>` refuses an anonymous clone with the actionable
# denial and accepts a `GARDENBED_TOKEN` clone/commit/publish.
#
# Mechanisms that deserve a word (they are the honest oracle available to
# a shell script):
#
#   * Changeset parents are not printed by any CLI verb, so the two-parent
#     assertion decodes the stored canonical bytes (format 0x01, version
#     0x01, varint parent count, 32-byte ids) straight out of
#     `metadata.sqlite3` — the same bytes the protocol serves.
#   * The daemon's per-fetch accounting is internal (`Fetcher::fetched`),
#     and `GetStats` is a gRPC call a shell cannot issue. The server
#     itself, however, logs one `request rpc=<name> repo=<name>` line per
#     resolved RPC at `RUST_LOG=debug` (crates/server/src/lib.rs,
#     `repo_guard`). GetTree/GetBlob counts taken from that log are the
#     server-side object requests, and they are exactly what §0 asks
#     about. The daemon re-validates authorization with one upstream
#     `ListRefs` per cache hit (crates/daemon/src/proxy.rs,
#     `authorize_hit`) — that consult is not an object request, so the
#     assertion counts only GetTree/GetBlob and says so.
#
# Usage:
#   demo/run-v0.2.sh [--keep-going] [--workdir DIR] [--keep-work]
#
# Environment:
#   GARDENBED_DEMO_CORPUS   a real git repository to use as the base
#                           history. Unset ⇒ the deterministic synthetic
#                           corpus is generated with gen-demo-corpus (a
#                           documented deviation: §0 says "a real
#                           history"; the synthetic corpus IS a real git
#                           repository, just a small deterministic one).
#                           A real corpus is copied and ONE commit is
#                           appended to its `main` carrying the shapes the
#                           later steps need (`story.md`, `big/f00000.txt`,
#                           `big/f00001.txt`; `main` is created from HEAD
#                           when the corpus only has `master`). Nothing in
#                           the caller's repository is touched.
#   GARDENBED_DEMO_INJECT   fault-injection switch for the demo's own
#                           tests; default off. The only value today is
#                           `wrong-parent-count`, which makes the merge
#                           step pretend the landed changeset has one
#                           parent (it must then report a failed step, not
#                           crash).
#
# The run writes demo/report-v0.2.md (next to this script) and exits
# non-zero on the first failed step — unless --keep-going, which runs
# every step and fails at the end. Every fixture lives in a fresh workdir
# (tmpfs when it has room — the object store fsyncs every object, and
# disk latency is not what this demo measures).

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
REPORT="$REPO_ROOT/demo/report-v0.2.md"

KEEP_GOING=0
KEEP_WORK=0
WORKDIR=""

usage() {
    sed -n '2,89p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --keep-going) KEEP_GOING=1 ;;
        --keep-work) KEEP_WORK=1 ;;
        --workdir)
            WORKDIR=$2
            shift
            ;;
        -h | --help) usage ;;
        *)
            echo "demo: unknown flag $1 (see --help)" >&2
            exit 2
            ;;
    esac
    shift
done

# ------------------------------------------------------------------ setup

for tool in git sqlite3 jq cmp python3 sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "demo: $tool is required but not installed" >&2
        exit 2
    }
done

# A booked environment must not hijack the run: the daemon step opts in
# explicitly, and no token may leak into the open-server steps.
unset GARDENBED_DAEMON GARDENBED_TOKEN GARDENBED_AUTHOR

mkdir -p "$(dirname "$REPORT")"
: >"$REPORT"

log() { printf '%s\n' "$*" | tee -a "$REPORT"; }

now_ms() { date +%s%3N; }

RUN_BAD=0
STEP_BAD=0

# `ok` counts as an assertion (it appears in the PASS total); `info`
# records setup context without claiming a checked property.
OK_COUNT=0
ok() {
    OK_COUNT=$((OK_COUNT + 1))
    printf '  ✓ %s\n' "$*" | tee -a "$REPORT"
}
info() { printf '  · %s\n' "$*" | tee -a "$REPORT"; }
bad() {
    printf '  ✗ %s\n' "$*" | tee -a "$REPORT"
    STEP_BAD=1
    RUN_BAD=1
}

# Records one step's wall time (ms) into the timings table.
TIMINGS="$REPORT.timings.$$"
: >"$TIMINGS"
timed() { # <label> <command...>
    local label=$1
    shift
    local t0
    t0=$(now_ms)
    "$@"
    local rc=$?
    printf '%s\t%s\n' "$label" "$(( $(now_ms) - t0 ))" >>"$TIMINGS"
    return $rc
}

# Workdir: tmpfs when it exists AND has room (GitHub runners have a small
# /dev/shm; the demo must not die on ENOSPC), else a normal tempdir.
free_kb() { df -Pk "$1" | awk 'NR == 2 {print $4}'; }
if [[ -n $WORKDIR ]]; then
    mkdir -p "$WORKDIR"
    W=$WORKDIR
elif [[ -d /dev/shm ]] && [[ $(free_kb /dev/shm) -gt 262144 ]]; then
    W=$(mktemp -d /dev/shm/gardenbed-demo-v0.2.XXXXXX)
else
    W=$(mktemp -d)
fi
SERVER_ROOT=$W/server
FIXTURE=$W/fixture
EXPORT=$W/export.git
MCP_HOME=$W/mcp-home
SERVER_LOG=$W/server.log
SERVER_ERR=$W/server.err

SERVER_PORT=""
SERVER_PID=""
LISTEN_MARKS=0
DAEMON_PIDS=()
DAEMON_PID=""
DAEMON_SOCKET=""
DAEMON_XDG=""

# Cross-step results. Every one is initialized so a step that never ran
# (an earlier failure under --keep-going) leaves later steps with an
# empty value they can report on, instead of aborting the whole run under
# `set -u` before the verdict is printed. The five binaries are assigned
# by step 0 alone, so they are seeded here too: a failed build must still
# reach the verdict.
SERVER_BIN=""
BED_BIN=""
MCP_BIN=""
DAEMON_BIN=""
GEN_BIN=""
SERVER_URL=""
SIGNED_SHA=""
PUB_ONE=""
RESOLUTION=""
MERGED=""
MERGE_PARENTS=""
QUEUED_IDS=()
BLOB_SHA=""
BIGBLOB_CS=""
TOKEN_CS=""
SHAPE_COMMIT=""
BETA_IMPORT_COMMITS=0

# Stops the server and asserts it is actually gone: `kill -0` must fail
# (the pid is not merely ours to forget). Returns nonzero when a process
# survives, so callers' `|| return 1` is load-bearing.
stop_server() { # <reason>
    local pid=${SERVER_PID:-}
    if [[ -n $pid ]]; then
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
        if kill -0 "$pid" 2>/dev/null; then
            bad "server pid $pid is still alive after SIGTERM ($1)"
            SERVER_PID=""
            return 1
        fi
    else
        bad "stop_server called with no server running ($1)"
        return 1
    fi
    SERVER_PID=""
    ok "server stopped and reaped ($1)"
    return 0
}

cleanup() {
    if [[ -n ${SERVER_PID:-} ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    local pid
    for pid in "${DAEMON_PIDS[@]:-}"; do
        [[ -n $pid ]] && kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    if [[ $KEEP_WORK -eq 0 ]]; then
        rm -rf "$W"
    else
        log "workdir kept: $W"
    fi
    rm -f "$TIMINGS"
}
trap cleanup EXIT

log "# gardenbed v0.2 — acceptance demo report (docs/v0.2.md §0)"
log ""
log "- date: $(date -u +%FT%TZ)"
log "- host: $(uname -srm), $(nproc) cores"
log "- git: $(git --version), sqlite3: $(sqlite3 --version | cut -d' ' -f1)"
log "- workdir: $W"
log ""

# ------------------------------------------------------------ build (step 0)

step_build() {
    log "## 0. build (cargo build --release)"
    log ""
    local t0
    t0=$(now_ms)
    if ! cargo build --release -q \
        -p gardenbed-server -p gardenbed -p gardenbed-mcp -p gardenbed-daemon \
        -p gardenbed-bench >>"$W/build.log" 2>&1; then
        bad "cargo build --release failed (see $W/build.log — kept under $W only when --keep-work)"
        return 1
    fi
    local target=${CARGO_TARGET_DIR:-$REPO_ROOT/target}
    SERVER_BIN=$target/release/gardenbed-server
    BED_BIN=$target/release/bed
    MCP_BIN=$target/release/gardenbed-mcp
    DAEMON_BIN=$target/release/gardenbed-daemon
    GEN_BIN=$target/release/gen-demo-corpus
    local bin
    for bin in "$SERVER_BIN" "$BED_BIN" "$MCP_BIN" "$DAEMON_BIN" "$GEN_BIN"; do
        [[ -x $bin ]] || {
            bad "expected binary missing: $bin"
            return 1
        }
    done
    ok "release binaries built in $(( $(now_ms) - t0 )) ms"
    log ""
    return 0
}

# ------------------------------------------------------- 1. import (§0.1)

# The signed commit below is hand-built with plumbing (no private key is
# available, and none is needed): a real commit object with a literal
# `gpgsig` header, written with `git hash-object -t commit -w --stdin`.
# Import must capture the raw bytes and export must reproduce the object
# verbatim — that is the fidelity claim, and the SHA is the proof.
add_signed_commit() {
    local main_sha main_tree raw
    main_sha=$(git -C "$FIXTURE" rev-parse refs/heads/main)
    main_tree=$(git -C "$FIXTURE" rev-parse "$main_sha^{tree}")
    printf 'tree %s\nparent %s\nauthor Demo Signer <signer@garden.example> 1700000500 +0000\ncommitter Demo Signer <signer@garden.example> 1700000500 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n iQEzBAABCgAdFiEEdemoOnlyNoPrivateKey\n =demo\n -----END PGP SIGNATURE-----\n\nsigned: a hand-built commit with a gpgsig header\n' \
        "$main_tree" "$main_sha" >"$W/signed-commit.raw"
    SIGNED_SHA=$(git -C "$FIXTURE" hash-object -t commit -w --stdin <"$W/signed-commit.raw")
    git -C "$FIXTURE" update-ref refs/heads/main "$SIGNED_SHA"
    if git -C "$FIXTURE" fsck --strict >/dev/null 2>"$W/fixture-fsck.err"; then
        ok "fixture carries a hand-built signed commit ($SIGNED_SHA), fsck clean"
    else
        bad "signed commit fixture fails git fsck: $(tail -2 "$W/fixture-fsck.err")"
        return 1
    fi
    git -C "$FIXTURE" cat-file commit "$SIGNED_SHA" >"$W/signed-commit.roundtrip"
    return 0
}

# Appends one commit on top of `base_commit` whose tree is `base_tree`
# plus the three paths the later steps exercise. Plumbing only (a temp
# index + `write-tree`), so it works in the bare scratch clone and cannot
# disturb anything else. Sets SHAPE_COMMIT.
add_shape_commit() { # <base-commit> <base-tree>
    local base_commit=$1 base_tree=$2
    local idx="$W/shape.index"
    rm -f "$idx"
    local story big0 big1
    story=$(printf 'corpus line 1\ncorpus line 2\ncorpus line 3\n' |
        git -C "$FIXTURE" hash-object -w --stdin) || {
        bad "cannot write the corpus story.md blob"
        return 1
    }
    big0=$(printf 'corpus shape file 0\n' |
        git -C "$FIXTURE" hash-object -w --stdin) || {
        bad "cannot write the corpus big/f00000.txt blob"
        return 1
    }
    big1=$(printf 'corpus shape file 1\n' |
        git -C "$FIXTURE" hash-object -w --stdin) || {
        bad "cannot write the corpus big/f00001.txt blob"
        return 1
    }
    if ! GIT_INDEX_FILE="$idx" git -C "$FIXTURE" read-tree "$base_tree" ||
        ! GIT_INDEX_FILE="$idx" git -C "$FIXTURE" update-index --add \
            --cacheinfo "100644,$story,story.md" \
            --cacheinfo "100644,$big0,big/f00000.txt" \
            --cacheinfo "100644,$big1,big/f00001.txt"; then
        bad "cannot stage the demo's shape paths in the scratch clone"
        rm -f "$idx"
        return 1
    fi
    local tree
    tree=$(GIT_INDEX_FILE="$idx" git -C "$FIXTURE" write-tree) || {
        bad "cannot write the shaped tree"
        rm -f "$idx"
        return 1
    }
    rm -f "$idx"
    SHAPE_COMMIT=$(git -C "$FIXTURE" \
        -c user.name="Demo Shape" -c user.email="shape@demo.example" \
        commit-tree "$tree" -p "$base_commit" \
        -m "demo: add the paths the v0.2 run exercises (story.md, big/)") || {
        bad "cannot create the shape commit"
        return 1
    }
    if ! git -C "$FIXTURE" update-ref refs/heads/main "$SHAPE_COMMIT"; then
        bad "cannot point main at the shape commit"
        return 1
    fi
    return 0
}

step_import() {
    log "## 1. import — two named repositories, one server process"
    log ""

    if [[ -n ${GARDENBED_DEMO_CORPUS:-} ]]; then
        git -C "$GARDENBED_DEMO_CORPUS" rev-parse --git-dir >/dev/null 2>&1 || {
            bad "GARDENBED_DEMO_CORPUS=$GARDENBED_DEMO_CORPUS is not a git repository"
            return 1
        }
        # A BARE scratch clone (local clones hardlink objects, so this is
        # cheap even for a big corpus; nothing is checked out) and ONE new
        # commit on `main`, built with plumbing, carrying the paths the
        # later steps exercise: `story.md`, `big/f00000.txt`,
        # `big/f00001.txt`. The corpus's own history is untouched, and the
        # caller's repository is never written to. `main` is created from
        # HEAD when the corpus only has `master` (the common mirror case).
        if ! git clone --bare -q "$GARDENBED_DEMO_CORPUS" "$FIXTURE" 2>"$W/corpus-clone.err"; then
            bad "cannot clone GARDENBED_DEMO_CORPUS: $(tail -2 "$W/corpus-clone.err")"
            return 1
        fi
        local base_ref=refs/heads/main
        if ! git -C "$FIXTURE" rev-parse --verify -q "$base_ref" >/dev/null; then
            base_ref=HEAD
            info "corpus has no main ref; shaping on its HEAD"
        fi
        local base_commit base_tree
        base_commit=$(git -C "$FIXTURE" rev-parse "$base_ref") || {
            bad "corpus has no commits to shape ($base_ref does not resolve)"
            return 1
        }
        base_tree=$(git -C "$FIXTURE" rev-parse "$base_commit^{tree}") || {
            bad "cannot read the corpus's root tree"
            return 1
        }
        if ! add_shape_commit "$base_commit" "$base_tree"; then
            return 1
        fi
        info "shape commit ${SHAPE_COMMIT:0:16} appended on the scratch clone's main"
        ok "corpus copied from $GARDENBED_DEMO_CORPUS (one shape commit appended on a scratch clone)"
    else
        if ! timed "corpus generation" \
            "$GEN_BIN" --out "$FIXTURE" --commits 60 --big-files 30 --merge-every 20 \
            >>"$W/gen.log" 2>&1; then
            bad "gen-demo-corpus failed (see $W/gen.log)"
            return 1
        fi
        ok "synthetic corpus generated: $(git -C "$FIXTURE" rev-list --branches --count) commits"
    fi

    add_signed_commit || return 1

    local git_commits branches
    git_commits=$(git -C "$FIXTURE" rev-list --branches --count)
    branches=$(git -C "$FIXTURE" for-each-ref --format='%(refname:short)' refs/heads | tr '\n' ' ')
    if [[ $git_commits -gt 0 ]] && [[ " $branches " == *" main "* ]]; then
        ok "history has $git_commits commits on main (branches: $branches)"
    else
        bad "corpus is empty or has no main branch (commits=$git_commits branches=$branches)"
    fi

    if ! timed "import alpha" \
        "$SERVER_BIN" import --from "$FIXTURE" --root "$SERVER_ROOT" --repo alpha \
        >"$W/import-alpha.log" 2>"$W/import-alpha.err"; then
        bad "import into alpha failed: $(tail -2 "$W/import-alpha.err")"
        return 1
    fi
    ok "alpha imported: $(head -1 "$W/import-alpha.log")"

    # A dedicated repository for the gc step: gc's "exactly the planted
    # orphans" claim is only honest over a store nothing else has touched,
    # so the demo isolates it (the same import path, a different name).
    if ! timed "import gamma" \
        "$SERVER_BIN" import --from "$FIXTURE" --root "$SERVER_ROOT" --repo gamma --limit 4 \
        >"$W/import-gamma.log" 2>"$W/import-gamma.err"; then
        bad "import into gamma failed: $(tail -2 "$W/import-gamma.err")"
        return 1
    fi
    ok "gamma imported for the gc step (a 4-commit slice): $(head -1 "$W/import-gamma.log")"

    local rows
    rows=$(sqlite3 "$SERVER_ROOT/alpha/metadata.sqlite3" 'SELECT COUNT(*) FROM git_map')
    if [[ $rows -eq $git_commits ]]; then
        ok "alpha git_map ↔ git bijection: $rows rows for $git_commits commits"
    else
        bad "alpha git_map has $rows rows for $git_commits git commits"
    fi
    log ""
    return 0
}

# ------------------------------------------------------------ 2. serve (§0.1)

# Sets SERVER_PORT. Deliberately does NOT echo: a `$(pick_server_port)`
# call site would run this in a subshell, so a bad() here would print its
# line but lose STEP_BAD — the same trap as any helper used in a command
# substitution.
pick_server_port() {
    local port
    for port in 19410 19411 19412 19413 19414; do
        if ! (echo >/dev/tcp/127.0.0.1/"$port") 2>/dev/null; then
            SERVER_PORT=$port
            return 0
        fi
    done
    SERVER_PORT=""
    bad "no free demo server port in 19410-19414"
    return 1
}

# The server logs one `request rpc=<name> repo=<repo>` line per RPC at
# debug level; the daemon step counts object requests from it. Logs
# append across restarts on purpose.
#
# `start_server [auth.toml]`: the auth form needs the `serve` subcommand
# (its --addr/--root live on the subcommand, and clap would otherwise
# parse top-level copies that the Serve arm ignores).
start_server() { # [auth.toml]
    if [[ -z ${SERVER_PORT:-} ]]; then
        bad "cannot start the server: no port was selected (step 2 did not run or failed)"
        return 1
    fi
    if [[ -z ${SERVER_BIN:-} ]]; then
        bad "cannot start the server: the build step produced no binary"
        return 1
    fi
    local extra=()
    if [[ -n ${1:-} ]]; then
        extra=(serve --auth "$1")
    fi
    RUST_LOG=debug "$SERVER_BIN" "${extra[@]}" \
        --addr "127.0.0.1:$SERVER_PORT" --root "$SERVER_ROOT" \
        >>"$SERVER_LOG" 2>>"$SERVER_ERR" &
    SERVER_PID=$!
    local line=""
    local i
    for i in $(seq 1 300); do
        line=$(grep -c "listening on 127.0.0.1:$SERVER_PORT" "$SERVER_LOG" 2>/dev/null || true)
        [[ ${line:-0} -gt $LISTEN_MARKS ]] && break
        kill -0 "$SERVER_PID" 2>/dev/null || {
            bad "server died before announcing: $(tail -2 "$SERVER_ERR")"
            return 1
        }
        sleep 0.1
    done
    if [[ ${line:-0} -gt $LISTEN_MARKS ]]; then
        LISTEN_MARKS=$line
        SERVER_URL="http://127.0.0.1:$SERVER_PORT"
        return 0
    fi
    bad "server never announced a listening address"
    return 1
}

step_serve() {
    log "## 2. serve (one gardenbed-server process, two repositories)"
    log ""
    pick_server_port || return 1
    LISTEN_MARKS=0
    start_server || return 1
    ok "server listening at $SERVER_URL (pid $SERVER_PID)"

    # §0.1: the second repository is CREATED while the first is served,
    # by the same process — an import takes the repository's shared
    # repo.lock, which a live server coexists with (the exclusive lock is
    # gc's and move's).
    local out
    if ! out=$("$SERVER_BIN" import --from "$FIXTURE" --root "$SERVER_ROOT" --repo beta --limit 6 2>&1); then
        bad "import of beta while the server was running failed: $out"
        return 1
    fi
    ok "beta created during serving: $(printf '%s\n' "$out" | head -1)"
    BETA_IMPORT_COMMITS=$(sed -n 's/^\([0-9]\+\) commits (.*/\1/p' <<<"$out" | head -1)
    BETA_IMPORT_COMMITS=${BETA_IMPORT_COMMITS:-0}
    kill -0 "$SERVER_PID" 2>/dev/null && ok "the server survived the import" ||
        bad "the server died during the import"

    # Both repositories addressed by URL path against the SAME process.
    mkdir -p "$W/worker-beta"
    if ! out=$(cd "$W/worker-beta" && "$BED_BIN" clone "$SERVER_URL/beta" 2>&1); then
        bad "URL clone of beta failed: $out"
        return 1
    fi
    ok "URL clone of $SERVER_URL/beta"
    mkdir -p "$W/worker-alpha"
    if ! out=$(cd "$W/worker-alpha" && "$BED_BIN" clone "$SERVER_URL/alpha" 2>&1); then
        bad "URL clone of alpha failed: $out"
        return 1
    fi
    ok "URL clone of $SERVER_URL/alpha (same server process, pid $SERVER_PID)"
    kill -0 "$SERVER_PID" 2>/dev/null && ok "the lone server process never restarted between the two clones" ||
        bad "the server process went away during the two-repository step"

    # Distinctness, size-independently: beta got its own store with the
    # `--limit 6` slice of the history, so its changeset count is
    # min(6, corpus commits) — a different store, not an alias for alpha
    # (a tiny real corpus can legitimately share alpha's tip).
    local want_slice got_slice
    want_slice=6
    [[ $BETA_IMPORT_COMMITS -lt $want_slice ]] && want_slice=$BETA_IMPORT_COMMITS
    got_slice=$(sqlite3 "$SERVER_ROOT/beta/metadata.sqlite3" 'SELECT COUNT(*) FROM changesets')
    if [[ $got_slice -eq $want_slice ]]; then
        ok "beta is a separate store with its own $got_slice-changeset slice"
    else
        bad "beta holds $got_slice changesets, want $want_slice (its own --limit 6 slice)"
    fi
    local alpha_tip beta_tip
    alpha_tip=$(tip_of "$W/worker-alpha")
    beta_tip=$(tip_of "$W/worker-beta")
    if [[ $alpha_tip != "$beta_tip" ]]; then
        ok "beta's main is its own tip (${beta_tip:0:16} ≠ alpha ${alpha_tip:0:16})"
    else
        info "beta shares alpha's tip — the corpus is smaller than the slice limit"
    fi

    # A vacated/unknown name names the served list.
    mkdir -p "$W/worker-nope"
    if out=$(cd "$W/worker-nope" && "$BED_BIN" clone "$SERVER_URL/garden" 2>&1); then
        bad "clone of the unknown name garden unexpectedly succeeded: $out"
        return 1
    fi
    local served
    served=$(printf '%s' "$out" | sed -n 's/.*serves: \(.*\)).*/\1/p')
    if [[ $served == *alpha* && $served == *beta* ]]; then
        ok "an unknown repository name 404s naming the served repositories ($served)"
    else
        bad "the not-found error does not name the served list: $out"
    fi
    log ""
    return 0
}

# The working copy's current changeset from its config.
tip_of() { # <working-copy-dir>
    sed -n 's/^current_cs = "\([0-9a-f]\{64\}\)"/\1/p' "$1/.bed/config.toml"
}

# A ref's SERVER-side tip, straight from the repository's metadata (the
# table the protocol's GetRef reads). `bed publish` echoes the local
# current changeset — it never asks where the ref landed — so any claim
# about main's tip must be read here.
server_ref_tip() { # <repo> <ref>
    sqlite3 "$SERVER_ROOT/$1/metadata.sqlite3" \
        "SELECT lower(hex(cs)) FROM refs WHERE name = 'refs/heads/$2'"
}

# ------------------------------------------------------------ 3. merge (§0.2)

# The stored canonical changeset's parents, decoded from the metadata
# store: format 0x01, version 0x01, varint count, then 32-byte ids.
cs_parents() { # <metadata.sqlite3> <hex-id>
    python3 - "$1" "$2" <<'PY'
import sqlite3, sys

db, hex_id = sys.argv[1], sys.argv[2]
con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
row = con.execute(
    "SELECT canonical FROM changesets WHERE id = ?", (bytes.fromhex(hex_id),)
).fetchone()
if row is None:
    sys.exit(f"no stored changeset {hex_id}")
blob = row[0]
if blob[0] != 1 or blob[1] != 1:
    sys.exit(f"unexpected canonical header {blob[:2].hex()}")
i, count, shift = 2, 0, 0
while True:
    b = blob[i]
    i += 1
    count |= (b & 0x7F) << shift
    if not b & 0x80:
        break
    shift += 7
print(count)
for k in range(count):
    print(blob[i + 32 * k:i + 32 * (k + 1)].hex())
PY
}

# Asserts the landed changeset's parents, decoded from its stored
# canonical bytes. The count is checked BEFORE any parent index is read:
# under `set -u` a bare `${parents[2]}` on a one-parent changeset would
# abort the whole script instead of reporting a failed step.
#
# GARDENBED_DEMO_INJECT=wrong-parent-count truncates the decoded list to
# one entry; it exists to prove that this check fails loudly (a ✗ line,
# --keep-going continues) rather than dying, and it is only read here.
check_merge_parents() { # <merged-id> <resolution-id> <published-one-id>
    local merged=$1 resolution=$2 published=$3
    local -a parents=()
    local line
    while IFS= read -r line; do parents+=("$line"); done < <(
        cs_parents "$SERVER_ROOT/alpha/metadata.sqlite3" "$merged"
    )
    local count=${parents[0]:-0}
    if [[ ${GARDENBED_DEMO_INJECT:-} == "wrong-parent-count" ]]; then
        info "fault injection: pretending the merge has one parent"
        count=1
        parents=("1" "${parents[1]:-}")
    fi
    if [[ $count -ne 2 ]]; then
        bad "the landed changeset has $count parent(s), want 2 (parents: ${parents[*]:-none})"
        return 1
    fi
    MERGE_PARENTS="${parents[1]:0:16} ${parents[2]:0:16}"
    ok "the landed changeset has TWO parents (${MERGE_PARENTS})"
    local wrong=0
    [[ ${parents[1]:-} == "$resolution" ]] &&
        ok "parent[0] is the resolution commit (${resolution:0:16})" ||
        { bad "parent[0] is '${parents[1]:-}', want $resolution"; wrong=1; }
    [[ ${parents[2]:-} == "$published" ]] &&
        ok "parent[1] is worker one's published tip (${published:0:16})" ||
        { bad "parent[1] is '${parents[2]:-}', want $published"; wrong=1; }
    return $wrong
}

step_merge() {
    log "## 3. merge — two workers, one conflict, one two-parent changeset"
    log ""

    mkdir -p "$W/w1" "$W/w2"
    local out
    if ! out=$(cd "$W/w1" && "$BED_BIN" clone "$SERVER_URL/alpha" 2>&1 &&
        cd "$W/w1" && "$BED_BIN" sparse add story.md 2>&1); then
        bad "worker 1 clone/sparse failed: $out"
        return 1
    fi
    if ! out=$(cd "$W/w2" && "$BED_BIN" clone "$SERVER_URL/alpha" 2>&1 &&
        cd "$W/w2" && "$BED_BIN" sparse add story.md 2>&1); then
        bad "worker 2 clone/sparse failed: $out"
        return 1
    fi
    info "two workers cloned alpha and materialized story.md"

    # Both edit the SAME line: a guaranteed content conflict.
    python3 - "$W/w1/story.md" "$W/w2/story.md" <<'PY'
import sys

for path, label in ((sys.argv[1], "A"), (sys.argv[2], "B")):
    lines = open(path).read().split("\n")
    lines[0] = f"line {label}: worker {label} tended the repository"
    open(path, "w").write("\n".join(lines))
PY

    if ! out=$(cd "$W/w1" && GARDENBED_AUTHOR="Worker One <one@demo.example>" \
        "$BED_BIN" commit -m "worker one edits line 1" 2>&1); then
        bad "worker 1 commit failed: $out"
        return 1
    fi
    info "worker 1 committed: $(printf '%s\n' "$out" | head -1)"
    if ! out=$(cd "$W/w1" && "$BED_BIN" publish 2>&1); then
        bad "worker 1 publish failed: $out"
        return 1
    fi
    info "worker 1 published: $(printf '%s\n' "$out" | head -1)"
    PUB_ONE=$(tip_of "$W/w1")

    if ! out=$(cd "$W/w2" && GARDENBED_AUTHOR="Worker Two <two@demo.example>" \
        "$BED_BIN" commit -m "worker two edits line 1" 2>&1); then
        bad "worker 2 commit failed: $out"
        return 1
    fi
    info "worker 2 committed: $(printf '%s\n' "$out" | head -1)"
    local before_conflict
    before_conflict=$(tip_of "$W/w2")

    # §0.2: the merge fails, listing the conflicting path.
    if out=$(cd "$W/w2" && "$BED_BIN" merge main 2>&1); then
        bad "conflicting merge unexpectedly succeeded: $out"
        return 1
    fi
    if printf '%s' "$out" | grep -q "conflicts on: story.md"; then
        ok "bed merge main listed the conflict (story.md) and stored nothing"
    else
        bad "the conflict message does not name story.md: $out"
    fi
    [[ $(tip_of "$W/w2") == "$before_conflict" ]] &&
        ok "the working copy did not move on the refused merge" ||
        bad "the working copy moved despite the conflict"

    # Resolve in the working copy: adopt main's line, keep the resolution
    # visible in the file.
    local main_line
    main_line=$(cd "$W/w2" && "$BED_BIN" file story.md --cs main 2>/dev/null | head -1)
    python3 - "$W/w2/story.md" "$main_line" <<'PY'
import sys

path, main_line = sys.argv[1], sys.argv[2]
lines = open(path).read().split("\n")
if lines and lines[-1] == "":
    lines.pop()
lines[0] = main_line
lines.append("resolved by worker two: adopted main on line 1")
open(path, "w").write("\n".join(lines) + "\n")
PY
    if ! out=$(cd "$W/w2" && GARDENBED_AUTHOR="Worker Two <two@demo.example>" \
        "$BED_BIN" commit -m "resolve the conflict" 2>&1); then
        bad "worker 2 resolution commit failed: $out"
        return 1
    fi
    info "resolution committed: $(printf '%s\n' "$out" | head -1)"
    RESOLUTION=$(tip_of "$W/w2")

    if ! out=$(cd "$W/w2" && GARDENBED_AUTHOR="Worker Two <two@demo.example>" \
        "$BED_BIN" merge main 2>&1); then
        bad "re-merge after resolution failed: $out"
        return 1
    fi
    info "re-merge: $(printf '%s\n' "$out" | head -1)"
    MERGED=$(tip_of "$W/w2")

    if ! out=$(cd "$W/w2" && "$BED_BIN" publish 2>&1); then
        bad "merge publish failed: $out"
        return 1
    fi
    local merged_ref
    merged_ref=$(server_ref_tip alpha main)
    if [[ $merged_ref == "$MERGED" ]]; then
        ok "merge published; main's server-side tip is the merge changeset (${MERGED:0:16})"
    else
        bad "main's server-side tip is '${merged_ref:-<none>}', want $MERGED"
    fi

    if ! check_merge_parents "$MERGED" "$RESOLUTION" "$PUB_ONE"; then
        return 1
    fi

    if out=$(cd "$W/w2" && "$BED_BIN" file story.md --cs main 2>&1); then
        local first last
        first=$(printf '%s\n' "$out" | head -1)
        last=$(printf '%s\n' "$out" | tail -1)
        if [[ $first == "$main_line" ]] && [[ $last == "resolved by worker two: adopted main on line 1" ]]; then
            ok "the landed content is the resolved one (main's line 1 + the resolution note)"
        else
            bad "landed content is not the resolved one: first='$first' last='$last'"
        fi
    else
        bad "reading story.md at main failed: $out"
    fi
    log ""
    return 0
}

# --------------------------------------------------------- 4. offline (§0.3)

step_offline() {
    log "## 4. offline — three queued changesets, sync, publish"
    log ""
    mkdir -p "$W/w3"
    local out
    if ! out=$(cd "$W/w3" && "$BED_BIN" clone "$SERVER_URL/alpha" 2>&1 &&
        cd "$W/w3" && "$BED_BIN" sparse add story.md 2>&1); then
        bad "worker 3 clone/sparse failed: $out"
        return 1
    fi
    info "worker 3 cloned alpha"

    stop_server "offline commits require the server down" || return 1

    QUEUED_IDS=()
    local i id
    for i in 1 2 3; do
        printf 'offline change %s\n' "$i" >>"$W/w3/story.md"
        if ! out=$(cd "$W/w3" && GARDENBED_AUTHOR="Worker Three <three@demo.example>" \
            "$BED_BIN" commit -m "offline change $i" 2>&1); then
            bad "offline commit $i failed: $out"
            return 1
        fi
        id=$(tip_of "$W/w3")
        QUEUED_IDS+=("$id")
        if printf '%s' "$out" | grep -q "queued as ${id:0:16} ($i pending"; then
            ok "offline commit $i queued as ${id:0:16} ($i pending)"
        else
            bad "offline commit $i did not report the queue: $out"
        fi
    done

    if out=$(cd "$W/w3" && "$BED_BIN" status 2>&1); then
        if printf '%s' "$out" | grep -q "^3 pending commit(s) not yet on the server"; then
            ok "bed status reports 3 pending commits while offline"
        else
            bad "status does not show 3 pending commits: $out"
        fi
    else
        bad "offline status failed: $out"
    fi

    start_server || return 1
    info "server restarted at $SERVER_URL"

    if ! out=$(cd "$W/w3" && "$BED_BIN" sync 2>&1); then
        bad "bed sync failed: $out"
        return 1
    fi
    printf '%s' "$out" | grep -q "synced 3 commit(s), queue empty" &&
        ok "bed sync drained the queue (3 synced)" ||
        bad "sync summary unexpected: $out"

    # Server-side ids equal the locally computed ones: the ids are content
    # addresses, so this is the whole replay-fidelity claim.
    local missing=0
    for i in "${QUEUED_IDS[@]}"; do
        printf '%s' "$out" | grep -q "synced ${i:0:16}" || missing=$((missing + 1))
        [[ $(sqlite3 "$SERVER_ROOT/alpha/metadata.sqlite3" \
            "SELECT COUNT(*) FROM changesets WHERE id = x'$i'") -eq 1 ]] ||
            missing=$((missing + 1))
    done
    if [[ $missing -eq 0 ]]; then
        ok "all three server ids equal the locally computed ids"
    else
        bad "$missing local/server id mismatches after sync"
    fi

    if out=$(cd "$W/w3" && "$BED_BIN" status 2>&1); then
        if printf '%s' "$out" | grep -q "pending"; then
            bad "status still reports pending work after sync: $out"
        elif printf '%s' "$out" | grep -q "working copy is clean"; then
            ok "status after sync: clean, nothing pending"
        else
            bad "status after sync: $out"
        fi
    else
        bad "status after sync failed: $out"
    fi

    if ! out=$(cd "$W/w3" && "$BED_BIN" publish 2>&1); then
        bad "publish after sync failed: $out"
        return 1
    fi
    info "published: $(printf '%s\n' "$out" | head -1)"
    local pushed
    pushed=$(server_ref_tip alpha main)
    [[ ${QUEUED_IDS[2]:-} == "$pushed" ]] &&
        ok "main's server-side tip is the third offline changeset (${QUEUED_IDS[2]:0:16})" ||
        bad "main's server-side tip is '${pushed:-<none>}', want ${QUEUED_IDS[2]:-?}"
    log ""
    return 0
}

# -------------------------------------------------------------- 5. gc (§0.4)

# Plants one orphan blob (GBLO1-shaped first byte) and one orphan tree
# (0x02 first byte) under alpha's object store; returns their hex ids.
plant_orphans() {
    python3 - "$SERVER_ROOT/gamma/objects" <<'PY'
import hashlib, os, sys

root = sys.argv[1]
for seed, payload in (
    (b"gardenbed-v0.2-demo-orphan-blob", b"GBLO1" + b"\x00" * 8 + b"unreferenced"),
    (b"gardenbed-v0.2-demo-orphan-tree", b"\x02" + b"orphan-junk"),
):
    hex_id = hashlib.sha256(seed).hexdigest()
    shard = os.path.join(root, hex_id[:2])
    os.makedirs(shard, exist_ok=True)
    with open(os.path.join(shard, hex_id[2:]), "wb") as f:
        f.write(payload)
    print(hex_id)
PY
}

count_objects() {
    find "$SERVER_ROOT/gamma/objects" -type f -not -name '.*' | wc -l
}

step_gc() {
    log "## 5. gc — dry-run reports the planted orphans, gc deletes them, second run is empty"
    log ""
    stop_server "offline gc needs the exclusive repo.lock" || return 1

    local out
    # Baseline: the freshly imported store has no garbage, so anything a
    # later run reports is the demo's own planting.
    if ! out=$("$SERVER_BIN" gc --root "$SERVER_ROOT/gamma" --dry-run --min-age 0 2>&1); then
        bad "baseline gc --dry-run failed: $out"
        return 1
    fi
    printf '%s' "$out" | grep -Eq "^gc: [0-9]+ object\(s\) scanned, [0-9]+ live, 0 blob\(s\) and 0 tree\(s\) deleted(,|$)" &&
        ok "baseline dry-run on the untouched store reports no garbage" ||
        bad "the imported store already had garbage: $out"

    local ids blob_orphan tree_orphan before
    ids=$(plant_orphans)
    blob_orphan=$(printf '%s\n' "$ids" | head -1)
    tree_orphan=$(printf '%s\n' "$ids" | tail -1)
    before=$(count_objects)
    info "planted 2 orphans (blob ${blob_orphan:0:16}, tree ${tree_orphan:0:16}); $before objects on disk"

    if ! out=$("$SERVER_BIN" gc --root "$SERVER_ROOT/gamma" --dry-run --min-age 0 2>&1); then
        bad "gc --dry-run failed: $out"
        return 1
    fi
    if printf '%s' "$out" | grep -Eq "^gc: [0-9]+ object\(s\) scanned, [0-9]+ live, 1 blob\(s\) and 1 tree\(s\) deleted(,|$)"; then
        ok "gc --dry-run reports exactly the two planted orphans: $(printf '%s\n' "$out" | head -1)"
    else
        bad "dry-run did not report exactly 1 blob + 1 tree: $out"
    fi
    [[ $(count_objects) -eq $before ]] &&
        ok "dry-run deleted nothing ($before objects still on disk)" ||
        bad "dry-run changed the object store"

    if ! out=$("$SERVER_BIN" gc --root "$SERVER_ROOT/gamma" --min-age 0 2>&1); then
        bad "gc failed: $out"
        return 1
    fi
    printf '%s' "$out" | grep -Eq "^gc: [0-9]+ object\(s\) scanned, [0-9]+ live, 1 blob\(s\) and 1 tree\(s\) deleted(,|$)" &&
        ok "gc deleted exactly the planted orphans: $(printf '%s\n' "$out" | head -1)" ||
        bad "gc deletion counts unexpected: $out"
    [[ $(count_objects) -eq $((before - 2)) ]] &&
        ok "object store shrank by exactly 2 ($((before - 2)) remain)" ||
        bad "object count is $(count_objects), want $((before - 2))"
    local orphan
    local orphans_gone=0
    for orphan in "$blob_orphan" "$tree_orphan"; do
        [[ -f "$SERVER_ROOT/gamma/objects/${orphan:0:2}/${orphan:2}" ]] ||
            orphans_gone=$((orphans_gone + 1))
    done
    [[ $orphans_gone -eq 2 ]] &&
        ok "both planted object files are gone by name" ||
        bad "only $orphans_gone of 2 planted object files were removed"

    if ! out=$("$SERVER_BIN" gc --root "$SERVER_ROOT/gamma" --min-age 0 2>&1); then
        bad "second gc failed: $out"
        return 1
    fi
    local settled
    settled=$(count_objects)
    printf '%s' "$out" | grep -Eq "^gc: [0-9]+ object\(s\) scanned, [0-9]+ live, 0 blob\(s\) and 0 tree\(s\) deleted(,|$)" &&
        ok "a second gc over the settled store deletes nothing" ||
        bad "second gc deleted something: $out"
    [[ $settled -eq $((before - 2)) ]] &&
        ok "the settled store still holds exactly $settled objects" ||
        bad "the second gc changed the store ($settled objects, want $((before - 2)))"

    start_server || return 1
    info "server restarted after gc"
    log ""
    return 0
}

# --------------------------------------------------------- 6. big blob (§0.5)

step_bigblob() {
    log "## 6. > 1 MiB blob — commit, clone, export with the id unchanged"
    log ""
    mkdir -p "$W/w4"
    local out
    if ! out=$(cd "$W/w4" && "$BED_BIN" clone "$SERVER_URL/alpha" 2>&1 &&
        cd "$W/w4" && "$BED_BIN" sparse add big 2>&1); then
        bad "big-blob worker clone/sparse failed: $out"
        return 1
    fi

    # 1 MiB + 12345 bytes, deterministic (sha256-chained blocks).
    python3 - "$W/bigblob.source" <<'PY'
import hashlib, sys

size = 1024 * 1024 + 12345
seed = b"gardenbed-v0.2-demo-big-blob"
out = bytearray()
i = 0
while len(out) < size:
    out += hashlib.sha256(seed + i.to_bytes(8, "little")).digest()
    i += 1
open(sys.argv[1], "wb").write(bytes(out[:size]))
PY
    cp "$W/bigblob.source" "$W/w4/big/roundtrip.bin"
    BLOB_SHA=$(sha256sum "$W/bigblob.source" | cut -d' ' -f1)
    info "source blob: $((1024 * 1024 + 12345)) bytes, sha256 ${BLOB_SHA:0:16}"

    if ! out=$(cd "$W/w4" && GARDENBED_AUTHOR="Blob Worker <blob@demo.example>" \
        "$BED_BIN" commit -m "big blob roundtrip" 2>&1); then
        bad "big-blob commit failed: $out"
        return 1
    fi
    ok "committed the > 1 MiB blob: $(printf '%s\n' "$out" | head -1)"
    if ! out=$(cd "$W/w4" && "$BED_BIN" publish 2>&1); then
        bad "big-blob publish failed: $out"
        return 1
    fi
    info "published: $(printf '%s\n' "$out" | head -1)"
    BIGBLOB_CS=$(tip_of "$W/w4")

    # Chunked storage (D15/GBLO2): the object file is named by the
    # content address and carries the GBLO2 envelope.
    local obj="$SERVER_ROOT/alpha/objects/${BLOB_SHA:0:2}/${BLOB_SHA:2}"
    if [[ -f $obj ]]; then
        local magic
        magic=$(head -c 5 "$obj")
        if [[ $magic == "GBLO2" ]]; then
            ok "stored object ${BLOB_SHA:0:16} is a GBLO2 chunked envelope"
        else
            bad "stored object ${BLOB_SHA:0:16} has magic '$magic', want GBLO2"
        fi
    else
        bad "no object file at ${obj#$SERVER_ROOT/alpha/}"
    fi

    # Fresh clone: byte compare and bed file's bytes.
    mkdir -p "$W/w5"
    if ! out=$(cd "$W/w5" && "$BED_BIN" clone "$SERVER_URL/alpha" 2>&1 &&
        cd "$W/w5" && "$BED_BIN" sparse add big 2>&1); then
        bad "fresh clone failed: $out"
        return 1
    fi
    if cmp -s "$W/bigblob.source" "$W/w5/big/roundtrip.bin"; then
        ok "fresh clone materialized the blob byte-identically"
    else
        bad "fresh clone's roundtrip.bin differs from the source"
    fi
    local file_sha
    file_sha=$(cd "$W/w5" && "$BED_BIN" file big/roundtrip.bin 2>/dev/null | sha256sum | cut -d' ' -f1)
    [[ $file_sha == "$BLOB_SHA" ]] &&
        ok "bed file returns the same content address (${BLOB_SHA:0:16})" ||
        bad "bed file sha256 $file_sha ≠ $BLOB_SHA"
    log ""
    return 0
}

# ------------------------------------------------------------ 7. export (§0.6)

# The blob id a stored changeset records for `path` (canonical decode:
# format 0x01, version 0x01, varint parents, varint changes, then per
# change: op byte, path string, optional blob id, optional src path, mode).
cs_blob() { # <metadata.sqlite3> <hex-id> <path>
    python3 - "$1" "$2" "$3" <<'PY'
import sqlite3, sys

db, hex_id, want = sys.argv[1], sys.argv[2], sys.argv[3]
con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
row = con.execute(
    "SELECT canonical FROM changesets WHERE id = ?", (bytes.fromhex(hex_id),)
).fetchone()
if row is None:
    sys.exit(f"no stored changeset {hex_id}")
b = row[0]


def varint(i):
    out, shift = 0, 0
    while True:
        byte = b[i]
        i += 1
        out |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return out, i
        shift += 7


i = 2
nparents, i = varint(i)
i += 32 * nparents
nchanges, i = varint(i)
for _ in range(nchanges):
    i += 1                              # op byte
    n, i = varint(i)
    path = b[i:i + n].decode()
    i += n
    present = b[i]
    i += 1
    blob = None
    if present:
        blob = b[i:i + 32].hex()
        i += 32
    present = b[i]
    i += 1
    if present:
        n, i = varint(i)
        i += n
    i += 1                              # mode byte
    if path == want:
        print(blob or "")
        sys.exit(0)
sys.exit(f"changeset {hex_id} has no change for {want!r}")
PY
}

step_export() {
    log "## 7. export — fsck clean, 100% changeset↔git roundtrip, verbatim signed commit"
    log ""
    local out
    if ! timed "export" \
        "$SERVER_BIN" export --root "$SERVER_ROOT" --repo alpha --into "$EXPORT" \
        --mapping "$W/mapping.tsv" >"$W/export.log" 2>"$W/export.err"; then
        bad "export failed: $(tail -2 "$W/export.err")"
        return 1
    fi
    info "exported: $(head -1 "$W/export.log" | sed 's/ *$//')"
    local total reported
    total=$(wc -l <"$W/mapping.tsv")
    reported=$(sed -n 's/^\([0-9]\+\) commits (.*/\1/p' "$W/export.log" | head -1)
    if [[ -n $reported && $total -eq $reported ]]; then
        ok "$total changeset→git-SHA mappings written (the export report names $reported commits)"
    else
        bad "mapping has $total rows but the export report names '${reported:-?}' commits"
    fi

    if git -C "$EXPORT" fsck --strict >/dev/null 2>"$W/fsck.err"; then
        ok "git fsck --strict on the exported repository: clean"
    else
        bad "git fsck found problems: $(tail -2 "$W/fsck.err")"
    fi

    # Every mapped SHA exists in the export.
    local missing=0 cs sha
    while IFS=$'\t' read -r cs sha; do
        git -C "$EXPORT" cat-file -e "$sha" 2>/dev/null || missing=$((missing + 1))
    done <"$W/mapping.tsv"
    [[ $missing -eq 0 ]] &&
        ok "every one of the $total mapped git SHAs exists in the export" ||
        bad "$missing mapped SHAs are absent from the export"

    # 100% roundtrip: re-import the export into a fresh root and export it
    # again; the mapping must be byte-identical, which means every
    # changeset id survived git verbatim.
    rm -rf "$W/reimport"
    if ! "$SERVER_BIN" import --from "$EXPORT" --root "$W/reimport" --repo alpha \
        >"$W/reimport.log" 2>"$W/reimport.err"; then
        bad "re-import of the export failed: $(tail -2 "$W/reimport.err")"
        return 1
    fi
    if ! "$SERVER_BIN" export --root "$W/reimport" --repo alpha --into "$W/export2.git" \
        --mapping "$W/mapping2.tsv" >/dev/null 2>"$W/export2.err"; then
        bad "second export failed: $(tail -2 "$W/export2.err")"
        return 1
    fi
    if cmp -s "$W/mapping.tsv" "$W/mapping2.tsv"; then
        ok "export→import→export is idempotent ($total/$total ids reproduced byte-for-byte)"
    else
        bad "re-imported mapping differs: $(diff "$W/mapping.tsv" "$W/mapping2.tsv" | head -4)"
    fi

    # The fidelity claim proper: for every commit the SOURCE history
    # contributed, the exported git SHA must be that commit's own SHA.
    #
    # Scope: `alpha` also holds bed-only changesets (the two workers'
    # commits, the resolution, the merge, the three offline commits and
    # the > 1 MiB blob commit). Those never existed in git, so they have
    # no source SHA to reproduce; they are counted and excluded, not
    # silently ignored. Everything else must match 1:1, and each corpus
    # commit's original import mapping (`git_map`) must name the same
    # changeset the export mapped.
    git -C "$FIXTURE" rev-list --branches | sort >"$W/corpus.shas"
    awk -F'\t' '{print $2}' "$W/mapping.tsv" | sort >"$W/mapping.shas"
    local corpus_total matched foreign
    corpus_total=$(wc -l <"$W/corpus.shas")
    matched=$(comm -12 "$W/corpus.shas" "$W/mapping.shas" | wc -l)
    foreign=$((total - matched))
    if [[ $matched -eq $corpus_total ]]; then
        ok "every source commit exported with its own git SHA ($matched/$corpus_total, 100%)"
    else
        bad "only $matched of $corpus_total source commits exported with their own SHA"
    fi
    info "excluded $foreign bed-only changeset(s) with no git origin (workers, merge, offline, big blob)"

    local id_mismatch=0 corpus_sha corpus_cs mapped_cs
    while IFS=$'\t' read -r corpus_sha corpus_cs; do
        mapped_cs=$(awk -F'\t' -v s="$corpus_sha" '$2 == s {print $1}' "$W/mapping.tsv")
        [[ -n $mapped_cs && $mapped_cs == "$corpus_cs" ]] || id_mismatch=$((id_mismatch + 1))
    done < <(sqlite3 -separator $'\t' "$SERVER_ROOT/alpha/metadata.sqlite3" \
        "SELECT git_sha, lower(hex(cs)) FROM git_map")
    if [[ $id_mismatch -eq 0 ]]; then
        ok "every corpus commit's exported SHA carries its original changeset id (0 mismatches)"
    else
        bad "$id_mismatch git_map rows disagree with the export mapping"
    fi

    # The hand-built signed commit: original SHA, gpgsig header, and the
    # object bytes themselves.
    cs=$(awk -F'\t' -v s="$SIGNED_SHA" '$2 == s {print $1}' "$W/mapping.tsv")
    if [[ -n $cs ]]; then
        ok "the signed commit exported with its original SHA ($SIGNED_SHA)"
    else
        bad "the signed commit $SIGNED_SHA is absent from the mapping"
    fi
    if git -C "$EXPORT" cat-file commit "$SIGNED_SHA" 2>/dev/null | grep -q "^gpgsig -----BEGIN PGP SIGNATURE-----"; then
        ok "the exported commit still carries the gpgsig header"
    else
        bad "the exported commit lost its gpgsig header"
    fi
    if git -C "$EXPORT" cat-file commit "$SIGNED_SHA" 2>/dev/null >"$W/signed-exported.raw" &&
        cmp -s "$W/signed-commit.raw" "$W/signed-exported.raw"; then
        ok "the exported commit object is byte-identical to the imported one"
    else
        bad "the exported commit object differs from the imported bytes"
    fi

    # The > 1 MiB blob's gardenbed identity survives the trip through git:
    # the re-imported store's changeset must reference the SAME blob id
    # (a content address, so this is the identity claim — not merely
    # "the bytes hash the same after a re-export").
    if [[ -n ${BLOB_SHA:-} && -n ${BIGBLOB_CS:-} ]]; then
        local back_id
        back_id=$(cs_blob "$W/reimport/alpha/metadata.sqlite3" "$BIGBLOB_CS" big/roundtrip.bin)
        if [[ $back_id == "$BLOB_SHA" ]]; then
            ok "the re-imported changeset references the same > 1 MiB blob id (${BLOB_SHA:0:16})"
        else
            bad "the re-imported changeset references blob '${back_id:-<none>}', want $BLOB_SHA"
        fi
        [[ $(sqlite3 "$W/reimport/alpha/metadata.sqlite3" \
            "SELECT COUNT(*) FROM changesets WHERE id = x'$BIGBLOB_CS'") -eq 1 ]] &&
            ok "the big-blob changeset id itself survived the roundtrip (${BIGBLOB_CS:0:16})" ||
            bad "the re-imported store lacks changeset $BIGBLOB_CS"
    else
        bad "step 6 did not provide the big blob's id/changeset for the export check"
    fi
    log ""
    return 0
}

# ------------------------------------------------------------ 8. daemon (§0.7)

# The server's `request rpc=<name>` lines are env_logger output on stderr;
# stdout carries only the `listening on` contract.
count_rpc() { # <RpcName>
    grep -c "request rpc=$1 " "$SERVER_ERR" 2>/dev/null || true
}

start_daemon() { # <name> <cache-root>
    local name=$1 cache=$2
    mkdir -p "$W/$name"
    "$DAEMON_BIN" serve --upstream "$SERVER_URL" \
        --socket "$W/$name/daemon.sock" --cache "$cache" \
        >"$W/$name.log" 2>"$W/$name.err" &
    local pid=$!
    DAEMON_PIDS+=("$pid")
    local i
    for i in $(seq 1 300); do
        grep -q "listening on $W/$name/daemon.sock" "$W/$name.log" 2>/dev/null && break
        kill -0 "$pid" 2>/dev/null || {
            bad "daemon $name died: $(tail -2 "$W/$name.err")"
            return 1
        }
        sleep 0.1
    done
    grep -q "listening on $W/$name/daemon.sock" "$W/$name.log" 2>/dev/null || {
        bad "daemon $name never announced its socket"
        return 1
    }
    DAEMON_PID=$pid
    DAEMON_SOCKET=$W/$name/daemon.sock
    return 0
}

# Runs bed through the daemon whose socket/cache are in the environment.
bed_via_daemon() { # <dir> <args...>
    local dir=$1
    shift
    (cd "$dir" && GARDENBED_DAEMON="$DAEMON_SOCKET" XDG_CACHE_HOME="$DAEMON_XDG" \
        "$BED_BIN" "$@")
}

step_daemon() {
    log "## 8. daemon — a warm second working copy makes zero server-side object requests"
    log ""

    DAEMON_XDG=$W/xdg1
    if ! start_daemon daemon1 "$W/daemon1/cache"; then return 1; fi
    info "daemon 1 serving $SERVER_URL with a cold shared cache"

    mkdir -p "$W/wd-a" "$W/wd-b"
    local out
    if ! out=$(bed_via_daemon "$W/wd-a" clone "$SERVER_URL/alpha" 2>&1 &&
        bed_via_daemon "$W/wd-a" sparse add big 2>&1); then
        bad "working copy A through the daemon failed: $out"
        return 1
    fi
    info "working copy A cloned + materialized through the daemon (cache warm)"

    local trees0 blobs0 trees1 blobs1
    trees0=$(count_rpc GetTree)
    blobs0=$(count_rpc GetBlob)

    if ! out=$(bed_via_daemon "$W/wd-b" clone "$SERVER_URL/alpha" 2>&1 &&
        bed_via_daemon "$W/wd-b" sparse add big 2>&1); then
        bad "working copy B through the same daemon failed: $out"
        return 1
    fi
    trees1=$(count_rpc GetTree)
    blobs1=$(count_rpc GetBlob)
    if [[ $trees1 -eq $trees0 ]] && [[ $blobs1 -eq $blobs0 ]]; then
        ok "B's warm fetch added ZERO server-side object requests (GetTree $trees0→$trees1, GetBlob $blobs0→$blobs1)"
    else
        bad "B's fetch hit the server: GetTree $trees0→$trees1, GetBlob $blobs0→$blobs1"
    fi
    if cmp -s "$W/wd-a/big/f00000.txt" "$W/wd-b/big/f00000.txt"; then
        ok "both working copies hold identical bytes"
    else
        bad "the two working copies disagree on big/f00000.txt"
    fi

    # Negative control: a cold daemon cache over the SAME server must make
    # object requests. This is what proves the counter can move at all.
    DAEMON_XDG=$W/xdg2
    if ! start_daemon daemon2 "$W/daemon2/cache"; then return 1; fi
    mkdir -p "$W/wd-c"
    if ! out=$(bed_via_daemon "$W/wd-c" clone "$SERVER_URL/alpha" 2>&1 &&
        bed_via_daemon "$W/wd-c" sparse add big 2>&1); then
        bad "working copy C through a cold daemon failed: $out"
        return 1
    fi
    local trees2 blobs2
    trees2=$(count_rpc GetTree)
    blobs2=$(count_rpc GetBlob)
    if [[ $trees2 -gt $trees1 ]] && [[ $blobs2 -gt $blobs1 ]]; then
        ok "negative control: the cold daemon did add requests (GetTree +$((trees2 - trees1)), GetBlob +$((blobs2 - blobs1)))"
    else
        bad "the cold daemon added no object requests — the counter cannot move, so the zero above proves nothing"
    fi
    cmp -s "$W/wd-a/big/f00000.txt" "$W/wd-c/big/f00000.txt" &&
        ok "the cold run produced the same bytes" ||
        bad "the cold run's bytes differ"

    if [[ $(find "$W/daemon1/cache" -type f -not -name '.*' | wc -l) -gt 0 ]]; then
        ok "the shared cache holds objects ($(find "$W/daemon1/cache" -type f -not -name '.*' | wc -l) files)"
    else
        bad "the shared cache is empty — the zero-request claim would be vacuous"
    fi
    log ""
    return 0
}

# --------------------------------------------------------------- 9. MCP (§0.8)

mcp_session() { # <requests-file> <responses-file> <home>
    (cd "$3" && HOME="$3" XDG_CACHE_HOME="$3/.cache" \
        "$MCP_BIN" --root "$SERVER_ROOT" --repo alpha <"$1" >"$2" 2>"$1.stderr")
}

step_mcp() {
    log "## 9. MCP — log/diff/file/blame with no working copy and no server"
    log ""
    if [[ -n ${SERVER_PID:-} ]]; then
        stop_server "MCP answers in process; the server is not needed" || return 1
    else
        info "no server running (an earlier step did not start one)"
    fi
    rm -rf "$MCP_HOME"
    mkdir -p "$MCP_HOME"

    # Oracles from the export (the same bytes the server's refs point at),
    # so no working copy and no server are involved.
    local tip lines
    tip=$(sqlite3 "$SERVER_ROOT/alpha/metadata.sqlite3" \
        "SELECT lower(hex(cs)) FROM refs WHERE name = 'refs/heads/main'" 2>/dev/null || true)
    if [[ -z $tip ]] || ! git -C "$EXPORT" show main:story.md >"$W/mcp-story.want" 2>/dev/null; then
        bad "no MCP oracle: the export or main ref is missing (an earlier step failed)"
        return 1
    fi
    lines=$(wc -l <"$W/mcp-story.want")
    info "MCP oracle: main = ${tip:0:16}, story.md = $lines lines"

    local init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"gardenbed-demo","version":"0.2"}}}'
    local inited='{"jsonrpc":"2.0","method":"notifications/initialized"}'

    # --- session A: handshake + tool discovery -----------------------------
    printf '%s\n%s\n{"jsonrpc":"2.0","id":2,"method":"tools/list"}\n' \
        "$init" "$inited" >"$W/mcp-a.req"
    mkdir -p "$W/mcp-a.home"
    if ! mcp_session "$W/mcp-a.req" "$W/mcp-a.resp" "$W/mcp-a.home"; then
        bad "MCP session A failed: $(tail -2 "$W/mcp-a.req.stderr")"
        return 1
    fi
    local ver tools
    ver=$(jq -r 'select(.id==1) | .result.protocolVersion' "$W/mcp-a.resp")
    tools=$(jq -r 'select(.id==2) | .result.tools | sort_by(.name) | map(.name) | join(",")' "$W/mcp-a.resp")
    [[ $ver == "2025-06-18" ]] && ok "initialize echoes the protocol version ($ver)" ||
        bad "initialize protocol version: $ver"
    [[ $tools == "blame,copies,diff,file,log" ]] &&
        ok "tools/list advertises log/diff/file/blame (+copies): $tools" ||
        bad "tools/list: $tools"

    # --- session B: log, file, diff, blame ---------------------------------
    local diff_a="${MERGED:-}" diff_b="$tip"
    printf '%s\n%s\n%s\n%s\n%s\n%s\n' \
        "$init" "$inited" \
        '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"log","arguments":{"limit":1}}}' \
        '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"file","arguments":{"path":"story.md"}}}' \
        "{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"tools/call\",\"params\":{\"name\":\"diff\",\"arguments\":{\"a\":\"$diff_a\",\"b\":\"$diff_b\"}}}" \
        '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"blame","arguments":{"path":"story.md"}}}' \
        >"$W/mcp-b.req"
    mkdir -p "$W/mcp-b.home"
    if ! mcp_session "$W/mcp-b.req" "$W/mcp-b.resp" "$W/mcp-b.home"; then
        bad "MCP session B failed: $(tail -2 "$W/mcp-b.req.stderr")"
        return 1
    fi

    local log_text
    log_text=$(jq -j 'select(.id==3) | .result.content[0].text' "$W/mcp-b.resp")
    if printf '%s' "$log_text" | head -1 | grep -q "^$tip" &&
        printf '%s' "$log_text" | grep -qF "big blob roundtrip"; then
        ok "log(limit 1): the main tip with the expected subject"
    else
        bad "log(limit 1) does not lead with $tip: $(printf '%s' "$log_text" | head -2)"
    fi

    jq -j 'select(.id==4) | .result.content[0].text' "$W/mcp-b.resp" >"$W/mcp-file.actual"
    if cmp -s "$W/mcp-file.actual" "$W/mcp-story.want"; then
        ok "file story.md: byte-identical to the exported git content ($(wc -c <"$W/mcp-file.actual") bytes)"
    else
        bad "file story.md differs from the git oracle"
    fi

    jq -j 'select(.id==5) | .result.content[0].text' "$W/mcp-b.resp" |
        tr '\t' ' ' | grep -v '^$' | sort >"$W/mcp-diff.actual"
    if [[ -z ${MERGED:-} ]]; then
        bad "the diff oracle needs the merge changeset from step 3, which did not produce one"
    else
        printf 'A big/roundtrip.bin\nM story.md\n' | sort >"$W/mcp-diff.want"
        if cmp -s "$W/mcp-diff.actual" "$W/mcp-diff.want"; then
            ok "diff merge→tip lists exactly the landed changes (A big/roundtrip.bin, M story.md)"
        else
            bad "diff does not match the known change set: $(diff "$W/mcp-diff.want" "$W/mcp-diff.actual" | head -4)"
        fi
    fi

    jq -j 'select(.id==6) | .result.content[0].text' "$W/mcp-b.resp" |
        grep -v '^$' >"$W/mcp-blame.actual"
    local blame_bad=0 lo hi last_hi=0 covered
    while read -r range id; do
        lo=${range%-*}
        hi=${range#*-}
        [[ $lo -eq $((last_hi + 1)) ]] || blame_bad=$((blame_bad + 1))
        last_hi=$hi
        [[ $id =~ ^[0-9a-f]{64}$ ]] || blame_bad=$((blame_bad + 1))
        [[ $id == "$RESOLUTION" ]] && covered=1
    done <"$W/mcp-blame.actual"
    if [[ $blame_bad -eq 0 ]] && [[ $last_hi -eq $lines || $last_hi -eq $((lines + 1)) ]] &&
        [[ ${covered:-0} -eq 1 ]]; then
        ok "blame covers all $lines lines contiguously and attributes the resolution to ${RESOLUTION:0:16}"
    else
        bad "blame ranges are wrong (bad=$blame_bad last=$last_hi want=$lines resolution_seen=${covered:-0})"
    fi

    # --- session C: the same answer with a pristine HOME -------------------
    mkdir -p "$W/mcp-c.home"
    if ! mcp_session "$W/mcp-b.req" "$W/mcp-c.resp" "$W/mcp-c.home"; then
        bad "MCP session C failed: $(tail -2 "$W/mcp-b.req.stderr")"
        return 1
    fi
    if cmp -s "$W/mcp-b.resp" "$W/mcp-c.resp"; then
        ok "a second stdio session in a pristine HOME gives byte-identical answers"
    else
        bad "the pristine-HOME session's answers differ"
    fi
    if [[ $(find "$MCP_HOME" "$W/mcp-a.home" "$W/mcp-b.home" "$W/mcp-c.home" -name .bed | wc -l) -eq 0 ]]; then
        ok "no .bed anywhere under the MCP sessions' homes — answers came from the store alone"
    else
        bad "an MCP session left a working copy behind"
    fi
    log ""
    return 0
}

# -------------------------------------------------------------- 10. auth

step_auth() {
    log "## 10. auth — anonymous denied, token allowed (clone/commit/publish)"
    log ""
    local auth="$W/auth.toml"
    # The format crates/bed/tests/auth.rs writes: one [tokens."<token>"]
    # entry with repos + role.
    printf '[tokens."demo-token"]\nrepos = ["*"]\nrole = "write"\n' >"$auth"

    if ! start_server "$auth"; then return 1; fi
    info "server restarted with --auth ($auth)"

    local out
    mkdir -p "$W/auth-anon"
    if out=$(cd "$W/auth-anon" && env -u GARDENBED_TOKEN "$BED_BIN" clone "$SERVER_URL/alpha" 2>&1); then
        bad "anonymous clone unexpectedly succeeded: $out"
        return 1
    fi
    if printf '%s' "$out" | grep -q "requires a token" &&
        printf '%s' "$out" | grep -q "GARDENBED_TOKEN"; then
        ok "anonymous clone denied with the actionable message (names GARDENBED_TOKEN)"
    else
        bad "anonymous denial is not actionable: $out"
    fi

    mkdir -p "$W/auth-wrong"
    if out=$(cd "$W/auth-wrong" && GARDENBED_TOKEN=not-the-token \
        "$BED_BIN" clone "$SERVER_URL/alpha" 2>&1); then
        bad "wrong-token clone unexpectedly succeeded: $out"
        return 1
    fi
    printf '%s' "$out" | grep -q "denied for this token" &&
        ok "a wrong token is denied with the token-role message" ||
        bad "wrong-token denial: $out"

    mkdir -p "$W/auth-ok"
    if ! out=$(cd "$W/auth-ok" && GARDENBED_TOKEN=demo-token \
        "$BED_BIN" clone "$SERVER_URL/alpha" 2>&1 &&
        cd "$W/auth-ok" && GARDENBED_TOKEN=demo-token "$BED_BIN" sparse add story.md 2>&1); then
        bad "tokened clone/sparse failed: $out"
        return 1
    fi
    printf 'a tokened change\n' >>"$W/auth-ok/story.md"
    if ! out=$(cd "$W/auth-ok" && GARDENBED_TOKEN=demo-token \
        GARDENBED_AUTHOR="Token User <token@demo.example>" \
        "$BED_BIN" commit -m "tokened change" 2>&1); then
        bad "tokened commit failed: $out"
        return 1
    fi
    info "tokened commit accepted: $(printf '%s\n' "$out" | head -1)"
    TOKEN_CS=$(tip_of "$W/auth-ok")
    if ! out=$(cd "$W/auth-ok" && GARDENBED_TOKEN=demo-token "$BED_BIN" publish 2>&1); then
        bad "tokened publish failed: $out"
        return 1
    fi
    info "tokened publish: $(printf '%s\n' "$out" | head -1)"
    [[ $(sqlite3 "$SERVER_ROOT/alpha/metadata.sqlite3" \
        "SELECT lower(hex(cs)) FROM refs WHERE name = 'refs/heads/main'") == "$TOKEN_CS" ]] &&
        ok "main's server-side tip is the tokened changeset" ||
        bad "main did not advance to the tokened changeset"
    log ""
    return 0
}

# ------------------------------------------------------------------ main

log "Preconditions: cargo, git, sqlite3, jq, python3 on PATH; binaries built release."
log ""

FAILED_STEPS=()
run_step() { # <name> <fn>
    local name=$1 fn=$2 rc=0
    STEP_BAD=0
    log "---"
    log ""
    # Both signals count:
    #  * a nonzero return — a step may fail through a path that never
    #    called bad() (`X=$(helper) || return 1`, where the helper's
    #    bad() ran inside the command substitution's subshell and its
    #    STEP_BAD write died with it);
    #  * STEP_BAD — a step can assert a failure and still return 0 on a
    #    later, independent path.
    if "$fn"; then
        rc=0
    else
        rc=$?
    fi
    if [[ $rc -ne 0 || $STEP_BAD -ne 0 ]]; then
        FAILED_STEPS+=("$name")
        log ""
        log "**step $name FAILED**"
        log ""
    fi
    if [[ $KEEP_GOING -eq 0 && ( $rc -ne 0 || $STEP_BAD -ne 0 ) ]]; then
        log ""
        log "stopping at the first failed step (rerun with --keep-going to continue)"
        exit 1
    fi
}

run_step "build" step_build
run_step "import" step_import
run_step "serve" step_serve
run_step "merge" step_merge
run_step "offline" step_offline
run_step "gc" step_gc
run_step "bigblob" step_bigblob
run_step "export" step_export
run_step "daemon" step_daemon
run_step "mcp" step_mcp
run_step "auth" step_auth

# ------------------------------------------------------------- timings

log "## Timings"
log ""
log "| phase | ms |"
log "|---|---|"
while IFS=$'\t' read -r label ms; do
    log "| $label | $ms |"
done <"$TIMINGS"
log ""

# The verdict must agree with run_step: a step that failed by return code
# only is already recorded in FAILED_STEPS and printed as `**step X FAILED**`,
# even though it never called bad() (so RUN_BAD stayed 0). PASS requires both
# signals clean; FAIL names the recorded steps (or the out-of-step bad()).
if [[ $RUN_BAD -eq 0 && ${#FAILED_STEPS[@]} -eq 0 ]]; then
    log "## Verdict"
    log ""
    log "**PASS** — $OK_COUNT ✓ checks, 0 ✗; every step green."
    exit 0
fi
log "## Verdict"
log ""
if [[ ${#FAILED_STEPS[@]} -gt 0 ]]; then
    log "**FAIL** — failed steps: ${FAILED_STEPS[*]}"
else
    log "**FAIL** — $OK_COUNT ✓ checks, failures reported outside a step (see the ✗ lines above)"
fi
exit 1
