for i in $(seq 1 200); do
    printf 'd2 %03d\n' "$i" >&2
done
echo d2_ok

