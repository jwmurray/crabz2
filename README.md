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

## License

Licensed under the [MIT license](LICENSE-MIT).
