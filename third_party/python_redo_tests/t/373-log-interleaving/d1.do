for i in $(seq 1 200); do
    printf 'd1 %03d\n' "$i" >&2
done
echo d1_ok

