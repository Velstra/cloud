#!/usr/bin/env bash
# Photograph every view, in both appearances, against the contract server.
#
#   tests/console/run-shots.sh /where/to/put/them
#
# Same fixture as run.sh — the point is that the pictures are of the same
# console the suite is green against, not of a page built by hand.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
out=${1:-/tmp/console-shots}
work=$(mktemp -d)
trap 'kill "${fake_pid:-0}" 2>/dev/null || true; rm -rf "$work"' EXIT

(cd "$root" && cargo run -q -p velstra-cloud-console --bin velstra-console-page) > "$work/console.html"

export CONSOLE_TOKEN=${CONSOLE_TOKEN:-testtoken}
CONSOLE_PAGE="$work/console.html" FAKE_PORT=0 node "$here/fake-api.mjs" > "$work/fake.log" 2>&1 &
fake_pid=$!
for _ in $(seq 40); do
  port=$(sed -n 's/^listening //p' "$work/fake.log" | head -1)
  [ -n "$port" ] && break
  sleep 0.25
done
[ -n "${port:-}" ] || { echo "the contract server never came up:" >&2; cat "$work/fake.log" >&2; exit 1; }

CONSOLE_URL="http://127.0.0.1:$port/" CONSOLE_SHOTS="$out" node "$here/shots.mjs"
