#!/bin/sh
# Build the package for Node and run the smoke test against it.
#
# This is the headless check that the *built artifact* works, not just the Rust:
# the wasm module, wasm-bindgen's generated glue, and the package layout npm would
# publish. `cargo test` covers the logic; this covers the boundary.
set -eu

cd "$(dirname "$0")/../.."

./build.sh nodejs pkg-node
node tests/node/smoke.cjs pkg-node
