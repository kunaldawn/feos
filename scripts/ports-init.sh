#!/bin/sh
# Fetch every third-party source the toolchain build needs, once. After this
# the whole tree builds with no network (see ports/README.md).
#
#   git clone https://github.com/kunaldawn/feos && cd feos && scripts/ports-init.sh
#
# Same as `git submodule update --init --recursive`, except that:
# - it skips the submodules of rust's own that no feos build uses (books, gcc,
#   enzyme, rustc-perf: several GB);
# - every fetch is shallow, at the pinned commit;
# - a submodule whose server won't send a commit by hash (Savannah answers 500)
#   names its tag as `feos-tag` in .gitmodules; that tag is fetched instead and
#   must resolve to exactly the pinned commit.
set -eu
cd "$(dirname "$0")/.."

by_hash=""
by_tag=""
for path in $(git config -f .gitmodules --get-regexp '^submodule\..*\.path$' | cut -d' ' -f2); do
    if git config -f .gitmodules "submodule.$path.feos-tag" >/dev/null; then
        by_tag="$by_tag $path"
    else
        by_hash="$by_hash $path"
    fi
done

# shellcheck disable=SC2086 # word splitting of the path lists is intended
git submodule update --init --depth 1 --jobs 4 -- $by_hash

for path in $by_tag; do
    tag=$(git config -f .gitmodules "submodule.$path.feos-tag")
    url=$(git config -f .gitmodules "submodule.$path.url")
    pin=$(git ls-tree HEAD "$path" | awk '{print $3}')
    git submodule init -- "$path"
    if [ ! -e "$path/.git" ]; then
        rmdir "$path" 2>/dev/null || true
        git -c advice.detachedHead=false clone --quiet --depth 1 --branch "$tag" "$url" "$path"
        git submodule absorbgitdirs -- "$path"
    elif [ "$(git -C "$path" rev-parse HEAD)" != "$pin" ]; then
        # Left at the wrong commit by an earlier run.
        git -C "$path" fetch --quiet --depth 1 "$url" tag "$tag"
        git -C "$path" -c advice.detachedHead=false checkout --quiet "$pin"
    fi
    got=$(git -C "$path" rev-parse HEAD)
    if [ "$got" != "$pin" ]; then
        echo "ports-init: $path: tag $tag is $got, but the pin is $pin" >&2
        exit 1
    fi
done

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
