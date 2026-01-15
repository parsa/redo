redo d4
for i in $(seq 1 200); do
    printf 'a4 %03d\n' "$i" >&2
done
echo a4_ok

