exec >&2
. ../skip-if-minimal-do.sh

rm -f checkx checkv *~ .*~

redo -x checkx
grep -q 'x:1 env:0' checkx || {
    echo "FAIL: expected -x to enable shell xtrace but clamp REDO_XTRACE to 0 in .do" >&2
    exit 11
}

redo -v checkv
grep -q 'v:1 env:0' checkv || {
    echo "FAIL: expected -v to enable shell verbose but clamp REDO_VERBOSE to 0 in .do" >&2
    exit 12
}

exit 0

