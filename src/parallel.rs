//! Parallel block decode (`parallel` feature).
//!
//! bzip2 blocks are independent: each carries its own Huffman tables, its own BWT,
//! and its own CRC. The only thing that ties them together is that a block's bits
//! begin wherever the previous block's bits ended — at an *arbitrary bit offset*,
//! with no length field anywhere in the format. So the parallel decoder cannot
//! partition the input up front; it has to guess.
//!
//! The guess is the 48-bit block magic `0x314159265359`. A scanner finds every bit
//! offset where that pattern occurs ([`scan_candidates`]), each candidate is decoded
//! speculatively on the thread pool, and the results are then chained strictly in
//! order: a decoded block is accepted only if it starts exactly where the previous
//! accepted block ended. The magic can also occur inside entropy-coded data, so a
//! candidate that is not a real block boundary either fails to decode (the common
//! case, usually within a few hundred bits) or, having decoded, is simply never
//! reached by the chain and is discarded.
//!
//! Correctness rule: **the output is byte-identical to the serial decoder, always.**
//! The fast path commits a block only when it is certain the serial decoder would
//! have produced exactly those bytes from exactly those bits; anything else — a bad
//! header, a missing or failed candidate at a chain position, a block larger than
//! the declared block size, a CRC mismatch — abandons the fast path and re-decodes
//! serially from the last committed boundary. Degrading to serial is always allowed;
//! degrading to different bytes is not.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::MaybeUninit;
use std::io;
use std::sync::Mutex;

use rayon::prelude::*;

use crate::{BitCursor, BlockDecoder, Error, Phase, Step, BLOCK_MAGIC, EOS_MAGIC};

/// The largest block any level can declare (level 9). Speculative decodes run
/// against this bound because the stream header that states the real one has not
/// necessarily been located yet; the chain re-checks each accepted block against
/// the declared size, so the looser bound can never loosen what we accept.
const MAX_BLOCK_SIZE: usize = 900_000;

/// Bytes of input per scanner task.
const SCAN_CHUNK: usize = 1 << 20;

const MASK48: u64 = (1 << 48) - 1;

/// Bit `k` is set for byte values whose low `8 - k` bits equal the top `8 - k` bits
/// of the block magic — i.e. the byte values that can *begin* a magic at bit offset
/// `k`. One table lookup rejects the overwhelming majority of positions before the
/// eight-way 48-bit comparison runs.
const PREFIX: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut b = 0usize;
    while b < 256 {
        let mut k = 0u32;
        let mut mask = 0u8;
        while k < 8 {
            let want = (BLOCK_MAGIC >> (40 + k)) as u8;
            let keep = ((1u16 << (8 - k)) - 1) as u8;
            if (b as u8) & keep == want {
                mask |= 1 << k;
            }
            k += 1;
        }
        table[b] = mask;
        b += 1;
    }
    table
};

#[inline]
fn byte_at(input: &[u8], i: usize) -> u64 {
    if i < input.len() {
        input[i] as u64
    } else {
        0
    }
}

/// Append every bit offset in `[start * 8, end * 8)` at which the 48-bit block magic
/// occurs. The window reads up to six bytes past `end`, so chunks tile the input
/// without overlap in their *output* while still finding magics that straddle a
/// chunk boundary.
fn scan_range(input: &[u8], start: usize, end: usize, out: &mut Vec<usize>) {
    let total_bits = input.len() * 8;
    // Prime the rolling window with the seven bytes that precede the first slot.
    let mut w: u64 = 0;
    for j in start..start + 7 {
        w = (w << 8) | byte_at(input, j);
    }
    for i in start..end {
        w = (w << 8) | byte_at(input, i + 7);
        // `w` now holds bytes `i ..= i + 7`, big-endian, zero-padded past the end.
        let mut mask = PREFIX[(w >> 56) as usize];
        while mask != 0 {
            let k = mask.trailing_zeros();
            mask &= mask - 1;
            if (w >> (16 - k)) & MASK48 == BLOCK_MAGIC {
                let bit = i * 8 + k as usize;
                if bit + 48 <= total_bits {
                    out.push(bit);
                }
            }
        }
    }
}

/// Every candidate block-magic bit offset in the input, ascending.
pub(crate) fn scan_candidates(input: &[u8]) -> Vec<usize> {
    if input.len() < 6 {
        return Vec::new();
    }
    if input.len() <= SCAN_CHUNK {
        let mut out = Vec::new();
        scan_range(input, 0, input.len(), &mut out);
        return out;
    }
    let chunks = (input.len() + SCAN_CHUNK - 1) / SCAN_CHUNK;
    let per: Vec<Vec<usize>> = (0..chunks)
        .into_par_iter()
        .map(|c| {
            let mut out = Vec::new();
            scan_range(
                input,
                c * SCAN_CHUNK,
                ((c + 1) * SCAN_CHUNK).min(input.len()),
                &mut out,
            );
            out
        })
        .collect();
    per.concat()
}

