#!/bin/sh
# Build the npm package with wasm-pack.
#
#   ./build.sh [target] [out-dir]
#
# Defaults to the `web` target (ES modules with an explicit `init()`, what the demo
# page under www/ loads) into ./pkg. Other useful invocations:
#
#   ./build.sh bundler          # webpack/rollup/vite consumers
#   ./build.sh nodejs pkg-node  # CommonJS, what the node smoke test loads
#   ./build.sh web www/pkg      # build straight into the demo page's directory
#
# The npm package is named `crabz2`; the Rust crate cannot be, because it shares a
# workspace with the library of that name. wasm-pack takes the package name from the
# crate, so the name is corrected here rather than left wrong in the published
# manifest. `--out-name` keeps the emitted files `crabz2.js` / `crabz2_bg.wasm` so
# imports read the way the package is named.
set -eu

target="${1:-web}"
out="${2:-pkg}"

cd "$(dirname "$0")"

wasm-pack build --release --target "$target" --out-dir "$out" --out-name crabz2

node -e '
const fs = require("fs");
const path = process.argv[1];
const pkg = JSON.parse(fs.readFileSync(path, "utf8"));
pkg.name = "crabz2";
pkg.repository = { type: "git", url: "git+https://github.com/jwmurray/crabz2.git" };
pkg.homepage = "https://github.com/jwmurray/crabz2";
pkg.keywords = ["bzip2", "bz2", "decompress", "wasm", "webassembly", "rust"];
// npm ships a file named LICENSE without being asked; ours is LICENSE-MIT, so it
// has to be listed or the tarball would carry an MIT declaration and no text.
if (!pkg.files.includes("LICENSE-MIT")) pkg.files.push("LICENSE-MIT");
fs.writeFileSync(path, JSON.stringify(pkg, null, 2) + "\n");
' "$out/package.json"

echo "built $out (npm package: crabz2)"
