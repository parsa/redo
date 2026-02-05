#!/usr/bin/env bash
set -euo pipefail

# Test that *toplevel* `redo` spawns `redo-log` with --ack-fd and sends some data to it.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

fakebin="$tmp/fakebin"
proj="$tmp/proj"
mkdir -p "$fakebin" "$proj"

cat >"$fakebin/redo-log" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

ack_fd=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --ack-fd=*)
      ack_fd="${1#--ack-fd=}"
      shift
      ;;
    --ack-fd)
      ack_fd="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done

echo "invoked" >"__TMP__/invoked"
echo "$ack_fd" >"__TMP__/ack_fd"

if [ -z "$ack_fd" ]; then
  echo "missing --ack-fd" >"__TMP__/error"
  exit 50
fi

# shellcheck disable=SC1083
printf 'REDO-OK\n' >&"$ack_fd"
echo "1" >"__TMP__/acked"

cat >"__TMP__/stdin"
exit 0
EOF

# Patch in tmp path
perl -pi -e "s#__TMP__#$tmp#g" "$fakebin/redo-log"
chmod +x "$fakebin/redo-log"

cat >"$proj/x.do" <<'EOF'
echo hello >"$3"
EOF

cd "$proj"

export PATH="$fakebin:$ROOT/target/debug:/usr/bin:/bin"
export REDO_LOG=1
export REDO_NO_PATH_PREPEND=1

TIMEOUT_SECS="${REDO_TEST_TIMEOUT_SECS:-20}"
if [ "$TIMEOUT_SECS" -gt 20 ]; then TIMEOUT_SECS=20; fi
TIMEOUT_BIN=""
if command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN="timeout"
fi

if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "$TIMEOUT_SECS" redo x >/dev/null 2>&1 || {
    echo "FAIL: redo x did not finish successfully (timeout or error)" >&2
    exit 10
  }
else
  set +e
  redo x >/dev/null 2>&1 &
  pid=$!
  timed_out="$tmp/timed_out"
  (
    sleep "$TIMEOUT_SECS"
    echo "FAIL: redo x did not finish successfully (timeout ${TIMEOUT_SECS}s)" >&2
    echo 1 >"$timed_out"
    kill -KILL "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
  ) &
  watchdog=$!
  wait "$pid"
  rv=$?
  kill "$watchdog" 2>/dev/null || true
  wait "$watchdog" 2>/dev/null || true
  set -e

  [ ! -e "$timed_out" ] || exit 10
  [ "$rv" -eq 0 ] || { echo "FAIL: redo x did not finish successfully" >&2; exit 10; }
fi

[ -f "$tmp/invoked" ] || { echo "FAIL: fake redo-log was not invoked" >&2; exit 11; }
[ -s "$tmp/ack_fd" ] || { echo "FAIL: fake redo-log did not receive --ack-fd" >&2; exit 14; }
[ -f "$tmp/acked" ] || { echo "FAIL: fake redo-log did not ack REDO-OK on ack-fd" >&2; exit 15; }
[ -s "$tmp/stdin" ] || { echo "FAIL: redo-log stdin was empty" >&2; exit 12; }
grep -q '@@REDO:' "$tmp/stdin" || { echo "FAIL: no @@REDO meta-lines seen on redo-log stdin" >&2; exit 13; }

exit 0

