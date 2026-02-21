#!/usr/bin/env bash
set -euo pipefail

# Action cache semantics test:
# - Targets that depend on redo-always (//ALWAYS) must never get cache hits.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

cat >out.do <<'EOF'
redo-always
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
run_ifchange out || { echo "FAIL: expected second redo-ifchange out to succeed" >&2; exit 11; }

grep -q '^hello$' out || { echo "FAIL: expected output 'hello' in out" >&2; exit 12; }

lines="$(wc -l < build.log | tr -d ' ')"
[ "$lines" -eq 2 ] || {
  echo "FAIL: expected redo-always to force rerun even with cache (runs=$lines)" >&2
  exit 13
}

exit 0

