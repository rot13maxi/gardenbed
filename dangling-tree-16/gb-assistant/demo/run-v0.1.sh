#!/usr/bin/env bash
# demo/run-v0.1.sh — the v0.1 north-star demo (docs/v0.1.md §0).
#
# An automated acceptance run of the whole loop, asserting every step a
# human would otherwise have to eyeball:
#
#   1. import   — a git history (≥ 50k commits) into the server, with a
#                 changeset↔git bijection proven in the metadata;
#   2. serve    — the server on an ephemeral port;
#   3. clone    — a sparse bed that materializes NOTHING, in < 5 s;
#   4. sparse add — every materialized file byte-identical to
#                 `git show <sha>:<path>`, with exec bits and symlinks;
#   5. commit      — a new changeset, queryable via the API in < 1 s;
#   6. export   — back to git: `git fsck` clean, `git push --dry-run` to a
#                 fresh mirror succeeds, and the exported SHAs roundtrip
#                 (100% for an unsigned history; ≥ 99% otherwise);
#   7. MCP      — log/diff/file/blame answered with no working copy: three
#                 sequential stdio sessions in a pristine home directory,
#                 verified against git itself.
#
# Usage:
#   demo/run-v0.1.sh [--keep-going] [--corpus DIR] [--commits N]
#                    [--workdir DIR] [--keep-work]
#
# Environment:
#   GARDENBED_DEMO_CORPUS   a real git repository to import (§0's pinned
#                           corpus). Unset ⇒ a deterministic synthetic
#                           corpus is generated (and cached under
#                           /dev/shm) with gen-demo-corpus — a documented
#                           deviation: v0.1 names a ≥ 1 GB real history.
#   GARDENBED_DEMO_COMMITS  synthetic corpus size (default 50000).
#
# The run writes demo/report.md (next to this script) and exits non-zero
# on the first failed step — unless --keep-going, which runs every step
# and fails at the end. Re-runs are safe: the corpus is cached and reused
# when it already has enough commits, and every other fixture lives in a
# fresh workdir (on tmpfs when /dev/shm exists — the object store fsyncs
# every object, and disk latency is not what this demo measures).

set -euo pipefail

REPO_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
REPORT="$REPO_ROOT/demo/report.md"

KEEP_GOING=0
KEEP_WORK=0
CORPUS=${GARDENBED_DEMO_CORPUS:-}
COMMITS=${GARDENBED_DEMO_COMMITS:-50000}
WORKDIR=""

usage() {
    sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --keep-going) KEEP_GOING=1 ;;
        --keep-work) KEEP_WORK=1 ;;
        --corpus)
            CORPUS=$2
            shift
            ;;
        --commits)
            COMMITS=$2
            shift
            ;;
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

for tool in git sqlite3 jq cmp; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "demo: $tool is required but not installed" >&2
        exit 2
    }
done

mkdir -p "$(dirname "$REPORT")"
: >"$REPORT"

log() { printf '%s\n' "$*" | tee -a "$REPORT"; }

now_ms() { date +%s%3N; }

RUN_BAD=0
STEP_BAD=0

ok() { printf '  ✓ %s\n' "$*" | tee -a "$REPORT"; }
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

# The workdir: tmpfs when it exists (fsync is part of the durability
# story — the demo measures the design, not the disk).
if [[ -n $WORKDIR ]]; then
    mkdir -p "$WORKDIR"
    W=$WORKDIR
elif [[ -d /dev/shm ]]; then
    W=$(mktemp -d /dev/shm/gardenbed-demo-v0.1.XXXXXX)
else
    W=$(mktemp -d)
fi
SERVER_ROOT=$W/server
GARDEN=$W/garden
MCP_HOME=$W/mcp-home

