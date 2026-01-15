#!/usr/bin/env bash
set -euo pipefail

# Regression tests for DJB:
# - honest-script: targets depend on the selected .do script (implicit dep)
# - honest-nonfile: targets depend on creation of missing more-specific .do scripts

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

cd "$tmp"

export PATH="$ROOT/target/debug:/usr/bin:/bin"

############################
# honest-script
############################

cat >out.do <<'EOF'
echo v1 >"$3"
EOF

redo-ifchange out >/dev/null 2>&1
[ "$(cat out)" = "v1" ] || { echo "FAIL: honest-script setup expected out=v1" >&2; exit 11; }

cat >out.do <<'EOF'
echo v2 >"$3"
EOF

redo-ifchange out >/dev/null 2>&1
[ "$(cat out)" = "v2" ] || { echo "FAIL: honest-script expected rebuild after out.do change" >&2; exit 12; }

############################
# honest-nonfile (auto create-dep for missing .do candidates)
############################

cat >default.o.do <<'EOF'
echo default >"$3"
EOF

rm -f foo.o.do
redo-ifchange foo.o >/dev/null 2>&1
[ "$(cat foo.o)" = "default" ] || { echo "FAIL: honest-nonfile setup expected foo.o=default" >&2; exit 13; }

cat >foo.o.do <<'EOF'
echo specific >"$3"
EOF

redo-ifchange foo.o >/dev/null 2>&1
[ "$(cat foo.o)" = "specific" ] || { echo "FAIL: honest-nonfile expected rebuild after creating foo.o.do" >&2; exit 14; }

exit 0

