#!/usr/bin/env bash
set -euo pipefail

# Job pool no-starvation test:
# With -j2 and pool depth 1, one pooled job should run while another pooled job
# waits *without consuming the second job slot*, allowing an unrelated job to run.
#
# We detect starvation by having `a` (pooled) require that `c` completes while
# `a` is still running. Without proper pool scheduling, `b` (pooled) will start
# immediately, occupying the second job slot and preventing `c` from running
# until after `a` finishes -> `a` fails.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

cat >a.do <<'EOF'
# redo-pool: pool1 1
set -euo pipefail
touch a_started

# Give `c` time to run concurrently (it must not be starved by pooled waiting).
i=0
while [ "$i" -lt 10 ]; do
  [ -e c_done ] && break
  i=$((i + 1))
  sleep 0.1
done
[ -e c_done ] || {
  echo "FAIL: c did not run while a was in progress (job slot starvation?)" >&2
  exit 66
}

sleep 0.2
echo a_ok >"$3"
EOF

cat >b.do <<'EOF'
# redo-pool: pool1 1
set -euo pipefail
# Hold a job slot long enough to starve `c` when pool scheduling is missing.
sleep 1.5
echo b_ok >"$3"
EOF

cat >c.do <<'EOF'
set -euo pipefail
echo c_ok >"$3"
touch c_done
EOF

cat >all.do <<'EOF'
set -euo pipefail
redo-ifchange a b c
echo all_ok >"$3"
EOF

export PATH="$ROOT/target/debug:/usr/bin:/bin"
export REDO_LOG=0

redo -j2 all >/dev/null 2>&1 || {
  echo "FAIL: expected redo -j2 all to succeed (no starvation)" >&2
  exit 10
}

grep -q '^a_ok$' a || { echo "FAIL: target a output mismatch" >&2; exit 11; }
grep -q '^b_ok$' b || { echo "FAIL: target b output mismatch" >&2; exit 12; }
grep -q '^c_ok$' c || { echo "FAIL: target c output mismatch" >&2; exit 13; }
grep -q '^all_ok$' all || { echo "FAIL: target all output mismatch" >&2; exit 14; }

exit 0

