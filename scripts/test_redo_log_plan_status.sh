#!/usr/bin/env bash
set -euo pipefail

# Verify redo-log consumes @@REDO:plan@@ and renders a Ninja-like status line:
# - denominator uses dirty_total
# - unchanged targets are counted as up-to-date (not "done")

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

export PATH="$ROOT/target/debug:/usr/bin:/bin"
export REDO_LOG=1

mkdir -p .redo
cat >.redo/plan.targets <<'EOF'
dep
hello
EOF

# Feed redo-log a synthetic stream:
# - plan says 1 dirty step, 1 up-to-date step
# - dep is unchanged (should NOT bump done)
# - hello starts, then we wait long enough to force a status line (via a debug tick)
python3 - <<'PY' | env REDO_LOG_TOP_TARGETS=all redo-log --follow --status --no-details --no-pretty - >out.txt 2>&1
import time
print('@@REDO:plan:1:0.0@@ dirty=1 total=2 uptodate=1', flush=True)
print('@@REDO:unchanged:1:0.0@@ dep', flush=True)
print('@@REDO:do:1:0.0@@ hello', flush=True)
time.sleep(1.2)
print('@@REDO:debug:1:0.0@@ tick', flush=True)
print('@@REDO:done:1:0.0@@ 0 hello', flush=True)
PY

# Normalize CR status updates into newline-separated lines for grepping.
tr '\r' '\n' <out.txt >out.norm.txt

grep -q 'redo 1/1 steps (0 done), 1 running' out.norm.txt || {
  echo "FAIL: expected dirty-step status line with 0 done while running" >&2
  echo "--- out.norm.txt ---" >&2
  cat out.norm.txt >&2 || true
  exit 10
}

grep -q 'up-to-date' out.norm.txt || {
  echo "FAIL: expected up-to-date count in status line" >&2
  echo "--- out.norm.txt ---" >&2
  cat out.norm.txt >&2 || true
  exit 11
}

exit 0

