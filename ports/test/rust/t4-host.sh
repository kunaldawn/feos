#!/bin/sh
# Stage T4's host side: the feos toolchain (ports/build-rust.sh's /build/rust-feos)
# knows the targets, and builds the feos test programs with them.
#
#   ports/test/rust/t4-host.sh
#
# In localhost/feos-toolchain, offline, after ports/build-rust.sh. No RUSTC_BOOTSTRAP,
# no JSON spec, no --sysroot or -L anywhere. Checks, for x86_64-unknown-feos and
# aarch64-unknown-feos:
#
# 1. rustc --print target-list names both; --print target-spec-json of the built-in
#    target is ports/targets/<target>.json (T1's spec, as rust 1.99.1 reads and prints
#    it) in every field but is-builtin (and the link arguments rustc also lists for
#    rust-lld's twin flavours), and --print cfg says target_os="feos",
#    target_family="unix", no target_env, crt-static.
# 2. stage T3's programs (ports/test/rust/t3) built with the built-in targets
#    (ports/test/rust/build.sh with RUST_FEOS), into /build/t4/t3/<arch>.
# 3. ports/test/rust/t4-cargo, a cargo project with crates.io dependencies from
#    ports/vendor (serde with its derive, serde_json, memchr, itoa), built --locked
#    --offline by the toolchain's cargo for each feos target, into
#    /build/t4/<arch>/t4-cargo; and for the host, where it runs (the toolchain's
#    host std and proc macros).
# Each feos binary is checked by check-elf.sh (a static ET_EXEC at the image base).
set -eu
P=$(cd "$(dirname "$0")/../.." && pwd)
out=${PORTS_BUILD:-/build}
tc=${RUST_FEOS:-$out/rust-feos}
host=x86_64-unknown-linux-gnu
unset RUSTC_BOOTSTRAP RUSTUP_TOOLCHAIN LD_LIBRARY_PATH
export PATH="$tc/bin:$PATH" CARGO_NET_OFFLINE=true
fails=0
pass() { echo "t4-host: PASS $*"; }
fail() { echo "t4-host: FAIL $*"; fails=$((fails + 1)); }

[ -x "$tc/bin/rustc" ] || { echo "t4-host: no $tc/bin/rustc; run ports/build-rust.sh" >&2; exit 1; }
rustc -vV
cargo -V

# 1. The built-in targets.
mkdir -p "$out/t4"
list=$(rustc --print target-list)
for t in x86_64-unknown-feos aarch64-unknown-feos; do
    if echo "$list" | grep -qx "$t"; then pass "target-list $t"; else fail "target-list $t"; fi
    # No RUSTC_BOOTSTRAP: the nightly channel must allow -Z by itself.
    if ! rustc -Zunstable-options --print target-spec-json --target "$t" > "$out/t4/$t.builtin.json"; then
        fail "target-spec-json $t: the built-in rustc refuses -Z (not a nightly channel?)"
        continue
    fi
    # T1's JSON as 1.99.1 itself takes it (the image's rustc), the reference.
    RUSTC_BOOTSTRAP=1 /usr/local/rustup/toolchains/1.99.1-$host/bin/rustc -Zunstable-options \
        --print target-spec-json --target "$P/targets/$t.json" > "$out/t4/$t.json.json"
    if python3 -I - "$out/t4/$t.builtin.json" "$out/t4/$t.json.json" <<'PY'
import json, sys
builtin, spec = (json.load(open(p)) for p in sys.argv[1:3])
for d in (builtin, spec):
    d.pop("is-builtin", None)
    for key in ("pre-link-args", "late-link-args", "post-link-args"):
        if key in d:
            d[key] = {k: v for k, v in d[key].items() if k == "gnu-lld"}
diff = sorted(k for k in set(builtin) | set(spec) if builtin.get(k) != spec.get(k))
for k in diff:
    print(f"  {k}: built-in {builtin.get(k)!r}, JSON {spec.get(k)!r}")
sys.exit(1 if diff else 0)
PY
    then pass "target-spec-json $t matches ports/targets/$t.json"
    else fail "target-spec-json $t differs from ports/targets/$t.json"
    fi
    cfg=$(rustc --print cfg --target "$t")
    ok=1
    for want in 'target_os="feos"' 'target_family="unix"' 'unix' 'target_env=""' 'target_feature="crt-static"'; do
        echo "$cfg" | grep -qx "$want" || { echo "  missing cfg $want"; ok=0; }
    done
    [ $ok = 1 ] && pass "cfg $t" || fail "cfg $t"
done

# 2. T3's programs.
if RUST_FEOS=$tc "$P/test/rust/build.sh"; then pass "t3 programs built (both targets)"; else fail "t3 programs"; fi

# 3. The cargo project.
export CARGO_TARGET_DIR=$out/t4/cargo-target RUSTC="$tc/bin/rustc"
cd "$P/test/rust/t4-cargo"
for arch in x86_64 aarch64; do
    t=$arch-unknown-feos
    mkdir -p "$out/t4/$arch"
    if cargo build --release --locked --offline --target "$t"; then
        cp "$CARGO_TARGET_DIR/$t/release/t4-cargo" "$out/t4/$arch/t4-cargo"
        if "$P/test/rust/check-elf.sh" "$out/t4/$arch/t4-cargo"; then
            pass "cargo build --target $t ($(stat -c %s "$out/t4/$arch/t4-cargo") bytes)"
        else
            fail "cargo build --target $t: not a feos program"
        fi
    else
        fail "cargo build --target $t"
    fi
done
if cargo build --release --locked --offline --target "$host" &&
        "$CARGO_TARGET_DIR/$host/release/t4-cargo" host | grep -q '^t4-cargo: ok$'; then
    pass "cargo build --target $host, run on the host"
else
    fail "cargo build --target $host"
fi

echo "t4-host: $fails failed"
[ $fails = 0 ]
