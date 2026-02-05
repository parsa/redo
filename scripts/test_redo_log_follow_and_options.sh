#!/usr/bin/env bash
set -euo pipefail

# Standalone redo-log behavior checks:
# - pretty vs raw output (no @@REDO: in default; @@REDO: in --no-pretty)
# - --no-details suppresses build output
# - -f follow sees progress lines from a slow build

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

export PATH="$ROOT/target/debug:/usr/bin:/bin"
export REDO_LOG=1

cat >x.do <<'EOF'
echo "x stdout"
echo "x stderr" >&2
EOF

redo x >/dev/null 2>&1
[ "$(cat x)" = "x stdout" ] || { echo "FAIL: expected x file to contain stdout output" >&2; exit 10; }

# 1) Default output should be pretty (no @@REDO meta-lines).
redo-log x >out.txt
if grep '@@REDO:' out.txt >/dev/null; then
  echo "FAIL: redo-log default output should be pretty (no @@REDO: meta-lines)" >&2
  exit 11
fi

# 2) --no-pretty should output raw meta-lines.
redo-log --no-pretty x >raw.txt
if ! grep '@@REDO:' raw.txt >/dev/null; then
  echo "FAIL: redo-log --no-pretty should output raw @@REDO: meta-lines" >&2
  exit 12
fi

# 3) --no-details should suppress build output (stderr lines).
redo-log --no-details x >nodetails.txt
if grep 'x stderr' nodetails.txt >/dev/null; then
  echo "FAIL: redo-log --no-details should suppress build stderr lines" >&2
  exit 13
fi

# 4) Follow mode should include ordered markers from a slow target build.
cat >slow.do <<'EOF'
echo start >&2
sleep 0.2
echo middle >&2
sleep 0.2
echo end >&2
echo ok >"$3"
EOF

TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-20}"
if [ "$TIMEOUT_SECS" -gt 20 ]; then TIMEOUT_SECS=20; fi

set +e
redo slow >/dev/null 2>&1 &
redopid=$!
sleep 0.05
redo-log -f slow >follow.out &
logpid=$!

timed_out="$tmp/timed_out"
(
  sleep "$TIMEOUT_SECS"
  echo "FAIL: redo-log follow test timed out (${TIMEOUT_SECS}s)" >&2
  echo 1 >"$timed_out"
  kill -KILL "$logpid" 2>/dev/null || true
  kill -KILL "-$redopid" 2>/dev/null || kill -KILL "$redopid" 2>/dev/null || true
) &
watchdog=$!

wait "$redopid"
rv_redo=$?
wait "$logpid"
rv_log=$?

kill "$watchdog" 2>/dev/null || true
wait "$watchdog" 2>/dev/null || true
set -e

[ ! -e "$timed_out" ] || exit 20
[ "$rv_redo" -eq 0 ] || { echo "FAIL: redo slow failed (exit $rv_redo)" >&2; exit 20; }
[ "$rv_log" -eq 0 ] || {
  echo "FAIL: redo-log -f slow failed (exit $rv_log)" >&2
  echo "--- follow.out ---" >&2
  cat follow.out >&2 || true
  exit 20
}

grep -q 'start' follow.out || {
  echo "FAIL: missing start in follow output" >&2
  echo "--- follow.out ---" >&2
  cat follow.out >&2 || true
  echo "--- .redo dir listing ---" >&2
  (ls -la .redo >&2) || true
  exit 21
}
grep -q 'middle' follow.out || {
  echo "FAIL: missing middle in follow output" >&2
  echo "--- follow.out ---" >&2
  cat follow.out >&2 || true
  exit 22
}
grep -q 'end' follow.out || {
  echo "FAIL: missing end in follow output" >&2
  echo "--- follow.out ---" >&2
  cat follow.out >&2 || true
  exit 23
}

exit 0

