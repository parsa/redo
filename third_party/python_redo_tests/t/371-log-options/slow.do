if [ ! -e first_built ]; then
    echo 1 >first_built
    echo ok >"$3"
    exit 0
fi

echo start >&2
sleep 2
echo end >&2
echo ok >"$3"

