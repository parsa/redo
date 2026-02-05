#!/usr/bin/env bash
set -euo pipefail

# Test that SIGINT causes redo to exit 200 and not leave stuck locks.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

cat >sleepy.do <<'EOF'
sleep 5
echo ok >"$3"
EOF

export PATH="$ROOT/target/debug:/usr/bin:/bin"

TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-20}"
if [ "$TIMEOUT_SECS" -gt 20 ]; then TIMEOUT_SECS=20; fi
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
fi

set +e
out="$tmp/redo.out"
redo -j2 sleepy >"$out" 2>&1 &
pid=$!
timed_out="$tmp/timed_out"
(
  sleep "$TIMEOUT_SECS"
  echo "FAIL: redo did not exit promptly after SIGINT (timeout ${TIMEOUT_SECS}s)" >&2
  echo 1 >"$timed_out"
  kill -KILL "$pid" 2>/dev/null || true
) &
watchdog=$!
sleep 0.5
kill -INT "-$pid" 2>/dev/null || kill -INT "$pid" 2>/dev/null
wait "$pid"
rv=$?
kill "$watchdog" 2>/dev/null || true
wait "$watchdog" 2>/dev/null || true
set -e

[ ! -e "$timed_out" ] || exit 13
[ "$rv" -eq 200 ] || {
  echo "FAIL: expected exit 200 on SIGINT, got $rv" >&2
  echo "--- redo output (tail) ---" >&2
  tail -200 "$out" >&2 || true
  exit 11
}

# Ensure a follow-up run works (no stuck locks).
if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "$TIMEOUT_SECS" redo sleepy >/dev/null 2>&1 || {
    echo "FAIL: redo did not recover after SIGINT (timeout or error)" >&2
    exit 12
  }
else
  redo sleepy >/dev/null 2>&1 || { echo "FAIL: redo did not recover after SIGINT" >&2; exit 12; }
fi

exit 0

