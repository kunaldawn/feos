#!/bin/sh
# Build both kernels of target/ports-base, with what inject.sh put in, in the
# localhost/feos-toolchain (or localhost/feos) container with the tree at /src
# and the feos-ports-target volume at /build. No network.
#
#   ports/test/build-kernels.sh            # both
#   ports/test/build-kernels.sh arm        # or x64
#
# The kernels land in target/ports-base/kernels/{arm,x64}. Cargo's output stays
# in /build/ports-base-target between runs, so only what changed is rebuilt;
# fresh.sh makes sure a base of another commit than the last build is rebuilt.
set -eu
cd "$(dirname "$0")/../.."
root=$(pwd)
# Cargo merges every .cargo/config.toml from the working directory up, so a
# base under /src/target would get the tree's rustflags as well as its own (the
# linker script twice). container.sh mounts target/ports-base at /base, outside
# the tree, and sets PORTS_BASE; build there.
base=${PORTS_BASE:-$root/target/ports-base}
case "$base" in "$root"/*)
    echo "build-kernels: $base is inside the tree; run in container.sh (PORTS_BASE=/base)" >&2; exit 1 ;;
esac
[ -f "$base/.ports-base-ref" ] || { echo "build-kernels: no base; run ports/test/base.sh" >&2; exit 1; }
export CARGO_TARGET_DIR=${PORTS_BASE_TARGET:-/build/ports-base-target}
export RUSTUP_TOOLCHAIN=${RUSTUP_TOOLCHAIN:-1.99.1}
. "$root/ports/test/fresh.sh"
# make build-PORT builds user-PORT first, so both are pieces of a port.
for p in ${1:-arm x64}; do pieces="${pieces:-} $p user-$p"; done
ports_base_sync $pieces
ref=$(cat "$base/.ports-base-ref")
mkdir -p "$base/kernels"
chmod +x "$base"/scripts/*.sh "$base"/scripts/*.py

# Lay ARCH's overlay (and the shared one) over fs/; `unlay` takes it off again.
lay() {
    : > "$base/.overlay-files"
    for o in all "$1"; do
        [ -d "$base/.overlay/$o" ] || continue
        (cd "$base/.overlay/$o" && find . -type f) | while read -r f; do
            dst="$base/boards/qemu-virt/fs/${f#./}"
            if [ -e "$dst" ]; then
                echo "build-kernels: overlay $o/$f would replace a file of the base" >&2
                exit 1
            fi
            mkdir -p "$(dirname "$dst")"
            cp -p "$base/.overlay/$o/$f" "$dst"
            echo "$dst" >> "$base/.overlay-files"
        done
    done
}
unlay() {
    while read -r f; do rm -f "$f"; done < "$base/.overlay-files"
    rm -f "$base/.overlay-files"
}

for port in ${1:-arm x64}; do
    case "$port" in
        arm) arch=aarch64 out=aarch64-unknown-none ;;
        x64) arch=x86_64 out=x86_64-feos ;;
        *) echo "build-kernels: unknown port $port" >&2; exit 2 ;;
    esac
    lay "$arch"
    make -C "$base" "build-$port" CARGO_TARGET_DIR="$CARGO_TARGET_DIR"
    unlay
    ports_base_built "$port"
    ports_base_built "user-$port"
    cp "$CARGO_TARGET_DIR/$out/release/feos" "$base/kernels/$port"
    echo "$ref" > "$base/kernels/$port.ref"
    echo "build-kernels: $port kernel at target/ports-base/kernels/$port ($base/kernels/$port), built from $ref"
done
