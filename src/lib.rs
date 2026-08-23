//! # crabz2
//!
//! Pure-Rust bzip2 **compression and decompression** — no C, no bundled `libbz2`, and
//! no third-party bzip2 crate. `crabz2` implements both pipelines from scratch:
//!
//! * decode: bit reader → Huffman → MTF/RLE2 → inverse Burrows–Wheeler transform →
//!   RLE1 → CRC-32/BZIP2 validation;
//! * encode: RLE1 → Burrows–Wheeler transform (SA-IS suffix sorting) → MTF/RLE2 →
//!   length-limited canonical Huffman in 2–6 tables → bit packing.
//!
//! The core is sans-io and `no_std + alloc`: [`decompress_to_vec`], [`compress`] and
//! the [`Error`] enum are available with `default-features = false`. The `std` feature
//! (on by default) adds the `io` adapters — `reader`, `Crabz2Reader`, `decompress`,
//! `writer`, `Crabz2Writer` — and `From<Error> for std::io::Error`. [`BlockDecoder`]
//! is the decode machine underneath, for callers that are pushed bytes rather than
//! pulling them.
//!
//! Both directions stream: one bzip2 block is processed at a time and drained through
//! `io::Read` or `io::Write`, so peak memory is bounded to roughly one block.
//! Concatenated (multi-stream) `.bz2` input is handled on decode, matching `bzip2 -dc`.
//!
//! The optional, non-default `parallel` feature adds [`decompress_parallel`], which
//! decodes a multi-block buffer across a rayon thread pool for byte-identical output.
//! It is the only thing in the crate with a dependency; without it crabz2 still has
//! none.
//!
//! ## Example
//!
//! ```no_run
//! # #[cfg(feature = "std")] fn main() -> std::io::Result<()> {
//! use std::io::Read;
//! use std::fs::File;
//!
//! let mut out = String::new();
//! crabz2::reader(File::open("corpus.csv.bz2")?).read_to_string(&mut out)?;
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "std"))] fn main() {}
//! ```
//!
//! For small in-memory buffers — [`decompress`] under `std`, [`decompress_to_vec`]
//! everywhere, differing only in the error type:
//!
//! ```
//! # const HELLO: &[u8] = &[
//! #     0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x71, 0x1c, 0x50, 0xc0, 0x00,
//! #     0x00, 0x03, 0xd9, 0x80, 0x00, 0x10, 0x40, 0x00, 0x10, 0x00, 0x3a, 0x44, 0x90, 0x10, 0x20,
//! #     0x00, 0x31, 0x03, 0x40, 0xd0, 0x29, 0x80, 0x1e, 0xa2, 0xe0, 0x4c, 0xed, 0x69, 0xe0, 0xe1,
//! #     0x77, 0x24, 0x53, 0x85, 0x09, 0x07, 0x11, 0xc5, 0x0c, 0x00,
//! # ];
//! let data = crabz2::decompress_to_vec(HELLO)?;
//! assert_eq!(data, b"hello crabz2\n");
//! # Ok::<(), crabz2::Error>(())
//! ```
//!
//! Compressing. [`compress`] works everywhere; [`writer`] is the `std` streaming form:
//!
//! ```
//! let packed = crabz2::compress(b"hello crabz2\n", crabz2::Level::BEST);
//! assert_eq!(crabz2::decompress_to_vec(&packed)?, b"hello crabz2\n");
//! # Ok::<(), crabz2::Error>(())
//! ```

#![cfg_attr(not(feature = "std"), no_std)]

// A thread pool needs threads. Rather than let `parallel` compile to something that
// panics or silently runs serially in a browser, refuse the combination outright.
#[cfg(all(feature = "parallel", target_family = "wasm"))]
compile_error!(
    "the `parallel` feature requires OS threads and is not available on wasm targets; \
     build crabz2 without it (the serial decoder is the same decoder)"
);

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::ptr;

#[cfg(feature = "std")]
use std::io::{self, Read, Write};

mod encode;

pub use encode::{compress, Level};

#[cfg(feature = "parallel")]
mod parallel;
#[cfg(feature = "parallel")]
pub use parallel::decompress_parallel;

const BLOCK_MAGIC: u64 = 0x3141_5926_5359; // pi digits
const EOS_MAGIC: u64 = 0x1772_4538_5090; // sqrt(pi) digits
const MAX_CODE_LEN: usize = 23;
const GROUP_SIZE: usize = 50;

/// Temporary Phase-A instrumentation: per-phase wall time inside `decode_block`,
/// accumulated globally and printed via [`phasetime::report`]. Dev-only.
#[cfg(feature = "phasetime")]
pub mod phasetime {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    pub static ACC: [AtomicU64; 4] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];
    pub const NAMES: [&str; 4] = [
        "header/selectors/tables",
        "huffman+MTF+RLE2",
        "cftab+threading",
        "walk+RLE1+CRC",
    ];

    pub struct Scope {
        last: Instant,
    }
    impl Scope {
        pub fn new() -> Self {
            Scope {
                last: Instant::now(),
            }
        }
        pub fn lap(&mut self, phase: usize) {
            let now = Instant::now();
            let ns = now.duration_since(self.last).as_nanos() as u64;
            ACC[phase].fetch_add(ns, Ordering::Relaxed);
            self.last = now;
        }
    }

    pub fn take() -> [f64; 4] {
        let mut out = [0.0; 4];
        for (i, a) in ACC.iter().enumerate() {
            out[i] = a.swap(0, Ordering::Relaxed) as f64 / 1e9;
        }
        out
    }
}

/// Everything the decoder can reject. Structural, not positional: a corrupt stream
/// names the invariant it broke, never a byte offset the caller cannot act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// `BZh` stream header or the 48-bit block/end-of-stream magic did not match.
    InvalidMagic,
    /// Block-size level outside `1..=9`.
    InvalidLevel,
    /// Input ended mid-symbol. Under the streaming adapters this is only surfaced
    /// once the source is genuinely drained.
    Truncated,
    /// Block or combined-stream CRC-32/BZIP2 disagreed with the stored value.
    CrcMismatch,
    /// Legacy "randomized" block (bzip2 < 0.9.5, ~1998). Rejected, never guessed at.
    RandomizedBlock,
    /// Huffman table, group count, or code length violated the format's bounds.
    InvalidHuffman,
    /// Selector list, symbol map, MTF index, or origin pointer was inconsistent.
    InvalidBlock,
    /// Decoded run exceeded the block size declared in the stream header. Every
    /// allocation in the decoder is bounded by that declaration.
    BlockOverflow,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::InvalidMagic => "not a bzip2 stream (bad BZh or block magic)",
            Error::InvalidLevel => "invalid bzip2 block-size level",
            Error::Truncated => "unexpected end of bzip2 stream",
            Error::CrcMismatch => "bzip2 CRC mismatch",
            Error::RandomizedBlock => "legacy randomized bzip2 block not supported",
            Error::InvalidHuffman => "invalid bzip2 Huffman table",
            Error::InvalidBlock => "invalid bzip2 block structure",
            Error::BlockOverflow => "bzip2 block exceeds declared size",
        })
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

#[cfg(feature = "std")]
impl From<Error> for io::Error {
    /// `InvalidData` for every variant, truncation included — 0.2 callers match on
    /// that kind, so the sans-io split must not move it.
    fn from(e: Error) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

#[inline]
fn mask64(n: u32) -> u64 {
    if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

/// CRC-32/BZIP2 (poly 0x04C11DB7, MSB-first, init/xorout 0xFFFFFFFF).
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0usize;
    while n < 256 {
        let mut c = (n as u32) << 24;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ 0x04C1_1DB7
            } else {
                c << 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

#[inline]
fn crc_update(crc: u32, byte: u8) -> u32 {
    (crc << 8) ^ CRC_TABLE[(((crc >> 24) ^ byte as u32) & 0xff) as usize]
}

/// MSB-first bit cursor over a caller-owned slice. Holds no io type and no buffer:
/// the position is an absolute bit index, so a caller can retry a failed read after
/// appending bytes simply by rebuilding the cursor at the last committed position.
struct BitCursor<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitCursor<'a> {
    #[inline]
    fn new(data: &'a [u8], bit: usize) -> Self {
        BitCursor { data, bit }
    }

    /// Read `n` bits (1 <= n <= 32), MSB first.
    #[inline]
    fn read_bits(&mut self, n: u32) -> Result<u32, Error> {
        debug_assert!((1..=32).contains(&n));
        let end = self.bit + n as usize;
        if end > self.data.len() * 8 {
            return Err(Error::Truncated);
        }
        let i = self.bit >> 3;
        let off = (self.bit & 7) as u32;
        let v = if i + 8 <= self.data.len() {
            // Fast path: one big-endian word covers any 32-bit read at any offset.
            let mut w = [0u8; 8];
            w.copy_from_slice(&self.data[i..i + 8]);
            ((u64::from_be_bytes(w) << off) >> (64 - n)) as u32
        } else {
            // Tail: at most five bytes span the request, so a u64 still holds them.
            let stop = (end + 7) >> 3;
            let mut acc = 0u64;
            let mut k = i;
            while k < stop {
                acc = (acc << 8) | self.data[k] as u64;
                k += 1;
            }
            let total = ((stop - i) * 8) as u32;
            ((acc >> (total - off - n)) & mask64(n)) as u32
        };
        self.bit = end;
        Ok(v)
    }

    #[inline]
    fn read_bit(&mut self) -> Result<u32, Error> {
        let i = self.bit >> 3;
        if i >= self.data.len() {
            return Err(Error::Truncated);
        }
        let v = (self.data[i] >> (7 - (self.bit & 7))) as u32 & 1;
        self.bit += 1;
        Ok(v)
    }

    /// Read the 48-bit block/stream magic.
    #[inline]
    fn read_magic(&mut self) -> Result<u64, Error> {
        let hi = self.read_bits(24)? as u64;
        let lo = self.read_bits(24)? as u64;
        Ok((hi << 24) | lo)
    }

    #[inline]
    fn align_to_byte(&mut self) {
        self.bit = (self.bit + 7) & !7;
    }

    /// Bytes left from the current (byte-aligned) position.
    #[inline]
    fn bytes_left(&self) -> usize {
        self.data.len() - (self.bit >> 3)
    }
}

/// A canonical bzip2 Huffman decode table (limit/base/perm form).
/// Width of the one-lookup Huffman fast path. 2^10 u16 entries = 2 KB per table —
/// six tables stay comfortably in L1.
const FAST_BITS: u32 = 10;

/// MSB-first bit reader with a 64-bit reservoir, for the symbol-decode hot loop.
/// [`refill`](BitReservoir::refill) tops the buffer up to at least 57 bits, so a
/// whole Huffman code (≤ 20 bits) can be peeked and consumed without touching the
/// input again. Past the end of input the buffer pads with zero bits; the overrun
/// is caught by [`consume`](BitReservoir::consume), which checks the absolute bit
/// position against the input length.
struct BitReservoir<'a> {
    data: &'a [u8],
    /// Absolute bit index of the next unconsumed bit; `pos == next * 8 - have`.
    pos: usize,
    /// The `have` bits ending at byte boundary `next * 8`, right-aligned.
    buf: u64,
    have: u32,
    next: usize,
}

impl<'a> BitReservoir<'a> {
    fn new(data: &'a [u8], bit: usize) -> Self {
        let mut r = BitReservoir {
            data,
            pos: (bit >> 3) << 3,
            buf: 0,
            have: 0,
            next: bit >> 3,
        };
        r.refill();
        // Discard the sub-byte offset; `pos` cannot overrun here because `bit` is a
        // committed position.
        r.have -= (bit & 7) as u32;
        r.pos = bit;
        r
    }

