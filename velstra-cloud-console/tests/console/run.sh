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

# One scope means one namespace, and a second top-level `function pick` does not
# collide loudly — it silently replaces the first, everywhere, including inside
# code written months earlier that has no idea the name was taken. That happened:
# a selection helper called `pick` replaced the accessor every model reader uses,
# and the console listed two projects whose names were both the empty string.
#
# `node --check` cannot see it (a redeclared function is legal JavaScript), so
# it is checked here, where the files are already concatenated.
# `-a`: the page carries mark glyphs, and grep calls a file with those bytes
# binary and prints "binary file matches" instead of the matches — which would
# make this check quietly pass for ever.
dupes=$(grep -haoE '^(function|const|let) [A-Za-z_$][A-Za-z0-9_$]*' "$work/console.js" \
  | awk '{print $2}' | sort | uniq -d)
if [ -n "$dupes" ]; then
  echo "two top-level declarations share a name, and the second wins everywhere:" >&2
  echo "$dupes" | sed 's/^/  /' >&2
  exit 1
fi
echo "no two top-level names collide" >&2
# The suite checks the names the page reads out of an object against the
# recorded shape of the API's answers — see `shapes.mjs`.
export CONSOLE_JS="$work/console.js"

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
