#!/usr/bin/env bash
set -euo pipefail

# Bulk deps input test (newline-separated):
# - Exercises paths with spaces (must not be split)
# - Exercises true parallelism when deps are supplied via --from-file in a single call
# - Exercises correct dependency tracking (changing the spaced path triggers rebuild)

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

mkdir -p "inc dir"
printf 'one\n' >"inc dir/some header.h"

cat >a.do <<'EOF'
touch started_a
i=0
while [ "$i" -lt 50 ]; do
  [ -e started_b ] && break
  i=$((i + 1))
  sleep 0.1
done
[ -e started_b ] || { echo "FAIL: a timed out waiting for started_b (no true parallelism?)" >&2; exit 55; }
echo a_ok >"$3"
EOF

cat >b.do <<'EOF'
touch started_b
i=0
while [ "$i" -lt 50 ]; do
  [ -e started_a ] && break
  i=$((i + 1))
  sleep 0.1
done
[ -e started_a ] || { echo "FAIL: b timed out waiting for started_a (no true parallelism?)" >&2; exit 56; }
echo b_ok >"$3"
EOF

cat >all.do <<'EOF'
# Build deps from a newline-separated list.
printf 'a\nb\ninc dir/some header.h\n' >deps.txt

# One bulk call; must preserve spaces in the filename.
redo-ifchange --from-file deps.txt

# Write an output that depends on the spaced file's contents.
cat "inc dir/some header.h" >"$3"
EOF

export PATH="$ROOT/target/debug:/usr/bin:/bin"

TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-20}"
if [ "$TIMEOUT_SECS" -gt 20 ]; then TIMEOUT_SECS=20; fi
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
fi

run_redo() {
  if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" "$TIMEOUT_SECS" redo -j2 all >/dev/null 2>&1
  else
    redo -j2 all >/dev/null 2>&1
  fi
}

run_redo || {
  echo "FAIL: expected redo -j2 all (with redo-ifchange --from-file) to succeed" >&2
  exit 10
}

grep -q '^one$' all || { echo "FAIL: expected initial output 'one' in target all" >&2; exit 11; }

# Changing the spaced dependency should force a rebuild of `all`.
printf 'two\n' >"inc dir/some header.h"
run_redo || {
  echo "FAIL: expected redo -j2 all to succeed after changing spaced dependency" >&2
  exit 12
}
grep -q '^two$' all || { echo "FAIL: expected updated output 'two' in target all" >&2; exit 13; }

exit 0

