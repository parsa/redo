#!/usr/bin/env bash
set -euo pipefail

# Run the Rust workspace tests plus our self-contained shell-script integration tests.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$ROOT"

TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-120}"
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
fi

if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "$TIMEOUT_SECS" cargo test --workspace
  "$TIMEOUT_BIN" "$TIMEOUT_SECS" cargo build --workspace
else
  cargo test --workspace
  cargo build --workspace
fi

for t in scripts/test_*.sh; do
  [ -x "$t" ] || chmod +x "$t" 2>/dev/null || true
  if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" "$TIMEOUT_SECS" bash "$t"
  else
    bash "$t"
  fi
done

exit 0

