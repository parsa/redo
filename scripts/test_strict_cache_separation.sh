#!/usr/bin/env bash
set -euo pipefail

# Strict mode cache separation:
# - Non-strict builds populate the default cache namespace.
# - Strict builds must not reuse those entries (separate ActionKey policy domain).

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

export PATH="$ROOT/target/debug:/usr/bin:/bin"

TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-20}"
if [ "$TIMEOUT_SECS" -gt 30 ]; then TIMEOUT_SECS=30; fi
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

cat > out.do <<'EOF'
echo run >> build.log
echo hello >"$3"
EOF

# Warm non-strict cache.
run_cmd env REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 redo-ifchange out || {
  echo "FAIL: expected non-strict redo-ifchange out to succeed" >&2
  exit 10
}

lines1="$(wc -l < build.log | tr -d ' ')"
[ "$lines1" -eq 1 ] || { echo "FAIL: expected 1 run recorded after warm, got $lines1" >&2; exit 11; }

rm -f out

# Strict build must not hit non-strict cache; it should execute out.do again.
run_cmd env REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 REDO_STRICT=1 redo-ifchange out || {
  echo "FAIL: expected strict redo-ifchange out to succeed" >&2
  exit 12
}

lines2="$(wc -l < build.log | tr -d ' ')"
[ "$lines2" -eq 2 ] || {
  echo "FAIL: expected strict build to not reuse non-strict cache (runs=$lines2)" >&2
  exit 13
}

exit 0

