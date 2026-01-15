# This target uses redo-stamp so a checksum (csum) is recorded in the db.
# Then chmod on the output should trigger checksum uncertainty (MustBuild([self]))
# which must rebuild directly (no redo-unlocked/OOB).

n=0
if [ -f buildcount ]; then
    read n <buildcount
fi
n=$((n + 1))
echo "$n" >buildcount

echo "build $n"
echo "build $n" | redo-stamp