/// A candidate that decoded cleanly. Nothing here is trusted yet: a false positive
/// inside entropy-coded data can produce one of these too. It becomes output only if
/// the chain reaches `start_bit`.
struct Decoded {
    end_bit: usize,
    out: Vec<u8>,
    crc: u32,
    /// Post-RLE2 block length, checked against the declared block size when accepted.
    nblock: usize,
}

/// Decode one block starting at the magic at `start_bit`. `None` for anything that
/// is not a well-formed block: a false positive, or a real block whose bits were
/// truncated. `dec` is caller-owned scratch: its buffers are reused across
/// speculative decodes on the same worker so each block does not re-fault fresh
/// pages for the multi-megabyte BWT scratch.
fn speculate(dec: &mut BlockDecoder, input: &[u8], start_bit: usize) -> Option<Decoded> {
    dec.bit = start_bit;
    dec.phase = Phase::Block;
    dec.block_size = MAX_BLOCK_SIZE;
    // The first block folded into a zeroed combined CRC is the block CRC.
    dec.combined_crc = 0;
    let mut out = Vec::new();
    match dec.next_block(input, &mut out) {
        Ok(Step::Block) => Some(Decoded {
            end_bit: dec.bit,
            out,
            crc: dec.combined_crc,
            nblock: dec.tt.len(),
        }),
        _ => None,
    }
}

/// Serial decode from a committed boundary to the end of the input, appending to
/// `out`. This is the ordinary [`BlockDecoder`] driven exactly as `decompress_to_vec`
/// drives it, so both the bytes and the errors match serial mode by construction.
fn finish_serially(
    input: &[u8],
    bit: usize,
    phase: Phase,
    block_size: usize,
    combined_crc: u32,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    let mut dec = BlockDecoder::new();
    dec.bit = bit;
    dec.phase = phase;
    dec.block_size = block_size;
    dec.combined_crc = combined_crc;
    loop {
        match dec.next_block(input, out)? {
            Step::Block => {}
            Step::Eof => return Ok(()),
        }
    }
}

/// One run of output bytes, in stream order: an accepted speculative block, or the
/// serial tail decoded after the fast path abandoned the chain.
enum Segment {
    Fast(Decoded),
    Serial(Vec<u8>),
}

impl Segment {
    fn bytes(&self) -> &[u8] {
        match self {
            Segment::Fast(d) => &d.out,
            Segment::Serial(v) => v,
        }
    }
}

/// Concatenate the segments. The chain walk is cheap (it reads only magics and
/// headers), so nearly all of the assembly cost is this copy; for large outputs the
/// segments land in disjoint slices of one pre-sized allocation in parallel.
fn stitch(segments: Vec<Segment>) -> Vec<u8> {
    let total: usize = segments.iter().map(|s| s.bytes().len()).sum();
    let mut out: Vec<u8> = Vec::with_capacity(total);

    // Small outputs: rayon overhead outweighs the copy.
    if total < (1 << 22) || segments.len() < 2 {
        for s in &segments {
            out.extend_from_slice(s.bytes());
        }
        return out;
    }

    // Carve the spare capacity into one disjoint `&mut` slice per segment.
    let mut spare: &mut [MaybeUninit<u8>] = &mut out.spare_capacity_mut()[..total];
    let mut slots: Vec<&mut [MaybeUninit<u8>]> = Vec::with_capacity(segments.len());
    for s in &segments {
        let (head, rest) = spare.split_at_mut(s.bytes().len());
        slots.push(head);
        spare = rest;
    }
    segments
        .par_iter()
        .zip(slots)
        .for_each(|(seg, slot)| unsafe {
            let src = seg.bytes();
            core::ptr::copy_nonoverlapping(src.as_ptr(), slot.as_mut_ptr() as *mut u8, src.len());
        });
    // Safety: the slots partition `0..total` and every byte was written above.
    unsafe { out.set_len(total) };
    out
}

