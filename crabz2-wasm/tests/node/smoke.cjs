// Smoke test for the built npm package: exercises both exports against real
// `bzip2`-produced streams, in the same runtime shape a browser sees (the wasm
// module, its generated glue, and nothing else).
//
//   node tests/node/smoke.cjs <pkg-dir>
//
// The package must have been built with `--target nodejs`. `run.sh` does both.

const assert = require("node:assert");
const { execFileSync } = require("node:child_process");
const path = require("node:path");

const pkgDir = path.resolve(process.argv[2] || "pkg-node");
const { decompress, Bz2Decoder } = require(path.join(pkgDir, "crabz2.js"));

let failures = 0;
function test(name, fn) {
  try {
    fn();
    console.log(`ok   ${name}`);
  } catch (e) {
    failures++;
    console.log(`FAIL ${name}\n     ${e && e.message}`);
  }
}

// A deterministic corpus: repetitive prose plus a high-entropy tail, so the stream
// has both easily-compressed and incompressible blocks. xorshift32 keeps it
// reproducible without a dependency.
function corpus(bytes) {
  const words = ["crabz2", "bzip2", "burrows", "wheeler", "huffman", "block", "sort"];
  const out = Buffer.alloc(bytes);
  let state = 0x1a2b3c4d;
  let at = 0;
  const rand = () => {
    state ^= state << 13;
    state >>>= 0;
    state ^= state >>> 17;
    state ^= state << 5;
    state >>>= 0;
    return state;
  };
  while (at < bytes) {
    if (rand() % 8 === 0) {
      out[at++] = rand() & 0xff;
      continue;
    }
    const w = Buffer.from(words[rand() % words.length] + " ");
    const n = Math.min(w.length, bytes - at);
    w.copy(out, at, 0, n);
    at += n;
  }
  return out;
}

function bzip2(buf, level) {
  return execFileSync("bzip2", [level, "-c"], { input: buf, maxBuffer: 1 << 28 });
}

// Drive the streaming class the way the demo page does.
function stream(compressed, chunkSize) {
  const dec = new Bz2Decoder();
  const parts = [];
  let peakBuffered = 0;
  for (let at = 0; at < compressed.length; at += chunkSize) {
    parts.push(Buffer.from(dec.push(compressed.subarray(at, at + chunkSize))));
    peakBuffered = Math.max(peakBuffered, dec.bytesBuffered);
  }
  parts.push(Buffer.from(dec.finish()));
  const out = Buffer.concat(parts);
  assert.strictEqual(dec.bytesIn, compressed.length, "bytesIn");
  assert.strictEqual(dec.bytesOut, out.length, "bytesOut");
  dec.free();
  return { out, peakBuffered };
}

let haveBzip2 = true;
try {
  execFileSync("bzip2", ["--help"], { stdio: "ignore" });
} catch {
  haveBzip2 = false;
}

// "hello crabz2\n" at level 9 — the crate's own vector, so the tests below still
// mean something on a machine without bzip2 installed.
const HELLO_BZ2 = Buffer.from(
  "425a6839314159265359711c50c0000003d9800010400010003a4490102000" +
    "310340d029801ea2e04ced69e0e177245385090711c50c00",
  "hex",
);
const HELLO = Buffer.from("hello crabz2\n");

test("decompress() on the reference vector", () => {
  assert.deepStrictEqual(Buffer.from(decompress(HELLO_BZ2)), HELLO);
});

test("decompress() rejects corrupt input", () => {
  const bad = Buffer.from(HELLO_BZ2);
  bad[bad.length - 6] ^= 0x01;
  assert.throws(() => decompress(bad), /bzip2/);
});

test("Bz2Decoder at every awkward chunk size", () => {
  for (const size of [1, 2, 3, 7, 13, 64, 4096]) {
    assert.deepStrictEqual(stream(HELLO_BZ2, size).out, HELLO, `chunk ${size}`);
  }
});

test("Bz2Decoder handles concatenated streams", () => {
  const cat = Buffer.concat([HELLO_BZ2, HELLO_BZ2, HELLO_BZ2]);
  assert.deepStrictEqual(stream(cat, 5).out, Buffer.concat([HELLO, HELLO, HELLO]));
});

test("Bz2Decoder reports truncation at finish()", () => {
  const dec = new Bz2Decoder();
  dec.push(HELLO_BZ2.subarray(0, HELLO_BZ2.length - 4));
  assert.throws(() => dec.finish(), /unexpected end of bzip2 stream/);
});

test("Bz2Decoder stays failed after bad input", () => {
  const dec = new Bz2Decoder();
  assert.throws(() => dec.push(Buffer.from("not a bzip2 stream at all")), /bzip2/);
  assert.throws(() => dec.push(HELLO_BZ2), /already failed/);
});

if (!haveBzip2) {
  console.log("SKIP large-file cases: system bzip2 not found");
} else {
  const plain = corpus(4 << 20); // 4 MiB

  for (const level of ["-1", "-9"]) {
    const compressed = bzip2(plain, level);

    test(`decompress() matches bzip2 ${level} (${compressed.length} bytes in)`, () => {
      assert.deepStrictEqual(Buffer.from(decompress(compressed)), plain);
    });

    test(`Bz2Decoder streams bzip2 ${level} in 4 KiB chunks`, () => {
      const { out, peakBuffered } = stream(compressed, 4096);
      assert.deepStrictEqual(out, plain);
      // The whole point: what is held is a block, not the file. Level 1 declares
      // 100 KB blocks and level 9 declares 900 KB; the retry threshold can run to
      // twice that before an attempt when a block does not compress, so allow two
      // blocks plus a chunk. Either way it is a constant, unrelated to file size.
      const cap = 2 * ((level === "-1" ? 100e3 : 900e3) + 4096) + 4096;
      assert.ok(
        peakBuffered <= cap,
        `peak buffered ${peakBuffered} > ${cap} for a ${compressed.length}-byte input`,
      );
    });
  }

  // Incompressible input is the case that exercises the fallback: a block of random
  // bytes comes out slightly *larger* than the block size the header declares, so
  // the block-sized target is not enough and the threshold has to grow.
  test("Bz2Decoder streams incompressible input", () => {
    const random = require("node:crypto").randomBytes(4 << 20);
    const compressed = bzip2(random, "-9");
    assert.ok(compressed.length > random.length * 0.99, "expected no compression");
    const { out, peakBuffered } = stream(compressed, 4096);
    assert.deepStrictEqual(out, random);
    assert.ok(peakBuffered <= 2 * (900e3 + 4096) + 4096, `peak buffered ${peakBuffered}`);
  });

  test("Bz2Decoder streams a multi-stream file", () => {
    const a = bzip2(plain.subarray(0, 1 << 20), "-9");
    const b = bzip2(plain.subarray(1 << 20, 2 << 20), "-1");
    const { out } = stream(Buffer.concat([a, b]), 8192);
    assert.deepStrictEqual(out, plain.subarray(0, 2 << 20));
  });

  test("Bz2Decoder rejects a CRC-corrupted block", () => {
    const compressed = Buffer.from(bzip2(plain.subarray(0, 1 << 20), "-1"));
    compressed[compressed.length >> 1] ^= 0x40;
    assert.throws(() => {
      const dec = new Bz2Decoder();
      dec.push(compressed);
      dec.finish();
    }, /bzip2/);
  });
}

console.log(failures ? `\n${failures} failing` : "\nall smoke tests passed");
process.exit(failures ? 1 : 0);
