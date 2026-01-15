#!/usr/bin/env bash
set -euo pipefail

# Standalone redo-log linearization / completeness test:
# Build several targets in parallel; each target + dependency writes many short
# stderr lines. Then assert redo-log recursion includes all lines (no loss).

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

N=100

mk_dep() {
  local i="$1"
  cat >"d${i}.do" <<EOF
for j in \$(seq 1 $N); do
  printf 'd${i} %03d\n' "\$j" >&2
done
echo "d${i}_ok" >"\$3"
EOF
}

mk_target() {
  local i="$1"
  cat >"a${i}.do" <<EOF
redo d${i}
for j in \$(seq 1 $N); do
  printf 'a${i} %03d\n' "\$j" >&2
done
echo "a${i}_ok" >"\$3"
EOF
}

for i in 1 2 3 4; do
  mk_dep "$i"
  mk_target "$i"
done

export PATH="$ROOT/target/debug:/usr/bin:/bin"
export REDO_LOG=1

redo -j4 a1 a2 a3 a4 >/dev/null 2>&1 || {
  echo "FAIL: expected parallel build to succeed" >&2
  exit 10
}

for i in 1 2 3 4; do
  redo-log -ru "a${i}" >"log_a${i}.txt" || exit 20
  c1=$(grep -c "^a${i} " "log_a${i}.txt" || true)
  c2=$(grep -c "^d${i} " "log_a${i}.txt" || true)
  [ "$c1" -eq "$N" ] || { echo "FAIL: expected $N a${i} lines, got $c1" >&2; exit 30; }
  [ "$c2" -eq "$N" ] || { echo "FAIL: expected $N d${i} lines, got $c2" >&2; exit 31; }
  grep -q "^a${i} ${N}$" "log_a${i}.txt" || { echo "FAIL: missing final a${i} marker" >&2; exit 32; }
  grep -q "^d${i} ${N}$" "log_a${i}.txt" || { echo "FAIL: missing final d${i} marker" >&2; exit 33; }
done

exit 0

