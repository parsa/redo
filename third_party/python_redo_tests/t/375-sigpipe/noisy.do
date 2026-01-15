echo start >&2

# Write enough stderr output that a downstream consumer closing early will
# trigger BrokenPipe in the log reader and/or SIGPIPE/EPIPE in writers.
i=0
while [ "$i" -lt 5000 ]; do
    i=$((i + 1))
    echo "noisy $i" >&2
done

sleep 0.2
echo done >&2
echo ok

