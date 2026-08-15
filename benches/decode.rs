//! Decode throughput: crabz2 (pure Rust, from scratch) vs libbz2 (C) on the same buffers.
//!
//! Nothing is checked into the repository: all three corpora are generated at bench
//! time by a seeded xorshift PRNG, so runs are reproducible without a fixture file.
//! Each corpus is compressed in memory with the `bzip2` crate (C libbz2) at level 9,
//! and crabz2's decode of that stream is asserted to be byte-identical to the original
//! before any timing happens — the benchmark doubles as a cross-validation fixture.
//!
//! Run with `cargo bench`. Under `cargo test` the bench binary is executed in
//! criterion's test mode; it then uses 1 MiB corpora so the compile/correctness check
//! stays fast.

use std::io::{Read, Write};
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

/// Corpus size for a real benchmark run.
const BENCH_BYTES: usize = 10 * 1024 * 1024;
/// Corpus size when the bench binary runs as a test (`cargo test`).
const TEST_BYTES: usize = 1024 * 1024;

/// xorshift64* — a deterministic PRNG so every machine benchmarks the same bytes.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        XorShift(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, n)`.
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() >> 1) as usize % n
    }

    /// Uniform in `[lo, hi]`.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A synthetic vocabulary of pronounceable words, sampled with Zipf-like frequencies.
///
/// A tiny word list would compress far better than real prose and flatter the decoder.
/// A 64k vocabulary drawn from a Zipf distribution lands in the same ballpark as English
/// text: bzip2 -9 gets about 3.8x here, against roughly 3.5x on enwik-style input.
struct Vocabulary {
    words: Vec<String>,
    cumulative: Vec<f64>,
}

impl Vocabulary {
    fn new(rng: &mut XorShift, count: usize) -> Self {
        const ONSETS: [&str; 24] = [
            "b", "c", "d", "f", "g", "h", "j", "k", "l", "m", "n", "p", "r", "s", "t", "v", "w",
            "ch", "sh", "th", "st", "tr", "pl", "br",
        ];
        const NUCLEI: [&str; 12] = [
            "a", "e", "i", "o", "u", "ea", "ee", "ou", "ai", "io", "au", "ie",
        ];
        const CODAS: [&str; 14] = [
            "", "", "n", "r", "s", "t", "l", "d", "m", "ng", "ck", "st", "nt", "ll",
        ];

        let mut words = Vec::with_capacity(count);
        for _ in 0..count {
            let syllables = rng.range(1, 3);
            let mut w = String::new();
            for _ in 0..syllables {
                w.push_str(ONSETS[rng.below(ONSETS.len())]);
                w.push_str(NUCLEI[rng.below(NUCLEI.len())]);
                w.push_str(CODAS[rng.below(CODAS.len())]);
            }
            words.push(w);
        }

        // Zipf weights: word i has weight 1/(i+1)^0.95, the shape natural language shows.
        let mut cumulative = Vec::with_capacity(count);
        let mut total = 0.0f64;
        for i in 0..count {
            total += 1.0 / ((i + 1) as f64).powf(0.95);
            cumulative.push(total);
        }
        for c in cumulative.iter_mut() {
            *c /= total;
        }

        Vocabulary { words, cumulative }
    }

    fn pick(&self, rng: &mut XorShift) -> &str {
        let u = rng.unit();
        let idx = self.cumulative.partition_point(|&c| c < u);
        &self.words[idx.min(self.words.len() - 1)]
    }
}

/// English-shaped prose: Zipf-distributed words, sentence and paragraph structure.
fn generate_text(len: usize) -> Vec<u8> {
    let mut rng = XorShift::new(0x5EED_1234_ABCD_0001);
    let vocab = Vocabulary::new(&mut rng, 65536);
    let mut out = Vec::with_capacity(len + 256);

    while out.len() < len {
        let sentences = rng.range(3, 9);
        for _ in 0..sentences {
            let words = rng.range(6, 22);
            for w in 0..words {
                let word = vocab.pick(&mut rng);
                if w == 0 {
                    let mut chars = word.chars();
                    if let Some(first) = chars.next() {
                        out.extend(first.to_uppercase().to_string().as_bytes());
                        out.extend(chars.as_str().as_bytes());
                    }
                } else {
                    out.extend(word.as_bytes());
                }
                if w + 1 < words {
                    // Commas roughly every dozen words, as in ordinary prose.
                    if rng.below(12) == 0 {
                        out.push(b',');
                    }
                    out.push(b' ');
                }
            }
            out.push(if rng.below(16) == 0 { b'?' } else { b'.' });
            out.push(b' ');
        }
        out.extend_from_slice(b"\n\n");
    }

    out.truncate(len);
    out
}

