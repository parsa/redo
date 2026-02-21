#!/usr/bin/env bash
set -euo pipefail

# Action cache basic hit test:
# - Build once to populate cache.
# - Delete only the output.
# - Rebuild via redo-ifchange and ensure the .do script did NOT run again.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

cat >out.do <<'EOF'
echo run >> build.log
echo hello >"$3"
EOF

export PATH="$ROOT/target/debug:/usr/bin:/bin"

TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-20}"
if [ "$TIMEOUT_SECS" -gt 20 ]; then TIMEOUT_SECS=20; fi
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
fi

run_ifchange() {
  if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" "$TIMEOUT_SECS" redo-ifchange "$1" >/dev/null 2>&1
  else
    redo-ifchange "$1" >/dev/null 2>&1
  fi
}

run_ifchange out || { echo "FAIL: expected initial redo-ifchange out to succeed" >&2; exit 10; }
grep -q '^hello$' out || { echo "FAIL: expected initial output 'hello' in out" >&2; exit 11; }

lines1="$(wc -l < build.log | tr -d ' ')"
[ "$lines1" -eq 1 ] || { echo "FAIL: expected 1 run recorded after first build, got $lines1" >&2; exit 12; }

rm -f out

run_ifchange out || { echo "FAIL: expected redo-ifchange out after wipe to succeed" >&2; exit 13; }
grep -q '^hello$' out || { echo "FAIL: expected output 'hello' in out after wipe rebuild" >&2; exit 14; }

lines2="$(wc -l < build.log | tr -d ' ')"
[ "$lines2" -eq 1 ] || {
  echo "FAIL: expected cache hit to avoid rerunning out.do after wiping output (runs=$lines2)" >&2
  exit 15
}

exit 0

