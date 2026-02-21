#!/usr/bin/env bash
set -euo pipefail

# Phase 4 remote execution Linux-only: dependency declarations must be propagated
# back to the client DB so input changes cause rebuilds.

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

printf 'one\n' > in.txt

cat > out.do <<'EOF'
echo run >> build.log
redo-ifchange in.txt
cat in.txt >"$3"
EOF

# First build locally to establish deps in sqlite.
env \
  REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 \
  REDO_ACTION_CACHE_REMOTE_URL="$URL" \
  redo-ifchange out > /dev/null 2>err1.txt

grep -q '^one$' out || { echo "FAIL: expected output 'one' in out" >&2; exit 11; }

rm -f build.log
rm -f out

# Second build remotely (deps already known). Remote execution should not create
# local side files.
env \
  REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 \
  REDO_REMOTE_EXEC=1 \
  REDO_REMOTE_PLATFORM_ID=test-platform \
  REDO_ACTION_CACHE_REMOTE_URL="$URL" \
  redo-ifchange out > /dev/null 2>err2.txt

grep -q '^one$' out || { echo "FAIL: expected output 'one' in out (remote rebuild)" >&2; exit 12; }
[ ! -f build.log ] || { echo "FAIL: expected remote exec (build.log exists)" >&2; exit 13; }

printf 'two\n' > in.txt

env \
  REDO_LOG=0 REDO_PRETTY=0 REDO_COLOR=0 \
  REDO_REMOTE_EXEC=1 \
  REDO_REMOTE_PLATFORM_ID=test-platform \
  REDO_ACTION_CACHE_REMOTE_URL="$URL" \
  redo-ifchange out > /dev/null 2>err3.txt

# If the dependency was propagated to the client DB, the change to in.txt must
# force a rebuild.
grep -q '^two$' out || {
  echo "FAIL: expected input change to trigger rebuild (out did not update)" >&2
  echo "stderr1:" >&2
  cat err1.txt >&2
  echo "stderr2:" >&2
  cat err2.txt >&2
  echo "stderr3:" >&2
  cat err3.txt >&2
  exit 12
}

exit 0

