#!/bin/sh
# Re-vendor every crate the ports build, and prune what no build reaches. The
# one step besides scripts/ports-init.sh and toolchain-image.sh that uses the
# network. Run it in localhost/feos-toolchain, with the tree at /src:
#
#   docker run --rm --name feos-ports-vendor -v "$PWD":/src -v feos-ports-target:/build \
#       localhost/feos-toolchain ports/vendor.sh
#
# 1. One `cargo vendor` over rust's workspaces, gitoxide and cbindgen into vendor/.
# 2. relibc on its own (see vendor-merge.py for why), merged in: its crates.io
#    packages into vendor/, its git ones into vendor-git/.
# 3. The keep list from `cargo metadata` of all of them on both Linux triples,
#    then prune-vendor.py.
# Afterwards, check that git ignores nothing under ports/ (ports/README.md).
set -eu
P=$(cd "$(dirname "$0")" && pwd)
# relibc and cbindgen pin nightlies in rust-toolchain.toml; every ports build uses
# the one toolchain the image has.
export RUSTUP_TOOLCHAIN=1.99.1 RUSTC_BOOTSTRAP=1 CARGO_NET_OFFLINE=false
export CARGO_HOME="${CARGO_HOME_DIR:-/build/cargo-home}"
mkdir -p "$CARGO_HOME"

# The workspaces of step 1, the rust workspace first (cargo vendor runs in it).
MAIN="src/rust/Cargo.toml src/rust/src/tools/cargo/Cargo.toml src/rust/library/Cargo.toml
src/rust/library/stdarch/Cargo.toml src/rust/src/bootstrap/Cargo.toml
src/rust/compiler/rustc_codegen_cranelift/Cargo.toml src/gitoxide/Cargo.toml src/cbindgen/Cargo.toml"
# Step 2's, each vendored apart from the main run.
APART="src/relibc/Cargo.toml"

sync=""
for m in $MAIN; do
    [ "$m" = src/rust/Cargo.toml ] || sync="$sync --sync $P/$m"
done
# shellcheck disable=SC2086
(cd "$P/src/rust" && cargo vendor --locked $sync "$P/vendor" >/dev/null)

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
gitcfg="$P/vendor-git.toml"
: > "$scratch/sources.toml"
for m in $APART; do
    rm -rf "$scratch/v"
    (cd "$P/$(dirname "$m")" && cargo vendor --locked --versioned-dirs "$scratch/v" >> "$scratch/sources.toml")
    python3 "$P/vendor-merge.py" "$scratch/v" "$P/vendor" "$P/vendor-git"
done
# The git source replacements cargo printed, for ports/.cargo/config.toml.
grep -A3 '^\[source\."git+' "$scratch/sources.toml" | grep -v '^--$' > "$gitcfg" || true
echo "vendor.sh: git sources (compare with ports/.cargo/config.toml):"
cat "$gitcfg"
rm -f "$gitcfg"

for m in $MAIN $APART; do
    for t in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
        for f in "" --all-features; do
            cargo metadata $f --format-version 1 --locked \
                --manifest-path "$P/$m" --filter-platform $t
        done
    done
done | python3 -c 'import json,sys
for line in sys.stdin:
    for p in json.loads(line)["packages"]:
        if p.get("source", "") and p["source"].startswith("registry+"): print(p["name"] + "\t" + p["version"])' \
    | sort -u > "$scratch/keep.txt"
python3 "$P/prune-vendor.py" "$P/vendor" "$scratch/keep.txt"
