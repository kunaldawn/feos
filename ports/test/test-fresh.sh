#!/bin/sh
# The harness's own test that a base's kernel is built from the base's commit
# (fresh.sh): git archive gives files their commit time, so without it a base
# of a commit older than the last build in the volume reuses that build. In
# localhost/feos-toolchain, --network none (boots nothing):
#
#   PORTS_FORCE=1 ports/test/container.sh fresh ports/test/test-fresh.sh [A B]
#
# A and B are two commits whose kernels differ (default
# refs/feos-checkpoints/1c.6 and HEAD). Builds the x64 kernel at A, then B,
# then B again, then A again, checking each time whether the base was touched
# (fresh.sh), which commit the kernel says it's from, that A's and B's kernels
# differ and that B's unchanged rebuild compiled less than its first build. (A
# new base always reruns the kernel's build script, so the kernel crate itself
# is relinked even then.)
set -eu
cd "$(dirname "$0")/../.."
t=ports/test
a=${1:-refs/feos-checkpoints/1c.6} b=${2:-HEAD}
base=${PORTS_BASE:?run in ports/test/container.sh}
log=$(mktemp)
status=0
fail() { echo "test-fresh: FAIL: $*"; status=1; }

# build REF EXPECT: EXPECT is touched or kept, for the base (or any); the kernel's hash
# in h, the number of crates compiled in n.
build() {
    $t/base.sh "$1"
    $t/build-kernels.sh x64 > "$log" 2>&1 || { cat "$log"; fail "build at $1"; exit 1; }
    grep -E 'ports-base:|built from' "$log" || true
    if grep -q 'touching the base' "$log"; then got=touched; else got=kept; fi
    [ "$2" = any ] || [ "$got" = "$2" ] || fail "at $1 the base was $got, expected $2"
    n=$(grep -c ' Compiling ' "$log" || true)
    want=$(git rev-parse --verify "$1^{commit}")
    [ "$(cat "$base/kernels/x64.ref")" = "$want" ] || fail "kernels/x64.ref is not $want"
    h=$(sha256sum < "$base/kernels/x64" | cut -d' ' -f1)
    echo "test-fresh: $1: base $got, $n crates compiled, kernel $h"
}

# A first, whatever the volume held before: touched unless A was built last.
build "$a" any; ha=$h
# B's files carry B's commit time, older than the build just made: the stale case.
build "$b" touched; hb=$h nb=$n
[ "$ha" != "$hb" ] || fail "the kernels of $a and $b are the same"
# The same commit again: nothing to rebuild, so the cache still works.
build "$b" kept; hb2=$h
[ "$hb2" = "$hb" ] || fail "an unchanged rebuild of $b changed its kernel"
[ "$n" -lt "$nb" ] || fail "an unchanged rebuild of $b compiled $n crates, as many as its first build"
build "$a" touched; ha2=$h
[ "$ha2" != "$hb" ] || fail "going back to $a kept $b's kernel"
[ "$ha2" = "$ha" ] || echo "test-fresh: note: $a's kernel differs from its first build (not reproducible)"
rm -f "$log"
[ $status -eq 0 ] && echo "test-fresh: PASS"
exit $status
