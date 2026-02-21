#!/usr/bin/env bash
set -euo pipefail

# Remote action cache basic hit test:
# - Start redo-cache-server.
# - Build once in workspace A with remote push enabled.
# - Build in workspace B (fresh dir, no .redo) with remote pull enabled and
#   ensure the .do script did NOT run.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
server_pid=""
cleanup() {
  if [ -n "${server_pid}" ]; then
    kill "${server_pid}" >/dev/null 2>&1 || true
    wait "${server_pid}" >/dev/null 2>&1 || true
  fi
  rm -rf "${tmp}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

export PATH="$ROOT/target/debug:/usr/bin:/bin"

server_root="$tmp/server"
url_file="$tmp/url.txt"
mkdir -p "$server_root"

redo-cache-server --listen 127.0.0.1:0 --root "$server_root" --write-url-file "$url_file" >/dev/null 2>&1 &
server_pid="$!"

for _ in $(seq 1 200); do
  if [ -s "$url_file" ]; then
    break
  fi
  sleep 0.05
done

URL="$(tr -d '\r\n' < "$url_file" || true)"
[ -n "$URL" ] || { echo "FAIL: server URL not written" >&2; exit 10; }

TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-20}"
if [ "$TIMEOUT_SECS" -gt 30 ]; then TIMEOUT_SECS=30; fi
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
fi

run_ifchange() {
  if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" "$TIMEOUT_SECS" redo-ifchange "$1" >/dev/null 2>&1
  else
    redo-ifchange "$1" >/dev/null 2>&1
  fi
}

# Workspace A: build and push.
ws1="$tmp/ws1"
mkdir -p "$ws1"
cd "$ws1"

cat >out.do <<'EOF'
echo run >> build.log
echo hello >"$3"
EOF

export REDO_ACTION_CACHE_REMOTE_URL="$URL"
export REDO_ACTION_CACHE_REMOTE_PUSH=1

run_ifchange out || { echo "FAIL: expected initial redo-ifchange out (ws1) to succeed" >&2; exit 11; }
grep -q '^hello$' out || { echo "FAIL: expected output 'hello' in out (ws1)" >&2; exit 12; }

lines1="$(wc -l < build.log | tr -d ' ')"
[ "$lines1" -eq 1 ] || { echo "FAIL: expected 1 run recorded in ws1, got $lines1" >&2; exit 13; }

# Workspace B: fresh dir, pull only; should hit remote and not run the script.
ws2="$tmp/ws2"
mkdir -p "$ws2"
cd "$ws2"

cat >out.do <<'EOF'
echo run >> build.log
echo hello >"$3"
EOF

unset REDO_ACTION_CACHE_REMOTE_PUSH
export REDO_ACTION_CACHE_REMOTE_URL="$URL"

run_ifchange out || { echo "FAIL: expected redo-ifchange out (ws2) to succeed" >&2; exit 14; }
grep -q '^hello$' out || { echo "FAIL: expected output 'hello' in out (ws2)" >&2; exit 15; }

if [ -f build.log ]; then
  lines2="$(wc -l < build.log | tr -d ' ')"
else
  lines2="0"
fi

[ "$lines2" -eq 0 ] || {
  echo "FAIL: expected remote cache hit to avoid rerunning out.do in ws2 (runs=$lines2)" >&2
  exit 16
}

exit 0

