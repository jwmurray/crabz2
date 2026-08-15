# crabz2 — Roadmap & Design

Eight workstreams, ordered by dependency, not importance. Items marked (M) are
the ones that make the crate *adoptable*; items marked (F) are foundations the
others sit on.

| # | Item | Ships in | Depends on |
|---|---|---|---|
| 1 | (F) `no_std + alloc` core | 0.3.x | — |
| 2 | (F) CI: tests, clippy, fmt, wasm32 check | 0.3.x | — |
| 3 | (M) Fuzzing (`cargo-fuzz`) | 0.3.x | — |
| 4 | (M) Benchmarks vs libbz2, numbers in README | 0.3.x | — |
| 5 | (M) Parallel block decode (`parallel` feature) | 0.4 | 1 |
| 6 | (M) WASM wrapper crate + npm package | **built** (`main`), npm at 0.4 | 1, 2 |
| 7 | (M) Encoder — pure-Rust bzip2 compression | **shipped** (`main`) | 1 (conventions only) |
| 8 | Announcement (r/rust, TWiR) | after 0.4 | 4, 5, 6 |

The core crate keeps its invariant: **zero runtime dependencies**. Everything
that needs a dependency (rayon, wasm-bindgen, criterion, libbz2) is a
non-default feature, a separate workspace crate, or a dev-dependency.

---

## 1. `no_std + alloc` (foundation)

**Why.** "Pure Rust, zero deps" is exactly what embedded and WASM users want;
`std::io::Read` in the core is the only thing standing in the way.

**Design.**
- `#![cfg_attr(not(feature = "std"), no_std)]`; `extern crate alloc`; `Vec`
  and boxed tables come from `alloc`.
- The decode pipeline already works block-at-a-time over buffered bytes. Split
  the core into a sans-io state machine: an internal `struct BlockDecoder`
  that consumes `&[u8]` (plus a bit cursor) and produces decompressed bytes
  into a caller-supplied `Vec` — no `io` types anywhere in it.
