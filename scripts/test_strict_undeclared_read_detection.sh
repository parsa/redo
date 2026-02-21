#!/usr/bin/env bash
set -euo pipefail

# Strict mode: detect undeclared reads (or clearly report tracing unavailable).

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

run_ifchange_capture() {
  local target="$1"
  local err="$2"
  if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" "$TIMEOUT_SECS" env \
      REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 \
      REDO_STRICT=1 \
      redo-ifchange "$target" > /dev/null 2>"$err"
  else
    env REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 REDO_STRICT=1 \
      redo-ifchange "$target" > /dev/null 2>"$err"
  fi
}

printf 'secret\n' > secret.txt

cat > out.do <<'EOF'
echo run >> build.log
cat secret.txt >"$3"
EOF

run_ifchange_capture out err1.txt || { echo "FAIL: strict redo-ifchange out should succeed" >&2; exit 10; }
grep -q '^secret$' out || { echo "FAIL: expected output 'secret' in out" >&2; exit 11; }

# In strict mode, this must emit a clear diagnostic (undeclared read or tracer unavailable).
grep -q 'strict' err1.txt || {
  echo "FAIL: expected strict diagnostic in stderr (no 'strict' substring found)" >&2
  echo "stderr:" >&2
  cat err1.txt >&2
  exit 12
}

# Now strict-fail: should fail (either because undeclared read detected, or because tracing is unavailable).
rm -f out
set +e
if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "$TIMEOUT_SECS" env \
    REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 \
    REDO_STRICT=1 REDO_STRICT_FAIL=1 \
    redo-ifchange out > /dev/null 2>err2.txt
else
  env REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 REDO_STRICT=1 REDO_STRICT_FAIL=1 \
    redo-ifchange out > /dev/null 2>err2.txt
fi
rv=$?
set -e
if [ "$rv" -eq 0 ]; then
  echo "FAIL: expected REDO_STRICT_FAIL=1 to fail on strict violation / no-trace" >&2
  echo "stderr:" >&2
  cat err2.txt >&2
  exit 13
fi

exit 0

