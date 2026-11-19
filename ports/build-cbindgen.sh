#!/bin/sh
# Build cbindgen from ports/src/cbindgen, offline, into /build/bin. relibc's
# headers are generated with it. In localhost/feos-toolchain, with the tree at
# /src and the feos-ports-target volume at /build:
#
#   ports/test/container.sh cbindgen ports/build-cbindgen.sh
set -eu
P=$(cd "$(dirname "$0")" && pwd)
export RUSTUP_TOOLCHAIN=1.99.1
out=${PORTS_BUILD:-/build}
mkdir -p "$out/bin"
cd "$P/src/cbindgen"
cargo build --release --locked --offline --target x86_64-unknown-linux-gnu \
    --target-dir "$out/cbindgen-target" --bin cbindgen
cp "$out/cbindgen-target/x86_64-unknown-linux-gnu/release/cbindgen" "$out/bin/cbindgen"
"$out/bin/cbindgen" --version