/// CSV shaped like a court bulk-data export (the CourtListener opinions/dockets shape).
fn generate_csv(len: usize) -> Vec<u8> {
    const SURNAMES: [&str; 32] = [
        "Smith",
        "Johnson",
        "Williams",
        "Brown",
        "Jones",
        "Garcia",
        "Miller",
        "Davis",
        "Rodriguez",
        "Martinez",
        "Hernandez",
        "Lopez",
        "Gonzalez",
        "Wilson",
        "Anderson",
        "Thomas",
        "Taylor",
        "Moore",
        "Jackson",
        "Martin",
        "Lee",
        "Perez",
        "Thompson",
        "White",
        "Harris",
        "Sanchez",
        "Clark",
        "Ramirez",
        "Lewis",
        "Robinson",
        "Walker",
        "Young",
    ];
    const ENTITIES: [&str; 12] = [
        "United States",
        "State of Utah",
        "Acme Holdings LLC",
        "Northern Pacific Railway Co.",
        "Commissioner of Internal Revenue",
        "Board of Education",
        "First National Bank",
        "Department of Labor",
        "Cascade Insurance Group",
        "City of Salt Lake",
        "Meridian Logistics Inc.",
        "Summit Health Partners",
    ];
    const COURTS: [&str; 14] = [
        "ca1",
        "ca2",
        "ca3",
        "ca4",
        "ca5",
        "ca9",
        "cafc",
        "scotus",
        "utah",
        "utahctapp",
        "dcd",
        "nysd",
        "cand",
        "txnd",
    ];
    const STATUS: [&str; 4] = ["Published", "Unpublished", "Errata", "Separate"];
    const REPORTERS: [&str; 6] = ["F.3d", "F. Supp. 3d", "U.S.", "P.3d", "S. Ct.", "F. App'x"];
    const SUITS: [&str; 8] = [
        "Contract: Other",
        "Civil Rights: Employment",
        "Torts: Personal Injury",
        "Labor: ERISA",
        "Bankruptcy Appeal",
        "Intellectual Property: Patent",
        "Prisoner Petitions: Habeas Corpus",
        "Statutes: Environmental Matters",
    ];

    let mut rng = XorShift::new(0x5EED_1234_ABCD_0002);
    let mut out = Vec::with_capacity(len + 512);
    out.extend_from_slice(
        b"id,date_created,date_modified,date_filed,case_name,docket_number,court_id,\
citation,precedential_status,judges,nature_of_suit,page_count\n",
    );

    let mut id = 1_000_000u64;
    while out.len() < len {
        id += rng.range(1, 4) as u64;
        let year = 1998 + rng.below(28);
        let month = 1 + rng.below(12);
        let day = 1 + rng.below(28);

        let plaintiff = if rng.below(4) == 0 {
            ENTITIES[rng.below(ENTITIES.len())].to_string()
        } else {
            SURNAMES[rng.below(SURNAMES.len())].to_string()
        };
        let defendant = if rng.below(3) == 0 {
            ENTITIES[rng.below(ENTITIES.len())].to_string()
        } else {
            SURNAMES[rng.below(SURNAMES.len())].to_string()
        };

        let judges = {
            let n = rng.range(1, 3);
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(SURNAMES[rng.below(SURNAMES.len())]);
            }
            v.join("; ")
        };

        let row = format!(
            "{id},{y}-{m:02}-{d:02} 14:{mi:02}:{s:02}.{frac:06},\
{y}-{m:02}-{d:02} 14:{mi2:02}:{s2:02}.{frac2:06},{y}-{m:02}-{d:02},\
\"{plaintiff} v. {defendant}\",{docket_year}:{dnum:02}-cv-{dseq:05},{court},\
\"{vol} {rep} {page}\",{status},\"{judges}\",\"{suit}\",{pages}\n",
            id = id,
            y = year,
            m = month,
            d = day,
            mi = rng.below(60),
            s = rng.below(60),
            frac = rng.below(1_000_000),
            mi2 = rng.below(60),
            s2 = rng.below(60),
            frac2 = rng.below(1_000_000),
            plaintiff = plaintiff,
            defendant = defendant,
            docket_year = year % 100,
            dnum = rng.range(1, 9),
            dseq = rng.below(100_000),
            court = COURTS[rng.below(COURTS.len())],
            vol = rng.range(1, 999),
            rep = REPORTERS[rng.below(REPORTERS.len())],
            page = rng.range(1, 1800),
            status = STATUS[rng.below(STATUS.len())],
            judges = judges,
            suit = SUITS[rng.below(SUITS.len())],
            pages = rng.range(1, 60),
        );
        out.extend_from_slice(row.as_bytes());
    }

    out.truncate(len);
    out
}

