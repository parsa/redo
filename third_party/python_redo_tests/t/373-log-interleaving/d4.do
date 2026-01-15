for i in $(seq 1 200); do
    printf 'd4 %03d\n' "$i" >&2
done
echo d4_ok

