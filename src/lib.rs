//! # crabz2
//!
//! Pure-Rust bzip2 **decompression** — no C, no bundled `libbz2`, and no third-party
//! decode crate. `crabz2` implements the full bzip2 decode pipeline from scratch:
//! bit reader → Huffman → MTF/RLE2 → inverse Burrows–Wheeler transform → RLE1 →
//! CRC-32/BZIP2 validation.
//!
//! It streams: one bzip2 block is decoded at a time and drained through [`std::io::Read`],
//! so peak memory is bounded to roughly one decompressed block. Concatenated (multi-stream)
//! `.bz2` input is handled, matching `bzip2 -dc`.
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
//! For small in-memory buffers:
//!
//! ```
//! let data = crabz2::decompress(HELLO)?;
//! assert_eq!(data, b"hello crabz2\n");
//! # const HELLO: &[u8] = &[
//! #     0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x71, 0x1c, 0x50, 0xc0, 0x00,
//! #     0x00, 0x03, 0xd9, 0x80, 0x00, 0x10, 0x40, 0x00, 0x10, 0x00, 0x3a, 0x44, 0x90, 0x10, 0x20,
//! #     0x00, 0x31, 0x03, 0x40, 0xd0, 0x29, 0x80, 0x1e, 0xa2, 0xe0, 0x4c, 0xed, 0x69, 0xe0, 0xe1,
//! #     0x77, 0x24, 0x53, 0x85, 0x09, 0x07, 0x11, 0xc5, 0x0c, 0x00,
//! # ];
//! # Ok::<(), std::io::Error>(())
//! ```

use std::io::{self, Read};

const BLOCK_MAGIC: u64 = 0x3141_5926_5359; // pi digits
const EOS_MAGIC: u64 = 0x1772_4538_5090; // sqrt(pi) digits
const MAX_CODE_LEN: usize = 23;
const GROUP_SIZE: usize = 50;

fn bad<T>(msg: &'static str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, msg))
}