    #[inline(always)]
    fn refill(&mut self) {
        while self.have <= 56 {
            let byte = if self.next < self.data.len() {
                let b = self.data[self.next];
                self.next += 1;
                b as u64
            } else {
                0
            };
            self.buf = (self.buf << 8) | byte;
            self.have += 8;
        }
    }

    /// The next `n` bits without consuming them. Requires `n <= have` (guaranteed
    /// for `n <= 57` after a refill); zero bits stand in past the end of input.
    #[inline(always)]
    fn peek(&self, n: u32) -> u32 {
        ((self.buf >> (self.have - n)) & mask64(n)) as u32
    }

    /// Consume `n` peeked bits, failing if they extend past the real input.
    #[inline(always)]
    fn consume(&mut self, n: u32) -> Result<(), Error> {
        self.pos += n as usize;
        self.have -= n;
        if self.pos > self.data.len() * 8 {
            return Err(Error::Truncated);
        }
        Ok(())
    }
}

struct HuffTable {
    min_len: u32,
    max_len: u32,
    limit: [i32; MAX_CODE_LEN + 2],
    base: [i32; MAX_CODE_LEN + 2],
    perm: Vec<usize>,
    /// One-lookup decode for codes up to [`FAST_BITS`] long: `(len << 12) | sym`,
    /// zero for longer codes (fall through to the canonical walk).
    fast: Vec<u16>,
}

impl HuffTable {
    fn build(len: &[u8]) -> Result<HuffTable, Error> {
        let alpha = len.len();
        let min_len = *len.iter().min().unwrap() as u32;
        let max_len = *len.iter().max().unwrap() as u32;
        if min_len < 1 || max_len as usize > MAX_CODE_LEN {
            return Err(Error::InvalidHuffman);
        }

        let mut perm = vec![0usize; alpha];
        let mut pp = 0;
        for l in min_len..=max_len {
            for (s, &sl) in len.iter().enumerate() {
                if sl as u32 == l {
                    perm[pp] = s;
                    pp += 1;
                }
            }
        }

        // Cumulative symbol counts by code length: `counts[l]` is the perm index of
        // the first symbol with length `l`. `base` starts as a copy and is then
        // repurposed as the canonical decode offset.
        let mut counts = [0i32; MAX_CODE_LEN + 2];
        for &sl in len {
            counts[sl as usize + 1] += 1;
        }
        for i in 1..counts.len() {
            counts[i] += counts[i - 1];
        }
        let mut base = counts;

        let mut limit = [0i32; MAX_CODE_LEN + 2];
        let mut vec = 0i32;
        for l in min_len..=max_len {
            vec += base[l as usize + 1] - base[l as usize];
            limit[l as usize] = vec - 1;
            vec <<= 1;
        }
        for l in (min_len + 1)..=max_len {
            base[l as usize] = ((limit[l as usize - 1] + 1) << 1) - base[l as usize];
        }

        // Fast table: for every code of length l <= FAST_BITS, stamp its symbol and
        // length into all 2^(FAST_BITS - l) slots sharing that l-bit prefix. Codes
        // of an over-subscribed (malformed) table that fall outside the valid range
        // are simply not stamped; they hit the canonical walk and fail there, so the
        // fast path can never accept what the slow path would reject.
        let mut fast = vec![0u16; 1 << FAST_BITS];
        for l in min_len..=max_len.min(FAST_BITS) {
            // Symbols with length l occupy perm indices `counts[l] .. counts[l + 1]`.
            let pp_start = counts[l as usize] as usize;
            let count = (counts[l as usize + 1] - counts[l as usize]) as usize;
            for k in 0..count {
                let idx = pp_start + k;
                let v = base[l as usize] + idx as i32;
                if v < 0 || v > limit[l as usize] || (v as u64) >= (1u64 << l) || idx >= alpha {
                    continue;
                }
                let sym = perm[idx];
                let lo = (v as usize) << (FAST_BITS - l);
                let entry = ((l as u16) << 12) | sym as u16;
                for slot in &mut fast[lo..lo + (1 << (FAST_BITS - l))] {
                    *slot = entry;
                }
            }
        }

        Ok(HuffTable {
            min_len,
            max_len,
            limit,
            base,
            perm,
            fast,
        })
    }

    #[inline(always)]
    fn decode(&self, r: &mut BitReservoir<'_>) -> Result<usize, Error> {
        r.refill();
        let e = self.fast[r.peek(FAST_BITS) as usize];
        if e != 0 {
            r.consume((e >> 12) as u32)?;
            return Ok((e & 0xfff) as usize);
        }
        // Canonical walk for codes longer than FAST_BITS (or malformed tables).
        // The reservoir holds >= 57 bits after the refill, so every peek below is
        // in range; a peek that leans on zero padding past the end of the input is
        // rejected by `consume`.
        let mut l = self.min_len;
        let mut v = r.peek(l) as i32;
        while l <= self.max_len {
            if v <= self.limit[l as usize] {
                let idx = (v - self.base[l as usize]) as usize;
                if idx >= self.perm.len() {
                    return Err(Error::InvalidHuffman);
                }
                r.consume(l)?;
                return Ok(self.perm[idx]);
            }
            l += 1;
            v = r.peek(l) as i32;
        }
        Err(Error::InvalidHuffman)
    }
}

enum Phase {
    StreamStart,
    Block,
}

/// One step of the prepare-only driver behind the pipelined whole-buffer decoder.
enum Prepared {
    Block { block_crc: u32, orig_ptr: usize },
    Eof,
}

/// One step of the sans-io machine: a block's plaintext was appended, or the input
/// ended cleanly at a stream boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// One block's plaintext was appended to the output buffer.
    Block,
    /// The input ran out at a clean boundary — after a stream's end-of-stream
    /// marker, or before any header. Nothing was appended. For a whole buffer this
    /// is the end of the data; for a caller still receiving input it only means the
    /// bytes in hand are fully consumed and a concatenated stream may still follow.
    Eof,
}

/// The whole decoder, free of io: it consumes `&[u8]` from a committed bit position
/// and appends plaintext to a caller-supplied `Vec`.
///
/// Restart doctrine: [`Error::Truncated`] leaves the cursor at the last committed
/// boundary (stream header, block, end-of-stream) and `out` at the length it had on
/// entry, so a caller that can obtain more input may append it and call again. Every
/// other error is terminal. Note that this is restart, not resume: a partially read
/// block is decoded again from its start once the rest of its bytes arrive, so a
/// caller feeding tiny increments should wait for a meaningful amount of new input
/// before retrying rather than retrying on every byte.
///
/// [`consumed`](BlockDecoder::consumed) and [`rebase`](BlockDecoder::rebase) let a
/// caller drop the bytes behind the committed position, which is what bounds memory
/// to roughly one block; the sub-byte offset is preserved across a rebase.
///
/// ```
/// # const HELLO: &[u8] = &[
/// #     0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x71, 0x1c, 0x50, 0xc0, 0x00,
/// #     0x00, 0x03, 0xd9, 0x80, 0x00, 0x10, 0x40, 0x00, 0x10, 0x00, 0x3a, 0x44, 0x90, 0x10, 0x20,
/// #     0x00, 0x31, 0x03, 0x40, 0xd0, 0x29, 0x80, 0x1e, 0xa2, 0xe0, 0x4c, 0xed, 0x69, 0xe0, 0xe1,
/// #     0x77, 0x24, 0x53, 0x85, 0x09, 0x07, 0x11, 0xc5, 0x0c, 0x00,
/// # ];
/// use crabz2::{BlockDecoder, Step};
///
/// let mut dec = BlockDecoder::new();
/// let mut out = Vec::new();
/// while dec.next_block(HELLO, &mut out)? == Step::Block {}
/// assert_eq!(out, b"hello crabz2\n");
/// # Ok::<(), crabz2::Error>(())
/// ```
pub struct BlockDecoder {
    bit: usize,
    phase: Phase,
    block_size: usize,
    combined_crc: u32,
    /// BWT scratch, reused across blocks: byte in the low 8 bits, source index above.
    tt: Vec<u32>,
    /// Post-MTF/RLE2 block bytes (the BWT last column), reused across blocks.
    bytes: Vec<u8>,
}

