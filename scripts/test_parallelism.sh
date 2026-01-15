#!/usr/bin/env bash
set -euo pipefail

# Standalone parallelism test:
# Requires *true* parallel execution under -j>1. Each target waits for the other
# to start; serial execution will time out.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

cat >a.do <<'EOF'
touch started_a
i=0
while [ "$i" -lt 50 ]; do
  [ -e started_b ] && break
  i=$((i + 1))
  sleep 0.1
done
[ -e started_b ] || { echo "FAIL: a timed out waiting for started_b (no true parallelism?)" >&2; exit 55; }
echo a_ok >"$3"
EOF

cat >b.do <<'EOF'
touch started_b
i=0
while [ "$i" -lt 50 ]; do
  [ -e started_a ] && break
  i=$((i + 1))
  sleep 0.1
done
[ -e started_a ] || { echo "FAIL: b timed out waiting for started_a (no true parallelism?)" >&2; exit 56; }
echo b_ok >"$3"
EOF

export PATH="$ROOT/target/debug:/usr/bin:/bin"

TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-20}"
if [ "$TIMEOUT_SECS" -gt 20 ]; then TIMEOUT_SECS=20; fi
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
fi

if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "$TIMEOUT_SECS" redo -j2 a b >/dev/null 2>&1 || {
    echo "FAIL: expected redo -j2 a b to succeed (requires true parallelism)" >&2
    exit 10
  }
else
  redo -j2 a b >/dev/null 2>&1 || {
    echo "FAIL: expected redo -j2 a b to succeed (requires true parallelism)" >&2
    exit 10
  }
fi

[ -e a ] || { echo "FAIL: target a did not produce output file" >&2; exit 11; }
[ -e b ] || { echo "FAIL: target b did not produce output file" >&2; exit 12; }
grep -q '^a_ok$' a || { echo "FAIL: target a output mismatch" >&2; exit 13; }
grep -q '^b_ok$' b || { echo "FAIL: target b output mismatch" >&2; exit 14; }

exit 0