/// Walk the stream structure, splicing in already-decoded blocks where the chain
/// confirms them and falling back to serial everywhere else. Returns the plaintext
/// and how many blocks the fast path accepted, which the tests use to prove the fast
/// path is doing the work rather than quietly deferring to serial.
fn assemble(input: &[u8], mut blocks: BTreeMap<usize, Decoded>) -> Result<(Vec<u8>, usize), Error> {
    let mut accepted = 0usize;
    let mut segments: Vec<Segment> = Vec::new();
    let mut bit = 0usize;
    let mut block_size = 0usize;
    let mut combined_crc = 0u32;
    // Mirrors `BlockDecoder::phase`; the two are kept in step so a fallback can hand
    // the serial decoder the exact state the fast path had reached.
    let mut at_stream_start = true;

    // Every fallback decodes serially to the end of the input, so it is always the
    // final segment.
    let fall_back = |bit: usize, phase: Phase, block_size: usize, crc: u32| {
        let mut tail = Vec::new();
        finish_serially(input, bit, phase, block_size, crc, &mut tail).map(|()| tail)
    };

    loop {
        let mut bits = BitCursor::new(input, bit);

        if at_stream_start {
            bits.align_to_byte();
            if bits.bytes_left() == 0 {
                // Clean end of input at a stream boundary, exactly as serial.
                return Ok((stitch(segments), accepted));
            }
            let header = (|| {
                if bits.bytes_left() < 4 {
                    return None;
                }
                let b0 = bits.read_bits(8).ok()? as u8;
                let b1 = bits.read_bits(8).ok()? as u8;
                let b2 = bits.read_bits(8).ok()? as u8;
                let lvl = bits.read_bits(8).ok()? as u8;
                if b0 != b'B' || b1 != b'Z' || b2 != b'h' || !(b'1'..=b'9').contains(&lvl) {
                    return None;
                }
                Some((lvl - b'0') as usize * 100_000)
            })();
            match header {
                Some(size) => {
                    block_size = size;
                    combined_crc = 0;
                    at_stream_start = false;
                    bit = bits.bit;
                }
                None => {
                    // Malformed header: let the serial decoder name the error.
                    segments.push(Segment::Serial(fall_back(bit, Phase::StreamStart, 0, 0)?));
                    return Ok((stitch(segments), accepted));
                }
            }
            continue;
        }

        let magic = match bits.read_magic() {
            Ok(m) => m,
            Err(_) => {
                segments.push(Segment::Serial(fall_back(
                    bit,
                    Phase::Block,
                    block_size,
                    combined_crc,
                )?));
                return Ok((stitch(segments), accepted));
            }
        };

        if magic == BLOCK_MAGIC {
            // Accept only a candidate that decoded and that the *declared* block size
            // also admits — serial would have rejected a longer one with BlockOverflow,
            // and the block-size bound is the only way the declared level can change
            // what `decode_block` does.
            match blocks.remove(&bit) {
                Some(d) if d.nblock <= block_size => {
                    combined_crc = combined_crc.rotate_left(1) ^ d.crc;
                    bit = d.end_bit;
                    accepted += 1;
                    segments.push(Segment::Fast(d));
                }
                _ => {
                    segments.push(Segment::Serial(fall_back(
                        bit,
                        Phase::Block,
                        block_size,
                        combined_crc,
                    )?));
                    return Ok((stitch(segments), accepted));
                }
            }
        } else if magic == EOS_MAGIC {
            let stored = bits.read_bits(32);
            if stored != Ok(combined_crc) {
                segments.push(Segment::Serial(fall_back(
                    bit,
                    Phase::Block,
                    block_size,
                    combined_crc,
                )?));
                return Ok((stitch(segments), accepted));
            }
            at_stream_start = true;
            bit = bits.bit;
        } else {
            segments.push(Segment::Serial(fall_back(
                bit,
                Phase::Block,
                block_size,
                combined_crc,
            )?));
            return Ok((stitch(segments), accepted));
        }
    }
}

/// The plaintext, plus how many blocks came from the thread pool rather than from a
/// serial fallback.
pub(crate) fn decode(input: &[u8]) -> Result<(Vec<u8>, usize), Error> {
    let candidates = scan_candidates(input);

    // A real block header is well over a hundred bits, so a legitimate stream has
    // orders of magnitude fewer candidates than this. An input deliberately stuffed
    // with the magic would otherwise buy unbounded speculative work; serial decode of
    // it is both correct and cheaper.
    if candidates.is_empty() || candidates.len() > input.len() / 16 + 64 {
        return Ok((crate::decompress_to_vec(input)?, 0));
    }

    // Per-worker scratch that outlives this call: pool threads persist, so the
    // multi-megabyte BWT buffers stay allocated (and their pages stay faulted in)
    // across blocks *and* across `decompress_parallel` calls.
    std::thread_local! {
        static SCRATCH: core::cell::RefCell<BlockDecoder> =
            core::cell::RefCell::new(BlockDecoder::new());
    }
    let decoded: Vec<Option<Decoded>> = candidates
        .par_iter()
        .map(|&start| SCRATCH.with(|d| speculate(&mut d.borrow_mut(), input, start)))
        .collect();

    let blocks: BTreeMap<usize, Decoded> = candidates
        .iter()
        .zip(decoded)
        .filter_map(|(&start, d)| d.map(|d| (start, d)))
        .collect();

    assemble(input, blocks)
}

