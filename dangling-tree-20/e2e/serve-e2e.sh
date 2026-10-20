#!/bin/sh
# Playwright webServer entry point: sweep squatters on this port's root,
# seed a fresh deterministic repo, then serve it. Doing this inside the
# server command makes seed/serve atomic regardless of when Playwright
# launches the webServer relative to globalSetup.
set -eu

ROOT=/tmp/gardenbed-web-e2e-root
WORKSPACE="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"

# Binary lives under the target dir; honor a globally-set CARGO_TARGET_DIR
# (cargo resolves a relative one against the directory you BUILD from, so
# cd to the workspace for both).
case "${CARGO_TARGET_DIR-}" in
  "")  TARGET="$WORKSPACE/target" ;;
  /*)  TARGET="$CARGO_TARGET_DIR" ;;
  *)   TARGET="$WORKSPACE/$CARGO_TARGET_DIR" ;;
esac
BIN="$TARGET/debug/gardenbed-web"

pkill -f "gardenbed-web --root $ROOT" 2>/dev/null || true

# Self-sufficient: Playwright launches the webServer before globalSetup,
# so the build must live here, not in a setup hook. cd first so a relative
# CARGO_TARGET_DIR resolves against the workspace, matching BIN above.
cd "$WORKSPACE"
cargo build -p gardenbed-web 1>&2

rm -rf "$ROOT"
"$BIN" seed --root "$ROOT" >/dev/null
exec "$BIN" --root "$ROOT" --addr 127.0.0.1:18430
