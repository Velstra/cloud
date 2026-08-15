#!/usr/bin/env bash
# Drive the console in a real browser.
#
#   tests/console/run.sh                 # against the in-memory contract server
#   CONSOLE_URL=http://host:8080/ \
#   CONSOLE_TOKEN=… CONSOLE_SCAFFOLD=0 \
#     tests/console/run.sh               # against a real API
#
# The default target is `fake-api.mjs`, which implements docs/rest-contract.md
# in memory. That is not a stand-in for the API: it is the contract, so a
# console that passes here is a console that read the contract the same way the
# API is being written to — and it can be run before the API exists, and after
# it does, without either team waiting on the other.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
work=$(mktemp -d)
trap 'kill "${fake_pid:-0}" 2>/dev/null || true; rm -rf "$work"' EXIT

echo "building the page…" >&2
(cd "$root" && cargo run -q -p velstra-cloud-console --bin velstra-console-page) > "$work/console.html"

# The whole console is one script in one scope, so a stray brace anywhere in it
# is a page that parses to nothing and renders blank. That failure is silent in
# a build and obvious here, one second in.
sed -n '/^<script>$/,/^<\/script>$/p' "$work/console.html" | sed '1d;$d' > "$work/console.js"
node --check "$work/console.js"
echo "the script parses ($(wc -l < "$work/console.js") lines)" >&2

if [ -z "${CONSOLE_URL:-}" ]; then
  export CONSOLE_TOKEN=${CONSOLE_TOKEN:-testtoken}
  CONSOLE_PAGE="$work/console.html" FAKE_PORT=0 node "$here/fake-api.mjs" > "$work/fake.log" 2>&1 &
  fake_pid=$!
  for _ in $(seq 40); do
    port=$(sed -n 's/^listening //p' "$work/fake.log" | head -1)
    [ -n "$port" ] && break
    sleep 0.25
  done
  if [ -z "${port:-}" ]; then
    echo "the contract server never came up:" >&2; cat "$work/fake.log" >&2; exit 1
  fi
  export CONSOLE_URL="http://127.0.0.1:$port/"
fi

echo "console tests against $CONSOLE_URL"
if ! node "$here/console.test.mjs"; then
  if [ -f "$work/fake.log" ]; then echo "--- the API said:" >&2; tail -40 "$work/fake.log" >&2; fi
  exit 1
fi
