# crabz2 (WebAssembly)

Pure-Rust **bzip2 decompression** in the browser and in Node — no C, no `libbz2`, no
JS reimplementation. This is a thin [wasm-bindgen](https://wasm-bindgen.github.io/wasm-bindgen/)
wrapper around the [`crabz2`](https://github.com/jwmurray/crabz2) crate, whose decoder
is a single dependency-free Rust file. The compiled module is about 33 KB of wasm.

Published to npm as **`crabz2`**. Not published to crates.io — the Rust crate here
exists only to be compiled to wasm.

## Install

```sh
npm install crabz2
```

## Use

### One buffer

```js
import init, { decompress } from "crabz2";

await init();                                   // loads the wasm module
const plain = decompress(new Uint8Array(bytes)); // Uint8Array in, Uint8Array out
```

`decompress` throws a JS `Error` on malformed, truncated, or CRC-mismatched input;
the message is the decoder's own (`bzip2 CRC mismatch`, `unexpected end of bzip2
stream`, …). Concatenated multi-stream files are handled, matching `bzip2 -dc`.

### Streaming, for files you would rather not hold twice

```js
import init, { Bz2Decoder } from "crabz2";

await init();

const dec = new Bz2Decoder();
const parts = [];
const reader = file.stream().getReader();

for (;;) {
  const { done, value } = await reader.read();
  if (done) break;
  const out = dec.push(value);            // plaintext of any blocks that completed
  if (out.length) parts.push(out);
}
parts.push(dec.finish());                 // the rest, and the end-of-stream check
dec.free();

const blob = new Blob(parts);
```

| Member | |
|---|---|
| `push(chunk: Uint8Array): Uint8Array` | Accept compressed bytes; return the plaintext of whatever blocks completed, usually empty. Throws on invalid input, after which the decoder is spent. |
| `finish(): Uint8Array` | End of input: return the remaining plaintext and verify the stream ended where it said it would. Throws on truncation. |
| `bytesIn` / `bytesOut` | Counters, for progress reporting. |
| `bytesBuffered` | Compressed bytes held but not yet decoded — the decoder's live footprint. |
| `free()` | Release the wasm-side object. Worth calling; not required for correctness. |

**Memory.** The decoder holds a block, not the file. Compressed bytes are dropped as
soon as the decoder commits past them and plaintext is handed to JS as it is
produced, so its own footprint is a small multiple of the bzip2 block size (100 KB to
900 KB, whatever the file declares) no matter how large the input. Anything you do
with the returned chunks — concatenating them into a `Blob`, say — is your memory,
not the decoder's.

**Chunk sizes.** Any size works, down to one byte. The core state machine restarts
rather than resumes a partially-read block, so the wrapper batches: it buffers, and
re-attempts a block only when enough new input has arrived to make the attempt
worthwhile — the size of the previous block, which is a tight estimate, growing
geometrically when that estimate falls short. Streaming a file in 4 KiB chunks costs
about 15% over decompressing the whole buffer at once, and about nothing at 64 KiB
(the chunk size a browser's `File.stream()` hands you).

### Plain browser, no bundler

The `web` target emits an ES module that fetches its own `.wasm`:

```html
<script type="module">
  import init, { Bz2Decoder } from "./pkg/crabz2.js";
  await init();
</script>
```

Serve it over HTTP — `file://` will not load the wasm.

### Bundlers

`npm install crabz2` gives you the `bundler` build, which webpack, rollup, and Vite
import directly; `init()` is not needed there.

## Demo

[`www/index.html`](www/index.html) is a dependency-free page that takes a dropped
`.bz2` file, decompresses it in the tab with `Bz2Decoder`, shows progress and the
decoder's live buffer, and hands back a download. To run it:

```sh
./build.sh web www/pkg
python3 -m http.server -d www 8000
```

## Build

```sh
./build.sh                  # web target -> pkg/
./build.sh bundler          # bundler target
./build.sh nodejs pkg-node  # CommonJS, what the smoke test loads
```

`build.sh` wraps `wasm-pack build --release --target … --out-name crabz2` and fixes
the package name: wasm-pack takes it from the crate, and the crate cannot be named
`crabz2` because it shares a Cargo workspace with the library of that name.

Requires [wasm-pack](https://rustwasm.github.io/wasm-pack/) and the
`wasm32-unknown-unknown` target:

```sh
cargo install wasm-pack --locked
rustup target add wasm32-unknown-unknown
```

## Test

```sh
cargo test -p crabz2-wasm   # the wrapper's logic, on the host
./tests/node/run.sh         # the built package, under node
```

The node smoke test is the one that exercises the real artifact: it compresses
several megabytes with the system `bzip2` at levels 1 and 9 — text, incompressible
bytes, concatenated streams — pushes them through the built module in 4 KiB chunks,
and checks the output byte for byte, that corruption and truncation throw, and that
buffering stays bounded by the block rather than the file. CI runs both on every
pull request.

## License

MIT, same as the crate. See [LICENSE-MIT](LICENSE-MIT).
