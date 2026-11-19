#!/bin/sh
# TEST_RUNNER for relibc's test suite (ports/test/relibc-linux.sh): runs the
# suite's own runner, then passes unless a failure is missing from
# relibc-known-failures.txt. Its output is kept as it is.
set -u
here=$(cd "$(dirname "$0")" && pwd)
log=$(mktemp)
"$@" > "$log" 2>&1
rc=$?
cat "$log"
known=$(grep -v '^#' "$here/relibc-known-failures.txt" | cut -f1)
bad=0
seen=0
# The runner lists each failure after "# FAILURES #" as "# PATH: why".
for path in $(sed -n '/# FAILURES #/,$p' "$log" | sed -n 's/^# \([^:]*\): .*/\1/p'); do
    name=$(echo "$path" | sed 's|.*/bins_[a-z_]*/||')
    seen=1
    if echo "$known" | grep -qx "$name"; then
        echo "relibc-runner: known failure $name"
    else
        echo "relibc-runner: FAILED $name"; bad=1
    fi
done
# A runner that failed without naming a failure failed some other way.
if [ $rc -ne 0 ] && [ $seen -eq 0 ]; then
    echo "relibc-runner: the runner exited $rc"; bad=1
fi
rm -f "$log"
exit $bad
