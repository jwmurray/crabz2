#![no_main]

//! Arbitrary bytes into the decoder.
//!
//! Three invariants are checked on every input:
//!
//! 1. **Never panics.** Any byte string is either decoded or rejected with an
//!    `io::Error`; nothing indexes out of range, overflows, or aborts.
//! 2. **Bounded allocation.** The block size declared in the stream header is
//!    the only thing that may size an allocation. A hostile header or a crafted
//!    run length must not be able to ask for more memory than the declared
//!    level structurally allows. This is asserted against a live allocation
//!    high-water mark, not merely against the size of the output.
//! 3. **The two APIs agree.** The streaming `reader` and the buffering
//!    `decompress` succeed and fail together, and produce the same byte count.
//!
//! The allocation bound is asserted around the *streaming* path, because that
//! is where the crate makes its memory claim ("peak memory ≈ one decompressed
//! block"). `decompress` buffers the whole plaintext by documented design, so
//! its footprint scales with the output, not with one block.

use libfuzzer_sys::fuzz_target;
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Read};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Bytes currently handed out by the Rust allocator, and the high-water mark
/// since the last reset. A fuzz target runs single-threaded, so relaxed
/// ordering is sufficient.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Tracking;

// `realloc` is deliberately left as the trait default (allocate, copy, free),
// so a growing `Vec` is charged for its old and its new buffer at the same
// time. That over-counts, which is the direction we want for a memory bound.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout);
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

/// Worst case RLE1 expansion: four literal bytes plus a repeat count of up to
/// 255 come out of every five bytes of BWT output, so 259/5 < 52.
const RLE1_MAX_EXPANSION: usize = 52;

/// Four bytes of BWT working buffer per block byte (`tt` is a `Vec<u32>`).
const BWT_BYTES_PER_ENTRY: usize = 4;

/// `Vec` grows geometrically and the default `realloc` above charges for both
/// buffers, so allow the ideal figure to be exceeded by a small constant.
const GROWTH_SLACK: usize = 3;

/// Fixed overhead: the compressed input the streaming adapter buffers (about one
/// block, itself bounded by the declared level), per-block Huffman tables,
/// selector and MTF vectors, and the copy buffer used to drain the stream.
const FIXED_OVERHEAD: usize = 2 << 20;

/// A runaway guard so one input cannot occupy the fuzzer indefinitely. Well
/// above a single fully expanded level-9 block (~47 MiB).
const MAX_PLAINTEXT: u64 = 128 << 20;

/// The largest block size any stream in `data` could declare.
///
/// Stream headers are read byte-aligned, so scanning every offset for `BZh<n>`
/// is a superset of the headers the decoder can actually consume — conservative
/// in the right direction. Concatenated streams may each declare their own
/// level, so take the maximum.
fn declared_block_size(data: &[u8]) -> usize {
    let mut level = 1u8;
    for w in data.windows(4) {
        if &w[..3] == b"BZh" && (b'1'..=b'9').contains(&w[3]) {
            level = level.max(w[3] - b'0');
        }
    }
    usize::from(level) * 100_000
}

/// Memory the decoder may structurally need for the declared level, and not one
/// byte more. Note that nothing here depends on the length of the input or on
/// any other header field: that is the property under test.
fn allocation_bound(data: &[u8]) -> usize {
    let block = declared_block_size(data);
    let per_block = block * BWT_BYTES_PER_ENTRY + block * RLE1_MAX_EXPANSION;
    per_block * GROWTH_SLACK + FIXED_OVERHEAD
}

/// Drain the streaming decoder, returning the plaintext length.
fn drain(data: &[u8]) -> io::Result<u64> {
    io::copy(
        &mut crabz2::reader(data).take(MAX_PLAINTEXT),
        &mut io::sink(),
    )
}

fuzz_target!(|data: &[u8]| {
    let bound = allocation_bound(data);

    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);

    let streamed = drain(data);

    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    assert!(
        peak <= bound,
        "streaming decode allocated {peak} bytes, above the {bound}-byte cap \
         implied by the declared block size of {} bytes",
        declared_block_size(data),
    );

    let buffered = crabz2::decompress(data);

    assert_eq!(
        streamed.is_ok(),
        buffered.is_ok(),
        "streaming and buffering decode disagree on whether this input is valid",
    );

    if let (Ok(n), Ok(v)) = (&streamed, &buffered) {
        if *n < MAX_PLAINTEXT {
            assert_eq!(
                *n as usize,
                v.len(),
                "streaming and buffering decode produced different lengths",
            );
        }
    }
});
