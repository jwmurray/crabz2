//! # crabz2
//!
//! Pure-Rust bzip2 **decompression** — no C, no bundled `libbz2`, and no third-party
//! decode crate. `crabz2` implements the full bzip2 decode pipeline from scratch:
//! bit reader → Huffman → MTF/RLE2 → inverse Burrows–Wheeler transform → RLE1 →
//! CRC-32/BZIP2 validation.
//!
//! The core is sans-io and `no_std + alloc`: [`decompress_to_vec`] and the [`Error`]
//! enum are available with `default-features = false`. The `std` feature (on by
//! default) adds the `io::Read` adapters — `reader`, `Crabz2Reader`, `decompress` —
//! and `From<Error> for std::io::Error`.
//!
//! It streams: one bzip2 block is decoded at a time and drained through `io::Read`,
//! so peak memory is bounded to roughly one compressed plus one decompressed block.
//! Concatenated (multi-stream) `.bz2` input is handled, matching `bzip2 -dc`.
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

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

#[cfg(feature = "std")]
use std::io::{self, Read};

const BLOCK_MAGIC: u64 = 0x3141_5926_5359; // pi digits
const EOS_MAGIC: u64 = 0x1772_4538_5090; // sqrt(pi) digits
const MAX_CODE_LEN: usize = 23;
const GROUP_SIZE: usize = 50;

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
struct HuffTable {
    min_len: u32,
    max_len: u32,
    limit: [i32; MAX_CODE_LEN + 2],
    base: [i32; MAX_CODE_LEN + 2],
    perm: Vec<usize>,
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

        let mut base = [0i32; MAX_CODE_LEN + 2];
        for &sl in len {
            base[sl as usize + 1] += 1;
        }
        for i in 1..base.len() {
            base[i] += base[i - 1];
        }

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

        Ok(HuffTable {
            min_len,
            max_len,
            limit,
            base,
            perm,
        })
    }

    #[inline]
    fn decode(&self, bits: &mut BitCursor<'_>) -> Result<usize, Error> {
        let mut l = self.min_len;
        let mut v = bits.read_bits(l)? as i32;
        while l <= self.max_len {
            if v <= self.limit[l as usize] {
                let idx = (v - self.base[l as usize]) as usize;
                if idx >= self.perm.len() {
                    return Err(Error::InvalidHuffman);
                }
                return Ok(self.perm[idx]);
            }
            l += 1;
            v = (v << 1) | bits.read_bit()? as i32;
        }
        Err(Error::InvalidHuffman)
    }
}

enum Phase {
    StreamStart,
    Block,
}

/// One step of the sans-io machine: a block's plaintext was appended, or the input
/// ended cleanly at a stream boundary.
enum Step {
    Block,
    Eof,
}

/// The whole decoder, free of io: it consumes `&[u8]` from a committed bit position
/// and appends plaintext to a caller-supplied `Vec`.
///
/// Restart doctrine: [`Error::Truncated`] leaves `bit` at the last committed boundary
/// (stream header, block, end-of-stream) and `out` at the length it had on entry, so
/// a caller that can obtain more input may append it and call again. Every other error
/// is terminal.
struct BlockDecoder {
    bit: usize,
    phase: Phase,
    block_size: usize,
    combined_crc: u32,
    /// BWT scratch, reused across blocks: byte in the low 8 bits, source index above.
    tt: Vec<u32>,
}

impl BlockDecoder {
    fn new() -> Self {
        BlockDecoder {
            bit: 0,
            phase: Phase::StreamStart,
            block_size: 0,
            combined_crc: 0,
            tt: Vec::new(),
        }
    }

