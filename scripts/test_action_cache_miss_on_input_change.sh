#!/usr/bin/env bash
set -euo pipefail

# Action cache correctness test:
# - Changing a declared input must cause a cache miss / rebuild (no false hit).

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

printf 'one\n' > in.txt

cat >out.do <<'EOF'
echo run >> build.log
redo-ifchange in.txt
cat in.txt >"$3"
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
grep -q '^one$' out || { echo "FAIL: expected initial output 'one' in out" >&2; exit 11; }

lines1="$(wc -l < build.log | tr -d ' ')"
[ "$lines1" -eq 1 ] || { echo "FAIL: expected 1 run recorded after first build, got $lines1" >&2; exit 12; }

printf 'two\n' > in.txt

run_ifchange out || { echo "FAIL: expected redo-ifchange out after input change to succeed" >&2; exit 13; }
grep -q '^two$' out || { echo "FAIL: expected updated output 'two' in out after input change" >&2; exit 14; }

lines2="$(wc -l < build.log | tr -d ' ')"
[ "$lines2" -eq 2 ] || {
  echo "FAIL: expected rebuild (no false cache hit) after input change (runs=$lines2)" >&2
  exit 15
}

exit 0

