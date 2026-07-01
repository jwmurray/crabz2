# crabz2

Pure-Rust **bzip2 decompression** — no C, no bundled `libbz2`, no C toolchain on your
build critical path. 🦀

`crabz2` layers on the pure-Rust [`bzip2-rs`](https://crates.io/crates/bzip2-rs) block
decoder and is licensed permissively (`MIT OR Apache-2.0`), so it drops cleanly into
closed-source projects with no BSD-notice obligation from `libbz2`.

## Status

| Version | What it ships |
|---|---|
| **0.1** (now) | Single-threaded streaming reader — `crabz2::reader(...)`, `crabz2::decompress(...)`. |
| 0.2 (roadmap) | `Crabz2Decoder`: rayon-backed **parallel** block decode with in-order reassembly, behind the `parallel` feature. bzip2 blocks are independent, so they scale across cores. |

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

## Why

Decode-only, pure Rust, permissively licensed. It exists to replace `libbz2`-backed
readers (and eventually the `pbzip2`/`lbzip2` shell-out) with an in-process, C-free,
license-clean path — with parallelism coming without changing the `Read`-based API.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
