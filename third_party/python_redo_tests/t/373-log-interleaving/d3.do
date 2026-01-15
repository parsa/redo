for i in $(seq 1 200); do
    printf 'd3 %03d\n' "$i" >&2
done
echo d3_ok