impl Default for BlockDecoder {
    fn default() -> Self {
        BlockDecoder::new()
    }
}

impl BlockDecoder {
    /// A decoder positioned at the start of a stream.
    pub fn new() -> Self {
        BlockDecoder {
            bit: 0,
            phase: Phase::StreamStart,
            block_size: 0,
            combined_crc: 0,
            tt: Vec::new(),
            bytes: Vec::new(),
        }
    }

    /// Whole bytes of the input slice the decoder has committed past. Everything
    /// before this offset may be dropped, provided the drop is announced with
    /// [`rebase`](BlockDecoder::rebase).
    pub fn consumed(&self) -> usize {
        self.bit >> 3
    }

    /// Announce that `n` bytes were removed from the front of the input slice, so
    /// the next call sees the same stream through a shorter slice.
    ///
    /// # Panics
    ///
    /// If `n` is past the committed position — those bytes are still needed.
    pub fn rebase(&mut self, n: usize) {
        assert!(n <= self.consumed(), "rebase past the committed position");
        self.bit -= n * 8;
    }

    /// Decode until one block's plaintext has been appended to `out`, or the input
    /// ends at a stream boundary.
    pub fn next_block(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<Step, Error> {
        let mark = out.len();
        match self.run(input, out) {
            Ok(step) => Ok(step),
            Err(e) => {
                out.truncate(mark);
                Err(e)
            }
        }
    }

    fn run(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<Step, Error> {
        let mut bits = BitCursor::new(input, self.bit);
        loop {
            match self.phase {
                Phase::StreamStart => {
                    bits.align_to_byte();
                    if bits.bytes_left() == 0 {
                        // Clean boundary: a concatenated stream may still arrive.
                        self.bit = bits.bit;
                        return Ok(Step::Eof);
                    }
                    if bits.bytes_left() < 4 {
                        return Err(Error::Truncated);
                    }
                    let b0 = bits.read_bits(8)? as u8;
                    let b1 = bits.read_bits(8)? as u8;
                    let b2 = bits.read_bits(8)? as u8;
                    let lvl = bits.read_bits(8)? as u8;
                    if b0 != b'B' || b1 != b'Z' || b2 != b'h' {
                        return Err(Error::InvalidMagic);
                    }
                    if !(b'1'..=b'9').contains(&lvl) {
                        return Err(Error::InvalidLevel);
                    }
                    self.block_size = (lvl - b'0') as usize * 100_000;
                    self.combined_crc = 0;
                    self.phase = Phase::Block;
                    self.bit = bits.bit;
                }
                Phase::Block => {
                    let magic = bits.read_magic()?;
                    if magic == BLOCK_MAGIC {
                        self.decode_block(&mut bits, out)?;
                        self.bit = bits.bit;
                        return Ok(Step::Block);
                    } else if magic == EOS_MAGIC {
                        let stored = bits.read_bits(32)?;
                        if stored != self.combined_crc {
                            return Err(Error::CrcMismatch);
                        }
                        // A concatenated stream may follow; loop back to try a header.
                        self.phase = Phase::StreamStart;
                        self.bit = bits.bit;
                    } else {
                        return Err(Error::InvalidMagic);
                    }
                }
            }
        }
    }

    fn decode_block(&mut self, bits: &mut BitCursor<'_>, out: &mut Vec<u8>) -> Result<(), Error> {
        let (block_crc, orig_ptr) = self.prepare_block(bits)?;

        let crc = WalkCursor::begin(&self.tt, orig_ptr, out).finish();
        if crc != block_crc {
            return Err(Error::CrcMismatch);
        }
        self.combined_crc = self.combined_crc.rotate_left(1);
        self.combined_crc ^= block_crc;

        Ok(())
    }

    /// Drive the stream machine to the next block's prepared state: headers and
    /// end-of-stream markers are consumed exactly as [`run`](BlockDecoder::run)
    /// consumes them, but the block itself is only prepared (bits read, `tt` built)
    /// — not walked, and its CRC not yet folded into the combined CRC.
    fn prepare_next(&mut self, input: &[u8]) -> Result<Prepared, Error> {
        let mut bits = BitCursor::new(input, self.bit);
        loop {
            match self.phase {
                Phase::StreamStart => {
                    bits.align_to_byte();
                    if bits.bytes_left() == 0 {
                        self.bit = bits.bit;
                        return Ok(Prepared::Eof);
                    }
                    if bits.bytes_left() < 4 {
                        return Err(Error::Truncated);
                    }
                    let b0 = bits.read_bits(8)? as u8;
                    let b1 = bits.read_bits(8)? as u8;
                    let b2 = bits.read_bits(8)? as u8;
                    let lvl = bits.read_bits(8)? as u8;
                    if b0 != b'B' || b1 != b'Z' || b2 != b'h' {
                        return Err(Error::InvalidMagic);
                    }
                    if !(b'1'..=b'9').contains(&lvl) {
                        return Err(Error::InvalidLevel);
                    }
                    self.block_size = (lvl - b'0') as usize * 100_000;
                    self.combined_crc = 0;
                    self.phase = Phase::Block;
                    self.bit = bits.bit;
                }
                Phase::Block => {
                    let magic = bits.read_magic()?;
                    if magic == BLOCK_MAGIC {
                        let (block_crc, orig_ptr) = self.prepare_block(&mut bits)?;
                        self.bit = bits.bit;
                        return Ok(Prepared::Block {
                            block_crc,
                            orig_ptr,
                        });
                    } else if magic == EOS_MAGIC {
                        let stored = bits.read_bits(32)?;
                        if stored != self.combined_crc {
                            return Err(Error::CrcMismatch);
                        }
                        self.phase = Phase::StreamStart;
                        self.bit = bits.bit;
                    } else {
                        return Err(Error::InvalidMagic);
                    }
                }
            }
        }
    }

    /// Everything in a block that reads bits: headers, Huffman decode, MTF/RLE2, and
    /// the IBWT threading pass. On success `self.tt` holds the successor vector and
    /// the returned pair is the stored block CRC and the walk's starting cell; no
    /// output has been produced yet.
    fn prepare_block(&mut self, bits: &mut BitCursor<'_>) -> Result<(u32, usize), Error> {
        #[cfg(feature = "phasetime")]
        let mut _pt = phasetime::Scope::new();
        let block_crc = bits.read_bits(32)?;
        if bits.read_bit()? != 0 {
            return Err(Error::RandomizedBlock);
        }
        let orig_ptr = bits.read_bits(24)? as usize;

        // Symbol map: which of the 256 byte values occur in this block.
        let mut used = [false; 256];
        let in_use16 = bits.read_bits(16)?;
        for i in 0..16 {
            if in_use16 & (1 << (15 - i)) != 0 {
                let bits16 = bits.read_bits(16)?;
                for j in 0..16 {
                    if bits16 & (1 << (15 - j)) != 0 {
                        used[i * 16 + j] = true;
                    }
                }
            }
        }
        let seq_to_unseq: Vec<u8> = (0..256).filter(|&b| used[b]).map(|b| b as u8).collect();
        let n_in_use = seq_to_unseq.len();
        if n_in_use == 0 {
            return Err(Error::InvalidBlock);
        }
        let alpha_size = n_in_use + 2;
        let eob = alpha_size - 1;

        // Selectors.
        let n_groups = bits.read_bits(3)? as usize;
        if !(2..=6).contains(&n_groups) {
            return Err(Error::InvalidHuffman);
        }
        let n_selectors = bits.read_bits(15)? as usize;
        if n_selectors == 0 {
            return Err(Error::InvalidBlock);
        }
        let mut group_pos: Vec<u8> = (0..n_groups as u8).collect();
        let mut selectors = Vec::with_capacity(n_selectors);
        for _ in 0..n_selectors {
            let mut j = 0usize;
            while bits.read_bit()? == 1 {
                j += 1;
                if j >= n_groups {
                    return Err(Error::InvalidBlock);
                }
            }
            // Undo the MTF on the selector list.
            let v = group_pos[j];
            group_pos.copy_within(0..j, 1);
            group_pos[0] = v;
            selectors.push(v as usize);
        }

        // Huffman code lengths per group, delta-coded.
        let mut tables = Vec::with_capacity(n_groups);
        for _ in 0..n_groups {
            let mut len = vec![0u8; alpha_size];
            let mut curr = bits.read_bits(5)? as i32;
            for slot in len.iter_mut() {
                loop {
                    if !(1..=20).contains(&curr) {
                        return Err(Error::InvalidHuffman);
                    }
                    if bits.read_bit()? == 0 {
                        break;
                    }
                    if bits.read_bit()? == 0 {
                        curr += 1;
                    } else {
                        curr -= 1;
                    }
                }
                *slot = curr as u8;
            }
            tables.push(HuffTable::build(&len)?);
        }

        #[cfg(feature = "phasetime")]
        _pt.lap(0); // header + selectors + tables
                    // MTF + RLE2 decode into the BWT byte buffer (the last column, one byte per
                    // cell — denser than decoding straight into `tt`, and run fills are memsets).
        self.bytes.clear();
        // Avoid reallocations for the common case by reserving the declared block size.
        self.bytes.reserve(self.block_size);
        let mut cftab = [0u32; 257];
        // MTF list as a sparse arena of 16-entry cache blocks (libbz2's scheme): a
        // front move shifts at most 15 bytes within one block plus one boundary
        // byte per block below it, instead of memmoving up to 255 bytes. Blocks
        // start at the top of the arena and creep down one slot per long move; the
        // arena is repacked upward when the front reaches slot zero.
        const MTFL: usize = 16;
        const MTFA_SIZE: usize = 4096;
        let mut mtfa = [0u8; MTFA_SIZE];
        let mut mtfbase = [0usize; 256 / MTFL];
        {
            let mut kk = MTFA_SIZE;
            for ii in (0..256 / MTFL).rev() {
                for jj in (0..MTFL).rev() {
                    kk -= 1;
                    let pos = ii * MTFL + jj;
                    mtfa[kk] = if pos < n_in_use { seq_to_unseq[pos] } else { 0 };
                }
                mtfbase[ii] = kk;
            }
        }
        let mut sel_idx = 0usize;
        let mut group_count = 0usize;
        let mut cur_table = 0usize;
        let mut run: u64 = 0;
        let mut run_bit: u32 = 0;

        // The symbol loop reads through a reservoir instead of the plain cursor;
        // the position is handed back to `bits` after the EOB symbol.
        let mut r = BitReservoir::new(bits.data, bits.bit);

        loop {
            if group_count == 0 {
                if sel_idx >= selectors.len() {
                    return Err(Error::InvalidBlock);
                }
                cur_table = selectors[sel_idx];
                sel_idx += 1;
                group_count = GROUP_SIZE;
            }
            group_count -= 1;
            let sym = tables[cur_table].decode(&mut r)?;

            if sym <= 1 {
                // RUNA (0) / RUNB (1): bijective base-2 zero-run length.
                //
                // Both the run and the bit position are attacker-controlled: a
                // stream can emit RUNA forever, so the accumulator has to be
                // bounded here rather than at the flush below. No legal run can
                // exceed the declared block size, and stopping there also keeps
                // the shift well inside `u64`.
                run += ((sym as u64) + 1) << run_bit;
                run_bit += 1;
                if run_bit >= 32 || run > self.block_size as u64 {
                    return Err(Error::BlockOverflow);
                }
                continue;
            }

            if run > 0 {
                let b = mtfa[mtfbase[0]];
                if self.bytes.len() + run as usize > self.block_size {
                    return Err(Error::BlockOverflow);
                }
                cftab[b as usize + 1] += run as u32;
                // Splat the run in one resize (a memset) instead of `run` pushes.
                self.bytes.resize(self.bytes.len() + run as usize, b);
                run = 0;
                run_bit = 0;
            }

            if sym == eob {
                // Hand the reservoir's position back to the caller's cursor.
                bits.bit = r.pos;
                break;
            }

            // MTF index (sym - 1): move that byte value to the front.
            let nn = sym - 1;
            if nn >= n_in_use {
                return Err(Error::InvalidBlock);
            }
            let b;
            if nn < MTFL {
                // Within the front block: shift at most 15 bytes.
                let pp = mtfbase[0];
                b = mtfa[pp + nn];
                mtfa.copy_within(pp..pp + nn, pp + 1);
                mtfa[pp] = b;
            } else {
                // Shift within the symbol's own block, then cascade one boundary
                // byte down through each block in front of it.
                let mut lno = nn / MTFL;
                let off = nn % MTFL;
                let mut pp = mtfbase[lno] + off;
                b = mtfa[pp];
                while pp > mtfbase[lno] {
                    mtfa[pp] = mtfa[pp - 1];
                    pp -= 1;
                }
                mtfbase[lno] += 1;
                while lno > 0 {
                    mtfbase[lno] -= 1;
                    mtfa[mtfbase[lno]] = mtfa[mtfbase[lno - 1] + MTFL - 1];
                    lno -= 1;
                }
                mtfbase[0] -= 1;
                mtfa[mtfbase[0]] = b;
                if mtfbase[0] == 0 {
                    // The blocks have crept to the bottom; repack them at the top.
                    let mut kk = MTFA_SIZE;
                    for ii in (0..256 / MTFL).rev() {
                        for jj in (0..MTFL).rev() {
                            kk -= 1;
                            mtfa[kk] = mtfa[mtfbase[ii] + jj];
                        }
                        mtfbase[ii] = kk;
                    }
                }
            }

            if self.bytes.len() + 1 > self.block_size {
                return Err(Error::BlockOverflow);
            }
            self.bytes.push(b);
            cftab[b as usize + 1] += 1;
        }

        #[cfg(feature = "phasetime")]
        _pt.lap(1); // huffman + MTF + RLE2
        let nblock = self.bytes.len();
        if nblock == 0 || orig_ptr >= nblock {
            return Err(Error::InvalidBlock);
        }

        // Cumulative counts -> starting index of each byte value.
        for i in 1..=256 {
            cftab[i] += cftab[i - 1];
        }

        // Inverse Burrows–Wheeler transform (fast form): build the successor vector,
        // then walk from `orig_ptr`. Cell `j` is written exactly once with
        // `(i << 8) | L[i]` — the source index *paired with its own byte* — rather
        // than libbz2's `tt[cftab[b]] |= i << 8` read-modify-write. The walk output
        // is identical (each step reads one cell and gets both the next position and
        // that position's byte), but this pass only *writes* the scattered lines and
        // reads the dense byte buffer sequentially.
        self.tt.clear();
        self.tt.reserve(nblock);
        // Safety: every cell in `0..nblock` is written exactly once below — `cftab`
        // partitions `0..nblock` into per-byte ranges and each write consumes one
        // slot — and nothing reads `tt` until after this loop.
        #[allow(clippy::uninit_vec)]
        unsafe {
            self.tt.set_len(nblock);
            let bp = self.bytes.as_ptr();
            let tp = self.tt.as_mut_ptr();
            for i in 0..nblock {
                let b = *bp.add(i) as usize;
                let idx = *cftab.get_unchecked(b) as usize;
                *cftab.get_unchecked_mut(b) = (idx + 1) as u32;
                *tp.add(idx) = ((i as u32) << 8) | b as u32;
            }
        }

        #[cfg(feature = "phasetime")]
        _pt.lap(2); // cftab + index threading
        Ok((block_crc, orig_ptr))
    }
}

/// The IBWT permutation walk over a prepared successor vector, applying RLE1 and the
/// CRC to append one block's plaintext to `out`.
///
/// This is a struct rather than a loop so two blocks' walks can be interleaved: each
/// step is a serial dependent load (`t = tt[t >> 8]`), so a single walk is bound by
/// memory latency, but two walks over different blocks are independent chains and
/// overlap almost perfectly. The CRC stays fused under the walk where it executes
/// for free.
///
/// [`step`](WalkCursor::step) must be called at most [`WalkCursor::rem`] times; state
/// is committed back to the `Vec` by [`finish`](WalkCursor::finish).
struct WalkCursor<'a> {
    tt: *const u32,
    /// Cells hold `(next_pos << 8) | byte_of_next_pos`, so one load per step yields
    /// both the byte to emit and where to go next.
    t_pos: u32,
    prev: i32,
    count: u32,
    crc: u32,
    /// Steps left; one step per `tt` cell.
    rem: usize,
    out: &'a mut Vec<u8>,
    /// Write cursor into `out`'s reserved capacity: one capacity check per step
    /// (against the 255-byte RLE1 worst case) instead of a `push` per byte, and RLE1
    /// runs become a single `write_bytes` (memset).
    len: usize,
    dst: *mut u8,
    room: usize,
}

impl<'a> WalkCursor<'a> {
    fn begin(tt: &[u32], orig_ptr: usize, out: &'a mut Vec<u8>) -> WalkCursor<'a> {
        let len = out.len();
        WalkCursor {
            tt: tt.as_ptr(),
            t_pos: tt[orig_ptr],
            prev: -1,
            count: 0,
            crc: 0xFFFF_FFFF,
            rem: tt.len(),
            dst: out.as_mut_ptr().wrapping_add(len),
            room: out.capacity() - len,
            len,
            out,
        }
    }

    /// # Safety
    /// At most `rem` total calls, with `rem` decremented by the caller per call.
    #[inline(always)]
    unsafe fn step(&mut self) {
        if self.room < 256 {
            self.out.set_len(self.len);
            self.out.reserve(self.rem.max(4096));
            self.dst = self.out.as_mut_ptr().add(self.len);
            self.room = self.out.capacity() - self.len;
        }
        let b = (self.t_pos & 0xff) as u8;
        self.t_pos = *self.tt.add((self.t_pos >> 8) as usize);

        if self.count == 4 {
            // `b` is the count of extra repeats beyond the four literals.
            ptr::write_bytes(self.dst, self.prev as u8, b as usize);
            for _ in 0..b {
                self.crc = crc_update(self.crc, self.prev as u8);
            }
            self.dst = self.dst.add(b as usize);
            self.len += b as usize;
            self.room -= b as usize;
            self.count = 0;
            self.prev = -1;
        } else {
            *self.dst = b;
            self.dst = self.dst.add(1);
            self.len += 1;
            self.room -= 1;
            self.crc = crc_update(self.crc, b);
            if b as i32 == self.prev {
                self.count += 1;
            } else {
                self.prev = b as i32;
                self.count = 1;
            }
        }
    }

    /// Run the remaining steps, commit the output length, and return the block CRC.
    fn finish(mut self) -> u32 {
        #[cfg(feature = "phasetime")]
        let mut _pt = phasetime::Scope::new();
        unsafe {
            while self.rem > 0 {
                self.step();
                self.rem -= 1;
            }
            self.out.set_len(self.len);
        }
        #[cfg(feature = "phasetime")]
        _pt.lap(3); // permutation walk + RLE1 + CRC
        !self.crc
    }
}

/// Walk two prepared blocks with their steps interleaved, so the two serial
/// dependent-load chains overlap in the memory system. Returns both block CRCs.
fn walk_pair(mut a: WalkCursor<'_>, mut b: WalkCursor<'_>) -> (u32, u32) {
    unsafe {
        while a.rem > 0 && b.rem > 0 {
            a.step();
            b.step();
            a.rem -= 1;
            b.rem -= 1;
        }
    }
    (a.finish(), b.finish())
}

/// Decompress an entire in-memory `.bz2` buffer. Available without `std`.
///
/// Handles concatenated (multi-stream) input and verifies both the per-block and the
/// combined-stream CRC. Peak memory is the plaintext plus two blocks of scratch:
/// consecutive blocks are decoded as a software-pipelined pair so their IBWT
/// permutation walks — each a serial dependent-load chain, and the majority of
/// decode time — overlap in the memory system.
pub fn decompress_to_vec(compressed: &[u8]) -> Result<Vec<u8>, Error> {
    decompress_to_vec_with(
        &mut BlockDecoder::new(),
        &mut BlockDecoder::new(),
        &mut Vec::new(),
        compressed,
    )
}

/// [`decompress_to_vec`] over caller-owned scratch, so a caller that decodes many
/// buffers (or the parallel front end handling a small stream) keeps the
/// multi-megabyte block buffers allocated and their pages faulted in.
pub(crate) fn decompress_to_vec_with(
    a: &mut BlockDecoder,
    b: &mut BlockDecoder,
    tmp: &mut Vec<u8>,
    compressed: &[u8],
) -> Result<Vec<u8>, Error> {
    a.bit = 0;
    a.phase = Phase::StreamStart;
    a.combined_crc = 0;
    let mut out = Vec::new();
    loop {
        match a.prepare_next(compressed)? {
            Prepared::Eof => return Ok(out),
            Prepared::Block {
                block_crc,
                orig_ptr,
            } => {
                // Peek: if another block of the same stream follows immediately,
                // prepare it too and walk the two interleaved. `b`'s output goes to
                // a reused side buffer because its final position in `out` is not
                // known until `a`'s RLE1 expansion finishes.
                b.bit = a.bit;
                b.phase = Phase::Block;
                b.block_size = a.block_size;
                let follow = {
                    let mut bits = BitCursor::new(compressed, b.bit);
                    if bits.read_magic() == Ok(BLOCK_MAGIC) {
                        b.prepare_block(&mut bits)
                            .ok()
                            .map(|(crc, ptr)| (crc, ptr, bits.bit))
                    } else {
                        None
                    }
                };
                match follow {
                    Some((crc_b, ptr_b, end_b)) => {
                        tmp.clear();
                        let (ca, cb) = walk_pair(
                            WalkCursor::begin(&a.tt, orig_ptr, &mut out),
                            WalkCursor::begin(&b.tt, ptr_b, &mut tmp),
                        );
                        if ca != block_crc {
                            return Err(Error::CrcMismatch);
                        }
                        a.combined_crc = a.combined_crc.rotate_left(1) ^ block_crc;
                        if cb == crc_b {
                            out.extend_from_slice(&tmp);
                            a.combined_crc = a.combined_crc.rotate_left(1) ^ crc_b;
                            a.bit = end_b;
                        }
                        // On a mismatch `b`'s output is simply dropped: `a` re-decodes
                        // that block on the next iteration and reports the exact error
                        // the serial machine would.
                    }
                    None => {
                        let crc = WalkCursor::begin(&a.tt, orig_ptr, &mut out).finish();
                        if crc != block_crc {
                            return Err(Error::CrcMismatch);
                        }
                        a.combined_crc = a.combined_crc.rotate_left(1) ^ block_crc;
                    }
                }
            }
        }
    }
}

/// Compressed bytes pulled per source read; also the floor for the geometric regrow
/// that bounds how often a block is re-decoded while waiting for its last bytes.
#[cfg(feature = "std")]
const CHUNK: usize = 1 << 16;

/// A streaming, from-scratch, pure-Rust bzip2 decompressor implementing [`std::io::Read`].
#[cfg(feature = "std")]
pub struct Crabz2Reader<R> {
    inner: R,
    dec: BlockDecoder,
    /// Compressed bytes not yet consumed by a committed decode step.
    inbuf: Vec<u8>,
    out: Vec<u8>,
    pos: usize,
    src_eof: bool,
    done: bool,
}

#[cfg(feature = "std")]
impl<R: Read> Crabz2Reader<R> {
    /// Create a decoder over a compressed `.bz2` byte source.
    pub fn new(inner: R) -> Self {
        Crabz2Reader {
            inner,
            dec: BlockDecoder::new(),
            inbuf: Vec::new(),
            out: Vec::new(),
            pos: 0,
            src_eof: false,
            done: false,
        }
    }

