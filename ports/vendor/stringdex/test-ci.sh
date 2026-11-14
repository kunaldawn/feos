#!/bin/bash
set -ex

cd `dirname "$0"`
rm -rf perf.data perf.data.old flamegraph.svg target examples/search.data examples/search.index examples/search-index.js js/search.data js/search.index js/search-index.js js/rng-seed.txt

expected_channel_line=`rustc --version | sed -E 's@rustc ([0-9\.]+) .+@channel = "\1"@'`
actual_channel_line=`tail -n1 rust-toolchain.toml`
[[ "$expected_channel_line" == "$actual_channel_line" ]]

cd js
if [ "$1" = "clean" ]; then
    rm -rf node_modules
fi
if ! [ -d "node_modules" ]; then
    npm install typescript@5.7.3
    npm install @types/node@
    npm install es-check@6.1.1 eslint@8.6.0
fi
rm -rf search.index search.data search-index.js
node node_modules/.bin/tsc --project tsconfig.json
node node_modules/.bin/es-check es2019 stringdex.js
node node_modules/.bin/eslint -c .eslintrc.js stringdex.js
# Yes, I'm running it with --release.
# That's pretty important, actually. Our tests take too long otherwise.
cargo test --features test_js --release

echo "making sure the test harness fails when js is broken"
export STRINGDEX_JS_SEARCH_BREAK=1
echo "cargo will write out a note about the tests failing"
echo "the line: \"error: test failed, to rerun pass \`--lib\`\" is expected"
! cargo test --features test_js --release 2>&1 >/dev/null
echo "the test harness has successfully failed"
