//! Minimal decompress-to-stdout CLI: `crabz2 <file.bz2>` (or stdin if no arg).
use anyhow::{Context, Result};
use clap::Parser;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Decompress a bzip2 stream to stdout — pure Rust, no libbz2.
#[derive(Parser)]
#[command(name = "crabz2", version, long_about = None)]
struct Args {
    /// The `.bz2` file to decompress. Omit it, or pass `-`, to read stdin.
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let src: Box<dyn Read> = match args.file.as_deref() {
        None => Box::new(io::stdin()),
        Some(p) if p == Path::new("-") => Box::new(io::stdin()),
        Some(path) => Box::new(
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?,
        ),
    };

    let mut out = io::stdout().lock();
    let mut reader = crabz2::reader(src);
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = reader.read(&mut buf).context("decompressing")?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).context("writing to stdout")?;
    }
    Ok(())
}
