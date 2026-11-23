#!/bin/sh
# The libc suite on feos, both ports: the one entry point (meant to become the boot-test
# phase `libc` once the kernel's phase 1 is done). In the localhost/feos-toolchain
# container with the tree at /src, the feos-ports-target volume at /build,
# --network none and /dev/kvm:
#
#   ports/test/container.sh libc ports/test/libc-suite.sh [REF]
#
# REF is the base the kernels are built from, default HEAD (a verified state; through
# stage 1c.9 when this was written). The spec wants 1c.8's KILL (testrun-self checks that
# an overrunning test is killed), and the lists say which tests wait on later stages. It
# 1. builds what goes into the image (build-libc-suite.sh: relibc for feos, the sysroot,
#    testrun and sysroot-check, relibc's tests, the libc-test subset), each piece a no-op
#    or quick when built and unchanged;
# 2. makes a base of REF and puts into each architecture's image: the sysroot at /usr
#    (usr/include, usr/lib, usr/share/feos-sysroot/MANIFEST), /bin/testrun,
#    /bin/sysroot-check, and the payloads under /usr/share/libc-suite/{selftest,relibc,
#    libc-test};
# 3. builds both kernels and runs specs/libc-suite.toml on each port.
#
# PORTS_SUITE_ARCHES (default "aarch64 x86_64") and PORTS_SUITE_PORTS (default
# "arm x64") narrow it to one port while debugging; PORTS_SUITE_STEPS (default
# "build boot") runs only the building (1 to 3 less the boots, fine alongside other
# work: PORTS_FORCE=1) or only the boots of kernels built before. While debugging,
# PORTS_SUITE_EXTRA_C (C files, absolute paths) adds programs to /bin and
# PORTS_SUITE_SPECS more spec files to run after the suite's. The guest gets FEOS_BOOT_TEST_MEM
# (default 2G): the image holds the payloads, and its RAM root copies them up when
# written. The log has every test's line (PORTS_SHOW_OUTPUT=1), each port's also in
# /build/libc-suite/boot-<port>.log, which ports/test/suite-report.py reads.
set -eu
cd "$(dirname "$0")/../.."
t=ports/test
ref=${1:-HEAD}
arches=${PORTS_SUITE_ARCHES:-aarch64 x86_64}
ports=${PORTS_SUITE_PORTS:-arm x64}
out=${PORTS_BUILD:-/build}
steps=${PORTS_SUITE_STEPS:-build boot}
base=${PORTS_BASE:?run in ports/test/container.sh}

build() {
# shellcheck disable=SC2086
$t/build-libc-suite.sh $arches

for arch in $arches; do
    self=$out/libc-suite/$arch/selftest
    rm -rf "$self"
    mkdir -p "$self"
    cp $t/suite-selftest/self.list $t/suite-selftest/expected-not.txt "$self/"
    # What sysroot-check prints for this sysroot.
    python3 -I - "$out/sysroot/$arch-unknown-feos/usr/share/feos-sysroot/MANIFEST" "$self/expected.txt" <<'PY'
import sys
paths = [line.split()[2] for line in open(sys.argv[1])]
count = lambda ext: sum(1 for p in paths if p.endswith(ext))
open(sys.argv[2], "w").write(f"sysroot-check: {len(paths)} files ok ({count('.h')} headers, "
                             f"{count('.a')} archives, {count('.o')} objects)\n")
PY
done

$t/base.sh "$ref"
export CARGO_TARGET_DIR=/build/ports-base-target RUSTUP_TOOLCHAIN=1.99.1
chmod +x $base/scripts/*.sh $base/scripts/*.py
. $t/fresh.sh
ports_base_sync user-arm user-x64 arm x64
make -C $base user-arm user-x64 CARGO_TARGET_DIR="$CARGO_TARGET_DIR"
ports_base_built user-arm
ports_base_built user-x64
for arch in $arches; do
    s=$out/libc-suite/$arch
    $t/inject.sh $arch "$out/sysroot/$arch-unknown-feos/usr" /usr
    for program in "$s"/bin/*; do
        $t/inject.sh $arch "$program" "/bin/$(basename "$program")"
    done
    $t/inject.sh $arch "$s/selftest" /usr/share/libc-suite/selftest
    $t/inject.sh $arch "$s/relibc-tests" /usr/share/libc-suite/relibc
    $t/inject.sh $arch "$s/libc-test" /usr/share/libc-suite/libc-test
done
# shellcheck disable=SC2086
$t/build-kernels.sh "$ports"
ls -la $base/kernels
}

case " $steps " in *" build "*) build ;; esac
case " $steps " in *" boot "*) ;; *) exit 0 ;; esac
for port in $ports; do
    [ -f $base/kernels/$port ] || { echo "libc-suite: no $port kernel; run the build step" >&2; exit 1; }
done
export PORTS_SHOW_OUTPUT=${PORTS_SHOW_OUTPUT:-1}
export FEOS_BOOT_TEST_MEM=${FEOS_BOOT_TEST_MEM:-2G}
status=0
for port in $ports; do
    # shellcheck disable=SC2086
    python3 $t/boot.py $port $base/kernels/$port $t/specs/libc-suite.toml ${PORTS_SUITE_SPECS:-} \
        > "$out/libc-suite/boot-$port.log" 2>&1 || status=1
    cat "$out/libc-suite/boot-$port.log"
done
exit $status
