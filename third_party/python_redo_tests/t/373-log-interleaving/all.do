exec >&2
. ../skip-if-minimal-do.sh

rm -f a1 a2 a3 a4 d1 d2 d3 d4 log_a*.txt *~ .*~

# Build multiple targets in parallel; each target and its dependency writes many
# short stderr lines. Then assert redo-log recursion includes all lines (no loss).
redo -j4 a1 a2 a3 a4 || {
    echo "FAIL: expected parallel build to succeed" >&2
    exit 10
}

N=200
i=1
while [ "$i" -le 4 ]; do
    redo-log -ru "a$i" >"log_a$i.txt" || exit 20
    c1=$(grep -c "^a$i " "log_a$i.txt" || true)
    c2=$(grep -c "^d$i " "log_a$i.txt" || true)
    [ "$c1" -eq "$N" ] || { echo "FAIL: expected $N a$i lines, got $c1" >&2; exit 30; }
    [ "$c2" -eq "$N" ] || { echo "FAIL: expected $N d$i lines, got $c2" >&2; exit 31; }
    grep -q "^a$i 200$" "log_a$i.txt" || { echo "FAIL: missing final a$i marker" >&2; exit 32; }
    grep -q "^d$i 200$" "log_a$i.txt" || { echo "FAIL: missing final d$i marker" >&2; exit 33; }
    i=$((i + 1))
done

exit 0

