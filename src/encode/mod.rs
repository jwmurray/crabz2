//! The bzip2 compression pipeline: RLE1 → BWT → MTF/RLE2 → grouped Huffman →
//! bit packing.
//!
//! Everything in here is sans-io — it works on slices and `Vec`s and never
//! touches `std::io`, so the same code serves the `std` writer adapter in
//! `lib.rs` and a `no_std + alloc` build.

pub mod bitwriter;
pub mod bwt;
pub mod crc;
pub mod huffman;
pub mod mtf;
pub mod rle1;

use alloc::vec::Vec;

use bitwriter::BitWriter;
use crc::Crc32;

const BLOCK_MAGIC: u64 = 0x3141_5926_5359;
const EOS_MAGIC: u64 = 0x1772_4538_5090;

/// Compression level — bzip2's `-1` through `-9`, selecting a block size of
/// 100 kB through 900 kB.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Level(u8);

impl Level {
    /// Level 1 — 100 kB blocks, least memory, worst ratio.
    pub const FASTEST: Level = Level(1);
    /// Level 9 — 900 kB blocks, best ratio. Matches `bzip2 -9`.
    pub const BEST: Level = Level(9);
    /// What `bzip2` itself defaults to: level 9.
    pub const DEFAULT: Level = Level(9);

    /// Build a level from a digit in `1..=9`.
    pub const fn new(level: u8) -> Option<Level> {
        if level >= 1 && level <= 9 {
            Some(Level(level))
        } else {
            None
        }
    }

    /// The level as a digit in `1..=9`.
    pub const fn get(self) -> u8 {
        self.0
    }

    /// Nominal block size in bytes (`level * 100_000`).
    pub const fn block_size(self) -> usize {
        self.0 as usize * 100_000
    }

    /// The most RLE1 output we put in one block. bzip2 leaves a small margin
    /// below the nominal size; the decoder rejects a block that reaches it.
    const fn block_limit(self) -> usize {
        self.block_size() - 19
    }
}

impl Default for Level {
    fn default() -> Self {
        Level::DEFAULT
    }
}

/// Incremental, io-free bzip2 encoder.
///
/// Feed plaintext with [`Encoder::push`] and close the stream with
/// [`Encoder::finish`]. Blocks are emitted as soon as they fill, so the output
/// buffer is the only thing that grows without bound.
pub struct Encoder {
    limit: usize,
    out: BitWriter,
    combined_crc: u32,
    /// RLE1 output accumulated for the block being built.
    block: Vec<u8>,
    /// CRC of the plaintext already committed to `block`.
    block_crc: Crc32,
    /// Splits incoming plaintext into RLE1 groups across `push` calls.
    runs: rle1::Splitter,
}

impl Encoder {
    /// Start a stream, writing the `BZh<level>` header.
    pub fn new(level: Level) -> Encoder {
        let mut out = BitWriter::with_capacity(1 << 16);
        out.write_u8(b'B');
        out.write_u8(b'Z');
        out.write_u8(b'h');
        out.write_u8(b'0' + level.get());
        Encoder {
            limit: level.block_limit(),
            out,
            combined_crc: 0,
            block: Vec::with_capacity(level.block_limit().min(1 << 20)),
            block_crc: Crc32::new(),
            runs: rle1::Splitter::new(),
        }
    }

    /// Feed plaintext.
    pub fn push(&mut self, data: &[u8]) {
        for &b in data {
            if let Some(group) = self.runs.push(b) {
                self.commit(group);
            }
        }
    }

    /// Close the stream and return whatever compressed bytes remain.
    pub fn finish(mut self) -> Vec<u8> {
        if let Some(group) = self.runs.finish() {
            self.commit(group);
        }
        self.flush_block();
        self.out.write_magic(EOS_MAGIC);
        self.out.write_u32(self.combined_crc);
        self.out.finish()
    }

    /// Take the compressed bytes produced so far, so a streaming caller can
    /// hand them off instead of holding the whole output in memory. Only the
    /// `std` writer adapter drains; `compress` takes everything at `finish`.
    #[cfg_attr(not(feature = "std"), allow(dead_code))]
    pub fn drain(&mut self) -> Vec<u8> {
        self.out.drain()
    }

    /// Place one RLE1 group, starting a new block first if it would not fit.
    ///
    /// A group is self-contained — the decoder resets its run state after each
    /// one — so a block boundary may fall on either side of it, but never
    /// inside it.
    fn commit(&mut self, group: rle1::Group) {
        if self.block.len() + group.encoded_len() > self.limit {
            self.flush_block();
        }
        group.write_into(&mut self.block);
        self.block_crc.push_repeat(group.byte, group.raw_len);
    }

    /// Compress the accumulated block and reset for the next one.
    fn flush_block(&mut self) {
        if self.block.is_empty() {
            return;
        }
        let block_crc = self.block_crc.finish();
        write_block(&mut self.out, &self.block, block_crc);
        self.combined_crc = crc::combine(self.combined_crc, block_crc);
        self.block.clear();
        self.block_crc = Crc32::new();
    }
}

