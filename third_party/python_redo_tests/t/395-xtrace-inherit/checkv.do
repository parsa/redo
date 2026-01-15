case "$-" in
    *v*) v=1 ;;
    *)   v=0 ;;
esac
echo "v:$v env:${REDO_VERBOSE:-}" >"$3"

