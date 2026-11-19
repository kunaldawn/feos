#!/bin/sh
# Make target/ports-base: a fresh copy of the tree at REF (default HEAD), from
# `git archive`, so a test kernel is built from a committed, verified state and
# never from the working tree, which other work may be in the middle of editing.
#
#   ports/test/base.sh [REF]
#
# REF may be any commit, e.g. refs/feos-checkpoints/1c.6. The commit is written
# to target/ports-base/.ports-base-ref. Runs on the host or in the
# localhost/feos-toolchain container (it needs git and tar).
set -eu
cd "$(dirname "$0")/../.."
ref=${1:-HEAD}
commit=$(git rev-parse --verify "$ref^{commit}")
# In a container started by container.sh this is the same directory, mounted
# at /base as well (see build-kernels.sh); emptied, not removed, so the mount stays.
base=${PORTS_BASE:-target/ports-base}
mkdir -p "$base"
find "$base" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
git archive "$commit" | tar -x -C "$base"
echo "$commit" > "$base/.ports-base-ref"
echo "ports-base: $base at $ref ($commit)"
