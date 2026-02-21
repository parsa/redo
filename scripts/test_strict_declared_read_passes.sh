#!/usr/bin/env bash
set -euo pipefail

# Strict mode: an honest rule that declares what it reads should still work.

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

run_ifchange() {
  local target="$1"
  if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" "$TIMEOUT_SECS" env \
      REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 \
      REDO_STRICT=1 \
      redo-ifchange "$target" > /dev/null 2>err.txt
  else
    env REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 REDO_STRICT=1 \
      redo-ifchange "$target" > /dev/null 2>err.txt
  fi
}

printf 'one\n' > in.txt

cat > out.do <<'EOF'
echo run >> build.log
redo-ifchange in.txt
cat in.txt >"$3"
EOF

run_ifchange out || { echo "FAIL: strict redo-ifchange out should succeed" >&2; exit 10; }
grep -q '^one$' out || { echo "FAIL: expected output 'one' in out" >&2; exit 11; }

lines="$(wc -l < build.log | tr -d ' ')"
[ "$lines" -eq 1 ] || { echo "FAIL: expected out.do to run once (runs=$lines)" >&2; exit 12; }

exit 0

