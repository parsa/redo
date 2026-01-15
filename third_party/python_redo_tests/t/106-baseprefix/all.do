# Regression test for `REDO_BASE` auto-selection semantics.
#
# Base selection uses a lexical common-prefix (string prefix) when picking a base,
# which can yield a base that is *not* a path-component common ancestor.
#
# We construct sibling dirs `projA` and `projB` (string-commonprefix "proj"),
# plus an existing `proj/` directory. Then run a fresh (toplevel) redo build
# from inside `projA` that touches both `projA` and `projB`.
#
# Expected: REDO_BASE == <tmp>/proj

tmp="$(mktemp -d)"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT

mkdir -p "$tmp/proj" "$tmp/projA" "$tmp/projB"

# Use a physical (symlink-resolved) temp path so the string-commonprefix
# doesn't collapse to "/" (eg. if mixing /var and /private/var).
tmp_phys="$(cd "$tmp" && /bin/pwd -P)"

cat >"$tmp/projA/x.do" <<'EOF'
echo "$REDO_BASE" >"$3"
EOF

cat >"$tmp/projB/y.do" <<'EOF'
echo "$REDO_BASE" >"$3"
EOF

# Pick the currently-active `redo` binary. (Don't rely on $REDO: when `redo`
# is invoked from PATH as a bare name, $REDO can end up as "$PWD/redo".)
redo_bin="${REDO:-}"
if [ -z "$redo_bin" ] || [ ! -f "$redo_bin" ] || [ ! -x "$redo_bin" ]; then
	redo_bin="$(command -v redo)"
fi

(
	cd "$tmp_phys/projA"
	# Use an absolute path for the second target so base-selection is driven by
	# the target directories (not lexical `..` components).
	env -i PATH="$PATH" "$redo_bin" --no-log x "$tmp_phys/projB/y"
)

got="$(cat "$tmp/projA/x")"
# Normalize symlink-y temp paths on macOS (eg. /var -> /private/var).
got="$(cd "$got" && /bin/pwd -P)"
want="$(cd "$tmp/proj" && /bin/pwd -P)"

if [ "$got" != "$want" ]; then
	echo "FAIL: REDO_BASE mismatch" >&2
	echo "  got:  $got" >&2
	echo "  want: $want" >&2
	exit 1
fi