/// Compress one block's RLE1 output and append it to `w`.
fn write_block(w: &mut BitWriter, rle1: &[u8], block_crc: u32) {
    let (last, orig_ptr) = bwt::transform(rle1);
    let symbols = mtf::encode(&last);
    let coding = huffman::build(&symbols.syms, symbols.alpha_size());

    w.write_magic(BLOCK_MAGIC);
    w.write_u32(block_crc);
    w.write_bit(0); // never randomized
    w.write_bits(24, orig_ptr as u32);
    write_symbol_map(w, &symbols.in_use);
    w.write_bits(3, coding.n_groups as u32);
    w.write_bits(15, coding.selectors.len() as u32);
    huffman::write_selectors(w, &coding.selectors, coding.n_groups);
    huffman::write_tables(w, &coding.lens);

    for (group, &table) in coding.selectors.iter().enumerate() {
        let start = group * huffman::GROUP_SIZE;
        let end = (start + huffman::GROUP_SIZE).min(symbols.syms.len());
        let lens = &coding.lens[table as usize];
        let codes = &coding.codes[table as usize];
        for &s in &symbols.syms[start..end] {
            w.write_bits(lens[s as usize] as u32, codes[s as usize]);
        }
    }
}

/// The two-level "which byte values occur" bitmap: 16 bits saying which groups
/// of 16 are present, then 16 bits for each present group.
fn write_symbol_map(w: &mut BitWriter, in_use: &[u8]) {
    let mut used = [false; 256];
    for &b in in_use {
        used[b as usize] = true;
    }
    let mut groups = 0u32;
    for (i, chunk) in used.chunks(16).enumerate() {
        if chunk.iter().any(|&u| u) {
            groups |= 1 << (15 - i);
        }
    }
    w.write_bits(16, groups);
    for (i, chunk) in used.chunks(16).enumerate() {
        if groups & (1 << (15 - i)) != 0 {
            let mut bits = 0u32;
            for (j, &u) in chunk.iter().enumerate() {
                if u {
                    bits |= 1 << (15 - j);
                }
            }
            w.write_bits(16, bits);
        }
    }
}

/// Compress a whole buffer at `level`.
pub fn compress(data: &[u8], level: Level) -> Vec<u8> {
    let mut enc = Encoder::new(level);
    enc.push(data);
    enc.finish()
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::decompress_to_vec;

    fn round_trip(data: &[u8], level: Level) {
        let packed = compress(data, level);
        let got = decompress_to_vec(&packed).expect("our decoder rejected our output");
        assert_eq!(got, data, "round trip failed at level {}", level.get());
    }

    #[test]
    fn levels_map_to_block_sizes() {
        assert_eq!(Level::new(1).unwrap().block_size(), 100_000);
        assert_eq!(Level::new(9).unwrap().block_size(), 900_000);
        assert_eq!(Level::new(0), None);
        assert_eq!(Level::new(10), None);
        assert_eq!(Level::default(), Level::BEST);
    }

    #[test]
    fn empty_input_matches_the_reference_empty_stream() {
        // Byte-for-byte what `printf '' | bzip2 -9` produces: header, end of
        // stream, zero combined CRC.
        assert_eq!(
            compress(b"", Level::BEST),
            vec![0x42, 0x5a, 0x68, 0x39, 0x17, 0x72, 0x45, 0x38, 0x50, 0x90, 0, 0, 0, 0]
        );
    }

    #[test]
    fn round_trips_hello() {
        round_trip(b"hello crabz2\n", Level::BEST);
    }

    #[test]
    fn round_trips_every_level() {
        let data: Vec<u8> = b"the quick brown fox jumps over the lazy dog\n"
            .iter()
            .cycle()
            .take(5000)
            .copied()
            .collect();
        for l in 1..=9u8 {
            round_trip(&data, Level::new(l).unwrap());
        }
    }

    #[test]
    fn round_trips_short_inputs() {
        for n in 0..300usize {
            let data: Vec<u8> = (0..n).map(|i| (i % 7) as u8).collect();
            round_trip(&data, Level::FASTEST);
        }
    }

    #[test]
    fn round_trips_single_byte_blocks() {
        round_trip(b"x", Level::BEST);
        round_trip(&[0u8], Level::BEST);
        round_trip(&[0xffu8; 1], Level::BEST);
    }

    #[test]
    fn writes_a_symbol_map_the_decoder_reads_back() {
        let mut w = BitWriter::new();
        write_symbol_map(&mut w, &[0, 15, 16, 200, 255]);
        let bytes = w.finish();
        // Groups 0 (bytes 0-15), 1 (16-31), 12 (192-207) and 15 (240-255).
        let groups = u16::from_be_bytes([bytes[0], bytes[1]]);
        assert_eq!(groups, 0b1100_0000_0000_1001u16);
        assert_eq!(bytes.len(), 2 + 4 * 2);
    }
}
