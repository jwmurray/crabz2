//! Download a CourtListener bulk-data file and decompress it with `crabz2` — a
//! pure-Rust, in-process stand-in for the classic `lbzip2 -dc file.bz2 | ...` shell-out.
//!
//! CourtListener (Free Law Project) publishes its bulk data as bzip2-compressed CSV
//! (dockets, opinions, opinion-clusters, citations, courts, financial disclosures, ...):
//!   <https://www.courtlistener.com/help/api/bulk-data/>
//!
//! The files live in a public S3 bucket. List what's available with:
//!   curl 'https://com-courtlistener-storage.s3-us-west-2.amazonaws.com/?list-type=2&prefix=bulk-data/'
//!
//! ## Run it
//!
//! ```text
//! cargo run --release --example courtlistener                 # small `courts` table (~80 KB)
//! cargo run --release --example courtlistener -- courts        # bare table name
//! cargo run --release --example courtlistener -- bulk-data/citations-2026-06-30.csv.bz2
//! cargo run --release --example courtlistener -- https://host/path/opinions.csv.bz2
//! ```
//!
//! The whole point: the compressed HTTP body streams *directly* into `crabz2::reader`,
//! which decompresses in-process in pure Rust. No `lbzip2`/`pbzip2`/`bzip2` child process,
//! no `libbz2` C dependency, and nothing is fully buffered — memory stays bounded to about
//! one bzip2 block regardless of how large the download is.

use std::io::{BufRead, BufReader};
use std::time::Instant;

const BUCKET: &str = "https://com-courtlistener-storage.s3-us-west-2.amazonaws.com";
/// A small table that makes for a quick, friendly demo (~80 KB compressed).
const DEFAULT_KEY: &str = "bulk-data/courts-2026-06-30.csv.bz2";
/// Latest bulk-data snapshot date, used to expand bare table names like `courts`.
const LATEST: &str = "2026-06-30";

/// Turn a CLI argument into a full URL. Accepts a full URL, an S3 key
/// (`bulk-data/…csv.bz2`), or a bare table name (`courts`, `opinions`, `citations`, …).
fn resolve(arg: Option<&str>) -> String {
    match arg {
        None => format!("{BUCKET}/{DEFAULT_KEY}"),
        Some(a) if a.starts_with("http://") || a.starts_with("https://") => a.to_string(),
        Some(a) if a.ends_with(".bz2") => format!("{BUCKET}/{}", a.trim_start_matches('/')),
        Some(table) => format!("{BUCKET}/bulk-data/{table}-{LATEST}.csv.bz2"),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = resolve(std::env::args().nth(1).as_deref());
    eprintln!("[crabz2] GET {url}");

    // 1. Open the download as a streaming `Read` (pure-Rust TLS via rustls).
    let response = ureq::get(&url).call()?;
    let compressed = response.into_reader();

    // 2. Decompress in-process. This single line is what replaces the `lbzip2 -dc`
    //    child process the ingester would otherwise spawn.
    let plaintext = crabz2::reader(compressed);

    // 3. Consume the CSV plaintext row-by-row without ever holding the whole file.
    let mut reader = BufReader::with_capacity(1 << 20, plaintext);
    let mut line = Vec::new();
    let (mut rows, mut bytes) = (0u64, 0u64);
    let mut preview: Vec<String> = Vec::new();
    let start = Instant::now();

    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        rows += 1;
        if preview.len() < 3 {
            let s: String = String::from_utf8_lossy(&line).trim_end().chars().take(160).collect();
            preview.push(s);
        }
    }

    let secs = start.elapsed().as_secs_f64();
    eprintln!("[crabz2] first rows:");
    for row in &preview {
        eprintln!("    {row}");
    }
    eprintln!(
        "[crabz2] decompressed {:.2} MB over {} CSV rows in {:.2}s ({:.1} MB/s) — pure Rust, no lbzip2, no C",
        bytes as f64 / 1e6,
        rows,
        secs,
        (bytes as f64 / 1e6) / secs.max(1e-9),
    );
    Ok(())
}
