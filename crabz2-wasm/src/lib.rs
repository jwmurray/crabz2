//! WebAssembly bindings for [`crabz2`] — pure-Rust bzip2 decompression, no C and no
//! `libbz2`, running in the browser or in Node.
//!
//! Two entry points:
//!
//! - [`decompress`] for a buffer you already hold entirely in memory;
//! - [`Bz2Decoder`], a push-based streaming decoder for files too large to hold
//!   twice, which drives the core's sans-io state machine one block at a time.

use crabz2::{BlockDecoder, Error, Step};
use wasm_bindgen::prelude::*;

/// Everything below the binding layer works in `Result<_, String>` and the bindings
/// turn that into a thrown JS `Error`. Keeping `JsError` out of the logic is what
/// lets the whole decoder be unit-tested on the host, where a `JsError` cannot be
/// constructed at all.
fn js_err(message: String) -> JsError {
    JsError::new(&message)
}

/// Decompress a whole `.bz2` buffer.
///
/// Throws on malformed input, a CRC mismatch, or truncation. Concatenated
/// (multi-stream) input is handled, matching `bzip2 -dc`.
///
/// ```js
/// const plain = decompress(new Uint8Array(await file.arrayBuffer()));
/// ```
#[wasm_bindgen]
pub fn decompress(input: &[u8]) -> Result<Vec<u8>, JsError> {
    crabz2::decompress_to_vec(input).map_err(|e| js_err(e.to_string()))
}

/// Smallest amount of new input that can justify re-attempting a block that came up
/// short, and the threshold used before any block has completed. See [`Bz2Decoder`]
/// for why re-attempts are the unit of work.
const RETRY_STEP: usize = 64 * 1024;

/// Added to the previous block's compressed size when predicting the next one's, so
/// a block that came out slightly larger than its predecessor does not cost an extra
/// attempt. Blocks vary by far less than this in practice.
const BLOCK_SLACK: usize = 4096;

/// A push-based streaming bzip2 decoder.
///
/// Feed it compressed bytes with `push()` — in whatever sizes the source hands them
/// over, a `ReadableStream` reader's chunks or 4 KiB slices — and each call returns
/// the plaintext of whatever blocks completed, usually empty. Call `finish()` once
/// the input is exhausted; it returns the last block(s) and verifies that the stream
/// ended cleanly.
///
/// ```js
/// const dec = new Bz2Decoder();
/// for await (const chunk of stream) out.push(dec.push(chunk));
/// out.push(dec.finish());
/// ```
///
/// **Memory.**
///
/// The decoder holds a block, not the file: compressed bytes are dropped as soon as
/// the state machine commits past them, and plaintext is handed to JS as it is
/// produced rather than accumulated. Peak Rust-side memory is a small multiple of
/// the block size — one decompressed block plus one compressed block, or two when a
/// block turns out not to compress at all — no matter how large the input. What the
/// caller does with the returned chunks is the caller's business.
///
/// **Why work is batched.**
///
/// The core is sans-io and restarts rather than resumes: when a block runs out of
/// input it reports truncation and rewinds to the last committed boundary, so the
/// block is decoded again from its start once the remaining bytes arrive. That is
/// what keeps the core free of any suspended-state machinery — but it means a
/// decoder that re-attempted on every 1 KiB `push` would re-decode the same block
/// hundreds of times, work quadratic in the block size.
///
/// So an attempt that comes up short arms a threshold, and pushes below it only
/// buffer. The threshold is an estimate of where the block in flight ends, and the
/// estimate is the size of the last block that did complete plus a little slack:
/// blocks in a stream come from one encoder at one level over one file, so their
/// compressed sizes cluster tightly, and the common case is exactly one decode
/// attempt per block. Before any block has completed there is nothing to estimate
/// from, so the first threshold is 64 KiB.
///
/// When the estimate is short — the first block, or a block that compresses worse
/// than the one before it — the threshold falls back to growing geometrically, by at
/// least 64 KiB and by a doubling once the buffer is larger than that. That bounds
/// the wasted work at a small constant multiple of a single pass however badly the
/// estimate is behaving, which is what keeps a hostile stream linear. A completed
/// block clears the threshold, so blocks already sitting in the buffer are drained
/// back to back, and `finish()` always makes a final attempt regardless.
#[wasm_bindgen]
pub struct Bz2Decoder {
    dec: BlockDecoder,
    /// Compressed bytes the state machine has not committed past yet.
    inbuf: Vec<u8>,
    /// Buffer length at which the next decode attempt becomes worthwhile.
    next_try: usize,
    /// Compressed size of the last block that decoded, which is the estimate for the
    /// next one. Zero until a block completes.
    last_block_bytes: usize,
    bytes_in: usize,
    bytes_out: usize,
    /// A terminal error was reported; the decoder will not be driven again.
    poisoned: bool,
    finished: bool,
}

impl Default for Bz2Decoder {
    fn default() -> Self {
        Bz2Decoder::new()
    }
}

