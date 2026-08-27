//! Compare `crabz2` (pure Rust) decompression to the C `libbz2` baseline via the
//! `bzip2` crate. Runs several iterations and reports MB/s for each implementation.
//!
//! Usage:
//! ```text
//! cargo run --release --example compare --features parallel -- corpus.csv.bz2 [iters] [threads]
//! ```
//! - `iters` defaults to 5. `threads` is optional and passed to `crabz2::decompress_parallel`.
use std::error::Error;
use std::io::Cursor;
use std::io::Read;
use std::time::Instant;

fn human_mb(bytes: usize) -> f64 {
    bytes as f64 / 1e6
}

fn run_crabz2(compressed: &[u8], threads: Option<usize>) -> Result<(usize, f64), Box<dyn Error>> {
    let start = Instant::now();
    let plain = crabz2::decompress_parallel(compressed, threads)?;
    let secs = start.elapsed().as_secs_f64();
    Ok((plain.len(), secs))
}

fn run_libbz2(compressed: &[u8]) -> Result<(usize, f64), Box<dyn Error>> {
    // bzip2::read::BzDecoder is a streaming wrapper around libbz2.
    let mut decoder = bzip2::read::BzDecoder::new(Cursor::new(compressed));
    let mut out = Vec::with_capacity(1024 * 1024);
    let start = Instant::now();
    decoder.read_to_end(&mut out)?;
    let secs = start.elapsed().as_secs_f64();
    Ok((out.len(), secs))
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let path = match args.next() {
        Some(p) => p,
        None => {
            eprintln!("usage: compare <file.bz2> [iters] [threads]");
            std::process::exit(2);
        }
    };
    let iters: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);
    let threads: Option<usize> = args.next().and_then(|s| s.parse().ok());

    eprintln!("loading {path} into memory...");
    let compressed = if let Some(gen) = path.strip_prefix("gen:") {
        // Generate a repetitive plaintext of approximately `gen` megabytes,
        // compress it with `crabz2::compress` and use the resulting compressed
        // bytes as the benchmark input. This avoids needing external files.
        let mb: usize = gen.parse().unwrap_or(10);
        eprintln!(
            "generating ~{} MB plaintext and compressing with crabz2...",
            mb
        );
        let mut plain = Vec::with_capacity(mb * 1_000_000);
        let sample = b"the quick brown fox jumps over the lazy dog\n";
        while plain.len() < mb * 1_000_000 {
            plain.extend_from_slice(sample);
        }
        let packed = crabz2::compress(&plain, crabz2::Level::BEST);
        eprintln!(
            "generated {} bytes plaintext -> {} bytes compressed",
            plain.len(),
            packed.len()
        );
        packed
    } else {
        std::fs::read(&path)?
    };
    eprintln!(
        "compressed {} bytes ({:.2} MB)",
        compressed.len(),
        human_mb(compressed.len())
    );

    // Warmup each implementation once.
    eprintln!("warmup: crabz2...");
    let (plain_len_c, secs_c) = run_crabz2(&compressed, threads)?;
    eprintln!("  crabz2 warmup: {} bytes in {:.3}s", plain_len_c, secs_c);

    eprintln!("warmup: libbz2...");
    let (plain_len_b, secs_b) = run_libbz2(&compressed)?;
    eprintln!("  libbz2 warmup: {} bytes in {:.3}s", plain_len_b, secs_b);

    if plain_len_b != plain_len_c {
        eprintln!(
            "warning: decompressed sizes differ (crabz2={} libbz2={})",
            plain_len_c, plain_len_b
        );
    }

    // Timed runs
    let mut crab_times = Vec::with_capacity(iters);
    let mut lib_times = Vec::with_capacity(iters);

    for i in 0..iters {
        eprint!("run {}/{} — crabz2: ", i + 1, iters);
        let (_len, secs) = run_crabz2(&compressed, threads)?;
        eprintln!("{:.3}s", secs);
        crab_times.push(secs);

        eprint!("run {}/{} — libbz2: ", i + 1, iters);
        let (_len, secs) = run_libbz2(&compressed)?;
        eprintln!("{:.3}s", secs);
        lib_times.push(secs);
    }

    // Median, not mean: scheduling noise only ever slows a run down, so the mean
    // is biased by outliers — equally for both implementations, but needlessly.
    let median = |times: &[f64]| -> f64 {
        let mut sorted = times.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        sorted[sorted.len() / 2]
    };
    let crab_avg: f64 = median(&crab_times);
    let lib_avg: f64 = median(&lib_times);

    let plain_mb = human_mb(plain_len_c);
    eprintln!("\nSummary ({} iterations):", iters);
    eprintln!(
        "  crabz2 average: {:.3}s -> {:.1} MB/s",
        crab_avg,
        plain_mb / crab_avg
    );
    eprintln!(
        "  libbz2 average: {:.3}s -> {:.1} MB/s",
        lib_avg,
        plain_mb / lib_avg
    );
    eprintln!("  speedup: {:.2}x", lib_avg / crab_avg);

    Ok(())
}
