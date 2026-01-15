#!/usr/bin/env bash
set -euo pipefail

# Regression test for DJB atomic.md crash window:
# A target must not be considered up to date until redo records the build in `.redo`.
# We simulate a crash after rename($3,$1) but before the DB commit, then ensure a
# subsequent `redo-ifchange` rebuilds (ie. reruns the .do script).

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

cat >foo.do <<'EOF'
echo run >> build.log
echo v1 >"$3"
EOF

export PATH="$ROOT/target/debug:/usr/bin:/bin"

export REDO_TEST_FSYNC_MARKER="$tmp/fsync.marker"

set +e
REDO_TEST_CRASH_AFTER_RENAME=1 redo foo >/dev/null 2>&1
rv=$?
set -e

[ "$rv" -ne 0 ] || { echo "FAIL: expected redo to crash (non-zero exit)" >&2; exit 11; }
[ -f foo ] || { echo "FAIL: expected foo to exist after rename crash window" >&2; exit 12; }
[ -f build.log ] || { echo "FAIL: expected build.log to exist" >&2; exit 13; }

lines="$(wc -l < build.log | tr -d ' ')"
[ "$lines" -eq 1 ] || { echo "FAIL: expected 1 run recorded before crash, got $lines" >&2; exit 14; }

# Now ensure redo-ifchange rebuilds (runs foo.do again) because `.redo` did not record success.
redo-ifchange foo >/dev/null 2>&1

lines2="$(wc -l < build.log | tr -d ' ')"
[ "$lines2" -eq 2 ] || {
  echo "FAIL: expected redo-ifchange to rebuild after crash window (run count 2), got $lines2" >&2
  exit 15
}

[ -f "$REDO_TEST_FSYNC_MARKER" ] || { echo "FAIL: expected fsync marker file to exist" >&2; exit 16; }
fslines="$(wc -l < "$REDO_TEST_FSYNC_MARKER" | tr -d ' ')"
[ "$fslines" -ge 1 ] || { echo "FAIL: expected at least 1 fsync marker line, got $fslines" >&2; exit 17; }

exit 0

