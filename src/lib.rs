//! # crabz2
//!
//! Pure-Rust bzip2 **decompression** — no C, no bundled `libbz2`, no build-time C
//! toolchain. Layered on the pure-Rust [`bzip2-rs`] block decoder.
//!
//! ## Status
//!
//! `0.1` ships the single-threaded reader ([`Crabz2Reader`] / [`reader`]). A parallel,
//! rayon-backed block decoder (`Crabz2Decoder`) is on the roadmap for `0.2` behind the
//! `parallel` feature — bzip2 blocks are independent, so they decode across cores and
//! reassemble in original file order.
//!
//! ## Example
//!
//! ```no_run
//! use std::io::Read;
//! use std::fs::File;
//!
//! let mut out = String::new();
//! crabz2::reader(File::open("corpus.csv.bz2")?).read_to_string(&mut out)?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! [`bzip2-rs`]: https://crates.io/crates/bzip2-rs

use std::io::{self, Read};

/// A streaming, pure-Rust bzip2 decompressor implementing [`std::io::Read`].
///
/// Reads a `.bz2` stream from any [`Read`] source and yields decompressed bytes.
/// Handles multi-stream (concatenated) `.bz2` input, matching `bzip2 -dc`.
pub type Crabz2Reader<R> = bzip2_rs::DecoderReader<R>;

/// Wrap a compressed [`Read`] source in a single-threaded pure-Rust bzip2 decoder.
///
/// This is the license-clean, C-free replacement for a `libbz2`-backed `BzDecoder`.
pub fn reader<R: Read>(inner: R) -> Crabz2Reader<R> {
    bzip2_rs::DecoderReader::new(inner)
}

/// Decompress an entire in-memory `.bz2` buffer to a `Vec<u8>`.
///
/// Convenience for small inputs; prefer [`reader`] for streaming large corpora so the
/// plaintext is never fully buffered.
pub fn decompress(compressed: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    reader(compressed).read_to_end(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // "hello crabz2\n" compressed with `bzip2 -9`.
    const HELLO_BZ2: &[u8] = &[
        0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x71, 0x1c, 0x50, 0xc0, 0x00,
        0x00, 0x03, 0xd9, 0x80, 0x00, 0x10, 0x40, 0x00, 0x10, 0x00, 0x3a, 0x44, 0x90, 0x10, 0x20,
        0x00, 0x31, 0x03, 0x40, 0xd0, 0x29, 0x80, 0x1e, 0xa2, 0xe0, 0x4c, 0xed, 0x69, 0xe0, 0xe1,
        0x77, 0x24, 0x53, 0x85, 0x09, 0x07, 0x11, 0xc5, 0x0c, 0x00,
    ];

    #[test]
    fn round_trips_a_small_stream() {
        let out = decompress(HELLO_BZ2).expect("decode");
        assert_eq!(out, b"hello crabz2\n");
    }
}
