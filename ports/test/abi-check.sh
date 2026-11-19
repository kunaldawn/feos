#!/bin/sh
# Host test of relibc's feos constants (ports/src/relibc/src/platform/feos/abi.rs)
# against the kernel's feos-abi: a base's libs/abi as a dev-dependency (structures and
# constants), its lib.rs as text (call and error numbers), and the tree's
# docs/feos-abi-v2.md section 3 (every call number from 31 to 108).
#
#   ports/test/abi-check.sh [REF]       # base REF, default refs/feos-checkpoints/1c.6
#
# In localhost/feos-toolchain (offline). Makes the base with base.sh, so don't run it
# while another script is using target/ports-base. FEOS_ABI_LIB names another
# libs/abi/src/lib.rs to compare call and error numbers with (the working tree's, say).
set -eu
cd "$(dirname "$0")/../.."
ports/test/base.sh "${1:-refs/feos-checkpoints/1c.6}"
export RUSTUP_TOOLCHAIN=1.99.1 CARGO_TARGET_DIR=${PORTS_BUILD:-/build}/abi-check-target
# From its directory, so ports/.cargo/config.toml (host target, vendored sources) is
# the configuration cargo takes, not only the root's (kernel target).
cd ports/test/abi-check
cargo test --offline -- --nocapture
