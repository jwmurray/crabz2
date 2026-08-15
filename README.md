# crabz2

Pure-Rust **bzip2 decompression** — no C, no bundled `libbz2`, no third-party decode
crate. 🦀

`crabz2` implements the entire bzip2 decode pipeline itself — bit reader → Huffman →
MTF/RLE2 → inverse Burrows–Wheeler transform → RLE1 → CRC-32/BZIP2 validation — with
**zero dependencies**. It's [MIT-licensed](LICENSE-MIT), so it drops cleanly into any
project, open or closed, with no attribution obligation beyond the MIT notice.

## Status

| Version | What it ships |
|---|---|
| **0.3.0** (now) | The 0.2 decoder, verified to compile untouched for `wasm32-unknown-unknown`, plus the project [ROADMAP](ROADMAP.md). |
| 0.3.x (roadmap) | Foundations: `no_std + alloc`, CI, `cargo-fuzz` targets, criterion benchmarks vs libbz2. |
| 0.4 (roadmap) | `parallel` feature: rayon-backed parallel block decode with in-order reassembly (bzip2 blocks are independent, so they scale across cores) — and a `crabz2-wasm` npm package with a streaming JS API. |
| 0.5 (roadmap) | Pure-Rust **encoder** — the piece the ecosystem lacks. See [ROADMAP](ROADMAP.md) for the design. |
| 0.2 | Own from-scratch, dependency-free streaming decoder. Verified byte-for-byte against `bzip2`. |
| 0.1 | Thin wrapper over `bzip2-rs` (superseded; `0.1.0` remains dual `MIT OR Apache-2.0`). |

## Usage

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

There's also a tiny example CLI:

```sh
cargo run --release --example crabz2 -- file.bz2 > file
```

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

## Correctness

`crabz2` streams one block at a time (peak memory ≈ one decompressed block), handles
concatenated multi-stream `.bz2` input, verifies both the per-block and combined-stream
CRCs, and errors loudly on truncated or corrupt input rather than emitting partial
garbage. It is tested byte-for-byte against system `bzip2` across block levels 1–9,
inputs from empty to tens of MB, RLE-heavy and high-entropy data, and multi-stream files.

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
