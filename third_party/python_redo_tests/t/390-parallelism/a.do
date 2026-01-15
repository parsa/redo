touch started_a

i=0
while [ "$i" -lt 50 ]; do
    [ -e started_b ] && break
    i=$((i + 1))
    sleep 0.1
done

[ -e started_b ] || { echo "FAIL: a timed out waiting for started_b (no true parallelism?)" >&2; exit 55; }
echo a_ok >"$3"

