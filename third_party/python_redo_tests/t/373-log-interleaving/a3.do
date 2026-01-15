redo d3
for i in $(seq 1 200); do
    printf 'a3 %03d\n' "$i" >&2
done
echo a3_ok

