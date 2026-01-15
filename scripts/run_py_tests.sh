#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$ROOT"
TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-120}"
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
fi

if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "$TIMEOUT_SECS" cargo build
else
  cargo build
fi

export PATH="$ROOT/target/debug:$PATH"

# Run Rust-only tests first (standalone).
if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "$TIMEOUT_SECS" bash "$ROOT/scripts/run_tests.sh"
else
  bash "$ROOT/scripts/run_tests.sh"
fi

# Optional parity: run the upstream Python test suite from an external checkout.
PY_DIR="${REDO_PY_DIR:-}"
if [ -z "$PY_DIR" ]; then
  echo "Skipping Python parity tests (set REDO_PY_DIR to enable)." >&2
  exit 0
fi

cd "$PY_DIR"

if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "$TIMEOUT_SECS" redo -j4 test
else
  redo -j4 test
fi


