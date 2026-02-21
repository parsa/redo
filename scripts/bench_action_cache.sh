#!/usr/bin/env bash
set -euo pipefail

# Simple local benchmark harness for Phase 1 action cache.
#
# Usage:
#   bash rust/scripts/bench_action_cache.sh [N_TARGETS] [JOBS]
#
# Notes:
# - Runs `redo -jN all` to set up a jobserver; the work is delegated to
#   `redo-ifchange` from within `all.do` (which is where the action cache is consulted).
# - Captures raw @@REDO:cache_*@@ meta-lines by forcing REDO_LOG=0 REDO_PRETTY=0.
# - Prefers `target/release` binaries when available; otherwise uses `target/debug`.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

N="${1:-80}"
J="${2:-4}"

SLEEP_SECS="${REDO_ACTION_CACHE_BENCH_SLEEP_SECS:-0.01}"

BIN_DIR="$ROOT/target/release"
if [ ! -x "$BIN_DIR/redo" ] || [ ! -x "$BIN_DIR/redo-ifchange" ]; then
  BIN_DIR="$ROOT/target/debug"
fi

if [ ! -x "$BIN_DIR/redo" ] || [ ! -x "$BIN_DIR/redo-ifchange" ]; then
  echo "FAIL: missing redo binaries at $ROOT/target/{release,debug}/" >&2
  echo "  Need: redo and redo-ifchange" >&2
  echo "  Hint: build it first: (cd $ROOT && cargo build --release)" >&2
  exit 10
fi

tmp="$(mktemp -d)"
cleanup() {
  if [ -n "${REDO_ACTION_CACHE_BENCH_KEEP_TMP:-}" ]; then
    echo "Keeping tmp dir: $tmp" >&2
    return
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

cd "$tmp"

targets=()
for i in $(seq -w 1 "$N"); do
  t="t${i}"
  targets+=("$t")
  cat >"${t}.do" <<EOF
sleep "$SLEEP_SECS"
echo "$t" >"\$3"
EOF
done

{
  echo "# Build all targets, then concatenate."
  echo "redo-ifchange ${targets[*]}"
  echo "cat ${targets[*]} >\"\$3\""
} > all.do

export PATH="$BIN_DIR:/usr/bin:/bin"

run_and_time() {
  local label="$1"
  local extra_env="$2"
  local log="${label}.log"
  local tlog="${label}.time"
  rm -f "$log" "$tlog"

  # Redo stderr -> $log; time output -> $tlog.
  /usr/bin/time -p sh -c "
    set -e
    export REDO_LOG=0
    export REDO_PRETTY=0
    export REDO_DEBUG=0
    export REDO_SHUFFLE=
    export REDO_VERBOSE=0
    export REDO_XTRACE=0
    export REDO_KEEP_GOING=
    export REDO_COLOR=0
    export REDO_ACTION_CACHE_MAX_BYTES=\${REDO_ACTION_CACHE_MAX_BYTES:-1073741824}
    ${extra_env}
    redo -j${J} all >/dev/null 2>\"$log\"
  " 2>"$tlog"
}

wipe_outputs() {
  rm -f all
  rm -f "${targets[@]}"
}

echo "tmp=$tmp"
echo "bin_dir=$BIN_DIR"
echo "n_targets=$N jobs=$J sleep=$SLEEP_SECS"

echo
echo "== Warm (populate cache) =="
wipe_outputs
run_and_time warm ""

echo
echo "== Rebuild with cache enabled =="
wipe_outputs
run_and_time cache_on ""

echo
echo "== Rebuild with cache disabled (REDO_NO_ACTION_CACHE=1) =="
wipe_outputs
run_and_time cache_off "export REDO_NO_ACTION_CACHE=1"

count_meta() {
  local log="$1"
  local k="$2"
  if [ ! -f "$log" ]; then
    echo 0
    return
  fi
  grep -c "@@REDO:${k}:" "$log" || true
}

echo
echo "== Cache meta counts (cache_on) =="
echo "cache_hit  $(count_meta cache_on.log cache_hit)"
echo "cache_miss $(count_meta cache_on.log cache_miss)"
echo "cache_store $(count_meta cache_on.log cache_store)"
echo "cache_skip $(count_meta cache_on.log cache_skip)"

echo
echo "== Timings =="
echo "warm:"
cat warm.time
echo "cache_on:"
cat cache_on.time
echo "cache_off:"
cat cache_off.time

exit 0