    /// Decode until one block's plaintext has been appended to `out`, or the input
    /// ends at a stream boundary.
    fn next_block(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<Step, Error> {
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

        // MTF + RLE2 decode into the BWT input buffer `tt` (byte in low 8 bits).
        self.tt.clear();
        let mut cftab = [0u32; 257];
        let mut mtf = seq_to_unseq.clone();
        let mut sel_idx = 0usize;
        let mut group_count = 0usize;
        let mut cur_table = 0usize;
        let mut run: u64 = 0;
        let mut run_bit: u32 = 0;

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
            let sym = tables[cur_table].decode(bits)?;

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
                let b = mtf[0];
                if self.tt.len() + run as usize > self.block_size {
                    return Err(Error::BlockOverflow);
                }
                let entry = b as u32;
                cftab[b as usize + 1] += run as u32;
                for _ in 0..run {
                    self.tt.push(entry);
                }
                run = 0;
                run_bit = 0;
            }

            if sym == eob {
                break;
            }

            // MTF index (sym - 1): move that byte value to the front.
            let nn = sym - 1;
            if nn >= mtf.len() {
                return Err(Error::InvalidBlock);
            }
            let b = mtf[nn];
            mtf.copy_within(0..nn, 1);
            mtf[0] = b;

            if self.tt.len() + 1 > self.block_size {
                return Err(Error::BlockOverflow);
            }
            self.tt.push(b as u32);
            cftab[b as usize + 1] += 1;
        }

        let nblock = self.tt.len();
        if nblock == 0 || orig_ptr >= nblock {
            return Err(Error::InvalidBlock);
        }

        // Cumulative counts -> starting index of each byte value.
        for i in 1..=256 {
            cftab[i] += cftab[i - 1];
        }

        // Inverse Burrows–Wheeler transform (fast form): thread the source index
        // into the high bits of each `tt` cell, then walk from `orig_ptr`.
        for i in 0..nblock {
            let b = (self.tt[i] & 0xff) as usize;
            let idx = cftab[b] as usize;
            self.tt[idx] |= (i as u32) << 8;
            cftab[b] += 1;
        }

        // Walk the permutation, applying RLE1 and CRC to produce the plaintext block.
        // No bits are read past this point, so `out` only grows once the block is
        // structurally sound.
        let mut crc = 0xFFFF_FFFFu32;
        let mut t_pos = self.tt[orig_ptr] >> 8;
        let mut prev: i32 = -1;
        let mut count: u32 = 0;
        for _ in 0..nblock {
            t_pos = self.tt[t_pos as usize];
            let b = (t_pos & 0xff) as u8;
            t_pos >>= 8;

            if count == 4 {
                // `b` is the count of extra repeats beyond the four literals.
                for _ in 0..b {
                    out.push(prev as u8);
                    crc = crc_update(crc, prev as u8);
                }
                count = 0;
                prev = -1;
            } else {
                out.push(b);
                crc = crc_update(crc, b);
                if b as i32 == prev {
                    count += 1;
                } else {
                    prev = b as i32;
                    count = 1;
                }
            }
        }

        let crc = !crc;
        if crc != block_crc {
            return Err(Error::CrcMismatch);
        }
        self.combined_crc = self.combined_crc.rotate_left(1);
        self.combined_crc ^= block_crc;

        Ok(())
    }
}

/// Decompress an entire in-memory `.bz2` buffer. Available without `std`.
///
/// Handles concatenated (multi-stream) input and verifies both the per-block and the
/// combined-stream CRC. Peak memory is the plaintext plus one block of scratch.
pub fn decompress_to_vec(compressed: &[u8]) -> Result<Vec<u8>, Error> {
    let mut dec = BlockDecoder::new();
    let mut out = Vec::new();
    loop {
        match dec.next_block(compressed, &mut out)? {
            Step::Block => {}
            Step::Eof => return Ok(out),
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
        let unconsumed = self.inbuf.len() - (self.dec.bit >> 3);
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
        let consumed = self.dec.bit >> 3;
        if consumed > 0 {
            self.inbuf.drain(..consumed);
            self.dec.bit &= 7;
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

        fn finish(mut self) -> Vec<u8> {
            if self.nbits > 0 {
                let pad = 8 - self.nbits;
                self.acc <<= pad;
                self.out.push(self.acc as u8);
            }
            self.out
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

    #[test]
    fn error_display_is_stable() {
        assert_eq!(
            Error::Truncated.to_string(),
            "unexpected end of bzip2 stream"
        );
    }
}
