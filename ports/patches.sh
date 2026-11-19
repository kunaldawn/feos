#!/bin/sh
# Keep feos's changes to the third-party submodules in this repo, as one patch per
# submodule under ports/patches/, until they live as commits in forks (see
# ports/PATCHES.md). A commit of this repo records only each submodule's pinned
# commit, so without these files a fresh clone would lose every change.
#
#   ports/patches.sh save    write ports/patches/<path>.patch from each submodule's
#                            working tree (new files included), drop stale ones
#   ports/patches.sh check   exit 1 if the saved patches differ from the working trees
#   ports/patches.sh apply   apply the saved patches to clean, pinned checkouts
#                            (scripts/ports-init.sh runs this); already-applied is fine
#
# Nothing here changes git state: `save` diffs through a throwaway index, never the
# submodule's own.
set -eu
cd "$(dirname "$0")/.."
root=$(pwd)
dir=ports/patches

# Every checked-out submodule, nested ones included (rust's llvm-project, cargo...).
subs() {
    git submodule foreach --recursive --quiet 'echo "$displaypath"' 2>/dev/null
}

name() { echo "$1" | sed 's|^ports/src/||; s|/|__|g'; }

diff_of() {
    idx=$(mktemp "$root/target/patches-idx.XXXXXX")
    rm -f "$idx"
    GIT_INDEX_FILE=$idx git -C "$1" read-tree HEAD
    # Nested submodules are their own patches, so leave gitlinks out.
    GIT_INDEX_FILE=$idx git -C "$1" add -A --ignore-errors . 2>/dev/null || true
    GIT_INDEX_FILE=$idx git -C "$1" diff --cached --binary --ignore-submodules=all HEAD
    rm -f "$idx"
}

mkdir -p target
case "${1:-}" in
save)
    mkdir -p "$dir"
    keep=""
    for s in $(subs); do
        p="$dir/$(name "$s").patch"
        d=$(diff_of "$s")
        if [ -n "$d" ]; then
            printf '%s\n' "$d" >"$p"
            keep="$keep $p"
            echo "saved $p ($(printf '%s\n' "$d" | grep -c '^diff --git') files)"
        fi
    done
    for p in "$dir"/*.patch; do
        [ -e "$p" ] || continue
        case " $keep " in *" $p "*) ;; *) rm -f "$p"; echo "removed stale $p" ;; esac
    done
    ;;
check)
    rc=0
    for s in $(subs); do
        p="$dir/$(name "$s").patch"
        d=$(diff_of "$s")
        if [ -n "$d" ]; then
            if [ ! -e "$p" ] || [ "$(printf '%s\n' "$d")" != "$(cat "$p")" ]; then
                echo "stale: $p (run ports/patches.sh save)"; rc=1
            fi
        elif [ -e "$p" ]; then
            echo "not applied: $p"; rc=1
        fi
    done
    exit $rc
    ;;
apply)
    for p in "$dir"/*.patch; do
        [ -e "$p" ] || continue
        s="ports/src/$(basename "$p" .patch | sed 's|__|/|g')"
        if [ ! -e "$s/.git" ]; then
            echo "patches: $s is not checked out" >&2; exit 1
        fi
        if git -C "$s" apply --binary --reverse --check "$root/$p" 2>/dev/null; then
            echo "already applied: $p"
        else
            git -C "$s" apply --binary "$root/$p"
            echo "applied $p"
        fi
    done
    ;;
*)
    echo "usage: $0 save|check|apply" >&2; exit 2 ;;
esac
