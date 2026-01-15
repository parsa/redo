exec >&2
. ../skip-if-minimal-do.sh

rm -f a b c d follow.out *~ .*~

# Don't redirect stderr: redo-log relies on the per-target stderr logs.
redo -j4 a b c d >/dev/null &
pid=$!

sleep 0.05
if [ -n "$REDO_LOG" ]; then
    redo-log -f a >follow.out
    grep -q '^a start$' follow.out || { echo "FAIL: missing a start in follow output" >&2; exit 11; }
    grep -q '^a end$' follow.out || { echo "FAIL: missing a end in follow output" >&2; exit 12; }
else
    # When redo-log is disabled, follow output won't contain build stderr lines.
    echo "377-jobserver-follow: skipping follow checks (REDO_LOG disabled)" >&2
fi
wait "$pid" || { echo "FAIL: parallel build failed" >&2; exit 10; }

[ -e a ] && [ -e b ] && [ -e c ] && [ -e d ] || { echo "FAIL: expected outputs a b c d" >&2; exit 13; }

# Re-run to ensure jobserver tokens are returned and follow doesn't leave us stuck.
rm -f a b c d

TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-20}"
if [ "$TIMEOUT_SECS" -gt 20 ]; then TIMEOUT_SECS=20; fi
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
fi

if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "$TIMEOUT_SECS" redo -j4 a b c d >/dev/null || {
    echo "FAIL: redo did not complete promptly on second run (deadlock/tokens?)" >&2
    exit 14
  }
else
  redo -j4 a b c d >/dev/null || { echo "FAIL: redo failed on second run" >&2; exit 14; }
fi

exit 0

