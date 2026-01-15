exec >&2
. ../skip-if-minimal-do.sh

rm -f x buildcount invoked *~ .*~
rm -rf fakebin
mkdir -p fakebin

if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "374-oob-self: skipping: sqlite3 not available" >&2
    exit 0
fi

cat >fakebin/redo-unlocked <<'EOF'
#!/bin/sh
echo invoked >"__INV__"
exit 99
EOF
sed "s#__INV__#$(pwd)/invoked#g" <fakebin/redo-unlocked >fakebin/redo-unlocked.tmp
mv fakebin/redo-unlocked.tmp fakebin/redo-unlocked
chmod +x fakebin/redo-unlocked

# First build establishes a csum via redo-stamp.
redo x

DB="${REDO_BASE}/.redo/db.sqlite3"
NAME="$(python3 - <<'PY'
import os
base = os.environ.get("REDO_BASE", "")
print(os.path.relpath(os.path.join(os.getcwd(), "x"), base))
PY
)"
csum="$(sqlite3 "$DB" "select csum from Files where name='$NAME';")"
[ -n "$csum" ] || { echo "FAIL: expected non-empty csum for x after redo-stamp" >&2; exit 10; }

# Force a stamp mismatch without changing the on-disk file, and without triggering
# override detection (which only considers the first two stamp fields: mtime+size).
stamp="$(sqlite3 "$DB" "select stamp from Files where name='$NAME';")"
[ -n "$stamp" ] || { echo "FAIL: missing stamp for x" >&2; exit 11; }
stamp2="${stamp}-999"
sqlite3 "$DB" "update Files set stamp='$stamp2' where name='$NAME';" || exit 11

# MustBuild([self]) should rebuild directly without invoking redo-unlocked.
PATH="$(pwd)/fakebin:$PATH" env -u REDO -u REDO_BASE -u REDO_STARTDIR -u REDO_RUNID -u REDO_PWD -u REDO_TARGET -u REDO_DEPTH redo-ifchange x || {
    echo "FAIL: redo-ifchange x failed" >&2
    exit 12
}
[ ! -e invoked ] || { echo "FAIL: redo-unlocked was invoked (expected direct rebuild)" >&2; exit 13; }

read n <buildcount
[ "$n" -eq 2 ] || { echo "FAIL: expected x to rebuild exactly once (buildcount=2), got $n" >&2; exit 14; }

exit 0

