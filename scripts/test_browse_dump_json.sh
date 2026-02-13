#!/usr/bin/env bash
set -euo pipefail

# Basic graph dump test for redo-browse.
# - Create a small redo project
# - Build it so deps are recorded in .redo/db.sqlite3
# - Ask redo-browse to dump a subgraph JSON
# - Assert expected nodes appear

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

cat >x.do <<'EOF'
set -euo pipefail
echo x >"$3"
EOF

cat >a.do <<'EOF'
set -euo pipefail
redo-ifchange x
echo a >"$3"
EOF

cat >b.do <<'EOF'
set -euo pipefail
echo b >"$3"
EOF

cat >all.do <<'EOF'
set -euo pipefail
redo-ifchange a b
echo all >"$3"
EOF

export PATH="$ROOT/target/debug:/usr/bin:/bin"
export REDO_LOG=0

redo -j2 all >/dev/null 2>&1 || {
  echo "FAIL: expected initial build to succeed" >&2
  exit 10
}

redo-browse --dump-json all --depth 3 --dir both >graph.json || {
  echo "FAIL: redo-browse --dump-json failed" >&2
  exit 11
}

grep -q '"root":"all"' graph.json || { echo "FAIL: missing root" >&2; exit 20; }
grep -q '"name":"a"' graph.json || { echo "FAIL: missing node a" >&2; exit 21; }
grep -q '"name":"b"' graph.json || { echo "FAIL: missing node b" >&2; exit 22; }
grep -q '"name":"x"' graph.json || { echo "FAIL: missing node x" >&2; exit 23; }
grep -q '"source":"a"' graph.json || { echo "FAIL: missing edge source a" >&2; exit 24; }

exit 0

