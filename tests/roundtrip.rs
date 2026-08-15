//! Property tests for the encoder: whatever we compress, our own decoder must
//! give back byte for byte.

use crabz2::{compress, decompress_to_vec, Level};

/// Deterministic xorshift, so a failure is always reproducible.
struct Rng(u32);

impl Rng {
    fn new(seed: u32) -> Rng {
        Rng(seed | 1)
    }

    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
}

fn check(data: &[u8], level: Level, what: &str) {
    let packed = compress(data, level);
    match decompress_to_vec(&packed) {
        Ok(got) => assert!(
            got == data,
            "{} at level {}: round trip mismatch ({} bytes in, {} bytes out)",
            what,
            level.get(),
            data.len(),
            got.len()
        ),
        Err(e) => panic!(
            "{} at level {}: our decoder rejected our own output: {e}",
            what,
            level.get()
        ),
    }
}

fn all_levels(data: &[u8], what: &str) {
    for l in 1..=9u8 {
        check(data, Level::new(l).unwrap(), what);
    }
}

#[test]
fn empty_input() {
    all_levels(b"", "empty");
}

#[test]
fn tiny_inputs() {
    for n in 0..64usize {
        let data: Vec<u8> = (0..n).map(|i| b'a' + (i % 26) as u8).collect();
        all_levels(&data, "tiny ascending");
    }
}

#[test]
fn single_byte_values() {
    for b in [0u8, 1, 0x7f, 0x80, 0xff] {
        check(&[b], Level::BEST, "one byte");
        check(&[b; 2], Level::BEST, "two bytes");
        check(&[b; 3], Level::BEST, "three bytes");
    }
}

/// RLE1 switches behaviour at a run of four, and its count byte saturates at
/// 259, so walk every boundary in that neighbourhood.
#[test]
fn rle1_run_length_edges() {
    let interesting = [
        1usize, 2, 3, 4, 5, 6, 7, 8, 254, 255, 256, 257, 258, 259, 260, 261, 262, 263, 517, 518,
        519, 1000,
    ];
    for &n in &interesting {
        let data = vec![b'r'; n];
        check(&data, Level::FASTEST, "pure run");
        check(&data, Level::BEST, "pure run");

        // The same run with different bytes either side, so the run is not the
        // whole block.
        let mut framed = vec![b'<'];
        framed.extend(std::iter::repeat(b'r').take(n));
        framed.push(b'>');
        check(&framed, Level::BEST, "framed run");

        // Back-to-back runs of two different bytes.
        let mut pair: Vec<u8> = std::iter::repeat(b'x').take(n).collect();
        pair.extend(std::iter::repeat(b'y').take(n));
        check(&pair, Level::BEST, "adjacent runs");
    }
}

#[test]
fn every_byte_value_present() {
    let data: Vec<u8> = (0..=255u8).collect();
    all_levels(&data, "all 256 values");

    // And with every value repeated past the RLE1 threshold.
    let mut runs = Vec::new();
    for b in 0..=255u8 {
        runs.extend(std::iter::repeat(b).take(300));
    }
    all_levels(&runs, "all 256 values in long runs");
}

#[test]
fn all_one_byte_large() {
    // 1 MB of a single byte: multi-block at level 1, and the case where RLE1
    // shrinks the input by ~50x.
    let data = vec![0x5au8; 1024 * 1024];
    check(&data, Level::FASTEST, "1 MB of one byte");
    check(&data, Level::BEST, "1 MB of one byte");
}

#[test]
fn highly_repetitive() {
    let mut data = Vec::new();
    while data.len() < 400_000 {
        data.extend_from_slice(b"abcabcabcabcabcabc");
    }
    all_levels(&data, "repetitive");
}

#[test]
fn random_binary() {
    let mut rng = Rng::new(0xC0FFEE);
    let data: Vec<u8> = (0..300_000).map(|_| (rng.next() >> 24) as u8).collect();
    check(&data, Level::FASTEST, "random binary");
    check(&data, Level::BEST, "random binary");
}

#[test]
fn low_entropy_random() {
    // Random over a small alphabet — the case that stresses MTF and the
    // zero-run coder without being trivially compressible.
    let mut rng = Rng::new(0x1234);
    for alphabet in [2u32, 3, 5, 17] {
        let data: Vec<u8> = (0..50_000).map(|_| (rng.next() % alphabet) as u8).collect();
        check(&data, Level::FASTEST, "small alphabet");
        check(&data, Level::BEST, "small alphabet");
    }
}

