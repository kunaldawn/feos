#!/bin/sh
# Put a file into the base's filesystem image, for one architecture or both.
#
#   ports/test/inject.sh ARCH SOURCE DEST
#
# ARCH is aarch64, x86_64 or all. DEST is a path in the guest's image:
# - /bin/NAME goes to the base's user/out/<arch>/NAME, where the boards' build
#   scripts take each architecture's programs from (boards/common/fs_image_build.rs);
# - anything else goes to target/ports-base/.overlay/<ARCH>/DEST, which
#   build-kernels.sh lays over boards/qemu-virt/fs/ for that architecture's
#   build only (fs/ itself is shared by both boards).
# A directory SOURCE goes in whole, as the tree DEST (never under /bin); the feos image
# leaves out dotfiles and has no links, so a link in it is copied as what it names.
# The copy gets the current time, not SOURCE's (no cp -p): cargo would take an
# injected file older than its last build for one it has already embedded.
set -eu
cd "$(dirname "$0")/../.."
[ $# -eq 3 ] || { echo "usage: $0 aarch64|x86_64|all SOURCE DEST" >&2; exit 2; }
arch=$1 source=$2 dest=$3
base=${PORTS_BASE:-target/ports-base}
[ -f "$base/.ports-base-ref" ] || { echo "inject: no base; run ports/test/base.sh" >&2; exit 1; }
case "$arch" in aarch64|x86_64) arches=$arch ;; all) arches="aarch64 x86_64" ;;
    *) echo "inject: unknown arch $arch" >&2; exit 2 ;; esac
case "$dest" in /*) ;; *) echo "inject: DEST must be absolute" >&2; exit 2 ;; esac
rest=${dest#/bin/}
if [ -d "$source" ]; then
    case "$dest" in /bin|/bin/*) echo "inject: a directory can't go in /bin" >&2; exit 2 ;; esac
    for a in $arches; do
        mkdir -p "$base/.overlay/$a$dest"
        cp -RL "$source/." "$base/.overlay/$a$dest/"
        echo "$a $dest/" >> "$base/.injected"
    done
elif [ "$rest" != "$dest" ] && [ "${rest#*/}" = "$rest" ]; then
    for a in $arches; do
        mkdir -p "$base/user/out/$a"
        cp "$source" "$base/user/out/$a/$rest"
        echo "$a $dest" >> "$base/.injected"
    done
else
    mkdir -p "$base/.overlay/$arch$(dirname "$dest")"
    cp "$source" "$base/.overlay/$arch$dest"
    echo "$arch $dest" >> "$base/.injected"
fi
