#!/usr/bin/env bash
set -euo pipefail

# Job pool serialization test:
# Two targets in the same depth-1 pool must not overlap.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

cat >a.do <<'EOF'
# redo-pool: pool1 1
set -euo pipefail
mkdir pool_lockdir 2>/dev/null || {
  echo "FAIL: job pool overlap detected (a)" >&2
  exit 55
}
sleep 0.4
rmdir pool_lockdir
echo a_ok >"$3"
EOF

cat >b.do <<'EOF'
# redo-pool: pool1 1
set -euo pipefail
mkdir pool_lockdir 2>/dev/null || {
  echo "FAIL: job pool overlap detected (b)" >&2
  exit 56
}
sleep 0.4
rmdir pool_lockdir
echo b_ok >"$3"
EOF

export PATH="$ROOT/target/debug:/usr/bin:/bin"
export REDO_LOG=0

redo -j2 a b >/dev/null 2>&1 || {
  echo "FAIL: expected redo -j2 a b to succeed (pool serialization)" >&2
  exit 10
}

grep -q '^a_ok$' a || { echo "FAIL: target a output mismatch" >&2; exit 11; }
grep -q '^b_ok$' b || { echo "FAIL: target b output mismatch" >&2; exit 12; }

exit 0

