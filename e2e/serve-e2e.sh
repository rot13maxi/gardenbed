#!/bin/sh
# Playwright webServer entry point: sweep squatters on this port's root,
# seed a fresh deterministic repo, then serve it. Doing this inside the
# server command makes seed/serve atomic regardless of when Playwright
# launches the webServer relative to globalSetup.
set -eu

ROOT=/tmp/gardenbed-web-e2e-root
BIN="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)/target/debug/gardenbed-web"

pkill -f "gardenbed-web --root $ROOT" 2>/dev/null || true
rm -rf "$ROOT"
"$BIN" seed --root "$ROOT" >/dev/null
exec "$BIN" --root "$ROOT" --addr 127.0.0.1:18430
