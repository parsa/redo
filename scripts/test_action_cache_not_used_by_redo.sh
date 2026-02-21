#!/usr/bin/env bash
set -euo pipefail

# Action cache semantics test:
# - Cache consult must NOT happen under `redo` (only under redo-ifchange).

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

run_cmd() {
  if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" "$TIMEOUT_SECS" "$@" >/dev/null 2>&1
  else
    "$@" >/dev/null 2>&1
  fi
}

# Warm (and populate cache) via redo-ifchange.
run_cmd redo-ifchange out || { echo "FAIL: expected initial redo-ifchange out to succeed" >&2; exit 10; }
lines1="$(wc -l < build.log | tr -d ' ')"
[ "$lines1" -eq 1 ] || { echo "FAIL: expected 1 run recorded after warm, got $lines1" >&2; exit 11; }

rm -f out

# `redo` must force rebuild (must run out.do again even if cache exists).
run_cmd redo out || { echo "FAIL: expected redo out to succeed" >&2; exit 12; }
grep -q '^hello$' out || { echo "FAIL: expected output 'hello' in out after redo" >&2; exit 13; }

lines2="$(wc -l < build.log | tr -d ' ')"
[ "$lines2" -eq 2 ] || {
  echo "FAIL: expected redo to force rebuild (runs=$lines2)" >&2
  exit 14
}

exit 0

