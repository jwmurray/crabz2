//! CRC-32/BZIP2 for the encoder (poly 0x04C11DB7, MSB-first, init/xorout 0xFFFFFFFF).
//!
//! The table is built at compile time so the encoder needs no runtime
//! initialisation and no allocation on this path.

const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0usize;
    while n < 256 {
        let mut c = (n as u32) << 24;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ 0x04C1_1DB7
            } else {
                c << 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

/// Incremental CRC-32/BZIP2 over one block's plaintext.
#[derive(Clone, Copy)]
pub struct Crc32 {
    state: u32,
}

impl Crc32 {
    pub fn new() -> Self {
        Crc32 { state: 0xFFFF_FFFF }
    }

    #[inline]
    pub fn push(&mut self, byte: u8) {
        self.state =
            (self.state << 8) ^ TABLE[(((self.state >> 24) ^ byte as u32) & 0xff) as usize];
    }

    /// Feed `count` copies of `byte` — the shape the RLE1 stage produces.
    #[inline]
    pub fn push_repeat(&mut self, byte: u8, count: usize) {
        for _ in 0..count {
            self.push(byte);
        }
    }

    pub fn finish(self) -> u32 {
        !self.state
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Crc32::new()
    }
}

/// CRC-32/BZIP2 over a whole buffer. The encoder itself only ever needs the
/// incremental form; this exists to check that form against known vectors.
#[cfg(test)]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    for &b in data {
        crc.push(b);
    }
    crc.finish()
}

/// Fold a block CRC into the running combined-stream CRC, exactly as bzip2 does.
pub fn combine(combined: u32, block_crc: u32) -> u32 {
    combined.rotate_left(1) ^ block_crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_vectors() {
        // CRC-32/BZIP2 check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xFC89_1918);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn matches_the_hello_block_crc() {
        // Block CRC embedded in the `HELLO_BZ2` decoder test vector.
        assert_eq!(crc32(b"hello crabz2\n"), 0x711c_50c0);
    }

    #[test]
    fn combine_rotates_then_xors() {
        assert_eq!(combine(0, 0x711c_50c0), 0x711c_50c0);
        assert_eq!(combine(0x8000_0000, 0), 1);
    }
}
