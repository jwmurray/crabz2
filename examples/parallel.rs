//! Decompress a `.bz2` file with the parallel block decoder, reporting throughput.
//!
//! ```text
//! cargo run --release --features parallel --example parallel -- corpus.csv.bz2 8 > /dev/null
//! ```
//!
//! The plaintext goes to stdout; the timing line goes to stderr. Omit the thread
//! count to use one worker per core.

use std::io::Write;
use std::time::Instant;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = match args.next() {
        Some(p) => p,
        None => {
            eprintln!("usage: parallel <file.bz2> [threads]");
            std::process::exit(2);
        }
    };
    let threads = args
        .next()
        .and_then(|t| t.to_str().and_then(|t| t.parse::<usize>().ok()));

    let compressed = std::fs::read(&path)?;
    let start = Instant::now();
    let plain = crabz2::decompress_parallel(&compressed, threads)?;
    let elapsed = start.elapsed();

    eprintln!(
        "{} -> {} bytes in {:.3}s ({:.1} MB/s of plaintext, {} threads)",
        compressed.len(),
        plain.len(),
        elapsed.as_secs_f64(),
        plain.len() as f64 / elapsed.as_secs_f64() / 1e6,
        threads.map_or_else(|| "all".to_string(), |t| t.to_string()),
    );

    std::io::stdout().write_all(&plain)
}
