#!/usr/bin/env bash
# The whole publishing protocol, from nothing, on this machine only: a fresh local database and buckets,
# two publishers with fresh tokens, the Worker under `wrangler dev`, then scripts/smoke.sh.
#
#   scripts/local-test.sh <package-dir> [path-to-godspeed-cli-binary]
#
# Never touches Cloudflare: every binding is local (.wrangler/state), and that state is wiped first.
set -eu
cd "$(dirname "$0")/.."
PKG=${1:?usage: local-test.sh <package-dir> [godspeed-binary]}
PORT=${PORT:-8787}
WRANGLER="node node_modules/wrangler/bin/wrangler.js"
LOG=$(mktemp)

rm -rf .wrangler/state
$WRANGLER d1 migrations apply godspeed-registry --local > /dev/null
node scripts/admin.mjs add-publisher alice alice > /dev/null
node scripts/admin.mjs add-publisher bob bob > /dev/null
ALICE_TOKEN=$(node scripts/admin.mjs issue-token alice --days 1 | tail -1)
BOB_TOKEN=$(node scripts/admin.mjs issue-token bob --days 1 | tail -1)

$WRANGLER dev --port "$PORT" --ip 127.0.0.1 > "$LOG" 2>&1 &
DEV=$!
trap 'kill $DEV 2>/dev/null; rm -f "$LOG"' EXIT
if ! curl -s --retry 150 --retry-connrefused --retry-delay 2 --retry-all-errors -o /dev/null "http://127.0.0.1:$PORT/"; then
  echo "the Worker did not start:"; cat "$LOG"; exit 1
fi

REGISTRY="http://127.0.0.1:$PORT" ALICE_TOKEN=$ALICE_TOKEN BOB_TOKEN=$BOB_TOKEN bash scripts/smoke.sh "$PKG" "${2:-}"
