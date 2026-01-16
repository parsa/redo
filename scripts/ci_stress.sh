#!/usr/bin/env bash
set -euo pipefail

# CI stress runner: loop the most race-sensitive integration tests to catch flakes.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ITERS="${REDO_STRESS_ITERS:-25}"
JITTER_MS="${REDO_STRESS_JITTER_MS:-0}"

if ! [[ "$ITERS" =~ ^[0-9]+$ ]] || [ "$ITERS" -lt 1 ]; then
  echo "ci_stress: invalid REDO_STRESS_ITERS=$ITERS" >&2
  exit 2
fi
if ! [[ "$JITTER_MS" =~ ^[0-9]+$ ]] || [ "$JITTER_MS" -lt 0 ]; then
  echo "ci_stress: invalid REDO_STRESS_JITTER_MS=$JITTER_MS" >&2
  exit 2
fi

if [ ! -x "$ROOT/target/debug/redo" ]; then
  cargo build --workspace
fi

maybe_jitter() {
  if [ "$JITTER_MS" -le 0 ]; then
    return 0
  fi
  ms=$((RANDOM % (JITTER_MS + 1)))
  sleep "$(printf '0.%03d' "$ms")"
}

run_test() {
  local t="$1"
  echo "ci_stress: iter $i/$ITERS: $t" >&2
  bash "$t"
  maybe_jitter
}

for i in $(seq 1 "$ITERS"); do
  run_test scripts/test_redo_log_follow_and_options.sh
  run_test scripts/test_redo_log_follow_race.sh
  run_test scripts/test_sigint.sh
  run_test scripts/test_toplevel_redo_log_pipe.sh
done

echo "ci_stress: ok ($ITERS iterations)" >&2

