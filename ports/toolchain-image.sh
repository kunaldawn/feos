#!/bin/sh
# Builds localhost/feos-toolchain from ports/toolchain.Dockerfile, on top of
# localhost/feos (build that first: `docker build -t feos .` at the root).
#
#   ports/toolchain-image.sh
#
# This and `scripts/ports-init.sh` are the only steps of the toolchain build
# that use the network. The image's context is an empty directory: nothing from
# the tree goes into it, so the tree's state never invalidates its layers.
set -eu
cd "$(dirname "$0")"
docker image inspect localhost/feos >/dev/null 2>&1 || {
    echo "toolchain-image: build localhost/feos first (docker build -t feos . at the root)" >&2
    exit 1
}
context=$(mktemp -d)
trap 'rm -rf "$context"' EXIT
docker build -t localhost/feos-toolchain -f toolchain.Dockerfile "$context"
