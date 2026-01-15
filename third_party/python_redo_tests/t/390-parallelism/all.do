exec >&2
. ../skip-if-minimal-do.sh

rm -f a b started_a started_b *~ .*~

# This test requires *true* parallel execution under -j>1.
# Each target waits for the other to start; serial execution will time out.
redo -j2 a b || {
    echo "FAIL: expected redo -j2 a b to succeed (requires true parallelism)" >&2
    exit 10
}

[ -e a ] || { echo "FAIL: target a did not produce output file" >&2; exit 11; }
[ -e b ] || { echo "FAIL: target b did not produce output file" >&2; exit 12; }
grep -q '^a_ok$' a || { echo "FAIL: target a output mismatch" >&2; exit 13; }
grep -q '^b_ok$' b || { echo "FAIL: target b output mismatch" >&2; exit 14; }

exit 0

