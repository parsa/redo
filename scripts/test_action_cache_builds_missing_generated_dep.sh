#!/usr/bin/env bash
set -euo pipefail

# Action cache hybrid dep behavior test:
# - Warm cache for `foo` which depends on generated `bar`.
# - Delete both `foo` and `bar`.
# - Rebuild `foo` via redo-ifchange; expect:
#   - `foo` restored without rerunning foo.do (cache hit)
#   - missing generated dep `bar` exists afterwards
#
# Note: `bar` is a directory target so its redo stamp stays stable across rebuilds
# (STAMP_DIR="dir"), allowing `foo` to hit cache even after `bar` is recreated.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

cat >bar.do <<'EOF'
mkdir -p "$3"
echo bar >"$3/data.txt"
EOF

cat >foo.do <<'EOF'
echo run >> foo.log
redo-ifchange bar
echo foo >"$3"
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

# Warm cache.
run_ifchange foo || { echo "FAIL: expected initial redo-ifchange foo to succeed" >&2; exit 10; }
grep -q '^foo$' foo || { echo "FAIL: expected initial output 'foo' in foo" >&2; exit 11; }
[ -d bar ] || { echo "FAIL: expected bar to exist after warm build" >&2; exit 12; }

lines1="$(wc -l < foo.log | tr -d ' ')"
[ "$lines1" -eq 1 ] || { echo "FAIL: expected 1 run recorded after warm, got $lines1" >&2; exit 13; }

rm -f foo
rm -rf bar

# Rebuild; should re-create missing dep and restore foo from cache without rerun.
run_ifchange foo || { echo "FAIL: expected redo-ifchange foo after wiping foo+bar to succeed" >&2; exit 14; }
[ -d bar ] || { echo "FAIL: expected missing generated dep bar to exist after cache rebuild" >&2; exit 15; }
grep -q '^foo$' foo || { echo "FAIL: expected output 'foo' in foo after cache rebuild" >&2; exit 16; }

lines2="$(wc -l < foo.log | tr -d ' ')"
[ "$lines2" -eq 1 ] || {
  echo "FAIL: expected cache hit for foo even with missing dep (runs=$lines2)" >&2
  exit 17
}

exit 0

