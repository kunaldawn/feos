#!/bin/sh
# Start a ports build or test as a detached container named feos-ports-NAME,
# its output in the feos-ports-target volume's NAME.log, and return at once.
#
#   ports/test/container.sh NAME COMMAND [ARGS...]
#   timeout 570 docker wait feos-ports-NAME     # repeat until it prints the status
#   ports/test/container.sh --log NAME          # the log
#   docker rm feos-ports-NAME
#
# Runs in localhost/feos-toolchain with the tree at /src, target/ports-base also
# at /base (PORTS_BASE; see build-kernels.sh), --network none,
# --cpus 6 --memory 9g and /dev/kvm. A boot under load times out, so it refuses
# to start while the other workflow's feos-gate container runs, unless
# PORTS_FORCE=1 (fine for a build that boots nothing).
set -eu
cd "$(dirname "$0")/../.."
if [ "${1:-}" = --log ]; then
    exec docker run --rm --network none -v feos-ports-target:/build localhost/feos-toolchain cat "/build/$2.log"
fi
[ $# -ge 2 ] || { echo "usage: $0 NAME COMMAND [ARGS...]" >&2; exit 2; }
name=$1
shift
if [ "${PORTS_FORCE:-0}" != 1 ] && docker ps --format '{{.Names}}' | grep -qx feos-gate; then
    echo "container: feos-gate is running; wait for it (or PORTS_FORCE=1 for a build)" >&2
    exit 3
fi
docker rm -f "feos-ports-$name" >/dev/null 2>&1 || true
mkdir -p target/ports-base
kvm=""
[ -e /dev/kvm ] && kvm="--device /dev/kvm"
# shellcheck disable=SC2086
docker run -d --name "feos-ports-$name" --network none --cpus 6 --memory 9g $kvm \
    -v "$PWD":/src -v "$PWD/target/ports-base":/base -e PORTS_BASE=/base \
    -v feos-ports-target:/build -w /src localhost/feos-toolchain \
    sh -c '"$@" > "/build/$0.log" 2>&1' "$name" "$@"
