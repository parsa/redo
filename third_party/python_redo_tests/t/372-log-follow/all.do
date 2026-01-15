exec >&2
. ../skip-if-minimal-do.sh

rm -f slow follow.out *~ .*~

# Start the build in the background, then follow logs until it completes.
redo slow &
pid=$!

sleep 0.05
redo-log -f slow >follow.out
wait "$pid"

grep -q 'start' follow.out || exit 11
grep -q 'middle' follow.out || exit 12
grep -q 'end' follow.out || exit 13

# BrokenPipe should be ignored.
redo-log -f slow | head -1 >/dev/null || exit 14

exit 0