    /// Pull more compressed bytes. `Ok(false)` once the source is drained.
    ///
    /// Sizing: once the stream header is read, a whole block's worth is requested, so
    /// the common case is one fill per block and no re-decode. The unconsumed
    /// remainder is the floor, which keeps a pathological source logarithmic rather
    /// than quadratic. Peak input buffering stays at roughly one compressed block.
    fn fill(&mut self) -> io::Result<bool> {
        if self.src_eof {
            return Ok(false);
        }
        let unconsumed = self.inbuf.len() - self.dec.consumed();
        let want = CHUNK.max(unconsumed).max(self.dec.block_size);
        let old = self.inbuf.len();
        self.inbuf.resize(old + want, 0);
        let mut got = 0usize;
        while got < want {
            match self.inner.read(&mut self.inbuf[old + got..]) {
                Ok(0) => {
                    self.src_eof = true;
                    break;
                }
                Ok(n) => got += n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.inbuf.truncate(old + got);
                    return Err(e);
                }
            }
        }
        self.inbuf.truncate(old + got);
        Ok(got > 0)
    }

    /// Drop the bytes the decoder has committed past; the bit cursor keeps its
    /// sub-byte offset.
    fn compact(&mut self) {
        let consumed = self.dec.consumed();
        if consumed > 0 {
            self.inbuf.drain(..consumed);
            self.dec.rebase(consumed);
        }
    }

