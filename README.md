# crabz2

[![CI](https://github.com/jwmurray/crabz2/actions/workflows/ci.yml/badge.svg)](https://github.com/jwmurray/crabz2/actions/workflows/ci.yml)

Pure-Rust **bzip2 compression and decompression** — no C, no bundled `libbz2`, no
third-party bzip2 crate. 🦀

`crabz2` implements both bzip2 pipelines itself, with **zero dependencies**:

- **decode** — bit reader → Huffman → MTF/RLE2 → inverse Burrows–Wheeler transform →
  RLE1 → CRC-32/BZIP2 validation;
- **encode** — RLE1 → Burrows–Wheeler transform (SA-IS suffix sorting) → MTF/RLE2 →
  length-limited canonical Huffman across 2–6 tables → bit packing.

It's [MIT-licensed](LICENSE-MIT), so it drops cleanly into any project, open or closed,
with no attribution obligation beyond the MIT notice. A pure-Rust MIT bzip2 *encoder* is
the piece the ecosystem was missing.

## Status

| Version | What it ships |
|---|---|
| **0.4.0** (now) | The full format in pure Rust. Pure-Rust **encoder** — `compress`, `Crabz2Writer`, levels 1–9, verified against system `bzip2`. Decoder behind a sans-io state machine: `no_std + alloc` core, `wasm32-unknown-unknown` and a bare-metal target checked in CI. Non-default `parallel` feature for multi-core decode. The [`crabz2-wasm`](crabz2-wasm/) npm package (`npm install crabz2`) with a streaming JS API and a [live browser demo](https://jwmurray.github.io/crabz2/). Criterion benchmarks vs libbz2 below. |
| 0.3.1 | The 0.2 decoder, hardened against crafted RLE2 run lengths, with a `cargo-fuzz` target. |
| 0.2 | Own from-scratch, dependency-free streaming decoder. Verified byte-for-byte against `bzip2`. |
| 0.1 | Thin wrapper over `bzip2-rs` (superseded; `0.1.0` remains dual `MIT OR Apache-2.0`). |

Next up: see the [ROADMAP](ROADMAP.md) — parallel *encode*, encoder profiling.

## Usage

### Decompressing

```rust
use std::io::Read;
use std::fs::File;

let mut out = String::new();
crabz2::reader(File::open("corpus.csv.bz2")?).read_to_string(&mut out)?;
```

For small in-memory buffers:

```rust
let plaintext = crabz2::decompress(&compressed_bytes)?;
```

### Compressing

```rust
let compressed = crabz2::compress(b"hello crabz2\n", crabz2::Level::BEST);
```

`Level::BEST` is level 9 (900 kB blocks, the `bzip2` default), `Level::FASTEST` is level
1, and `Level::new(1..=9)` covers the rest. For anything large, stream it — peak memory
stays around one block instead of the whole file:

```rust
use std::io::Write;
use std::fs::File;

let mut w = crabz2::writer(File::create("corpus.csv.bz2")?, crabz2::Level::BEST);
w.write_all(&plaintext)?;
w.finish()?;   // required: writes the end-of-stream marker and the stream CRC
```

`Crabz2Writer` implements `std::io::Write`, but you must call `finish()` — that is where
the end-of-stream marker and the combined-stream CRC go. Dropping the writer without it
leaves a truncated `.bz2`.

There's also a tiny example CLI:

```sh
cargo run --release --example crabz2 -- file.bz2 > file
```

## Parallel decode

bzip2 blocks are independent — own Huffman tables, own BWT, own CRC — so a multi-block
file can be decoded across cores. That is what `lbzip2` does out of process; the
`parallel` feature does it in yours:

```toml
[dependencies]
crabz2 = { version = "0.3", features = ["parallel"] }
```

```rust
// None uses one worker per core; Some(n) uses a private pool of n.
let plaintext = crabz2::decompress_parallel(&compressed_bytes, None)?;
```

The feature is **off by default** and additive: without it the crate still has zero
dependencies and the serial decoder compiles exactly as before. `rayon` is the only
dependency the crate has ever had, and only under this flag.

**How it finds the blocks.** The format has no block index and no length fields, and a
block begins at an arbitrary *bit* offset — you cannot know where block *n+1* starts
without decoding block *n*. So the decoder guesses: a scanner sweeps the compressed
bytes with a rolling 64-bit window and reports every bit offset carrying the 48-bit
block magic `0x314159265359`. Each candidate is decoded speculatively on the pool, and
the results are then chained in order — a decoded block counts only if it begins
exactly where the previous accepted block ended. The magic can also occur *inside*
entropy-coded data; such a candidate either fails to decode (usually within a few
hundred bits) or is simply never reached by the chain.

**The rule.** Output is byte-identical to `decompress`, for every input, valid or not,
and the same errors are reported for the same reasons. A block is committed only when
the serial decoder would have produced exactly those bytes from exactly those bits;
anything else — a bad header, a missing or failed candidate at a chain position, a
block bigger than the declared level allows, a CRC mismatch — re-decodes serially from
the last committed boundary. Degrading to serial is always allowed; degrading to
different bytes is not. The tests assert the identity over the fixtures, the fuzz
corpus, every truncation of a multi-block stream, and every single-bit flip in one.

**Numbers.** Apple M5 Max, 18 cores; 100 MB of mixed prose and incompressible data
(26 MB for the level-1 column), decoded from a buffer:

| Threads | level 9 (54.8 MB in, 117 blocks) | level 1 (13.8 MB in, 263 blocks) |
|---|---|---|
| 1 (serial) | 69 MB/s — 1.00× | 90 MB/s — 1.00× |
| 2 | 107 MB/s — 1.54× | 129 MB/s — 1.43× |
| 4 | 191 MB/s — 2.77× | 246 MB/s — 2.73× |
| 8 | 338 MB/s — 4.89× | 484 MB/s — 5.36× |
| 16 | 526 MB/s — 7.62× | 853 MB/s — 9.44× |

Throughput is plaintext bytes per second. Scaling tracks the block count, so a file
below the level's block size (900 KB of input at level 9) has nothing to divide and
costs what serial costs. Reproduce with the example CLI:

```sh
cargo run --release --features parallel --example parallel -- file.bz2 8 > /dev/null
```


## Multi-bench script results

The `scripts/run_bench_multi.sh` script writes a CSV named [bench_multi.csv](bench_multi.csv). The most recent run produced these numbers (MB/s of plaintext out):

| Input MB | crabz2 MB/s | libbz2 MB/s | bzip2 MB/s | parallel MB/s | parallel cmd | threads |
|---:|---:|---:|---:|---:|---|---:|
| 1 | 195.1 | 229.7 | 144.5 | 112.1 | /opt/homebrew/bin/lbzip2 | 8 |
| 5 | 1007.1 | 312.5 | 264.2 | 711.1 | /opt/homebrew/bin/lbzip2 | 8 |
| 10 | 1183.5 | 323.1 | 311.5 | 955.4 | /opt/homebrew/bin/lbzip2 | 8 |
| 50 | 1643.6 | 339.6 | 331.6 | 1854.6 | /opt/homebrew/bin/lbzip2 | 8 |
| 100 | 1719.5 | 336.2 | 346.5 | 1868.0 | /opt/homebrew/bin/lbzip2 | 8 |

See `scripts/run_bench_multi.sh` for how the measurements were collected.

**Not offered on wasm.** `parallel` needs OS threads; enabling it for a `wasm` target
is a `compile_error!` rather than a runtime surprise.

## `no_std`

Both halves are sans-io: the decoder is a state machine over `&[u8]` and a bit cursor,
the encoder works on slices and `Vec`s, and neither names an `io` type. `std` is a
default feature that adds only the adapters — `Crabz2Reader<R: Read>`, `reader`,
`decompress`, `Crabz2Writer<W: Write>`, `writer`, and `From<Error> for io::Error`.

```toml
[dependencies]
crabz2 = { version = "0.3", default-features = false }   # no_std + alloc
```

Without `std` the buffer API is `decompress_to_vec`, returning the crate's own
[`Error`](src/lib.rs) enum (`InvalidMagic`, `Truncated`, `CrcMismatch`, …). `compress`
is available in both configurations and cannot fail:

```rust
let plaintext: Vec<u8> = crabz2::decompress_to_vec(&compressed_bytes)?;
let compressed: Vec<u8> = crabz2::compress(&plaintext, crabz2::Level::BEST);
```

The decode state machine underneath is public too, for callers that are *pushed*
bytes rather than pulling them (a WebAssembly binding, an interrupt handler, an async
socket): `BlockDecoder::next_block` decodes one block from a `&[u8]`, and `consumed`
/ `rebase` let you drop what it has committed past, which is what keeps memory at one
block. Truncation is a restart, not a failure — append more input and call again.

`alloc` is required (the BWT tables and output buffer are heap-allocated); on decode
every allocation is bounded by the block size declared in the stream header, and on
encode by the level. CI checks
`wasm32-unknown-unknown` and `thumbv7em-none-eabihf`, and the MSRV is 1.63.

## WASM / browser

**Live demo: <https://jwmurray.github.io/crabz2/>** — drop a `.bz2` file, it
decompresses client-side; nothing leaves the browser.

[`crabz2-wasm/`](crabz2-wasm/) is a [wasm-bindgen](https://wasm-bindgen.github.io/wasm-bindgen/)
wrapper — the same decoder, compiled to about 33 KB of WebAssembly and published to
npm as [`crabz2`](crabz2-wasm/README.md). It exports `decompress(Uint8Array)` for a
buffer you already hold, and a push-based `Bz2Decoder` class for files you would
rather not hold twice:

```js
const dec = new Bz2Decoder();
for await (const chunk of file.stream()) parts.push(dec.push(chunk));
parts.push(dec.finish());
```

Because it drives the same sans-io state machine, the streaming class holds one block
rather than the file, whatever its size. [`crabz2-wasm/www/`](crabz2-wasm/www/) is a
dependency-free demo page: drop a `.bz2` file, watch it decompress in the tab, get the
result back — nothing is uploaded anywhere.

### Real-world example: CourtListener bulk data

[`examples/courtlistener.rs`](examples/courtlistener.rs) streams a bzip2-compressed
[CourtListener](https://www.courtlistener.com/help/api/bulk-data/) bulk CSV download
straight through the decoder — the pure-Rust, in-process replacement for an
`lbzip2 -dc file.bz2 | …` shell-out. The HTTP body flows directly into `crabz2::reader`,
so nothing is fully buffered no matter how large the file:

```sh
cargo run --release --example courtlistener            # small `courts` table (~80 KB)
cargo run --release --example courtlistener -- citations
cargo run --release --example courtlistener -- bulk-data/opinions-2026-06-30.csv.bz2
```

It downloads with a pure-Rust HTTPS client (`ureq` + rustls, a dev-dependency only — it
is **not** in the dependency graph for anyone who depends on `crabz2`).

## Compression ratio

The encoder is not a byte-for-byte clone of libbz2 — it makes its own table decisions —
so the bar is the compressed size. Measured against system `bzip2` 1.0.8 at the same
level (negative means crabz2 is smaller):

| Input | `-1` | `-9` |
|---|---|---|
| English text, 3 MB | −1.00% | −0.20% |
| Rust source, 3 MB | −0.18% | −0.15% |
| CSV (CourtListener shape), 3.5 MB | −0.35% | −0.01% |
| `/usr/share/dict/words`, 2.5 MB | −0.01% | +0.00% |
| Incompressible random bytes, 3 MB | −0.01% | +0.01% |

The small edge comes from using package-merge for the length-limited Huffman codes,
which is optimal where libbz2's length-capping heuristic is not, plus one extra
table-selection pass. The test suite asserts a much looser envelope — within 15% — so
these numbers can drift without turning CI red.

Speed is mixed and not yet tuned; that is what the benchmark workstream in the
[ROADMAP](ROADMAP.md) is for. On ordinary data libbz2 compresses roughly 1.8× faster
than crabz2; on highly repetitive input the ranking flips, because SA-IS is O(n) where
libbz2's block sorter degrades. Decompression speed is unchanged by this work.

## Serial decode speed vs libbz2

Single-thread decode throughput, **Apple M-series (M5 Max, macOS), one core**, measured
with `cargo bench` (criterion) against **libbz2 1.0.8** — the C reference, compiled from
source by `bzip2-sys` and driven through the `bzip2` crate — decoding the exact same
streams. Each input is 10 MiB of plaintext compressed with `bzip2 -9`; throughput is
plaintext bytes out per second. The corpora are generated at bench time by a seeded
PRNG, so `cargo bench` reproduces them anywhere and nothing large is checked in.

| Input (10 MiB) | `bzip2 -9` ratio | crabz2 | libbz2 (C) | crabz2 vs C |
|---|---|---|---|---|
| English-like text | 3.8x | 56 MB/s | 57 MB/s | −2% |
| CSV, court bulk-data shape | 6.0x | 71 MB/s | 74 MB/s | −4% |
| Incompressible random bytes | 1.0x | 43 MB/s | 34 MB/s | +29% |

Figures are the best of five runs on a machine that was not otherwise idle; contention
only ever costs throughput, and the run-to-run spread reached 10%.

The honest summary: on compressible input the from-scratch decoder lands a couple of
percent behind C — close enough that prose and CSV are best read as parity rather than
a win either way — and about 30% ahead on incompressible input, where the RLE1 pass has
nothing to expand. This is one microarchitecture (aarch64) and one compiler pair; the
remaining difference has not been profiled and no inner-loop micro-optimization has been
done. Multi-core numbers are in [Parallel decode](#parallel-decode) above.

The benchmark also cross-validates: every corpus must decode byte-identically through
crabz2 before it is timed.

```sh
cargo bench
```

## Correctness

`crabz2` streams one block at a time (peak memory ≈ one compressed plus one
decompressed block), handles concatenated multi-stream `.bz2` input on decode, verifies
both the per-block and combined-stream CRCs, and errors loudly on truncated or corrupt
input rather than emitting partial garbage. It is tested byte-for-byte against system
`bzip2` across block levels 1–9, inputs from empty to tens of MB, RLE-heavy and
high-entropy data, and multi-stream files.

The encoder is held to three bars, all in the test suite: everything it produces must
round-trip through our own decoder; system `bzip2 -d` must reproduce the input byte for
byte, and `bzip2 -t` must accept our CRCs; and compressed size must stay inside a stated
envelope of libbz2's. The `bzip2` tests skip with a message when the binary is absent.

Legacy "randomized" blocks (bzip2 < 0.9.5, ~1998) are rejected with a clear error rather
than silently miscorrupted.

## Fuzzing

A codec for untrusted input has to say what it is fuzzed for. The
[`fuzz/`](fuzz/) directory is a [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz)
crate (nightly-only, outside the parent crate, so `crabz2` itself stays at zero
dependencies):

| Target | Input | Asserts |
|---|---|---|
| `fuzz_decompress` | arbitrary bytes as a `.bz2` stream | decode invariants 1–3 |
| `fuzz_roundtrip` | first byte picks the level, rest is plaintext | encode invariants 4–6 |

**Decode invariants.**

1. **Never panics.** Every byte string is either decoded or rejected with an
   `io::Error`. No input can cause an out-of-range index, an arithmetic overflow,
   an abort, or a non-terminating decode.
2. **Bounded allocation.** The block size declared in the stream header — nothing
   else in the input — bounds memory. The target measures the decoder's live
   allocation high-water mark with a tracking global allocator and asserts it stays
   inside the cap that the declared level structurally implies (the BWT buffer plus
   the worst-case RLE1 expansion of one block). No crafted header, run length, or
   symbol count can make the decoder ask for more.
3. **Clean errors on malformed input.** Corrupt, truncated, and hostile streams
   produce an `Err`, never partial or fabricated plaintext, and the streaming
   `reader` and buffering `decompress` always agree on validity and output length.

**Encode invariants.**

4. **Never panics.** No plaintext, however degenerate, aborts the compressor.
5. **Lossless round trip.** `decompress(compress(data, level)) == data` byte for
   byte, at every level 1–9. The level is taken from the first input byte so the
   fuzzer explores all nine block sizes, and the stream is checked to declare the
   level it was asked for.
6. **Chunking is invisible.** Feeding the same plaintext to `Crabz2Writer` in
   arbitrary, input-derived chunk sizes — including zero-length writes — produces
   byte-identical output to one-shot `compress`. The RLE1 splitter carries run
   state across `push` calls, so this is where a streaming encoder goes wrong.

The committed seed corpus holds the crate's own test vectors, small files compressed
by system `bzip2` at levels 1 and 9 (prose, run-heavy, incompressible, and full-256
byte alphabets), concatenated multi-stream files, RLE1 run-length edge cases, and
minimized inputs from past findings. CI runs a 60-second smoke pass of each target on
nightly, on pull requests touching `src/` and on a weekly schedule; longer campaigns
are run out of band.

```sh
cargo +nightly fuzz run fuzz_decompress -- -max_total_time=300
cargo +nightly fuzz run fuzz_roundtrip  -- -max_total_time=300 -max_len=150000 -len_control=0
```

The round-trip target wants the larger `-max_len`: level 1 fills a block at 100 kB of
RLE1 output, and anything smaller never emits a second block, leaving the block
boundary logic untested. `-len_control=0` is what makes that limit bite — libFuzzer
otherwise ramps input length up so gradually that a short run never approaches it.

**Findings.** Fuzzing found one real bug, fixed in 0.3.1: the RLE2 zero-run
accumulator shifted by an attacker-controlled bit count, so a stream of more than 64
consecutive RUNA/RUNB symbols panicked with a shift overflow instead of being
rejected. Runs are now bounded by the declared block size, as libbz2 does. The
round-trip target has not turned up an encoder defect.

## License

Licensed under the [MIT license](LICENSE-MIT).