#[wasm_bindgen]
impl Bz2Decoder {
    /// A decoder positioned at the start of a stream.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Bz2Decoder {
        Bz2Decoder {
            dec: BlockDecoder::new(),
            inbuf: Vec::new(),
            next_try: 0,
            last_block_bytes: 0,
            bytes_in: 0,
            bytes_out: 0,
            poisoned: false,
            finished: false,
        }
    }

    /// Feed compressed bytes; get back the plaintext of every block that completed.
    ///
    /// Returns an empty array whenever the block in progress is still short of
    /// input, which is the common case for chunks smaller than a block. Throws on
    /// malformed input — the decoder is spent after that and every later call throws
    /// too.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, JsError> {
        self.push_bytes(chunk).map_err(js_err)
    }

    /// Signal end of input: returns the remaining plaintext and verifies that the
    /// stream ended where it said it would.
    ///
    /// Throws if the input stopped mid-stream (`unexpected end of bzip2 stream`) or
    /// if a final CRC does not match. Calling it twice returns nothing the second
    /// time.
    pub fn finish(&mut self) -> Result<Vec<u8>, JsError> {
        self.finish_bytes().map_err(js_err)
    }

    /// Compressed bytes accepted so far.
    #[wasm_bindgen(getter, js_name = bytesIn)]
    pub fn bytes_in(&self) -> f64 {
        self.bytes_in as f64
    }

    /// Plaintext bytes produced so far.
    #[wasm_bindgen(getter, js_name = bytesOut)]
    pub fn bytes_out(&self) -> f64 {
        self.bytes_out as f64
    }

    /// Compressed bytes buffered but not yet decoded — the decoder's live footprint
    /// beyond its block scratch. Useful when watching that streaming really is
    /// streaming.
    #[wasm_bindgen(getter, js_name = bytesBuffered)]
    pub fn bytes_buffered(&self) -> f64 {
        self.inbuf.len() as f64
    }
}

impl Bz2Decoder {
    /// `push` without the binding layer.
    fn push_bytes(&mut self, chunk: &[u8]) -> Result<Vec<u8>, String> {
        if self.finished {
            return Err("push() after finish()".into());
        }
        self.check_usable()?;
        self.inbuf.extend_from_slice(chunk);
        self.bytes_in += chunk.len();
        self.pump(false)
    }

