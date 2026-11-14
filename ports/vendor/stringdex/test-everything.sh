#!/bin/bash
set -ex
cd $(dirname "$0")
rm -f perf.data perf.data.old
bash test-ci.sh
cargo test --features test_js,quickcheck --release
cargo run --example dictionary --release

