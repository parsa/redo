redo d1
for i in $(seq 1 200); do
    printf 'a1 %03d\n' "$i" >&2
done
echo a1_ok

