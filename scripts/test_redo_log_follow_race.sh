#!/usr/bin/env bash
set -euo pipefail

# Regression test: `redo-log -f <target>` can start before `redo <target>` has
# recorded the target in the state DB, and should still follow the build log.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

export PATH="$ROOT/target/debug:/usr/bin:/bin"
export REDO_LOG=1

cat >x.do <<'EOF'
echo ok >"$3"
EOF

cat >slow.do <<'EOF'
echo start >&2
sleep 0.2
echo middle >&2
sleep 0.2
echo end >&2
echo ok >"$3"
EOF

TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-20}"
if [ "$TIMEOUT_SECS" -gt 20 ]; then TIMEOUT_SECS=20; fi

set +e
# Create the state DB first so the follow race covers "target not yet recorded",
# not concurrent DB schema creation.
REDO=1 redo x >/dev/null 2>&1 || { echo "FAIL: setup redo x failed" >&2; exit 29; }

redo-log -f slow >follow.out &
logpid=$!

# Start the build shortly after (must be within redo-log's follow grace window).
# Force non-toplevel behavior so we don't spawn a competing internal redo-log,
# but still keep per-target log capture enabled via REDO_LOG=1.
REDO=1 redo slow >/dev/null 2>&1 &
redopid=$!

timed_out="$tmp/timed_out"
(
  sleep "$TIMEOUT_SECS"
  echo "FAIL: redo-log follow race test timed out (${TIMEOUT_SECS}s)" >&2
  echo 1 >"$timed_out"
  kill -KILL "$logpid" "$redopid" 2>/dev/null || true
) &
watchdog=$!

wait "$redopid"
rv_redo=$?
wait "$logpid"
rv_log=$?

kill "$watchdog" 2>/dev/null || true
wait "$watchdog" 2>/dev/null || true
set -e

[ ! -e "$timed_out" ] || exit 30
[ "$rv_redo" -eq 0 ] || { echo "FAIL: redo failed (exit $rv_redo)" >&2; exit 31; }
[ "$rv_log" -eq 0 ] || { echo "FAIL: redo-log failed (exit $rv_log)" >&2; exit 32; }

grep -q 'start' follow.out || { echo "FAIL: missing start in follow output" >&2; cat follow.out >&2; exit 33; }
grep -q 'middle' follow.out || { echo "FAIL: missing middle in follow output" >&2; cat follow.out >&2; exit 34; }
grep -q 'end' follow.out || { echo "FAIL: missing end in follow output" >&2; cat follow.out >&2; exit 35; }

exit 0