/// Decompress an entire in-memory `.bz2` buffer using a thread pool.
///
/// Output is byte-identical to [`decompress`](crate::decompress) for every input,
/// valid or not, and the same errors are reported for the same reasons; only the
/// speed differs. Blocks are found by scanning for the block magic at every bit
/// offset and decoding candidates speculatively, so the win scales with the number
/// of blocks: a single-block stream (anything under the level's block size — 900 KB
/// of input at level 9) has nothing to parallelize and costs the same as serial.
///
/// `threads` selects the pool: `None` uses rayon's global pool (one worker per core),
/// `Some(1)` decodes serially on the calling thread, and `Some(n)` uses a private
/// pool of `n` threads. Private pools are built once per distinct `n` and cached for
/// the life of the process, so repeated calls do not pay thread spawn-up again.
///
/// Peak memory is the whole plaintext, as with `decompress`, plus the blocks decoded
/// ahead of the chain.
///
/// ```
/// # const HELLO: &[u8] = &[
/// #     0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x71, 0x1c, 0x50, 0xc0, 0x00,
/// #     0x00, 0x03, 0xd9, 0x80, 0x00, 0x10, 0x40, 0x00, 0x10, 0x00, 0x3a, 0x44, 0x90, 0x10, 0x20,
/// #     0x00, 0x31, 0x03, 0x40, 0xd0, 0x29, 0x80, 0x1e, 0xa2, 0xe0, 0x4c, 0xed, 0x69, 0xe0, 0xe1,
/// #     0x77, 0x24, 0x53, 0x85, 0x09, 0x07, 0x11, 0xc5, 0x0c, 0x00,
/// # ];
/// let data = crabz2::decompress_parallel(HELLO, Some(4))?;
/// assert_eq!(data, b"hello crabz2\n");
/// # Ok::<(), std::io::Error>(())
/// ```
pub fn decompress_parallel(compressed: &[u8], threads: Option<usize>) -> io::Result<Vec<u8>> {
    match threads {
        Some(1) => crate::decompress(compressed),
        None | Some(0) => Ok(decode(compressed)?.0),
        Some(n) => {
            let pool = pool_for(n).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            Ok(pool.install(|| decode(compressed))?.0)
        }
    }
}

/// The cached private pool for `n` threads, built on first use. `Mutex<Option<..>>`
/// rather than `OnceLock` to hold the crate's 1.63 MSRV.
fn pool_for(n: usize) -> Result<Arc<rayon::ThreadPool>, rayon::ThreadPoolBuildError> {
    static POOLS: Mutex<Option<BTreeMap<usize, Arc<rayon::ThreadPool>>>> = Mutex::new(None);
    let mut guard = POOLS.lock().unwrap_or_else(|e| e.into_inner());
    let pools = guard.get_or_insert_with(BTreeMap::new);
    if let Some(pool) = pools.get(&n) {
        return Ok(Arc::clone(pool));
    }
    let pool = Arc::new(rayon::ThreadPoolBuilder::new().num_threads(n).build()?);
    pools.insert(n, Arc::clone(&pool));
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scanner must find a magic at every bit alignment, including one that
    /// straddles the byte boundaries of a shifted stream.
    #[test]
    fn scanner_finds_magic_at_every_bit_offset() {
        for shift in 0..8u32 {
            // 96 bits: 8 zero bits of lead-in, the magic at `shift`, zeros after.
            let mut bytes = [0u8; 16];
            let bit = 8 + shift as usize;
            for i in 0..48 {
                if (BLOCK_MAGIC >> (47 - i)) & 1 == 1 {
                    let p = bit + i as usize;
                    bytes[p >> 3] |= 0x80 >> (p & 7);
                }
            }
            let found = scan_candidates(&bytes);
            assert!(
                found.contains(&bit),
                "magic at bit {bit} (shift {shift}) not found: {found:?}"
            );
        }
    }

    /// The prefix filter is an optimization, so it must never be the reason a magic
    /// is missed: the byte at a match's own offset always passes it.
    #[test]
    fn prefix_filter_admits_every_real_start() {
        for k in 0..8u32 {
            let first = ((BLOCK_MAGIC >> (40 + k)) as u8) & (((1u16 << (8 - k)) - 1) as u8);
            for high in 0..(1u16 << k) {
                let b = ((high << (8 - k)) as u8) | first;
                assert!(PREFIX[b as usize] & (1 << k) != 0, "byte {b:#04x}, k {k}");
            }
        }
    }

    #[test]
    fn scanner_reports_no_candidate_in_empty_or_tiny_input() {
        assert!(scan_candidates(&[]).is_empty());
        assert!(scan_candidates(&[0x31, 0x41, 0x59]).is_empty());
    }
}
