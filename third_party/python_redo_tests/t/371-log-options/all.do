exec >&2
. ../skip-if-minimal-do.sh

rm -f x out.txt raw.txt nodetails.txt color.txt debugpids.txt \
    slow first_built status_on.err status_off.err pid
pid=$$
echo "$pid" >pid

# Build a target that writes to stdout and stderr so there is log content.
redo x

# 1) Default redo-log output should be pretty (no @@REDO meta-lines).
redo-log x >out.txt
if grep '@@REDO:' out.txt >/dev/null; then
    echo "FAIL: redo-log default output should be pretty (no @@REDO: meta-lines)" >&2
    exit 11
fi

# 2) --no-pretty should output raw meta-lines.
redo-log --no-pretty x >raw.txt
if ! grep '@@REDO:' raw.txt >/dev/null; then
    echo "FAIL: redo-log --no-pretty should output raw @@REDO: meta-lines" >&2
    exit 12
fi

# 3) --no-details should suppress non-meta build output (like our stderr lines).
redo-log --no-details x >nodetails.txt
if grep 'x stderr' nodetails.txt >/dev/null; then
    echo "FAIL: redo-log --no-details should suppress build stderr lines" >&2
    exit 13
fi
if grep 'x stdout' nodetails.txt >/dev/null; then
    echo "FAIL: redo-log --no-details should suppress build stdout lines" >&2
    exit 14
fi

# 4) --color should force ANSI escapes even when stdout is not a tty.
redo-log --color x >color.txt
esc="$(printf '\033')"
if ! grep -q "${esc}\[" color.txt; then
    echo "FAIL: redo-log --color should emit ANSI escapes in pretty output" >&2
    exit 15
fi

# 5) --debug-pids should include pid prefixes in pretty output.
# Force --no-color so ANSI escapes don't hide the pid at column 0.
redo-log --no-color --debug-pids x >debugpids.txt
if ! grep -Eq '^[0-9]+[[:space:]]+redo[[:space:]]' debugpids.txt; then
    echo "FAIL: redo-log --debug-pids should prefix pretty lines with pid" >&2
    exit 16
fi

# 6) --status should produce progress output on stderr in --follow mode
# even when stderr is not a tty; --no-status should suppress it.
#
# We do two short rebuilds of a slow target so redo-log has time to print a status line.
redo slow   # quick first build to ensure the target exists in the db

rm -f slow
redo slow >/dev/null 2>&1 &
pid=$!
sleep 0.05
redo-log -f --status slow >/dev/null 2>status_on.err
wait "$pid"
if ! grep -q 'redo ' status_on.err; then
    echo "FAIL: redo-log -f --status should write status/progress to stderr" >&2
    exit 17
fi

rm -f slow
redo slow >/dev/null 2>&1 &
pid=$!
sleep 0.05
redo-log -f --no-status slow >/dev/null 2>status_off.err
wait "$pid"
if [ -s status_off.err ]; then
    echo "FAIL: redo-log -f --no-status should not write progress to stderr" >&2
    exit 18
fi

exit 0

