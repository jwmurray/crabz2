# crabz2

[![CI](https://github.com/jwmurray/crabz2/actions/workflows/ci.yml/badge.svg)](https://github.com/jwmurray/crabz2/actions/workflows/ci.yml)

Pure-Rust **bzip2 compression and decompression** — no C, no bundled `libbz2`, no
third-party bzip2 crate. 🦀

**Faster than C `libbz2` in our decode benchmarks — like for like, on both axes.**
Single-threaded on 10 MiB corpora, `crabz2` decodes **9–17% faster** than C `libbz2`
(via the `bzip2` crate) across prose, CSV, and incompressible input. With the
`parallel` feature on 8 threads, it decodes **1.15x faster** than parallel C `lbzip2`
at 50–100 MB. Both comparisons, and what they do and do not show, are below.

![Decode throughput: crabz2 vs libbz2, bzip2, and lbzip2](https://raw.githubusercontent.com/jwmurray/crabz2/main/docs/bench_multi.svg)

*The chart plots `crabz2` with `parallel` (8 threads) alongside single-threaded
`libbz2` and `bzip2` and parallel `lbzip2`, from an earlier run of
`scripts/run_bench_multi.sh`. The single-threaded C columns are there for scale, not
as a like-for-like comparison — only `crabz2` serial vs `libbz2`, and `crabz2`
parallel vs `lbzip2`, compare equal thread counts. Current numbers are in the tables
below.*

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
| **0.7.0** (now) | Decode pairs block walks only when the pool is *not* one worker per core. Pairing doubles a worker's resident `tt` footprint (~3.6 MB per level-9 block) to buy chain overlap that thread-level parallelism already supplies, so past a few cores it was pure cache pressure. Physical-core count is now detected (sysfs on Linux, `sysctl` on macOS) to tell one-per-core from SMT oversubscription. Measured on a 100 MB realistic corpus: M5 Max default 370 -> 483 MB/s, Ryzen 9 7940HS 166 -> 219 at 8 threads and 120 -> 220 at 4; no configuration regressed. |
| 0.6.1 | Benchmark documentation corrected: like-for-like comparisons only (crabz2 serial vs `libbz2`, crabz2 `parallel(8)` vs `lbzip2 -n 8`), the parallel-vs-serial "6.6x" headline removed, decode numbers re-measured, and `run_bench_multi.sh` extended with a serial crabz2 column. No code changes. |
| 0.6.0 | **Zero `unsafe` by default**, enforced by `#![forbid(unsafe_code)]`; the raw-pointer hot paths moved behind the non-default `unsafe-fast` feature after benchmarks showed the difference is within noise. |
| 0.5.0 | Decode hot path rewritten from profiling: write-only IBWT threading, interleaved pair walks (software-pipelined serially, paired speculation in parallel), fast-table Huffman over a 64-bit bit reservoir, arena MTF, cached thread pools, parallel output assembly. Decode now **beats C libbz2 single-threaded and lbzip2 in parallel at every benchmarked size**. |
| 0.4.0 | The full format in pure Rust. Pure-Rust **encoder** — `compress`, `Crabz2Writer`, levels 1–9, verified against system `bzip2`. Decoder behind a sans-io state machine: `no_std + alloc` core, `wasm32-unknown-unknown` and a bare-metal target checked in CI. Non-default `parallel` feature for multi-core decode. The [`crabz2-wasm`](crabz2-wasm/) npm package (`npm install crabz2`) with a streaming JS API and a [live browser demo](https://jwmurray.github.io/crabz2/). Criterion benchmarks vs libbz2 below. |
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
crabz2 = { version = "0.6", features = ["parallel"] }
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

The `scripts/run_bench_multi.sh` script writes a CSV named [bench_multi.csv](bench_multi.csv)
and is the source of the chart above ([docs/bench_multi.svg](docs/bench_multi.svg)).
Throughput is MB/s of plaintext out, median of five iterations. The two comparisons
that hold thread count equal are **crabz2 serial vs libbz2** and **crabz2 parallel(8)
vs lbzip2 -n 8**; the ratio columns give those and only those.

| Input MB | crabz2 serial | libbz2 (C, 1 thread) | **serial ratio** | crabz2 par(8) | lbzip2 -n 8 (C) | **parallel ratio** | bzip2 (C, 1 thread) |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 265.1 | 242.4 | **1.09x** | 259.4 | 132.1 | **1.96x** | 184.1 |
| 5 | 396.8 | 309.1 | **1.28x** | 1192.5 | 726.2 | **1.64x** | 280.2 |
| 10 | 409.9 | 314.6 | **1.30x** | 1508.4 | 964.9 | **1.56x** | 288.2 |
| 50 | 425.7 | 323.7 | **1.32x** | 2013.8 | 1758.5 | **1.15x** | 329.2 |
| 100 | 397.9 | 328.4 | **1.21x** | 2171.3 | 1888.0 | **1.15x** | 333.1 |

**Read these with three caveats.**

1. **The corpus is a best case.** The script builds its plaintext by repeating the
   44-byte sentence `the quick brown fox jumps over the lazy dog\n`. That is far more
   redundant than real data and flatters every stage of the decoder. For throughput on
   realistic input, use the criterion numbers in
   [Serial decode speed vs libbz2](#serial-decode-speed-vs-libbz2) below, which is the
   comparison to quote.
2. **`lbzip2` and `bzip2` pay process costs that `crabz2` and `libbz2` do not.** The
   two C binaries are timed as subprocesses reading a file and writing to a pipe;
   `crabz2` and `libbz2` are timed in-process on a warm in-memory buffer. This inflates
   the parallel ratio, most visibly at 1 MB, where process startup dominates and the
   1.96x figure should be discounted accordingly.
3. **Parallel speedup tracks block count, not input size.** At 1 MB there are only two
   blocks to divide, which is why `crabz2 par(8)` is no faster than `crabz2 serial`
   there.

See `scripts/run_bench_multi.sh` for how the measurements were collected.

**Not offered on wasm.** `parallel` needs OS threads; enabling it for a `wasm` target
is a `compile_error!` rather than a runtime surprise.

## Zero `unsafe` by default (and the `unsafe-fast` switch)

**The default build contains no `unsafe` code at all**, and the compiler proves it:
`#![forbid(unsafe_code)]` is active whenever the non-default `unsafe-fast` feature
is off, so a stray `unsafe` block is a compile error, not a convention. The decoder's
three hot paths — the IBWT threading pass, the permutation walk, and the parallel
output assembly — have raw-pointer variants behind `unsafe-fast` (this is Rust's
answer to a C `#ifdef` build variant: `#[cfg(feature = …)]` conditional
compilation, with the guarantee machine-checked):

```toml
crabz2 = { version = "0.6" }                              # zero unsafe, default
crabz2 = { version = "0.6", features = ["unsafe-fast"] }  # raw-pointer hot paths
```

**Measured difference: nothing outside noise.** On the multi-bench corpus and a
word-salad corpus (Apple M5 Max, median of 5, both builds run back-to-back):

| Metric | safe (default) | `unsafe-fast` |
|---|---:|---:|
| serial, repetitive 100 MB | 410 MB/s | 406 MB/s |
| parallel(8), repetitive 100 MB | 2235 MB/s | 2168 MB/s |
| serial, word-salad 100 MB | 106 MB/s | 104 MB/s |
| parallel(8), word-salad 100 MB | 301 MB/s | 318 MB/s |

The hot loops are memory-latency- and bandwidth-bound, so bounds-check branches
predict perfectly and hide under the dependent-load chains; the zero-fill that
replaces the uninitialized buffer streams at memory bandwidth. The `unsafe` paths
date from when the loops were compute-bound; after the hot-path restructuring they
buy roughly nothing on this hardware — the switch exists so the trade can be
measured on other hardware rather than taken on faith. Both configurations are
tested and linted in CI and produce byte-identical output.

## `no_std`

Both halves are sans-io: the decoder is a state machine over `&[u8]` and a bit cursor,
the encoder works on slices and `Vec`s, and neither names an `io` type. `std` is a
default feature that adds only the adapters — `Crabz2Reader<R: Read>`, `reader`,
`decompress`, `Crabz2Writer<W: Write>`, `writer`, and `From<Error> for io::Error`.

```toml
[dependencies]
crabz2 = { version = "0.6", default-features = false }   # no_std + alloc
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
libbz2's block sorter degrades. These are *encoder* figures; decode throughput is a
separate matter, rewritten in 0.5.0 and measured below.

## Serial decode speed vs libbz2

Single-thread decode throughput, **Apple M-series (M5 Max, macOS), one core**, measured
with `cargo bench` (criterion) against **libbz2 1.0.8** — the C reference, compiled from
source by `bzip2-sys` and driven through the `bzip2` crate — decoding the exact same
streams. Each input is 10 MiB of plaintext compressed with `bzip2 -9`; throughput is
plaintext bytes out per second. The corpora are generated at bench time by a seeded
PRNG, so `cargo bench` reproduces them anywhere and nothing large is checked in.

| Input (10 MiB) | `bzip2 -9` ratio | crabz2 | libbz2 (C) | crabz2 vs C |
|---|---|---|---|---|
| English-like text | 3.8x | 69.0 MiB/s | 59.2 MiB/s | **+17%** |
| CSV, court bulk-data shape | 6.0x | 82.5 MiB/s | 72.3 MiB/s | **+14%** |
| Incompressible random bytes | 1.0x | 35.7 MiB/s | 32.9 MiB/s | **+9%** |

Criterion's 95% confidence intervals do not overlap on any of the three corpora
(text `[67.8, 70.2]` vs `[58.7, 59.6]`; csv `[78.9, 85.4]` vs `[70.7, 73.6]`; random
`[35.3, 36.2]` vs `[32.7, 33.1]`), so the margin is larger than the measurement noise.

This is the comparison to quote: same thread count, same buffers, realistic corpora.
The from-scratch decoder is 9–17% ahead of the C reference on one core — a reversal of
the 0.4.0 result, which trailed libbz2 by 2–4% on prose and CSV. The 0.5.0 hot-path
work is what closed and crossed that gap: write-only IBWT threading, interleaved pair
walks, fast-table Huffman over a 64-bit bit reservoir, and an arena MTF. This is one
microarchitecture (aarch64, Apple M-series) and one compiler pair; results on x86-64
have not been measured. Multi-core numbers are in
[Parallel decode](#parallel-decode) above.

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
