//! Minimal decompress-to-stdout CLI: `crabz2 <file.bz2>` (or stdin if no arg).
use std::io::{self, Read, Write};

fn main() -> io::Result<()> {
    let arg = std::env::args().nth(1);
    let src: Box<dyn Read> = match arg.as_deref() {
        None | Some("-") => Box::new(io::stdin()),
        Some(path) => Box::new(std::fs::File::open(path)?),
    };
    let mut out = io::stdout().lock();
    let mut reader = crabz2::reader(src);
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
    }
    Ok(())
}
