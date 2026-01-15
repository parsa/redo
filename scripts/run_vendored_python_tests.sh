#!/usr/bin/env bash
set -euo pipefail

# Run the vendored test suite (Apache-2.0) against the Rust binaries.
# This runs in a temporary copy to avoid polluting the working tree.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$ROOT"

# The full vendored suite can take a while on slower disks/CI runners.
TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-1800}"
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
fi

if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "$TIMEOUT_SECS" cargo build --workspace
else
  cargo build --workspace
fi

tmp="$(mktemp -d)"
cleanup() {
  # Some tests intentionally create read-only directories/files (eg. 205-readonly).
  # Make a best-effort attempt to make the tree writable so cleanup doesn't fail.
  chmod -R u+w "$tmp" >/dev/null 2>&1 || true
  rm -rf "$tmp" >/dev/null 2>&1 || true
}
trap cleanup EXIT

mkdir -p "$tmp/third_party"
rsync -a "$ROOT/third_party/python_redo_tests/" "$tmp/third_party/python_redo_tests/"

export PATH="$ROOT/target/debug:/usr/bin:/bin"

cd "$tmp/third_party/python_redo_tests/t"

if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "$TIMEOUT_SECS" redo -j4 all
else
  redo -j4 all
fi

exit 0