    /// Advance until `self.out` holds a freshly decoded block. `Ok(false)` at EOF.
    fn refill(&mut self) -> io::Result<bool> {
        loop {
            if self.done {
                return Ok(false);
            }
            self.out.clear();
            self.pos = 0;
            match self.dec.next_block(&self.inbuf, &mut self.out) {
                Ok(Step::Block) => {
                    self.compact();
                    return Ok(true);
                }
                Ok(Step::Eof) => {
                    if self.fill()? {
                        continue;
                    }
                    self.done = true;
                    return Ok(false);
                }
                // Truncation is only real once the source itself is drained.
                Err(Error::Truncated) => {
                    if self.fill()? {
                        continue;
                    }
                    return Err(Error::Truncated.into());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

#[cfg(feature = "std")]
impl<R: Read> Read for Crabz2Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.out.len() {
                let n = buf.len().min(self.out.len() - self.pos);
                buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if !self.refill()? {
                return Ok(0);
            }
        }
    }
}

/// Wrap a compressed `.bz2` [`Read`] source in a streaming pure-Rust bzip2 decoder.
#[cfg(feature = "std")]
pub fn reader<R: Read>(inner: R) -> Crabz2Reader<R> {
    Crabz2Reader::new(inner)
}

/// Decompress an entire in-memory `.bz2` buffer to a `Vec<u8>`.
///
/// Convenience for small inputs; prefer [`reader`] for streaming large corpora so the
/// plaintext is never fully buffered.
#[cfg(feature = "std")]
pub fn decompress(compressed: &[u8]) -> io::Result<Vec<u8>> {
    Ok(decompress_to_vec(compressed)?)
}

/// A streaming, from-scratch, pure-Rust bzip2 compressor implementing [`std::io::Write`].
///
/// Plaintext written here is compressed a block at a time and pushed straight into the
/// wrapped writer, so peak memory stays around one block regardless of input size.
///
/// **You must call [`finish`](Crabz2Writer::finish)**: the end-of-stream marker and the
/// combined-stream CRC are only written there. Dropping the writer without finishing
/// leaves a truncated `.bz2`.
#[cfg(feature = "std")]
pub struct Crabz2Writer<W: Write> {
    inner: Option<W>,
    enc: Option<encode::Encoder>,
}

#[cfg(feature = "std")]
impl<W: Write> Crabz2Writer<W> {
    /// Create a compressor writing a `.bz2` stream into `inner`.
    pub fn new(inner: W, level: Level) -> Self {
        Crabz2Writer {
            inner: Some(inner),
            enc: Some(encode::Encoder::new(level)),
        }
    }

    /// Push whatever compressed bytes are ready into the wrapped writer.
    fn drain(&mut self) -> io::Result<()> {
        let ready = self.enc.as_mut().expect("writer already finished").drain();
        if !ready.is_empty() {
            self.inner
                .as_mut()
                .expect("writer already finished")
                .write_all(&ready)?;
        }
        Ok(())
    }

    /// Finish the stream — writes the end-of-stream marker and stream CRC — and return
    /// the wrapped writer.
    pub fn finish(mut self) -> io::Result<W> {
        let tail = self.enc.take().expect("writer already finished").finish();
        let mut inner = self.inner.take().expect("writer already finished");
        inner.write_all(&tail)?;
        inner.flush()?;
        Ok(inner)
    }
}

#[cfg(feature = "std")]
impl<W: Write> Write for Crabz2Writer<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.enc
            .as_mut()
            .expect("writer already finished")
            .push(buf);
        self.drain()?;
        Ok(buf.len())
    }

    /// Flushes only what is already compressed. bzip2 has no mid-block sync point, so
    /// buffered plaintext stays buffered until its block fills.
    fn flush(&mut self) -> io::Result<()> {
        self.drain()?;
        self.inner
            .as_mut()
            .expect("writer already finished")
            .flush()
    }
}

/// Wrap a [`Write`] sink in a streaming pure-Rust bzip2 compressor.
///
/// Call [`Crabz2Writer::finish`] when done; dropping without it truncates the stream.
///
/// ```
/// use std::io::Write;
///
/// let mut w = crabz2::writer(Vec::new(), crabz2::Level::BEST);
/// w.write_all(b"hello crabz2\n")?;
/// let compressed = w.finish()?;
///
/// assert_eq!(crabz2::decompress(&compressed)?, b"hello crabz2\n");
/// # Ok::<(), std::io::Error>(())
/// ```
#[cfg(feature = "std")]
pub fn writer<W: Write>(inner: W, level: Level) -> Crabz2Writer<W> {
    Crabz2Writer::new(inner, level)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    // "hello crabz2\n" compressed with `bzip2 -9`.
    const HELLO_BZ2: &[u8] = &[
        0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x71, 0x1c, 0x50, 0xc0, 0x00,
        0x00, 0x03, 0xd9, 0x80, 0x00, 0x10, 0x40, 0x00, 0x10, 0x00, 0x3a, 0x44, 0x90, 0x10, 0x20,
        0x00, 0x31, 0x03, 0x40, 0xd0, 0x29, 0x80, 0x1e, 0xa2, 0xe0, 0x4c, 0xed, 0x69, 0xe0, 0xe1,
        0x77, 0x24, 0x53, 0x85, 0x09, 0x07, 0x11, 0xc5, 0x0c, 0x00,
    ];

    // Empty input: `printf '' | bzip2 -9` (header + end-of-stream, no blocks).
    const EMPTY_BZ2: &[u8] = &[
        0x42, 0x5a, 0x68, 0x39, 0x17, 0x72, 0x45, 0x38, 0x50, 0x90, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    #[cfg(feature = "std")]
    fn decodes_small_stream() {
        assert_eq!(decompress(HELLO_BZ2).unwrap(), b"hello crabz2\n");
    }

    #[test]
    #[cfg(feature = "std")]
    fn decodes_empty_stream() {
        assert_eq!(decompress(EMPTY_BZ2).unwrap(), b"");
    }

    #[test]
    #[cfg(feature = "std")]
    fn detects_corruption() {
        let mut bad = HELLO_BZ2.to_vec();
        let n = bad.len();
        bad[n - 6] ^= 0x01; // flip a bit in the payload
        assert!(decompress(&bad).is_err());
    }

    /// MSB-first bit writer, used to synthesize hostile streams for the tests below.
    struct BitWriter {
        out: Vec<u8>,
        acc: u32,
        nbits: u32,
    }

    impl BitWriter {
        fn new() -> Self {
            BitWriter {
                out: Vec::new(),
                acc: 0,
                nbits: 0,
            }
        }

        fn put(&mut self, val: u32, n: u32) {
            for i in (0..n).rev() {
                self.acc = (self.acc << 1) | ((val >> i) & 1);
                self.nbits += 1;
                if self.nbits == 8 {
                    self.out.push(self.acc as u8);
                    self.acc = 0;
                    self.nbits = 0;
                }
            }
        }

        /// Copy the bit range `[start, end)` out of another stream verbatim. Bzip2
        /// blocks are not byte-aligned, so re-homing one is a bit-level operation.
        fn put_range(&mut self, src: &[u8], start: usize, end: usize) {
            let mut c = BitCursor::new(src, start);
            for _ in start..end {
                let b = c.read_bit().expect("bit range inside the source stream");
                self.put(b, 1);
            }
        }

        fn finish(mut self) -> Vec<u8> {
            if self.nbits > 0 {
                let pad = 8 - self.nbits;
                self.acc <<= pad;
                self.out.push(self.acc as u8);
            }
            self.out
        }
    }

    fn bz_crc(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc = crc_update(crc, b);
        }
        !crc
    }

    /// Bit ranges of every block in a *single-stream* input, with each block's CRC:
    /// `(first bit of the block magic, first bit past the block, block CRC)`.
    fn blocks_of(stream: &[u8]) -> Vec<(usize, usize, u32)> {
        let mut dec = BlockDecoder::new();
        let mut out = Vec::new();
        let mut found = Vec::new();
        let mut start = 32; // past the four-byte `BZh<level>` header
        let mut prev = 0u32;
        loop {
            match dec.next_block(stream, &mut out).expect("valid stream") {
                Step::Block => {
                    // combined = prev.rotate_left(1) ^ block, inverted.
                    found.push((start, dec.bit, dec.combined_crc ^ prev.rotate_left(1)));
                    prev = dec.combined_crc;
                    start = dec.bit;
                }
                Step::Eof => return found,
            }
        }
    }

    /// Build one multi-block stream out of the blocks of several single-stream
    /// inputs. Multi-block fixtures otherwise need an encoder, and the crate does
    /// not have one; splicing real blocks at their real bit offsets exercises the
    /// same thing the compressor would produce, including the unaligned handoff
    /// from one block to the next.
    fn splice(streams: &[&[u8]]) -> Vec<u8> {
        let level = streams.iter().map(|s| s[3]).max().expect("no streams");
        let mut w = BitWriter::new();
        w.put(u32::from(b'B'), 8);
        w.put(u32::from(b'Z'), 8);
        w.put(u32::from(b'h'), 8);
        w.put(u32::from(level), 8);

        let mut combined = 0u32;
        for s in streams {
            for (start, end, crc) in blocks_of(s) {
                w.put_range(s, start, end);
                combined = combined.rotate_left(1) ^ crc;
            }
        }

        w.put(0x177245, 24); // end-of-stream magic, high half
        w.put(0x385090, 24); // end-of-stream magic, low half
        w.put(combined, 32);
        w.finish()
    }

    /// The block magic, written out as `pad` leading zero bits followed by the 48
    /// bits of the pattern, then closed so the whole thing parses as a sequence of
    /// unary selector codes. Returns the bits and how many selectors they encode.
    fn selector_bits_carrying_magic(pad: usize) -> (Vec<u8>, usize) {
        let mut bits = vec![0u8; pad];
        for i in 0..48 {
            bits.push(((BLOCK_MAGIC >> (47 - i)) & 1) as u8);
        }
        let mut count = 0usize;
        let mut i = 0usize;
        while i < bits.len() {
            let mut run = 0;
            while i < bits.len() && bits[i] == 1 {
                run += 1;
                i += 1;
            }
            // A selector is `run` ones then a zero, and must name a group below
            // n_groups; the magic's longest run of ones is two, so this holds.
            assert!(run < 6, "selector run of {run} ones is not encodable");
            if i == bits.len() {
                bits.push(0);
            }
            i += 1;
            count += 1;
        }
        (bits, count)
    }

    /// A valid one-block stream that contains the 48-bit block magic *inside* the
    /// block's own data — in the selector list, which is the one section whose bits
    /// a stream can choose freely. `pad` shifts the pattern's bit alignment.
    ///
    /// The block itself decodes to a single `A`: one distinct byte means the whole
    /// payload is the RUNA that stands for a zero-run of one, then end-of-block.
    fn magic_inside_block_data(pad: usize) -> Vec<u8> {
        let (sel, n_selectors) = selector_bits_carrying_magic(pad);
        let mut w = BitWriter::new();
        w.put(u32::from(b'B'), 8);
        w.put(u32::from(b'Z'), 8);
        w.put(u32::from(b'h'), 8);
        w.put(u32::from(b'1'), 8);

        w.put(0x314159, 24);
        w.put(0x265359, 24);
        w.put(bz_crc(b"A"), 32);
        w.put(0, 1); // not randomized
        w.put(0, 24); // orig_ptr

        // Symbol map: 'A' is 0x41, so group 4, member 1.
        w.put(1 << (15 - 4), 16);
        w.put(1 << (15 - 1), 16);

        w.put(6, 3); // n_groups: the widest selector alphabet
        w.put(n_selectors as u32, 15);
        for b in sel {
            w.put(u32::from(b), 1);
        }

        // All six groups: every symbol two bits, so RUNA=00, RUNB=01, EOB=10.
        for _ in 0..6 {
            w.put(2, 5);
            for _ in 0..3 {
                w.put(0, 1);
            }
        }

        w.put(0b00, 2); // RUNA: a zero-run of one, i.e. the single 'A'
        w.put(0b10, 2); // EOB

        w.put(0x177245, 24);
        w.put(0x385090, 24);
        w.put(bz_crc(b"A"), 32); // one block, so combined == block CRC
        w.finish()
    }

    #[test]
    fn spliced_multi_block_stream_decodes() {
        let s = splice(&[HELLO_BZ2, HELLO_BZ2, HELLO_BZ2]);
        assert_eq!(
            decompress_to_vec(&s).unwrap(),
            b"hello crabz2\nhello crabz2\nhello crabz2\n"
        );
    }

    #[test]
    fn block_carrying_the_magic_in_its_data_decodes() {
        for pad in 0..8 {
            let s = magic_inside_block_data(pad);
            assert_eq!(decompress_to_vec(&s).unwrap(), b"A", "pad {pad}");
        }
    }

    /// Build a single-block stream whose RLE2 section is nothing but `n_runs`
    /// consecutive RUNA symbols — a zero-run declaration with no terminating
    /// literal. Every symbol in the block alphabet is given a 2-bit code, so
    /// RUNA is `00`, RUNB is `01`, EOB is `10`.
    fn runa_flood(n_runs: usize) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(u32::from(b'B'), 8);
        w.put(u32::from(b'Z'), 8);
        w.put(u32::from(b'h'), 8);
        w.put(u32::from(b'9'), 8);

        w.put(0x314159, 24); // block magic, high half
        w.put(0x265359, 24); // block magic, low half
        w.put(0, 32); // block CRC (never reached)
        w.put(0, 1); // not randomized
        w.put(0, 24); // orig_ptr

        // Symbol map: byte 0x00 only, so n_in_use = 1 and alpha_size = 3.
        w.put(0x8000, 16); // group 0 present
        w.put(0x8000, 16); // within group 0, byte 0

        w.put(2, 3); // n_groups
        w.put(2, 15); // n_selectors (2 * 50 symbols of headroom)
        w.put(0, 1); // selector 0
        w.put(0, 1); // selector 0

        // Both groups: every one of the 3 symbols gets code length 2.
        for _ in 0..2 {
            w.put(2, 5); // starting length
            for _ in 0..3 {
                w.put(0, 1); // no delta, take the current length
            }
        }

        for _ in 0..n_runs {
            w.put(0b00, 2); // RUNA
        }
        w.finish()
    }

