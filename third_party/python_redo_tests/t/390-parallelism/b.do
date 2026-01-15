touch started_b

i=0
while [ "$i" -lt 50 ]; do
    [ -e started_a ] && break
    i=$((i + 1))
    sleep 0.1
done

[ -e started_a ] || { echo "FAIL: b timed out waiting for started_a (no true parallelism?)" >&2; exit 56; }
echo b_ok >"$3"