- `std` (default feature) keeps today's public API exactly: `Crabz2Reader<R:
  Read>`, `reader()`, `decompress()` become thin adapters over the state
  machine. No breaking change for existing users.
- `no_std` gains `decompress_to_vec(&[u8]) -> Result<Vec<u8>, Error>` with an
  own `Error` enum (`InvalidMagic`, `CrcMismatch`, `Truncated`, …).
  `From<Error> for io::Error` under `std`.
- MSRV stays 1.63. CI builds `--no-default-features` for
  `thumbv7em-none-eabihf` (a true no_std target) and `wasm32-unknown-unknown`.

## 2. CI

GitHub Actions, one workflow: `cargo test` (stable + MSRV 1.63), `cargo
clippy -- -D warnings`, `cargo fmt --check`, `cargo check --target
wasm32-unknown-unknown`, `cargo check --no-default-features` on a no_std
target. Badge in README.

## 3. Fuzzing

**Why.** A from-scratch parser of untrusted compressed input must state its
fuzzing posture; security-conscious adopters check for it.

**Design.**
- `fuzz/` (cargo-fuzz, nightly-only, not in the workspace default-members):
  - `fuzz_decompress`: arbitrary bytes → `decompress` must never panic, never
    OOM beyond the structural cap (block size declared in the header bounds
    every allocation — assert that bound in the target), and either produce
    bytes or a clean `Error`.
  - `fuzz_roundtrip` (after item 7): `compress(data) |> decompress == data`.
- Seed corpus: the test vectors already in `src/lib.rs` + small files
  compressed at levels 1 and 9, single- and multi-stream.
- CI: a 60-second smoke run of each target on nightly (scheduled + on PRs
  touching `src/`). README gets a "Fuzzing" section stating the targets and
  invariants.

## 4. Benchmarks

**Why.** Nobody adopts a from-scratch decoder without numbers.

**Design.**
- `benches/` with criterion (dev-dependency). Inputs: enwik-style text,
  CSV (the CourtListener shape), and incompressible random bytes, each ~10 MB,
  generated deterministically by an xshift PRNG at bench time (nothing large
  checked into the repo).
- Baselines: `bzip2` crate (libbz2 bindings) as a dev-dependency only, and —
  once item 5 lands — crabz2 parallel at 2/4/8 threads.
- README gets a results table (MB/s, machine noted) and the honest sentence
  about where we lose to C and by how much.

## 5. Parallel block decode (`parallel` feature, non-default)

**Why.** bzip2 blocks are independent; this is the lbzip2-in-process pitch and
the crate's clearest differentiator.

**Design.**
- Blocks start with the 48-bit magic `0x314159265359` at **arbitrary bit
  offsets**. A scanner walks the compressed stream finding candidate magics
  (bit-shifted match over a rolling 64-bit window). Candidates can be false
  positives (the magic can occur inside entropy-coded data), so:
  speculative-decode each candidate block on the pool; a false positive fails
  fast (Huffman/structure error or CRC mismatch) and is discarded; true blocks
  are reassembled strictly in order and the stream CRC is verified at the end
  exactly as in serial mode.
- Correctness rule: **parallel output must be byte-identical to serial** —
  property-tested. On any ambiguity (overlapping candidate decodes), serial
  decode of the region wins; worst case degrades to serial, never to wrong
  bytes.
- API: `decompress_parallel(&[u8], threads: Option<usize>)` and
  `par_reader()` returning ordered output; rayon behind
  `feature = "parallel"`, which is **off by default** and additive — the
  serial path compiles identically without it.
- WASM: `parallel` is not offered on wasm targets (`compile_error!` on the
  combination is kinder than a runtime surprise; see item 6).

## 6. WASM wrapper + npm package

**Why.** Item 1 makes the core wasm-clean (0.2.1 already compiles for
`wasm32-unknown-unknown` untouched); this makes it *usable* from JS.

**Status: implemented on `main`.** Not yet published to npm — that waits on the
0.4 release and the owner's npm credentials. `crabz2-wasm/README.md` is the
package's own documentation; what follows is the design and where it landed.

**Design.**
- Workspace: the core crate stays exactly as published (`crabz2`, zero deps)
  and is *itself* the workspace root — `[workspace] members = [".",
  "crabz2-wasm"]` — rather than the virtual workspace originally sketched
  here. A virtual workspace would have moved `src/`, which is not worth
  breaking every in-flight branch and every link into the repository for.
  Nothing about how the crate builds, packages, or resolves changes.
  `crabz2-wasm/` sits beside it: `publish = false`, published to **npm** as
  `crabz2` via wasm-pack (the Rust package cannot take that name while it
  shares a workspace with the library, so `crabz2-wasm/build.sh` corrects the
  name in the generated `package.json`).
- Exports: `decompress(input: Uint8Array) -> Uint8Array` for the common case,
  and a push-based streaming class for large files —
  `Bz2Decoder { push(chunk: Uint8Array): Uint8Array; finish(): Uint8Array }`
  — driving the sans-io state machine from item 1, so memory stays ~one block
  regardless of file size.
- The state machine restarts rather than resumes a partially-read block, so
  the wrapper must not re-attempt a decode on every `push`; that would be
  quadratic in the block size. It buffers instead, and re-attempts only when
  enough new input has arrived to pay for the attempt — the size of the
  previous block, which is a tight estimate within one file, growing
  geometrically when that estimate falls short. The core's contract is
  untouched; item 1's `BlockDecoder` only gained the two accessors
  (`consumed`, `rebase`) a push-based caller needs to drop committed bytes,
  which the `io::Read` adapter was already doing through the private field.
- Demo page under `crabz2-wasm/www/`: drop a `.bz2`, get the file —
  decompressed client-side, with progress and the decoder's live buffer
  shown; doubles as the announcement artifact.
- CI (`wasm.yml`, separate from `ci.yml`): `wasm-pack build`, then a node
  smoke test of the built package — both exports against multi-megabyte
  `bzip2` output at levels 1 and 9, several chunk sizes, corruption,
  truncation, and an assertion that streaming memory stays bounded by the
  block rather than the file. A browser harness would test the same two
  functions through more moving parts.

## 7. Encoder (the big one)

**Status: shipped on `main`.** `src/encode/` implements the pipeline below;
`compress(&[u8], Level)` and `writer(W, Level) -> Crabz2Writer<W>` are public.
All three verification bars hold, and compressed size came in slightly *under*
libbz2's at every level measured (see the README table). Two notes against the
design as written: the code-length cap is **17**, not the 20 our decoder's delta
reader would accept, because 17 is what libbz2 emits and staying in that window
keeps third-party decoders happy; and RLE1 runs as an incremental splitter over
the input stream rather than a buffer-at-a-time pass, so a block boundary always
falls between two self-contained RLE1 groups. Parallel *encode* remains out of
scope, as planned.

**Why.** Decode-only excludes half the audience; a pure-Rust MIT bzip2
*encoder* does not exist in the ecosystem. This completes the story.

**Design.** Mirror of the decode pipeline, correctness-first:

- **RLE1** → **BWT** → **MTF + RLE2 (RUNA/RUNB)** → grouped **Huffman**
  (2–6 tables, selectors every 50 symbols, selector-MTF, canonical code
  emission) → bit packing, block CRC, final combined stream CRC. Level 1–9 =
  block size 100k–900k, same as bzip2.
- **BWT strategy** (the hard part): v1 uses SA-IS (O(n), from scratch, stays
  zero-dep) over the doubled block to sort *rotations* (bzip2 sorts rotations,
  not sentinel-terminated suffixes: build SA of `block ‖ block`, keep
  positions < n). ~2× transient memory at level 9 (≈ 1.8 MB extra) — accepted
  for v1; a dedicated rotation sort (libbz2-style main/fallback) is a later
  optimization with the benchmark suite to justify it.
- **Huffman table construction**: package-merge for length-limited canonical
  codes; iterative table refinement across groups (libbz2-style: assign
  groups to tables, recount, rebuild, a few passes). Code-length cap taken
  from what our own decoder accepts — verified against the spec and the
  reference implementation during implementation, not assumed.
- **Verification bar** (the doctrine): output need not be byte-identical to
  libbz2 — it must (a) round-trip through our own decoder for every test
  vector and fuzz input, (b) decompress byte-identically under **system
  `bzip2`** (integration test, skipped when the binary is absent), (c) hold
  the compressed-size ratio within a stated envelope of libbz2 at the same
  level (benchmarked, number in README).
- API: `compress(&[u8], level: Level) -> Vec<u8>`, `writer(W, level) ->
  Crabz2Writer<W>` (std), `no_std` variant returns `Vec` via the sans-io
  machine. `Crabz2Writer` implements `io::Write` with `finish()`.
- Parallel *encode* (independent blocks again) falls out nearly free after
  item 5's pool machinery — explicitly out of scope for the first encoder PR.

## 8. Announcement

Not a code item, listed so it isn't forgotten: r/rust post + This Week in Rust
submission once 4 + 5 + 6 give the story numbers ("pure-Rust bzip2, zero
deps, parallel, runs in the browser"), linking the demo page.
