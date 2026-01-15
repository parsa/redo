exec >&2
. ../skip-if-minimal-do.sh

rm -f x out.txt *~ .*~

redo x

# stdout should be captured into the output file, not the log.
grep -q '^x stdout$' x || { echo "FAIL: expected stdout in output file x" >&2; exit 10; }
if grep -q 'x stderr' x; then
    echo "FAIL: stderr leaked into output file x" >&2
    exit 11
fi

redo-log x >out.txt
grep -q '^x stderr$' out.txt || { echo "FAIL: expected stderr in redo-log output" >&2; exit 12; }
grep -q '^script=.*x\.do$' out.txt || { echo "FAIL: expected script path (x.do) in redo-log output" >&2; exit 13; }
if grep -q '^x stdout$' out.txt; then
    echo "FAIL: stdout leaked into redo-log output" >&2
    exit 14
fi

exit 0

