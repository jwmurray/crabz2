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
| **0.2** (now) | Own from-scratch, dependency-free streaming decoder. Verified byte-for-byte against `bzip2`. |
| 0.1 | Thin wrapper over `bzip2-rs` (superseded; `0.1.0` remains dual `MIT OR Apache-2.0`). |
| 0.3 (roadmap) | `parallel` feature: rayon-backed parallel block decode with in-order reassembly. bzip2 blocks are independent, so they scale across cores. |

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