#[test]
fn text() {
    let mut data = Vec::new();
    while data.len() < 500_000 {
        data.extend_from_slice(
            b"It is a truth universally acknowledged, that a single man in \
              possession of a good fortune, must be in want of a wife.\n",
        );
    }
    all_levels(&data, "text");
}

/// Sizes that land exactly on, just under and just over each level's block
/// boundary — where the encoder has to decide whether to start a new block.
#[test]
fn block_boundary_sizes() {
    for l in 1..=9u8 {
        let level = Level::new(l).unwrap();
        let block = level.block_size();
        // -19 is exactly the RLE1 limit, so -20/-19/-18 straddle it.
        for delta in [-20isize, -19, -18, -1, 0, 1, 20] {
            let n = (block as isize + delta) as usize;
            // Incompressible-ish content so RLE1 is a no-op and the boundary
            // really is where we think it is.
            let mut rng = Rng::new(l as u32 * 7919);
            let data: Vec<u8> = (0..n).map(|_| (rng.next() >> 24) as u8).collect();
            check(&data, level, "block boundary");
        }
    }
}

/// The same, but where RLE1 compresses hard, so a nominal-size block holds far
/// more plaintext than its byte count suggests.
#[test]
fn block_boundaries_with_long_runs() {
    for l in [1u8, 2, 9] {
        let level = Level::new(l).unwrap();
        // 259 raw bytes per 5 RLE1 bytes, so this spans several blocks.
        let n = level.block_size() / 5 * 259 + 137;
        let data: Vec<u8> = (0..n).map(|i| (i / 259 % 251) as u8).collect();
        check(&data, level, "run-heavy block boundary");
    }
}

#[test]
fn multi_block_at_level_one() {
    let mut rng = Rng::new(0xABCD);
    // Ten-ish blocks at 100 kB each.
    let data: Vec<u8> = (0..1_000_000).map(|_| (rng.next() >> 24) as u8).collect();
    check(&data, Level::FASTEST, "multi-block random");

    let mut text = Vec::new();
    while text.len() < 1_000_000 {
        text.extend_from_slice(b"the quick brown fox jumps over the lazy dog\n");
    }
    check(&text, Level::FASTEST, "multi-block text");
}

#[test]
fn structured_csv_like_data() {
    // The shape the crate's CourtListener example handles.
    let mut data = Vec::new();
    for i in 0..20_000u32 {
        data.extend_from_slice(
            format!("{},{},\"Opinion of the court\",{}\n", i, i * 3 + 1, i % 97).as_bytes(),
        );
    }
    check(&data, Level::FASTEST, "csv");
    check(&data, Level::BEST, "csv");
}

#[test]
#[cfg(feature = "std")]
fn writer_matches_compress_for_every_chunking() {
    use std::io::Write;

    let data: Vec<u8> = {
        let mut rng = Rng::new(0x5EED);
        (0..250_000).map(|_| (rng.next() % 11) as u8).collect()
    };
    let want = compress(&data, Level::FASTEST);

    for chunk in [1usize, 7, 1024, 65_536, data.len()] {
        let mut w = crabz2::writer(Vec::new(), Level::FASTEST);
        for part in data.chunks(chunk) {
            w.write_all(part).unwrap();
        }
        let got = w.finish().unwrap();
        assert_eq!(
            got, want,
            "chunk size {chunk} changed the output; the encoder must not depend on how input arrives"
        );
        assert_eq!(decompress_to_vec(&got).unwrap(), data);
    }
}

#[test]
fn concatenated_streams_still_decode() {
    // Our output must survive being concatenated, as `bzip2 -dc` allows.
    let a = compress(b"first stream\n", Level::BEST);
    let b = compress(b"second stream\n", Level::FASTEST);
    let mut both = a;
    both.extend_from_slice(&b);
    assert_eq!(
        decompress_to_vec(&both).unwrap(),
        b"first stream\nsecond stream\n"
    );
}

#[test]
fn output_is_self_describing() {
    let packed = compress(b"anything at all", Level::new(5).unwrap());
    assert_eq!(&packed[..4], b"BZh5");
}