cleanup() {
    if [[ ${SERVER_PID:-} ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [[ $KEEP_WORK -eq 0 ]]; then
        rm -rf "$W"
    else
        log "workdir kept: $W"
    fi
    rm -f "$TIMINGS"
}
trap cleanup EXIT

log "# gardenbed v0.1 — north-star demo report"
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
        -p gardenbed-server -p gardenbed -p gardenbed-mcp -p gardenbed-bench \
        >>"$W/build.log" 2>&1; then
        bad "cargo build --release failed (see $W/build.log — kept under $W only when --keep-work)"
        return 1
    fi
    local target=${CARGO_TARGET_DIR:-$REPO_ROOT/target}
    SERVER_BIN=$target/release/gardenbed-server
    BED_BIN=$target/release/bed
    MCP_BIN=$target/release/gardenbed-mcp
    GEN_BIN=$target/release/gen-demo-corpus
    local bin
    for bin in "$SERVER_BIN" "$BED_BIN" "$MCP_BIN" "$GEN_BIN"; do
        [[ -x $bin ]] || {
            bad "expected binary missing: $bin"
            return 1
        }
    done
    ok "release binaries built in $(( $(now_ms) - t0 )) ms"
    return 0
}

# ------------------------------------------------------------ 1. import

step_import() {
    log "## 1. import — history into the server"
    log ""

    # Corpus acquisition: a real pinned repo when one is named, else the
    # deterministic synthetic one (cached — regenerating 50k commits on
    # every rerun would waste a minute for zero information).
    if [[ -z $CORPUS ]]; then
        CORPUS=${GARDENBED_DEMO_CORPUS_CACHE:-/dev/shm/gardenbed-demo-corpus}
        local cached=0
        if git -C "$CORPUS" rev-parse --git-dir >/dev/null 2>&1; then
            local have
            have=$(git -C "$CORPUS" rev-list --branches --count)
            [[ $have -ge $COMMITS ]] && cached=1
        fi
        if [[ $cached -eq 1 ]]; then
            ok "corpus reused: $CORPUS"
        else
            rm -rf "$CORPUS"
            if ! timed "corpus generation" \
                "$GEN_BIN" --out "$CORPUS" --commits "$COMMITS" >>"$W/gen.log" 2>&1; then
                bad "gen-demo-corpus failed (see $W/gen.log)"
                return 1
            fi
            ok "synthetic corpus generated: $CORPUS"
        fi
    fi
    git -C "$CORPUS" rev-parse --git-dir >/dev/null 2>&1 || {
        bad "corpus $CORPUS is not a git repository"
        return 1
    }

    GIT_COMMITS=$(git -C "$CORPUS" rev-list --branches --count)
    if [[ $GIT_COMMITS -ge 50000 ]]; then
        ok "history has $GIT_COMMITS commits (§0 wants ≥ 50000)"
    else
        bad "history has only $GIT_COMMITS commits (§0 wants ≥ 50000)"
    fi
    if [[ ${GARDENBED_DEMO_CORPUS:-} ]]; then
        # The §0 bar for a *real* corpus: ≥ 1 GiB packed. The synthetic
        # corpus cannot meet it — that is the documented deviation.
        local pack_kb
        pack_kb=$(git -C "$CORPUS" count-objects -v | awk -F': ' '/size-pack/ {print $2}')
        if [[ $((pack_kb / 1024 / 1024)) -ge 1 ]]; then
            ok "packed history ≥ 1 GiB ($((pack_kb / 1024)) MiB)"
        else
            bad "packed history is only $((pack_kb / 1024)) MiB (§0 wants ≥ 1 GiB)"
        fi
    else
        log "  (deviation: synthetic corpus — `git count-objects` size bar skipped)"
    fi

    if ! timed "import" \
        "$SERVER_BIN" import --from "$CORPUS" --root "$SERVER_ROOT" \
        >"$W/import.log" 2>"$W/import.err"; then
        bad "gardenbed-server import failed (stderr: $(tail -2 "$W/import.err"))"
        return 1
    fi

    # Bijection: every imported commit has exactly one changeset, and the
    # metadata's git_map holds one row per git commit (the ingest is
    # 1:1 by construction — content-addressed ids make duplicates
    # impossible; the count proves the walk found everything).
    GB_COMMITS=$(sqlite3 "$SERVER_ROOT/metadata.sqlite3" 'SELECT COUNT(*) FROM git_map')
    if [[ $GB_COMMITS -eq $GIT_COMMITS ]]; then
        ok "git_map ↔ git bijection: $GB_COMMITS changesets for $GIT_COMMITS commits"
    else
        bad "git_map has $GB_COMMITS rows for $GIT_COMMITS git commits — the import is not a bijection"
    fi
    log ""
    return 0
}

# ------------------------------------------------------------ 2. serve

step_serve() {
    log "## 2. serve (gardenbed-server, ephemeral port)"
    log ""
    "$SERVER_BIN" --addr 127.0.0.1:0 --root "$SERVER_ROOT" \
        >"$W/server.log" 2>"$W/server.err" &
    SERVER_PID=$!
    local addr=""
    for _ in $(seq 1 300); do
        addr=$(head -1 "$W/server.log" 2>/dev/null || true)
        [[ $addr == "listening on "* ]] && break
        kill -0 "$SERVER_PID" 2>/dev/null || {
            bad "server died before announcing: $(tail -2 "$W/server.err")"
            return 1
        }
        sleep 0.1
    done
    if [[ $addr == "listening on "* ]]; then
        SERVER_URL="http://${addr#listening on }"
        ok "server listening at $SERVER_URL"
    else
        bad "server never announced a listening address"
        return 1
    fi
    log ""
    return 0
}

# ------------------------------------------------------------ 3. clone

step_clone() {
    log "## 3. bed clone (sparse bed, materializing nothing)"
    log ""
    mkdir -p "$GARDEN"
    local out
    if ! out=$(cd "$GARDEN" && timed "bed clone" "$BED_BIN" clone "$SERVER_URL" 2>&1); then
        bad "bed clone failed: $out"
        return 1
    fi
    local ms
    ms=$(awk -F'\t' '$1 == "bed clone" {print $2}' "$TIMINGS")
    if [[ $ms -lt 5000 ]]; then
        ok "clone in ${ms} ms (§0 wants < 5000)"
    else
        bad "clone took ${ms} ms (§0 wants < 5000)"
    fi
    local strangers
    strangers=$(find "$GARDEN" -type f -not -path "$GARDEN/.bed/*" | wc -l)
    if [[ $strangers -eq 0 ]]; then
        ok "zero working files outside .bed/ (nothing materialized)"
    else
        bad "clone materialized $strangers file(s) — v0.1 clone must materialize nothing"
    fi
    log ""
    return 0
}

# ------------------------------------------------------------ 4. sparse add

step_sparse_add() {
    log "## 4. bed sparse add big (fetch + materialize, byte-verified)"
    log ""
    local out
    if ! out=$(cd "$GARDEN" && timed "bed sparse add big" "$BED_BIN" sparse add big 2>&1); then
        bad "bed sparse add failed: $out"
        return 1
    fi
    local files_in_sparse
    files_in_sparse=$(printf '%s\n' "$out" | sed -n 's/.*(\([0-9]*\) file(s) in the selected paths).*/\1/p')
    ok "sparse add: ${files_in_sparse:-?} files in the selected paths"

    # Full byte sweep — ~1k files, under the 2k full-sweep bar: every
    # materialized file must be byte-identical to `git show HEAD:<path>`.
    # Symlinks are skipped here (`cmp` follows them, which would compare
    # the *target's* content) — their link strings are verified below.
    # ls-tree lines are `<mode> <type> <sha>\t<path>`.
    local mismatches=0 compared=0
    local p mode meta
    while IFS=$'\t' read -r meta p; do
        mode=${meta%% *}
        [[ $mode == 120000 ]] && continue
        compared=$((compared + 1))
        if ! git -C "$CORPUS" show "refs/heads/main:$p" | cmp -s - "$GARDEN/$p"; then
            mismatches=$((mismatches + 1))
            bad "byte mismatch: $p"
        fi
    done < <(git -C "$CORPUS" ls-tree -r refs/heads/main -- big/)
    if [[ $mismatches -eq 0 ]]; then
        ok "byte-identical to git show for all $compared materialized files"
    else
        bad "$mismatches of $compared files differ from git show"
    fi

    # Symlinks: the §4 mode survived the roundtrip — a real symlink whose
    # target is the blob content.
    local links=0 bad_links=0
    while IFS=$'\t' read -r meta p; do
        mode=${meta%% *}
        [[ $mode == 120000 ]] || continue
        links=$((links + 1))
        local want
        want=$(git -C "$CORPUS" show "refs/heads/main:$p")
        if [[ -L "$GARDEN/$p" && $(readlink "$GARDEN/$p") == "$want" ]]; then
            :
        else
            bad_links=$((bad_links + 1))
            bad "symlink wrong: $p"
        fi
    done < <(git -C "$CORPUS" ls-tree -r refs/heads/main -- big/)
    [[ $bad_links -eq 0 ]] && ok "all $links symlinks materialize as symlinks with exact targets"

    # Exec bits likewise.
    local execs=0 bad_execs=0
    while IFS=$'\t' read -r meta p; do
        mode=${meta%% *}
        [[ $mode == 100755 ]] || continue
        execs=$((execs + 1))
        [[ -x "$GARDEN/$p" ]] || {
            bad_execs=$((bad_execs + 1))
            bad "exec bit lost: $p"
        }
    done < <(git -C "$CORPUS" ls-tree -r refs/heads/main -- big/)
    [[ $bad_execs -eq 0 ]] && ok "all $execs executable files kept their mode bit"

    # And the offline heart agrees.
    if out=$(cd "$GARDEN" && "$BED_BIN" status 2>&1) && printf '%s' "$out" | grep -q "working copy is clean"; then
        ok "bed status: working copy is clean"
    else
        bad "bed status after sparse add: $out"
    fi
    log ""
    return 0
}

# ------------------------------------------------------------ 5. commit

step_commit() {
    log "## 5. bed commit (write path: edit → commit → query < 1 s)"
    log ""
    printf 'demo tend %s\n' "$(date -u +%FT%TZ)" >>"$GARDEN/big/f00000.txt"
    printf 'demo tend %s\n' "$(date -u +%FT%TZ)" >>"$GARDEN/big/d0/g00.md"
    printf 'a brand new file, created by the demo\n' >"$GARDEN/big/demo-note.txt"

    local out
    if ! out=$(cd "$GARDEN" && timed "bed commit" "$BED_BIN" commit -m "v0.1 demo" 2>&1); then
        bad "bed commit failed: $out"
        return 1
    fi
    ok "commit accepted: $(printf '%s\n' "$out" | head -1)"

    # §0 step 4: the new changeset is queryable via the API in < 1 s.
    local ms out2
    out2=$(cd "$GARDEN" && timed "bed log --limit 1" "$BED_BIN" log --limit 1 2>&1) || {
        bad "bed log failed: $out2"
        return 1
    }
    ms=$(awk -F'\t' '$1 == "bed log --limit 1" {print $2}' "$TIMINGS")
    if [[ $ms -lt 1000 ]]; then
        ok "log --limit 1 answered in ${ms} ms (§0 wants < 1000)"
    else
        bad "log --limit 1 took ${ms} ms (§0 wants < 1000)"
    fi
    if printf '%s' "$out2" | grep -q "v0.1 demo"; then
        ok "the created changeset is the newest history entry"
    else
        bad "bed log does not show the created changeset: $(printf '%s' "$out2" | head -3)"
    fi

    if out=$(cd "$GARDEN" && "$BED_BIN" status 2>&1) && printf '%s' "$out" | grep -q "working copy is clean"; then
        ok "bed status after commit: working copy is clean"
    else
        bad "bed status after commit: $out"
    fi
    log ""
    return 0
}

# ------------------------------------------------------------ 6. export

step_export() {
    log "## 6. export — back to git (fsck + push --dry-run + SHA roundtrip)"
    log ""
    if ! timed "export" \
        "$SERVER_BIN" export --root "$SERVER_ROOT" --into "$W/export.git" \
        --mapping "$W/mapping.tsv" >"$W/export.log" 2>"$W/export.err"; then
        bad "gardenbed-server export failed: $(tail -2 "$W/export.err")"
        return 1
    fi
    ok "export stream written ($(wc -l <"$W/mapping.tsv") commits mapped)"

    if git -C "$W/export.git" fsck --strict >/dev/null 2>"$W/fsck.err"; then
        ok "git fsck --strict on the exported repository: clean"
    else
        bad "git fsck found problems: $(tail -2 "$W/fsck.err")"
    fi

    git clone --mirror -q "$W/export.git" "$W/mirror.git" 2>/dev/null || {
        bad "mirroring the export failed"
        return 1
    }
    if git -C "$W/export.git" push --dry-run "$W/mirror.git" 'refs/heads/*:refs/heads/*' >/dev/null 2>&1; then
        ok "git push --dry-run to a fresh local mirror succeeds"
    else
        bad "git push --dry-run failed"
    fi

    # SHA roundtrip: the exported git SHAs must be the corpus's own SHAs
    # (gardenbed stores everything git does; an unsigned history must
    # reproduce them exactly). Signed histories drop signatures at import
    # — those commits legitimately re-hash; the bar is ≥ 99%.
    local total matched pct
    total=$(wc -l <"$W/mapping.tsv")
    matched=$(comm -12 \
        <(awk -F'\t' '{print $2}' "$W/mapping.tsv" | sort) \
        <(git -C "$CORPUS" rev-list --branches | sort) | wc -l)
    pct=$((matched * 100 / total))
    if [[ ${GARDENBED_DEMO_CORPUS:-} ]]; then
        if [[ $pct -ge 99 ]]; then
            ok "SHA roundtrip: $matched/$total ($pct%) exported SHAs match git's (≥ 99% bar)"
        else
            bad "SHA roundtrip only $pct% ($matched/$total) — below the 99% bar"
        fi
    else
        if [[ $matched -eq $total ]]; then
            ok "SHA roundtrip: all $total exported SHAs equal the synthetic corpus's own SHAs (100%)"
        else
            bad "SHA roundtrip incomplete: $matched/$total SHAs match (unsigned history must be 100%)"
        fi
    fi
    log ""
    return 0
}

# ------------------------------------------------------------ 7. MCP

# One MCP stdio session: feed a request file, keep the responses.
mcp_session() { # <requests-file> <responses-file>
    (
        cd "$MCP_HOME" &&
            "$MCP_BIN" --root "$SERVER_ROOT" --repo garden \
                <"$1" >"$2" 2>"$1.stderr"
    )
}

step_mcp() {
    log "## 7. MCP — log/diff/file/blame with no working copy"
    log ""
    mkdir -p "$MCP_HOME"

    local head_sha parent_sha head_cs parent_cs
    head_sha=$(git -C "$CORPUS" rev-parse refs/heads/main)
    parent_sha=$(git -C "$CORPUS" rev-parse 'refs/heads/main^')
    head_cs=$(awk -F'\t' -v s="$head_sha" '$2 == s {print $1}' "$W/mapping.tsv")
    parent_cs=$(awk -F'\t' -v s="$parent_sha" '$2 == s {print $1}' "$W/mapping.tsv")
    [[ -n $head_cs && -n $parent_cs ]] || {
        bad "the export mapping lacks the head/parent commits — cannot build MCP oracles"
        return 1
    }

    local init_req='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"gardenbed-demo","version":"0.1"}}}'
    local inited_req='{"jsonrpc":"2.0","method":"notifications/initialized"}'

    # --- session A: handshake + tool discovery -----------------------------
    printf '%s\n%s\n{"jsonrpc":"2.0","id":2,"method":"tools/list"}\n' \
        "$init_req" "$inited_req" >"$W/mcp-a.req"
    if ! mcp_session "$W/mcp-a.req" "$W/mcp-a.resp"; then
        bad "MCP session A failed: $(tail -2 "$W/mcp-a.req.stderr")"
        return 1
    fi
    local ver tools
    ver=$(jq -r 'select(.id==1) | .result.protocolVersion' "$W/mcp-a.resp")
    tools=$(jq -r 'select(.id==2) | .result.tools | sort_by(.name) | map(.name) | join(",")' "$W/mcp-a.resp")
    [[ $ver == "2025-06-18" ]] && ok "initialize echoes the protocol version ($ver)"
    [[ $tools == "blame,diff,file,log" ]] && ok "tools/list advertises exactly the four tools ($tools)"

    # --- session B: log + diff + file, checked against git -----------------
    printf '%s\n%s\n%s\n%s\n%s\n' \
        "$init_req" "$inited_req" \
        '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"log","arguments":{"limit":3}}}' \
        "{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"tools/call\",\"params\":{\"name\":\"diff\",\"arguments\":{\"a\":\"$parent_cs\",\"b\":\"$head_cs\"}}}" \
        '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"file","arguments":{"path":"story.md"}}}' \
        >"$W/mcp-b.req"
    if ! mcp_session "$W/mcp-b.req" "$W/mcp-b.resp"; then
        bad "MCP session B failed: $(tail -2 "$W/mcp-b.req.stderr")"
        return 1
    fi

    # log: newest entry is the corpus head (id and subject).
    local log_text
    log_text=$(jq -j 'select(.id==3) | .result.content[0].text' "$W/mcp-b.resp")
    local want_subject
    want_subject=$(git -C "$CORPUS" log -1 --format=%s refs/heads/main)
    if printf '%s' "$log_text" | grep -q "^$head_cs" && printf '%s' "$log_text" | grep -qF "$want_subject"; then
        ok "log(limit 3): newest entry is the head changeset with git's own subject"
    else
        bad "log(limit 3) does not lead with $head_cs / $want_subject: $(printf '%s' "$log_text" | head -2)"
    fi

    # diff: exactly git diff-tree's name-status list (rename detection off
    # on both sides).
    jq -j 'select(.id==4) | .result.content[0].text' "$W/mcp-b.resp" \
        | tr '\t' ' ' | sed 's/ *$//' | grep -v '^$' >"$W/mcp-diff.actual"
    git -C "$CORPUS" diff-tree -r --name-status "$parent_sha" "$head_sha" \
        | tr '\t' ' ' >"$W/mcp-diff.want"
    if cmp -s "$W/mcp-diff.actual" "$W/mcp-diff.want"; then
        ok "diff parent→head matches git diff-tree --name-status ($(wc -l <"$W/mcp-diff.actual") paths)"
    else
        bad "diff parent→head differs from git: $(diff "$W/mcp-diff.want" "$W/mcp-diff.actual" | head -4)"
    fi

    # file: story.md's exact bytes.
    jq -j 'select(.id==5) | .result.content[0].text' "$W/mcp-b.resp" >"$W/mcp-file.actual"
    git -C "$CORPUS" show "refs/heads/main:story.md" >"$W/mcp-file.want"
    if cmp -s "$W/mcp-file.actual" "$W/mcp-file.want"; then
        ok "file story.md: byte-identical to git show ($(wc -c <"$W/mcp-file.actual") bytes)"
    else
        bad "file story.md differs from git show"
    fi

    # --- session C: blame vs `git blame --first-parent` --------------------
    printf '%s\n%s\n%s\n' \
        "$init_req" "$inited_req" \
        '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"blame","arguments":{"path":"story.md"}}}' \
        >"$W/mcp-c.req"
    if ! mcp_session "$W/mcp-c.req" "$W/mcp-c.resp"; then
        bad "MCP session C failed: $(tail -2 "$W/mcp-c.req.stderr")"
        return 1
    fi
    jq -j 'select(.id==3) | .result.content[0].text' "$W/mcp-c.resp" \
        | grep -v '^$' >"$W/mcp-blame.actual"

    # Expected ranges: git's porcelain blame groups into (lo-hi sha); the
    # import/export mapping turns each git sha into its changeset id.
    # Porcelain headers are `<40-hex> <orig> <final> [<num>]` — `<num>` is
    # omitted for single-line groups; `author`/`summary`/`previous` lines
    # are filtered by the field-shape checks below.
    git -C "$CORPUS" blame --first-parent --porcelain "refs/heads/main" -- story.md \
        >"$W/mcp-blame.porcelain"
    awk -F'\t' 'NR==FNR { map[$2]=$1; next } /^\t/ { next } {
        if (split($0, f, /[ \t]+/) < 3) next;
        if (f[3] !~ /^[0-9]+$/) next;          # previous/boundary lines
        sha = f[1];
        n = (f[4] ~ /^[0-9]+$/) ? f[4] : 1;
        for (l = f[3]; l < f[3] + n; l++) lines[l] = sha;
    } END {
        l = 1;
        while (l in lines) {
            s = lines[l]; hi = l;
            while ((hi + 1) in lines && lines[hi + 1] == s) hi++;
            printf "%d-%d %s\n", l, hi, (s in map) ? map[s] : "UNMAPPED";
            l = hi + 1;
        }
    }' "$W/mapping.tsv" "$W/mcp-blame.porcelain" >"$W/mcp-blame.want"
    if cmp -s "$W/mcp-blame.actual" "$W/mcp-blame.want"; then
        ok "blame story.md: $(wc -l <"$W/mcp-blame.actual") ranges, every attribution == git blame --first-parent"
    else
        bad "blame ranges differ from git blame --first-parent: $(diff "$W/mcp-blame.want" "$W/mcp-blame.actual" | head -4)"
    fi

    # The zero-checkout claim: nothing under the MCP home is (or became) a
    # working copy.
    if [[ $(find "$MCP_HOME" -name .bed | wc -l) -eq 0 ]]; then
        ok "no .bed anywhere under the MCP session's home — the server answered alone"
    else
        bad "the MCP session left a working copy behind"
    fi
    log ""
    return 0
}