    /// A stream that declares a zero-run longer than any legal block must be
    /// rejected as data corruption. Before this was bounded, the bijective
    /// base-2 run accumulator shifted by an attacker-controlled bit count and
    /// panicked (shift overflow) once the run passed 64 RUNA/RUNB symbols.
    #[test]
    fn rejects_absurd_run_length() {
        for n_runs in [40, 64, 65, 128, 400] {
            let stream = runa_flood(n_runs);
            assert_eq!(
                decompress_to_vec(&stream),
                Err(Error::BlockOverflow),
                "a {n_runs}-symbol zero-run should be rejected, not decoded"
            );
            #[cfg(feature = "std")]
            assert!(decompress(&stream).is_err());
        }
    }

    /// The declared block size bounds the decoded block: no crafted header or
    /// run length may make the decoder buffer more than the level allows.
    #[test]
    fn run_length_cannot_exceed_declared_block_size() {
        // Level 1 declares a 100 KB block; a run claiming far more than that
        // must fail before anything is buffered.
        let mut stream = runa_flood(64);
        stream[3] = b'1';
        assert_eq!(decompress_to_vec(&stream), Err(Error::BlockOverflow));
    }

    // The sans-io path, exercised exactly as a no_std consumer sees it.
    #[test]
    fn no_std_path_decodes_vectors() {
        assert_eq!(decompress_to_vec(HELLO_BZ2).unwrap(), b"hello crabz2\n");
        assert_eq!(decompress_to_vec(EMPTY_BZ2).unwrap(), b"");
    }