    /// `finish` without the binding layer.
    fn finish_bytes(&mut self) -> Result<Vec<u8>, String> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.check_usable()?;
        let out = self.pump(true)?;
        self.finished = true;
        // Nothing more will be decoded; give the block scratch back now rather than
        // at whatever point JS gets around to dropping the object.
        self.inbuf = Vec::new();
        self.dec = BlockDecoder::new();
        Ok(out)
    }

    fn check_usable(&self) -> Result<(), String> {
        if self.poisoned {
            return Err("decoder already failed on invalid input".into());
        }
        Ok(())
    }

    /// Drive the state machine as far as the buffered input allows.
    ///
    /// With `last` set the input is known to be complete, so every attempt is made
    /// regardless of the retry threshold and a short read is a real truncation.
    fn pump(&mut self, last: bool) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        loop {
            if !last && self.inbuf.len() < self.next_try {
                break;
            }
            match self.dec.next_block(&self.inbuf, &mut out) {
                Ok(Step::Block) => {
                    // What this block cost is the estimate for the next one.
                    self.last_block_bytes = self.dec.consumed();
                    self.compact();
                    // A completed block may be followed by another already in hand.
                    self.next_try = 0;
                }
                Ok(Step::Eof) => {
                    // Every buffered byte is consumed at a clean stream boundary.
                    // Done if the input is complete; otherwise a concatenated stream
                    // may still follow.
                    self.compact();
                    self.next_try = 0;
                    break;
                }
                Err(Error::Truncated) if !last => {
                    self.arm_retry();
                    break;
                }
                Err(e) => {
                    self.poisoned = true;
                    return Err(e.to_string());
                }
            }
        }
        self.bytes_out += out.len();
        Ok(out)
    }

    /// Decide how much more input has to arrive before the next decode attempt.
    ///
    /// The buffer is rebased to the last committed boundary, so its length is the
    /// bytes seen so far of the block in flight. Aim at where the previous block
    /// ended; short of that, wait. Once the buffer is past the estimate the estimate
    /// was wrong, so grow the threshold geometrically instead — by at least 64 KiB,
    /// and by a doubling once the buffer is larger than that.
    fn arm_retry(&mut self) {
        let n = self.inbuf.len();
        let estimate = if self.last_block_bytes > 0 {
            self.last_block_bytes + BLOCK_SLACK
        } else {
            RETRY_STEP
        };
        self.next_try = if n < estimate {
            estimate
        } else {
            n + n.max(RETRY_STEP)
        };
    }

    /// Drop the compressed bytes the decoder has committed past.
    fn compact(&mut self) {
        let n = self.dec.consumed();
        if n > 0 {
            self.inbuf.drain(..n);
            self.dec.rebase(n);
        }
    }
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

    /// Drive the streaming decoder with a fixed chunk size, as JS would.
    fn stream(input: &[u8], chunk: usize) -> Result<Vec<u8>, String> {
        let mut dec = Bz2Decoder::new();
        let mut out = Vec::new();
        for part in input.chunks(chunk) {
            out.extend_from_slice(&dec.push_bytes(part)?);
        }
        out.extend_from_slice(&dec.finish_bytes()?);
        Ok(out)
    }

    #[test]
    fn buffer_api_round_trips() {
        assert_eq!(
            crabz2::decompress_to_vec(HELLO_BZ2).unwrap(),
            b"hello crabz2\n"
        );
    }

    #[test]
    fn streaming_matches_buffer_api_at_every_chunk_size() {
        for chunk in [1, 2, 3, 7, 16, 53, 4096] {
            assert_eq!(
                stream(HELLO_BZ2, chunk).unwrap(),
                b"hello crabz2\n",
                "chunk size {chunk}"
            );
        }
    }

    #[test]
    fn streaming_handles_concatenated_streams() {
        let mut cat = Vec::new();
        cat.extend_from_slice(HELLO_BZ2);
        cat.extend_from_slice(HELLO_BZ2);
        assert_eq!(stream(&cat, 5).unwrap(), b"hello crabz2\nhello crabz2\n");
    }

    #[test]
    fn truncation_is_reported_at_finish() {
        assert!(stream(&HELLO_BZ2[..HELLO_BZ2.len() - 4], 8).is_err());
    }

    #[test]
    fn corruption_is_reported() {
        let mut bad = HELLO_BZ2.to_vec();
        let n = bad.len();
        bad[n - 6] ^= 0x01;
        assert!(stream(&bad, 8).is_err());
        assert!(crabz2::decompress_to_vec(&bad).is_err());
    }

    #[test]
    fn a_failed_decoder_stays_failed() {
        let mut bad = HELLO_BZ2.to_vec();
        bad[0] = b'X';
        let mut dec = Bz2Decoder::new();
        assert!(dec.push_bytes(&bad).is_err());
        assert_eq!(
            dec.push_bytes(&bad).unwrap_err(),
            "decoder already failed on invalid input"
        );
        assert!(dec.finish_bytes().is_err());
    }

    // Buffering tracks the block in flight, never the file. These fixtures are far
    // smaller than one block, so the real stress of this invariant is the node smoke
    // test, which streams multi-megabyte `bzip2` output and asserts the same bound
    // against a file many times larger.
    #[test]
    fn buffering_is_bounded_by_the_block_in_flight() {
        let mut cat = Vec::new();
        for _ in 0..64 {
            cat.extend_from_slice(HELLO_BZ2);
        }
        let mut dec = Bz2Decoder::new();
        let mut out = Vec::new();
        for part in cat.chunks(1) {
            out.extend_from_slice(&dec.push_bytes(part).unwrap());
            assert!(
                dec.bytes_buffered() <= (RETRY_STEP + 1) as f64,
                "buffered {} bytes after {} in",
                dec.bytes_buffered(),
                dec.bytes_in()
            );
        }
        out.extend_from_slice(&dec.finish_bytes().unwrap());
        assert_eq!(out.len(), 64 * b"hello crabz2\n".len());
        assert_eq!(dec.bytes_out(), out.len() as f64);
        assert_eq!(dec.bytes_in(), cat.len() as f64);
    }

    // Re-attempts are rationed: a byte-at-a-time source must not provoke a decode
    // attempt per byte. The threshold is observable, so the policy can be asserted
    // rather than described.
    #[test]
    fn retries_are_rationed_then_predicted() {
        let mut dec = Bz2Decoder::new();
        let mut armed = Vec::new();
        for part in HELLO_BZ2.chunks(1) {
            let before = dec.next_try;
            dec.push_bytes(part).unwrap();
            if dec.next_try != before {
                armed.push(dec.next_try);
            }
        }
        // One decode attempt for the whole 55-byte stream, not fifty-five, and
        // nothing to predict from yet, so the threshold is the flat first step.
        assert_eq!(armed, [RETRY_STEP]);
        assert_eq!(dec.finish_bytes().unwrap(), b"hello crabz2\n");

        // Once a block has decoded, its size is the estimate for the next one — so a
        // well-compressed stream is not made to wait for 64 KiB it will never see.
        let mut cat = Vec::new();
        for _ in 0..4 {
            cat.extend_from_slice(HELLO_BZ2);
        }
        let mut dec = Bz2Decoder::new();
        // Stop short of the last stream's end so the pump arms rather than reporting
        // a clean end of input.
        dec.push_bytes(&cat[..cat.len() - 5]).unwrap();
        // One stream's worth of bytes — its block plus the framing around it — not
        // the flat 64 KiB the decoder started with.
        assert!(
            (1..=HELLO_BZ2.len()).contains(&dec.last_block_bytes),
            "estimate {} should be about one {}-byte stream",
            dec.last_block_bytes,
            HELLO_BZ2.len()
        );
        assert_eq!(dec.next_try, dec.last_block_bytes + BLOCK_SLACK);
        assert!(dec.next_try < RETRY_STEP);
    }
}