# ------------------------------------------------------------------ main

log "Preconditions: cargo, git, sqlite3, jq on PATH; binaries built release."
log ""

FAILED_STEPS=()
run_step() { # <name> <fn>
    local name=$1 fn=$2
    STEP_BAD=0
    log "---"
    log ""
    if ! "$fn"; then
        FAILED_STEPS+=("$name")
        log ""
        log "**step $name FAILED**"
        log ""
    fi
    if [[ $KEEP_GOING -eq 0 && $STEP_BAD -eq 1 ]]; then
        log ""
        log "stopping at the first failed step (rerun with --keep-going to continue)"
        exit 1
    fi
}

run_step "build" step_build
run_step "import" step_import
run_step "serve" step_serve
run_step "clone" step_clone
run_step "sparse-add" step_sparse_add
run_step "commit" step_commit
run_step "export" step_export
run_step "mcp" step_mcp

# ------------------------------------------------------------- timings

log "## Timings"
log ""
log "| phase | ms |"
log "|---|---|"
while IFS=$'\t' read -r label ms; do
    log "| $label | $ms |"
done <"$TIMINGS"
log ""

if [[ $RUN_BAD -eq 0 ]]; then
    log "## Verdict"
    log ""
    log "**PASS** — all seven §0 steps asserted clean."
    exit 0
fi
log "## Verdict"
log ""
log "**FAIL** — failed steps: ${FAILED_STEPS[*]}"
exit 1