    // Concatenated streams, matching `bzip2 -dc`, including empty ones between blocks.
    #[test]
    fn decodes_multi_stream() {
        let mut cat = Vec::new();
        cat.extend_from_slice(HELLO_BZ2);
        cat.extend_from_slice(EMPTY_BZ2);
        cat.extend_from_slice(HELLO_BZ2);
        assert_eq!(
            decompress_to_vec(&cat).unwrap(),
            b"hello crabz2\nhello crabz2\n"
        );
        #[cfg(feature = "std")]
        assert_eq!(decompress(&cat).unwrap(), b"hello crabz2\nhello crabz2\n");
    }

    #[test]
    fn truncated_input_errors() {
        // Every proper prefix is either an incomplete stream or an incomplete header;
        // none may decode successfully, and the buffer decoder must say Truncated.
        for cut in 1..HELLO_BZ2.len() {
            assert_eq!(decompress_to_vec(&HELLO_BZ2[..cut]), Err(Error::Truncated));
        }
        assert_eq!(decompress_to_vec(&EMPTY_BZ2[..10]), Err(Error::Truncated));
    }

    #[test]
    #[cfg(feature = "std")]
    fn truncated_stream_reader_errors() {
        use std::io::Read as _;
        let mut out = Vec::new();
        let err = reader(&HELLO_BZ2[..HELLO_BZ2.len() - 4])
            .read_to_end(&mut out)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    // A reader whose reads are one byte long: the state machine must produce the same
    // bytes when a block spans arbitrarily many refills.
    #[cfg(feature = "std")]
    struct Dribble<'a>(&'a [u8]);

