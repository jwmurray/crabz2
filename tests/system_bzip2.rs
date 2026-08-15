//! Integration tests against the system `bzip2` binary.
//!
//! Round-tripping through our own decoder proves the two halves agree with each
//! other; only the reference implementation proves we actually speak bzip2.
//! Every test here skips with a message when `bzip2` is not installed.

use std::io::Write;
use std::process::{Command, Stdio};

use crabz2::{compress, decompress_to_vec, Level};

/// `true` if a usable `bzip2` is on `PATH`.
fn have_bzip2() -> bool {
    Command::new("bzip2")
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success() || s.code() == Some(1)) // bzip2 --help exits 1
        .unwrap_or(false)
}

macro_rules! require_bzip2 {
    () => {
        if !have_bzip2() {
            eprintln!("skipping: system `bzip2` not found on PATH");
            return;
        }
    };
}

/// Pipe `input` through `bzip2 <args>` and return stdout.
fn run_bzip2(args: &[&str], input: &[u8]) -> Vec<u8> {
    let mut child = Command::new("bzip2")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn bzip2");

    // Write on a worker so a large input cannot deadlock against the pipe
    // buffer while we wait for output.
    let mut stdin = child.stdin.take().expect("bzip2 stdin");
    let owned = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&owned);
    });

    let out = child.wait_with_output().expect("bzip2 did not run");
    writer.join().expect("stdin writer panicked");
    assert!(
        out.status.success(),
        "bzip2 {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Compress with crabz2, decompress with the reference binary, compare.
fn check_against_bzip2(data: &[u8], level: Level, what: &str) {
    let packed = compress(data, level);
    let got = run_bzip2(&["-dc"], &packed);
    assert!(
        got == data,
        "{what} at level {}: system bzip2 decoded {} bytes, expected {}",
        level.get(),
        got.len(),
        data.len()
    );
}

#[test]
fn system_bzip2_decodes_our_output() {
    require_bzip2!();

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one byte", b"x".to_vec()),
        ("hello", b"hello crabz2\n".to_vec()),
        ("short run", vec![b'a'; 4]),
        ("run of 259", vec![b'a'; 259]),
        ("run of 260", vec![b'a'; 260]),
        ("all byte values", (0..=255u8).collect()),
        (
            "text",
            std::iter::repeat(&b"the quick brown fox jumps over the lazy dog\n"[..])
                .take(2000)
                .flatten()
                .copied()
                .collect(),
        ),
        ("one byte repeated", vec![0x5au8; 500_000]),
    ];

    for (what, data) in &cases {
        for l in [1u8, 5, 9] {
            check_against_bzip2(data, Level::new(l).unwrap(), what);
        }
    }
}

#[test]
fn system_bzip2_decodes_our_multi_block_output() {
    require_bzip2!();

    // Pseudo-random, so blocks are genuinely full and levels 1 and 9 differ in
    // block count by 9x.
    let mut state = 0x9E37_79B9u32;
    let data: Vec<u8> = (0..1_500_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        })
        .collect();

    check_against_bzip2(&data, Level::FASTEST, "multi-block random");
    check_against_bzip2(&data, Level::BEST, "multi-block random");
}

#[test]
fn system_bzip2_verifies_our_crcs() {
    require_bzip2!();

    // `bzip2 -t` checks both the per-block and combined-stream CRCs and exits
    // non-zero on any mismatch, so this is a direct check of our CRC chain.
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 200) as u8).collect();
    for l in 1..=9u8 {
        let packed = compress(&data, Level::new(l).unwrap());
        let status = Command::new("bzip2")
            .arg("-t")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.take().unwrap().write_all(&packed)?;
                child.wait_with_output()
            })
            .expect("bzip2 -t did not run");
        assert!(
            status.status.success(),
            "bzip2 -t rejected our level-{l} stream: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }
}

#[test]
fn we_decode_what_system_bzip2_produces() {
    require_bzip2!();

    // The other direction, level by level — the decoder already had coverage,
    // but this pins the pairing across the whole level range.
    let data: Vec<u8> = std::iter::repeat(&b"crabz2 interoperability check\n"[..])
        .take(5000)
        .flatten()
        .copied()
        .collect();
    for l in 1..=9u8 {
        let packed = run_bzip2(&[&format!("-{l}"), "-c"], &data);
        assert_eq!(decompress_to_vec(&packed).unwrap(), data, "level {l}");
    }
}

/// A representative text corpus for the ratio check.
fn ratio_corpus() -> Vec<u8> {
    let paragraphs: [&str; 6] = [
        "It is a truth universally acknowledged, that a single man in possession of a good \
         fortune, must be in want of a wife.\n",
        "However little known the feelings or views of such a man may be on his first entering \
         a neighbourhood, this truth is so well fixed in the minds of the surrounding families, \
         that he is considered the rightful property of some one or other of their daughters.\n",
        "\"My dear Mr. Bennet,\" said his lady to him one day, \"have you heard that Netherfield \
         Park is let at last?\"\n",
        "Mr. Bennet replied that he had not.\n",
        "\"But it is,\" returned she; \"for Mrs. Long has just been here, and she told me all \
         about it.\"\n",
        "Mr. Bennet made no answer.\n",
    ];
    let mut out = Vec::new();
    let mut i = 0usize;
    while out.len() < 1_000_000 {
        out.extend_from_slice(paragraphs[i % paragraphs.len()].as_bytes());
        // Vary it a little so this is not just one repeated block.
        out.extend_from_slice(format!("[{}]\n", i * 7919 % 100_003).as_bytes());
        i += 1;
    }
    out
}

/// The compressed-size envelope: our output must stay within 15% of what
/// `bzip2 -9` produces on representative text.
#[test]
fn compressed_size_is_within_fifteen_percent_of_libbz2() {
    require_bzip2!();

    let data = ratio_corpus();
    let ours = compress(&data, Level::BEST).len();
    let theirs = run_bzip2(&["-9", "-c"], &data).len();

    let overhead = (ours as f64 - theirs as f64) / theirs as f64 * 100.0;
    eprintln!(
        "ratio check: {} bytes in -> crabz2 {ours}, bzip2 -9 {theirs} ({overhead:+.2}%)",
        data.len()
    );

    assert!(
        ours as f64 <= theirs as f64 * 1.15,
        "crabz2 -9 produced {ours} bytes vs bzip2 -9 {theirs} ({overhead:+.2}%), outside the \
         stated 15% envelope"
    );
}

/// Same envelope on non-text data, where the Huffman stage carries more of the
/// work and any table-selection weakness would show up.
#[test]
fn compressed_size_envelope_holds_on_binary_data() {
    require_bzip2!();

    let mut state = 0x1234_5678u32;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state
    };
    // Structured binary: mostly small values with occasional spikes.
    let data: Vec<u8> = (0..800_000)
        .map(|_| {
            let r = next();
            if r % 32 == 0 {
                (r >> 16) as u8
            } else {
                (r % 12) as u8
            }
        })
        .collect();

    for l in [1u8, 9] {
        let ours = compress(&data, Level::new(l).unwrap()).len();
        let theirs = run_bzip2(&[&format!("-{l}"), "-c"], &data).len();
        let overhead = (ours as f64 - theirs as f64) / theirs as f64 * 100.0;
        eprintln!("ratio check (binary, -{l}): crabz2 {ours}, bzip2 {theirs} ({overhead:+.2}%)");
        assert!(
            ours as f64 <= theirs as f64 * 1.15,
            "level {l}: crabz2 {ours} vs bzip2 {theirs} ({overhead:+.2}%), outside the 15% envelope"
        );
    }
}
