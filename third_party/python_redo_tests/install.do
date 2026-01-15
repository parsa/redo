exec >&2

# Minimal install target for the vendored test suite.
#
# This script exists so the Apache-2.0 test
# `t/999-installer/all.do` can validate that DESTDIR installs work.

: "${DESTDIR:=NONE}"
: "${PREFIX:=/usr}"
: "${INSTALL:=install}"

if [ "$DESTDIR" = "NONE" ] || [ -z "$DESTDIR" ]; then
	echo "$0: fatal: set DESTDIR before trying to install." >&2
	exit 99
fi

BINDIR="$DESTDIR$PREFIX/bin"
"$INSTALL" -d "$BINDIR"

bins="
redo
redo-ifchange
redo-ifcreate
redo-always
redo-stamp
redo-log
redo-ood
redo-targets
redo-sources
redo-whichdo
redo-unlocked
"

for b in $bins; do
	src="$(command -v "$b" || true)"
	if [ -z "$src" ]; then
		echo "$0: fatal: missing binary in PATH: $b" >&2
		exit 1
	fi
	"$INSTALL" -m 0755 "$src" "$BINDIR/$b"
done

exit 0