    #[cfg(feature = "std")]
    impl Read for Dribble<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.0[0];
            self.0 = &self.0[1..];
            Ok(1)
        }
    }

    #[test]
    #[cfg(feature = "std")]
    fn survives_byte_at_a_time_source() {
        use std::io::Read as _;
        let mut cat = Vec::new();
        cat.extend_from_slice(HELLO_BZ2);
        cat.extend_from_slice(HELLO_BZ2);
        let mut out = Vec::new();
        reader(Dribble(&cat)).read_to_end(&mut out).unwrap();
        assert_eq!(out, b"hello crabz2\nhello crabz2\n");
    }

    // Corrupt input is rejected, never fatal: no arithmetic overflow, no index panic,
    // no unbounded allocation. Single-bit flips reach the RUNA/RUNB run accumulator,
    // the Huffman tables, and the origin pointer.
    #[test]
    fn single_bit_flips_never_panic() {
        for byte in 0..HELLO_BZ2.len() {
            for bit in 0..8 {
                let mut corrupt = HELLO_BZ2.to_vec();
                corrupt[byte] ^= 1 << bit;
                let _ = decompress_to_vec(&corrupt);
            }
        }
    }

    // The public sans-io API, driven the way a push-based (non-`Read`) caller has to:
    // append bytes, retry on `Truncated`, and drop what the decoder committed past.
    // Peak buffering must stay at the unconsumed remainder, never the whole input.
    #[test]
    fn sans_io_api_streams_with_rebase() {
        let mut cat = Vec::new();
        cat.extend_from_slice(HELLO_BZ2);
        cat.extend_from_slice(EMPTY_BZ2);
        cat.extend_from_slice(HELLO_BZ2);

        let mut dec = BlockDecoder::new();
        let mut inbuf: Vec<u8> = Vec::new();
        let mut out = Vec::new();
        let mut fed = 0usize;

        loop {
            match dec.next_block(&inbuf, &mut out) {
                Ok(Step::Block) => {}
                Ok(Step::Eof) | Err(Error::Truncated) => {
                    if fed == cat.len() {
                        break;
                    }
                    inbuf.push(cat[fed]);
                    fed += 1;
                    continue;
                }
                Err(e) => panic!("unexpected error: {e}"),
            }
            let n = dec.consumed();
            inbuf.drain(..n);
            dec.rebase(n);
            assert!(
                inbuf.len() < HELLO_BZ2.len(),
                "buffer should not accumulate"
            );
        }

        assert_eq!(out, b"hello crabz2\nhello crabz2\n");
        // Everything was consumed: a clean `Eof` leaves nothing unread.
        assert_eq!(inbuf.len() - dec.consumed(), 0);
    }

    #[test]
    #[should_panic(expected = "rebase past the committed position")]
    fn rebase_past_the_cursor_panics() {
        let mut dec = BlockDecoder::new();
        dec.rebase(1);
    }

    // ---- parallel block decode -------------------------------------------------
    //
    // Every test here asserts the same property in a different corner: for any input
    // whatsoever, valid or corrupt, `decompress_parallel` returns exactly what
    // `decompress` returns. The serial decoder is the oracle; the fixtures are only
    // there to make the oracle's answer interesting.

    #[cfg(feature = "parallel")]
    fn assert_matches_serial(input: &[u8], threads: Option<usize>, what: &str) {
        let serial = decompress(input).map_err(|e| e.to_string());
        let parallel = decompress_parallel(input, threads).map_err(|e| e.to_string());
        assert!(
            serial == parallel,
            "{what}: parallel({threads:?}) disagrees with serial \
             (serial {:?}, parallel {:?})",
            serial.as_ref().map(|v| v.len()),
            parallel.as_ref().map(|v| v.len()),
        );
    }

    /// Deterministic incompressible bytes; bzip2 still blocks them, which is what we
    /// want — many blocks, no shortcuts.
    #[cfg(feature = "parallel")]
    fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect()
    }

    /// Highly compressible bytes with enough structure to survive RLE1 and fill
    /// blocks rather than collapse into one.
    #[cfg(feature = "parallel")]
    fn repetitive(n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n);
        let mut i = 0u32;
        while v.len() < n {
            let line = std::format!("{i:08} the quick brown fox jumps over the lazy dog\n");
            v.extend_from_slice(line.as_bytes());
            i = i.wrapping_add(1);
        }
        v.truncate(n);
        v
    }

    #[cfg(feature = "parallel")]
    fn system_bzip2(data: &[u8], level: u8, tag: &str) -> Option<Vec<u8>> {
        use std::process::{Command, Stdio};
        let path = std::env::temp_dir().join(std::format!(
            "crabz2-par-{}-{tag}-{level}.bin",
            std::process::id()
        ));
        std::fs::write(&path, data).ok()?;
        let run = Command::new("bzip2")
            .arg(std::format!("-{level}"))
            .arg("-c")
            .arg(&path)
            .stderr(Stdio::null())
            .output();
        let _ = std::fs::remove_file(&path);
        let run = run.ok()?;
        if !run.status.success() {
            return None;
        }
        Some(run.stdout)
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn parallel_matches_serial_on_the_test_vectors() {
        let mut cat = Vec::new();
        cat.extend_from_slice(HELLO_BZ2);
        cat.extend_from_slice(EMPTY_BZ2);
        cat.extend_from_slice(HELLO_BZ2);

        let spliced = splice(&[HELLO_BZ2, HELLO_BZ2, HELLO_BZ2, HELLO_BZ2]);
        let planted = magic_inside_block_data(3);
        let mixed = splice(&[HELLO_BZ2, &planted, HELLO_BZ2, &planted]);

        let cases: [(&str, &[u8]); 6] = [
            ("single block", HELLO_BZ2),
            ("empty stream", EMPTY_BZ2),
            ("multi-stream", &cat),
            ("spliced multi-block", &spliced),
            ("magic planted in block data", &planted),
            ("multi-block around planted magic", &mixed),
        ];

        for (what, input) in cases {
            for threads in [Some(1), Some(2), Some(4), Some(8), None] {
                assert_matches_serial(input, threads, what);
            }
        }
    }

    /// Multi-block fixtures with no system `bzip2` and nothing checked in: our own
    /// encoder makes them. It also means the two halves of the crate check each
    /// other — a block the encoder emits, decoded on a pool, must come back exactly.
    #[test]
    #[cfg(feature = "parallel")]
    fn parallel_matches_serial_on_our_own_compressed_output() {
        let mut plain = repetitive(160 * 1024);
        plain.extend_from_slice(&pseudo_random(160 * 1024, 0x243F_6A88_85A3_08D3));
        plain.extend_from_slice(&repetitive(160 * 1024));

        for level in [Level::FASTEST, Level::new(2).unwrap()] {
            let compressed = compress(&plain, level);
            let blocks = blocks_of(&compressed).len();
            assert!(
                blocks >= 2,
                "level {} produced {blocks} blocks",
                level.get()
            );
            assert_eq!(
                parallel::decode(&compressed).unwrap().1,
                blocks,
                "level {}: chain fell back to serial",
                level.get()
            );
            for threads in [Some(2), Some(4), None] {
                assert!(
                    decompress_parallel(&compressed, threads).unwrap() == plain,
                    "level {}, {threads:?} threads",
                    level.get()
                );
            }

            // Concatenating our own streams keeps the multi-stream path in view.
            let mut cat = compressed.clone();
            cat.extend_from_slice(&compress(b"tail\n", Level::BEST));
            let mut expected = plain.clone();
            expected.extend_from_slice(b"tail\n");
            assert_eq!(decompress_parallel(&cat, None).unwrap(), expected);
        }
    }

    /// A healthy multi-block stream must be decoded entirely by the fast path. Without
    /// this the suite would still pass if the chain silently gave up and re-decoded
    /// everything serially — correct output, no parallelism, nobody notices.
    #[test]
    #[cfg(feature = "parallel")]
    fn fast_path_accepts_every_block_of_a_healthy_stream() {
        let spliced = splice(&[HELLO_BZ2; 5]);
        let (out, accepted) = parallel::decode(&spliced).unwrap();
        assert_eq!(accepted, 5, "chain fell back to serial");
        assert_eq!(out, b"hello crabz2\n".repeat(5));

        // A stream with no blocks at all has nothing to accept, and must still work.
        let (out, accepted) = parallel::decode(EMPTY_BZ2).unwrap();
        assert_eq!((out.len(), accepted), (0, 0));
    }

    /// The declared block size is the one thing that makes a block's decode depend on
    /// its stream header, and speculation runs before the header is known. Relabelling
    /// a level-9 stream as level 1 declares a 100 KB block that the real block
    /// overflows: serial rejects it, so the chain must reject it too rather than
    /// accept the speculative decode that ran against the 900 KB bound.
    #[test]
    #[cfg(feature = "parallel")]
    fn block_over_the_declared_size_is_rejected_as_serial_does() {
        let plain = repetitive(400 * 1024);
        if let Some(mut compressed) = system_bzip2(&plain, 9, "relabel") {
            assert_eq!(blocks_of(&compressed).len(), 1);
            compressed[3] = b'1';
            assert_eq!(decompress_to_vec(&compressed), Err(Error::BlockOverflow));
            for threads in [Some(2), Some(4), None] {
                assert_matches_serial(&compressed, threads, "level relabelled to 1");
            }
        }
    }

    /// The planted pattern must actually reach the scanner — otherwise the test
    /// above proves nothing about false positives. One block, at least two
    /// candidates: the real one and the one sitting in the selector list.
    #[test]
    #[cfg(feature = "parallel")]
    fn planted_magic_is_a_real_false_positive() {
        for pad in 0..8 {
            let s = magic_inside_block_data(pad);
            let candidates = parallel::scan_candidates(&s);
            assert_eq!(blocks_of(&s).len(), 1, "pad {pad}");
            assert!(
                candidates.len() >= 2,
                "pad {pad}: planted magic was not found by the scanner ({candidates:?})"
            );
            assert_eq!(decompress_parallel(&s, Some(4)).unwrap(), b"A", "pad {pad}");
        }
    }

    /// Truncation: the parallel path must report the same error at the same cut,
    /// never a short but successful read. Candidate blocks past the cut decode or
    /// fail on their own; only the chain decides what counts.
    #[test]
    #[cfg(feature = "parallel")]
    fn parallel_matches_serial_on_truncated_input() {
        let spliced = splice(&[HELLO_BZ2, HELLO_BZ2, HELLO_BZ2]);
        for cut in 0..spliced.len() {
            assert_matches_serial(&spliced[..cut], None, "truncated multi-block");
        }
        for cut in 0..HELLO_BZ2.len() {
            assert_matches_serial(&HELLO_BZ2[..cut], None, "truncated single block");
        }
    }

    /// Corruption: every single-bit flip in a multi-block stream, compared against
    /// serial. This is where a wrong chain rule shows up — a flipped bit can create
    /// a candidate, destroy one, or move a block boundary.
    #[test]
    #[cfg(feature = "parallel")]
    fn parallel_matches_serial_on_every_single_bit_flip() {
        let spliced = splice(&[HELLO_BZ2, HELLO_BZ2, HELLO_BZ2]);
        for byte in 0..spliced.len() {
            for bit in 0..8 {
                let mut corrupt = spliced.clone();
                corrupt[byte] ^= 1 << bit;
                assert_matches_serial(&corrupt, None, "bit flip");
            }
        }
    }

    /// The fuzz corpus doubles as a regression corpus here: whatever those inputs
    /// do serially, they must do in parallel.
    #[test]
    #[cfg(feature = "parallel")]
    fn parallel_matches_serial_on_the_fuzz_corpus() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus/fuzz_decompress");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            // Not present in a published .crate; nothing to check.
            Err(_) => return,
        };
        let mut seen = 0;
        for entry in entries.flatten() {
            let data = match std::fs::read(entry.path()) {
                Ok(data) => data,
                Err(_) => continue,
            };
            let name = entry.file_name();
            assert_matches_serial(&data, None, &name.to_string_lossy());
            assert_matches_serial(&data, Some(2), &name.to_string_lossy());
            seen += 1;
        }
        assert!(seen > 0, "corpus directory {} was empty", dir.display());
    }

    /// Real multi-block streams from the reference compressor, at both ends of the
    /// block-size range and over both compressible and incompressible input.
    #[test]
    #[cfg(feature = "parallel")]
    fn parallel_matches_serial_on_system_bzip2_output() {
        let mut plain = repetitive(700 * 1024);
        plain.extend_from_slice(&pseudo_random(700 * 1024, 0x9E37_79B9_7F4A_7C15));

        let mut checked = 0;
        for (level, min_blocks) in [(1u8, 4usize), (9, 2)] {
            let compressed = match system_bzip2(&plain, level, "mixed") {
                Some(c) => c,
                // No bzip2 on this machine.
                None => continue,
            };
            let blocks = blocks_of(&compressed).len();
            assert!(
                blocks >= min_blocks,
                "level {level} produced {blocks} blocks, expected at least {min_blocks}"
            );
            assert_eq!(decompress(&compressed).unwrap(), plain);
            assert_eq!(
                parallel::decode(&compressed).unwrap().1,
                blocks,
                "level {level}: chain fell back to serial"
            );
            for threads in [Some(1), Some(2), Some(4), Some(8), None] {
                let out = decompress_parallel(&compressed, threads).unwrap();
                assert!(out == plain, "level {level}, {threads:?} threads");
            }
            checked += 1;
        }
        if checked == 0 {
            std::eprintln!("system bzip2 not available; skipped");
        }
    }

    #[test]
    fn error_display_is_stable() {
        assert_eq!(
            Error::Truncated.to_string(),
            "unexpected end of bzip2 stream"
        );
    }

    // The two halves of the crate, checked against each other on the no_std path.
    #[test]
    fn round_trips_through_our_own_encoder() {
        let data = b"hello crabz2\n";
        assert_eq!(
            decompress_to_vec(&compress(data, Level::BEST)).unwrap(),
            data
        );
    }

    #[test]
    #[cfg(feature = "std")]
    fn writer_produces_a_stream_the_reader_accepts() {
        use std::io::Write as _;
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let mut w = writer(Vec::new(), Level::FASTEST);
        w.write_all(&data).unwrap();
        let packed = w.finish().unwrap();
        assert_eq!(decompress(&packed).unwrap(), data);
    }
}
