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
| **0.3.1** (now) | The 0.2 decoder, hardened against crafted RLE2 run lengths, with a `cargo-fuzz` target. See the [ROADMAP](ROADMAP.md). |
| unreleased (`main`) | The decoder behind a sans-io state machine: `no_std + alloc` core, and `wasm32-unknown-unknown` plus a bare-metal target checked in CI. Plus the pure-Rust **encoder** — `compress`, `Crabz2Writer`, levels 1–9 — verified against system `bzip2`. |
| 0.3.x (roadmap) | Remaining foundation: criterion benchmarks vs libbz2. |
| 0.4 (roadmap) | `parallel` feature: rayon-backed parallel block decode with in-order reassembly (bzip2 blocks are independent, so they scale across cores) — and a `crabz2-wasm` npm package with a streaming JS API. |
| 0.2 | Own from-scratch, dependency-free streaming decoder. Verified byte-for-byte against `bzip2`. |
| 0.1 | Thin wrapper over `bzip2-rs` (superseded; `0.1.0` remains dual `MIT OR Apache-2.0`). |

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

`alloc` is required (the BWT tables and output buffer are heap-allocated); on decode
every allocation is bounded by the block size declared in the stream header, and on
encode by the level. CI checks
`wasm32-unknown-unknown` and `thumbv7em-none-eabihf`, and the MSRV is 1.63.

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

A decoder for untrusted input has to say what it is fuzzed for. The
[`fuzz/`](fuzz/) directory is a [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz)
crate (nightly-only, outside the parent crate, so `crabz2` itself stays at zero
dependencies):

| Target | Input | Asserts |
|---|---|---|
| `fuzz_decompress` | arbitrary bytes | the three invariants below |

**Invariants.**

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

The committed seed corpus holds the crate's own test vectors, small files compressed
by system `bzip2` at levels 1 and 9 (prose, run-heavy, incompressible, and full-256
byte alphabets), concatenated multi-stream files, and minimized inputs from past
findings. CI runs a 60-second smoke pass of each target on nightly, on pull requests
touching `src/` and on a weekly schedule; longer campaigns are run out of band.

```sh
cargo +nightly fuzz run fuzz_decompress -- -max_total_time=300
```

**Findings.** Fuzzing this target found one real bug, fixed in 0.3.x: the RLE2
zero-run accumulator shifted by an attacker-controlled bit count, so a stream of
more than 64 consecutive RUNA/RUNB symbols panicked with a shift overflow instead
of being rejected. Runs are now bounded by the declared block size, as libbz2 does.

## License

Licensed under the [MIT license](LICENSE-MIT).
