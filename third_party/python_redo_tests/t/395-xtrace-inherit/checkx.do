case "$-" in
    *x*) x=1 ;;
    *)   x=0 ;;
esac
echo "x:$x env:${REDO_XTRACE:-}" >"$3"