/// Incompressible bytes — the worst case for any bzip2 implementation.
fn generate_random(len: usize) -> Vec<u8> {
    let mut rng = XorShift::new(0x5EED_1234_ABCD_0003);
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        out.extend_from_slice(&rng.next_u64().to_le_bytes());
    }
    out.truncate(len);
    out
}

fn bzip2_compress(data: &[u8]) -> Vec<u8> {
    let mut enc = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::new(9));
    enc.write_all(data).expect("bzip2 compression failed");
    enc.finish().expect("bzip2 compression failed")
}

fn bzip2_decompress(mut compressed: &[u8]) -> Vec<u8> {
    // `bufread` (not `read`) so libbz2 consumes the input slice directly, exactly as
    // crabz2::decompress does — `read::BzDecoder` would interpose a BufReader copy.
    let mut out = Vec::new();
    bzip2::bufread::BzDecoder::new(&mut compressed)
        .read_to_end(&mut out)
        .expect("libbz2 decompression failed");
    out
}

struct Corpus {
    name: &'static str,
    plain: Vec<u8>,
    compressed: Vec<u8>,
}

impl Corpus {
    fn build(name: &'static str, plain: Vec<u8>) -> Self {
        let compressed = bzip2_compress(&plain);

        // Cross-validation, before any timing: our decoder must reproduce the input
        // byte for byte, and so must libbz2 (which keeps the comparison honest).
        assert_eq!(
            crabz2::decompress(&compressed).expect("crabz2 decode failed"),
            plain,
            "crabz2 output differs from the original {name} corpus"
        );
        assert_eq!(
            bzip2_decompress(&compressed),
            plain,
            "libbz2 output differs from the original {name} corpus"
        );

        Corpus {
            name,
            plain,
            compressed,
        }
    }

    fn ratio(&self) -> f64 {
        self.plain.len() as f64 / self.compressed.len() as f64
    }
}

fn bench_decode(c: &mut Criterion) {
    // `cargo bench` passes `--bench`; under `cargo test` criterion runs the binary in
    // test mode (one iteration each), and small corpora keep that check fast.
    let test_mode = !std::env::args().any(|a| a == "--bench");
    let size = if test_mode { TEST_BYTES } else { BENCH_BYTES };

    let corpora = [
        Corpus::build("text", generate_text(size)),
        Corpus::build("csv", generate_csv(size)),
        Corpus::build("random", generate_random(size)),
    ];

    for corpus in &corpora {
        eprintln!(
            "{}: {} bytes -> {} bytes compressed ({:.2}x)",
            corpus.name,
            corpus.plain.len(),
            corpus.compressed.len(),
            corpus.ratio(),
        );
    }

    let mut group = c.benchmark_group("decode");
    group.sample_size(20);
    if !test_mode {
        // A 10 MiB decode is ~0.2 s, so criterion's 5 s default cannot fit its samples.
        group.warm_up_time(Duration::from_secs(3));
        group.measurement_time(Duration::from_secs(15));
    }
    for corpus in &corpora {
        // Throughput is reported over the *decompressed* size: MB/s of plaintext out.
        group.throughput(Throughput::Bytes(corpus.plain.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("crabz2", corpus.name),
            &corpus.compressed,
            |b, compressed| {
                b.iter(|| {
                    let out = crabz2::decompress(black_box(compressed)).unwrap();
                    black_box(out.len())
                })
            },
        );
        group.bench_with_input(
            BenchmarkId::new("libbz2", corpus.name),
            &corpus.compressed,
            |b, compressed| {
                b.iter(|| {
                    let out = bzip2_decompress(black_box(compressed));
                    black_box(out.len())
                })
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);
