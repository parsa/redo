redo d2
for i in $(seq 1 200); do
    printf 'a2 %03d\n' "$i" >&2
done
echo a2_ok

