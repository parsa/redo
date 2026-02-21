#!/usr/bin/env bash
set -euo pipefail

# Phase 4 remote execution + strict mode Linux-only: strict diagnostics must
# still be emitted (undeclared reads or trace-unavailable markers), and the job
# should have run remotely (no local side effects).

if [ "$(uname -s)" != "Linux" ]; then
  exit 0
fi

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

redo-cache-server --listen 127.0.0.1:0 --root "$server_root" --write-url-file "$url_file" --enable-exec >/dev/null 2>&1 &
server_pid="$!"

for _ in $(seq 1 200); do
  if [ -s "$url_file" ]; then
    break
  fi
  sleep 0.05
done

URL="$(tr -d '\r\n' < "$url_file" || true)"
[ -n "$URL" ] || { echo "FAIL: server URL not written" >&2; exit 10; }

ws="$tmp/ws"
mkdir -p "$ws"
cd "$ws"

printf 'secret\n' > secret.txt

cat > out.do <<'EOF'
echo run >> build.log
cat secret.txt >"$3"
EOF

env \
  REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 \
  REDO_REMOTE_EXEC=1 \
  REDO_REMOTE_PLATFORM_ID=test-platform \
  REDO_ACTION_CACHE_REMOTE_URL="$URL" \
  REDO_STRICT=1 \
  redo-ifchange out > /dev/null 2>err.txt

grep -q '^secret$' out || { echo "FAIL: expected output 'secret' in out" >&2; exit 11; }

[ ! -f build.log ] || {
  echo "FAIL: expected remote exec (local out.do ran; build.log exists)" >&2
  exit 12
}

grep -qi 'strict' err.txt || {
  echo "FAIL: expected strict diagnostic in stderr" >&2
  echo "stderr:" >&2
  cat err.txt >&2
  exit 13
}

exit 0

