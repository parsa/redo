#!/usr/bin/env bash
set -euo pipefail

# Strict mode + remote cache:
# - If tracing is available and the action is trace-clean, strict mode should
#   push to remote and a clean workspace should hit remote (no .do rerun).
# - If tracing is unavailable, strict mode must emit a clear diagnostic and
#   treat the action as non-cacheable (so the clean workspace will rerun).

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

run_ifchange_capture() {
  local target="$1"
  local err="$2"
  if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" "$TIMEOUT_SECS" env \
      REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 \
      REDO_STRICT=1 \
      REDO_ACTION_CACHE_REMOTE_URL="$URL" \
      "$@" redo-ifchange "$target" > /dev/null 2>"$err"
  else
    env REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 REDO_STRICT=1 REDO_ACTION_CACHE_REMOTE_URL="$URL" \
      "$@" redo-ifchange "$target" > /dev/null 2>"$err"
  fi
}

ws1="$tmp/ws1"
mkdir -p "$ws1"
cd "$ws1"

cat > out.do <<'EOF'
echo run >> build.log
echo hello >"$3"
EOF

run_ifchange_capture out err1.txt REDO_ACTION_CACHE_REMOTE_PUSH=1 || {
  echo "FAIL: strict ws1 build should succeed" >&2
  exit 11
}

ws2="$tmp/ws2"
mkdir -p "$ws2"
cd "$ws2"

cat > out.do <<'EOF'
echo run >> build.log
echo hello >"$3"
EOF

run_ifchange_capture out err2.txt || {
  echo "FAIL: strict ws2 build should succeed" >&2
  exit 12
}

if [ -f build.log ]; then
  # Reran: acceptable only if strict mode clearly reported it could not trace/prove.
  lines="$(wc -l < build.log | tr -d ' ')"
  [ "$lines" -eq 1 ] || { echo "FAIL: expected exactly 1 run in ws2 (runs=$lines)" >&2; exit 13; }
  grep -q 'strict' err2.txt || {
    echo "FAIL: expected strict diagnostic when strict remote reuse did not happen" >&2
    echo "stderr:" >&2
    cat err2.txt >&2
    exit 14
  }
else
  # Remote hit: out.do did not rerun.
  :
fi

exit 0

