#!/usr/bin/env bash
set -euo pipefail

# Regression test for DJB atomic.md "new target" rule:
# When a missing file is treated as a target, redo must persist that decision
# to `.redo` before the build script runs (so later file creation can't flip it).

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

cat >foo.do <<'EOF'
echo started > started.marker
sleep 10
echo ok >"$3"
EOF

export PATH="$ROOT/target/debug:/usr/bin:/bin"

set +e
redo foo >/dev/null 2>&1 &
pid=$!
set -e

# Wait until the .do script has started (deterministic point after intent should be committed).
for _ in $(seq 1 100); do
  [ -e started.marker ] && break
  sleep 0.05
done
[ -e started.marker ] || { echo "FAIL: .do script never started" >&2; kill -KILL "$pid" 2>/dev/null || true; exit 11; }

# Kill redo mid-build (we only care that intent was persisted before file creation/commit).
kill -KILL "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true

python3 - <<'PY'
import sqlite3, sys
db = ".redo/db.sqlite3"
con = sqlite3.connect(db)
row = con.execute("select is_generated from Files where name='foo'").fetchone()
if row is None:
    print("FAIL: no DB row for 'foo'", file=sys.stderr)
    sys.exit(12)
val = row[0] or 0
if int(val) != 1:
    print(f"FAIL: expected Files.is_generated=1 for missing target intent; got {val}", file=sys.stderr)
    sys.exit(13)
PY

exit 0

