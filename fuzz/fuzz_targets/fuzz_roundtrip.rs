#![no_main]

//! Arbitrary bytes through the encoder and back.
//!
//! The first input byte picks the compression level (`% 9 + 1`) so the fuzzer
//! explores all nine block sizes, and the rest is the plaintext. Three things
//! are checked:
//!
//! 1. **The encoder never panics.** No plaintext, however degenerate, may abort
//!    the compressor.
//! 2. **Round trip is lossless.** `decompress(compress(data, level)) == data`,
//!    exactly, for every level.
//! 3. **Chunked writing is equivalent.** Feeding the same plaintext to
//!    `Crabz2Writer` in arbitrary chunk sizes must produce byte-identical output
//!    to one-shot `compress`. The RLE1 splitter carries run state across `push`
//!    calls, so a block or run boundary landing mid-chunk is exactly where a
//!    streaming encoder goes wrong.
//!
//! The corresponding decode-side invariants (never panic, bounded allocation,
//! clean errors on malformed input) live in `fuzz_decompress`.

use libfuzzer_sys::fuzz_target;
use std::io::Write;

use crabz2::{compress, Crabz2Writer, Level};

/// Split `data` into chunks whose sizes are themselves driven by the data, so
/// the fuzzer can steer where the boundaries fall rather than always writing
/// one big slice.
fn chunked_compress(data: &[u8], level: Level) -> Vec<u8> {
    let mut w = Crabz2Writer::new(Vec::new(), level);
    let mut rest = data;
    let mut i = 0usize;
    while !rest.is_empty() {
        // Chunk sizes cycle through the plaintext itself: mostly small, with
        // the occasional zero-length write, which must also be harmless.
        let n = usize::from(data[i % data.len()]).min(rest.len());
        let (head, tail) = rest.split_at(n);
        w.write_all(head).expect("writing to a Vec cannot fail");
        rest = tail;
        i += 1;
        if n == 0 {
            // A zero-length chunk makes no progress; take one byte so the loop
            // terminates, after having exercised the empty write.
            let (head, tail) = rest.split_at(1);
            w.write_all(head).expect("writing to a Vec cannot fail");
            rest = tail;
        }
    }
    w.finish().expect("writing to a Vec cannot fail")
}

fuzz_target!(|data: &[u8]| {
    // First byte selects the level; the remainder is the payload.
    let (level, payload) = match data.split_first() {
        Some((&sel, rest)) => (Level::new(sel % 9 + 1).unwrap(), rest),
        None => (Level::DEFAULT, &data[..]),
    };

    let packed = compress(payload, level);

    // The stream must declare the level we asked for.
    assert_eq!(
        packed.get(3).copied(),
        Some(b'0' + level.get()),
        "compressed stream does not declare level {}",
        level.get(),
    );

    let unpacked = crabz2::decompress(&packed).unwrap_or_else(|e| {
        panic!(
            "our own decoder rejected our own output at level {}: {e}",
            level.get(),
        )
    });

    assert_eq!(
        unpacked,
        payload,
        "round trip changed the data at level {} ({} plaintext bytes -> {} compressed)",
        level.get(),
        payload.len(),
        packed.len(),
    );

    let streamed = chunked_compress(payload, level);
    assert_eq!(
        streamed,
        packed,
        "chunked Crabz2Writer output differs from one-shot compress at level {}",
        level.get(),
    );
});
