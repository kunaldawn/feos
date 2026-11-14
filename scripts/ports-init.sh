#!/bin/sh
# Fetch every third-party source the toolchain build needs, once. After this
# the whole tree builds with no network (see ports/README.md).
#
#   git clone https://github.com/kunaldawn/feos && cd feos && scripts/ports-init.sh
#
# Same as `git submodule update --init --recursive`, except that it skips the
# submodules of rust's own that no feos build uses (books, gcc, enzyme,
# rustc-perf: several GB). Every fetch is shallow, at the pinned commit.
set -eu
cd "$(dirname "$0")/.."

git submodule update --init --depth 1 --jobs 4
# rust: LLVM, cargo and std's backtrace. Nothing else is built.
git -C ports/src/rust submodule update --init --depth 1 \
    src/llvm-project src/tools/cargo library/backtrace
# relibc: openlibm and dlmalloc-rs.
git -C ports/src/relibc submodule update --init --depth 1 --recursive

# `+` is a checkout off its pinned commit, `U` a conflict. (`-`, not fetched,
# is expected for the rust submodules skipped above.)
if git submodule status --recursive | grep '^[+U]'; then
    echo "ports-init: the submodules above are not at their pinned commits" >&2
    exit 1
fi
echo "ports-init: sources ready; builds from here on need no network"