#[inline]
fn mask64(n: u32) -> u64 {
    if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

/// MSB-first bit reader over an arbitrary byte source, with an internal read buffer.
struct BitReader<R> {
    inner: R,
    buf: Box<[u8; 1 << 16]>,
    buf_pos: usize,
    buf_len: usize,
    acc: u64,
    nbits: u32,
}

impl<R: Read> BitReader<R> {
    fn new(inner: R) -> Self {
        BitReader {
            inner,
            buf: Box::new([0u8; 1 << 16]),
            buf_pos: 0,
            buf_len: 0,
            acc: 0,
            nbits: 0,
        }
    }

    #[inline]
    fn next_raw_byte(&mut self) -> io::Result<Option<u8>> {
        if self.buf_pos == self.buf_len {
            self.buf_len = self.inner.read(&mut self.buf[..])?;
            self.buf_pos = 0;
            if self.buf_len == 0 {
                return Ok(None);
            }
        }
        let b = self.buf[self.buf_pos];
        self.buf_pos += 1;
        Ok(Some(b))
    }

    /// Read `n` bits (n <= 32), MSB first.
    #[inline]
    fn read_bits(&mut self, n: u32) -> io::Result<u32> {
        while self.nbits < n {
            match self.next_raw_byte()? {
                Some(b) => {
                    self.acc = (self.acc << 8) | b as u64;
                    self.nbits += 8;
                }
                None => return bad("unexpected end of bzip2 stream"),
            }
        }
        let shift = self.nbits - n;
        let val = ((self.acc >> shift) & mask64(n)) as u32;
        self.nbits = shift;
        self.acc &= mask64(shift);
        Ok(val)
    }

    #[inline]
    fn read_bit(&mut self) -> io::Result<u32> {
        self.read_bits(1)
    }

    /// Read the 48-bit block/stream magic.
    fn read_magic(&mut self) -> io::Result<u64> {
        let hi = self.read_bits(24)? as u64;
        let lo = self.read_bits(24)? as u64;
        Ok((hi << 24) | lo)
    }

    /// Discard buffered bits down to the next byte boundary.
    fn align_to_byte(&mut self) {
        let drop = self.nbits % 8;
        self.nbits -= drop;
        self.acc >>= drop;
    }

    /// Read one whole byte assuming we are byte-aligned; `None` at a clean EOF.
    fn read_byte_aligned(&mut self) -> io::Result<Option<u8>> {
        if self.nbits >= 8 {
            let shift = self.nbits - 8;
            let v = ((self.acc >> shift) & 0xff) as u8;
            self.nbits = shift;
            self.acc &= mask64(shift);
            Ok(Some(v))
        } else {
            self.next_raw_byte()
        }
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
    fn build(len: &[u8]) -> io::Result<HuffTable> {
        let alpha = len.len();
        let min_len = *len.iter().min().unwrap() as u32;
        let max_len = *len.iter().max().unwrap() as u32;
        if min_len < 1 || max_len as usize > MAX_CODE_LEN {
            return bad("invalid Huffman code length");
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
    fn decode<R: Read>(&self, bits: &mut BitReader<R>) -> io::Result<usize> {
        let mut l = self.min_len;
        let mut v = bits.read_bits(l)? as i32;
        while l <= self.max_len {
            if v <= self.limit[l as usize] {
                let idx = (v - self.base[l as usize]) as usize;
                if idx >= self.perm.len() {
                    return bad("Huffman symbol out of range");
                }
                return Ok(self.perm[idx]);
            }
            l += 1;
            v = (v << 1) | bits.read_bit()? as i32;
        }
        bad("Huffman code too long")
    }
}

/// CRC-32/BZIP2 (poly 0x04C11DB7, MSB-first, init/xorout 0xFFFFFFFF).
struct Crc {
    table: [u32; 256],
}

impl Crc {
    fn new() -> Self {
        let mut table = [0u32; 256];
        for (n, slot) in table.iter_mut().enumerate() {
            let mut c = (n as u32) << 24;
            for _ in 0..8 {
                c = if c & 0x8000_0000 != 0 {
                    (c << 1) ^ 0x04C1_1DB7
                } else {
                    c << 1
                };
            }
            *slot = c;
        }
        Crc { table }
    }

    #[inline]
    fn update(&self, crc: u32, byte: u8) -> u32 {
        (crc << 8) ^ self.table[(((crc >> 24) ^ byte as u32) & 0xff) as usize]
    }
}

enum Phase {
    StreamStart,
    Block,
    Done,
}

/// A streaming, from-scratch, pure-Rust bzip2 decompressor implementing [`std::io::Read`].
pub struct Crabz2Reader<R> {
    bits: BitReader<R>,
    crc: Crc,
    phase: Phase,
    block_size: usize,
    combined_crc: u32,
    out: Vec<u8>,
    pos: usize,
    // Scratch reused across blocks.
    tt: Vec<u32>,
}

impl<R: Read> Crabz2Reader<R> {
    /// Create a decoder over a compressed `.bz2` byte source.
    pub fn new(inner: R) -> Self {
        Crabz2Reader {
            bits: BitReader::new(inner),
            crc: Crc::new(),
            phase: Phase::StreamStart,
            block_size: 0,
            combined_crc: 0,
            out: Vec::new(),
            pos: 0,
            tt: Vec::new(),
        }
    }

    /// Read a `"BZh<level>"` stream header. `Ok(None)` on a clean end-of-input.
    fn read_header(&mut self) -> io::Result<Option<u32>> {
        self.bits.align_to_byte();
        let b0 = match self.bits.read_byte_aligned()? {
            Some(b) => b,
            None => return Ok(None),
        };
        let b1 = self.bits.read_byte_aligned()?;
        let b2 = self.bits.read_byte_aligned()?;
        let lvl = self.bits.read_byte_aligned()?;
        match (b1, b2, lvl) {
            (Some(b1), Some(b2), Some(lvl)) => {
                if b0 != b'B' || b1 != b'Z' || b2 != b'h' {
                    return bad("not a bzip2 stream (missing BZh magic)");
                }
                if !(b'1'..=b'9').contains(&lvl) {
                    return bad("invalid bzip2 block-size level");
                }
                Ok(Some((lvl - b'0') as u32))
            }
            _ => bad("truncated bzip2 stream header"),
        }
    }

    /// Advance until `self.out` holds a freshly decoded block. `Ok(false)` at EOF.
    fn refill(&mut self) -> io::Result<bool> {
        loop {
            match self.phase {
                Phase::Done => return Ok(false),
                Phase::StreamStart => match self.read_header()? {
                    None => {
                        self.phase = Phase::Done;
                        return Ok(false);
                    }
                    Some(level) => {
                        self.block_size = level as usize * 100_000;
                        self.combined_crc = 0;
                        self.phase = Phase::Block;
                    }
                },
                Phase::Block => {
                    let magic = self.bits.read_magic()?;
                    if magic == BLOCK_MAGIC {
                        self.decode_block()?;
                        return Ok(true);
                    } else if magic == EOS_MAGIC {
                        let stored = self.bits.read_bits(32)?;
                        if stored != self.combined_crc {
                            return bad("bzip2 stream CRC mismatch");
                        }
                        // A concatenated stream may follow; loop back to try a header.
                        self.phase = Phase::StreamStart;
                    } else {
                        return bad("invalid bzip2 block magic");
                    }
                }
            }
        }
    }

    fn decode_block(&mut self) -> io::Result<()> {
        let block_crc = self.bits.read_bits(32)?;
        if self.bits.read_bit()? != 0 {
            return bad("legacy randomized bzip2 block not supported");
        }
        let orig_ptr = self.bits.read_bits(24)? as usize;

        // Symbol map: which of the 256 byte values occur in this block.
        let mut used = [false; 256];
        let in_use16 = self.bits.read_bits(16)?;
        for i in 0..16 {
            if in_use16 & (1 << (15 - i)) != 0 {
                let bits16 = self.bits.read_bits(16)?;
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
            return bad("bzip2 block uses no symbols");
        }
        let alpha_size = n_in_use + 2;
        let eob = alpha_size - 1;

        // Selectors.
        let n_groups = self.bits.read_bits(3)? as usize;
        if !(2..=6).contains(&n_groups) {
            return bad("invalid bzip2 Huffman group count");
        }
        let n_selectors = self.bits.read_bits(15)? as usize;
        if n_selectors == 0 {
            return bad("bzip2 block has no selectors");
        }
        let mut group_pos: Vec<u8> = (0..n_groups as u8).collect();
        let mut selectors = Vec::with_capacity(n_selectors);
        for _ in 0..n_selectors {
            let mut j = 0usize;
            while self.bits.read_bit()? == 1 {
                j += 1;
                if j >= n_groups {
                    return bad("invalid bzip2 selector");
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
            let mut curr = self.bits.read_bits(5)? as i32;
            for slot in len.iter_mut() {
                loop {
                    if curr < 1 || curr > 20 {
                        return bad("invalid bzip2 Huffman delta length");
                    }
                    if self.bits.read_bit()? == 0 {
                        break;
                    }
                    if self.bits.read_bit()? == 0 {
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
                    return bad("ran out of bzip2 selectors");
                }
                cur_table = selectors[sel_idx];
                sel_idx += 1;
                group_count = GROUP_SIZE;
            }
            group_count -= 1;
            let sym = tables[cur_table].decode(&mut self.bits)?;

            if sym <= 1 {
                // RUNA (0) / RUNB (1): bijective base-2 zero-run length.
                run += ((sym as u64) + 1) << run_bit;
                run_bit += 1;
                continue;
            }

            if run > 0 {
                let b = mtf[0];
                if self.tt.len() + run as usize > self.block_size {
                    return bad("bzip2 block exceeds declared size");
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
                return bad("bzip2 MTF index out of range");
            }
            let b = mtf[nn];
            mtf.copy_within(0..nn, 1);
            mtf[0] = b;

            if self.tt.len() + 1 > self.block_size {
                return bad("bzip2 block exceeds declared size");
            }
            self.tt.push(b as u32);
            cftab[b as usize + 1] += 1;
        }

        let nblock = self.tt.len();
        if nblock == 0 || orig_ptr >= nblock {
            return bad("invalid bzip2 origin pointer");
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
        self.out.clear();
        self.pos = 0;
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
                    self.out.push(prev as u8);
                    crc = self.crc.update(crc, prev as u8);
                }
                count = 0;
                prev = -1;
            } else {
                self.out.push(b);
                crc = self.crc.update(crc, b);
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
            return bad("bzip2 block CRC mismatch");
        }
        self.combined_crc = (self.combined_crc << 1) | (self.combined_crc >> 31);
        self.combined_crc ^= block_crc;

        Ok(())
    }
}

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
pub fn reader<R: Read>(inner: R) -> Crabz2Reader<R> {
    Crabz2Reader::new(inner)
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

    // Empty input: `printf '' | bzip2 -9` (header + end-of-stream, no blocks).
    const EMPTY_BZ2: &[u8] = &[
        0x42, 0x5a, 0x68, 0x39, 0x17, 0x72, 0x45, 0x38, 0x50, 0x90, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn decodes_small_stream() {
        assert_eq!(decompress(HELLO_BZ2).unwrap(), b"hello crabz2\n");
    }

    #[test]
    fn decodes_empty_stream() {
        assert_eq!(decompress(EMPTY_BZ2).unwrap(), b"");
    }

    #[test]
    fn detects_corruption() {
        let mut bad = HELLO_BZ2.to_vec();
        let n = bad.len();
        bad[n - 6] ^= 0x01; // flip a bit in the payload
        assert!(decompress(&bad).is_err());
    }
}
