exec >&2
. ../skip-if-minimal-do.sh

rm -f noisy *~ .*~

TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-20}"
if [ "$TIMEOUT_SECS" -gt 20 ]; then TIMEOUT_SECS=20; fi
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
fi

cmd='set -o pipefail; REDO_LOG=1 redo noisy 2>&1 | head -1 >/dev/null'
cmd='set -o pipefail; env -u REDO -u REDO_BASE -u REDO_STARTDIR -u REDO_RUNID -u REDO_PWD -u REDO_TARGET -u REDO_DEPTH -u REDO_LOG_INODE REDO_LOG=1 redo noisy 2>&1 | head -1 >/dev/null'
if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" "$TIMEOUT_SECS" bash -c "$cmd" || {
        echo "FAIL: redo noisy pipeline failed (SIGPIPE/BrokenPipe?)" >&2
        exit 10
    }
else
    bash -c "$cmd" || {
        echo "FAIL: redo noisy pipeline failed (SIGPIPE/BrokenPipe?)" >&2
        exit 10
    }
fi

[ -e noisy ] || { echo "FAIL: expected target noisy to be produced" >&2; exit 11; }

exit 0

