#!/usr/bin/env bash
set -euo pipefail

# This script now compares three decompression paths:
# - `crabz2` (pure Rust, parallel)
# - `libbz2` via the `bzip2` crate (Rust wrapper around C libbz2)
# - system `bzip2` binary (native C)

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR"

OUT=bench_multi.csv
echo "input_mb,crabz2_mb_s,libbz2_mb_s,bzip2_mb_s,parallel_bzip2_mb_s,parallel_cmd,threads" > "$OUT"

THREADS=8
ITERS=3

# detect parallel bzip2 implementations for a multi-threaded C baseline
PBZIP=$(command -v pbzip2 || true)
LBZIP=$(command -v lbzip2 || true)
PAR_CMD=""
PAR_TYPE=""
if [ -n "$PBZIP" ]; then
  PAR_CMD="$PBZIP"
  PAR_TYPE="pbzip2"
elif [ -n "$LBZIP" ]; then
  PAR_CMD="$LBZIP"
  PAR_TYPE="lbzip2"
fi

for sz in 1 5 10 50 100; do
  echo "Running ${sz} MB test..."
  PLAIN=/tmp/crabz2_plain_${sz}.dat
  BZ=/tmp/crabz2_test_${sz}.bz2

  # generate plaintext
  python3 - <<PY > "$PLAIN"
from itertools import cycle
sample = b"the quick brown fox jumps over the lazy dog\n"
target = $(( $sz * 1000000 ))
out = bytearray()
for b in cycle(sample):
    out.append(b)
    if len(out) >= target:
        break
import sys
sys.stdout.buffer.write(out)
PY

  # compress with system bzip2 (single-thread) to produce a .bz2 file
  if command -v bzip2 >/dev/null 2>&1; then
    bzip2 -c "$PLAIN" > "$BZ"
  else
    echo "bzip2 compressor not found; please install bzip2." >&2
    exit 1
  fi

  # Run the examples/compare.rs example which runs both crabz2 and libbz2.
  echo "  running cargo compare example (crabz2 vs libbz2) ..."
  COMP_OUT=$(mktemp)
  cargo run --release --example compare --features "libbz2 parallel" -- "$BZ" $ITERS $THREADS > "$COMP_OUT" 2>&1 || (cat "$COMP_OUT" && exit 1)

  # Extract MB/s from the compare example output.
  crab_mbs=$(sed -nE 's/.*crabz2 average: .*-> ([0-9.]+) MB\/s/\1/p' "$COMP_OUT" | head -n1)
  lib_mbs=$(sed -nE 's/.*libbz2 average: .*-> ([0-9.]+) MB\/s/\1/p' "$COMP_OUT" | head -n1)
  if [ -z "$crab_mbs" ] || [ -z "$lib_mbs" ]; then
    echo "Failed to parse compare example output:" >&2
    sed -n '1,200p' "$COMP_OUT" >&2
    exit 1
  fi

  # Measure system bzip2 (native C) decompression MB/s using Python timing.
  echo "  running system bzip2 (single-thread)..."
  python3 - <<PY > /tmp/bzip2_time.txt 2>/dev/null
import time, subprocess
cmd = ["bzip2", "-dc", "$BZ"]
start = time.time()
subprocess.check_call(cmd, stdout=subprocess.DEVNULL)
end = time.time()
print(end-start)
PY
  bzip2_real=$(cat /tmp/bzip2_time.txt)
  if [ -z "$bzip2_real" ] || awk "BEGIN{print ($bzip2_real<=0)}" | grep -q 1; then
    bzip2_real=0.000001
  fi
  plain_size=$(stat -f%z "$PLAIN")
  bzip2_mbs=$(awk -v b="$plain_size" -v s="$bzip2_real" 'BEGIN{printf "%.1f", (b/1e6)/s}')

  # Measure parallel pbzip2/lbzip2 if available
  par_mbs=""
  if [ -n "$PAR_CMD" ]; then
    echo "  running $PAR_CMD - threads=$THREADS..."
    if [ "$PAR_TYPE" = "pbzip2" ]; then
      python3 - <<PY > /tmp/parallel_time.txt 2>/dev/null
import time, subprocess
cmd = ["$PAR_CMD", "-dc", "-p", str($THREADS), "$BZ"]
start = time.time()
subprocess.check_call(cmd, stdout=subprocess.DEVNULL)
end = time.time()
print(end-start)
PY
    else
      python3 - <<PY > /tmp/parallel_time.txt 2>/dev/null
import time, subprocess
cmd = ["$PAR_CMD", "-dc", "-n", str($THREADS), "$BZ"]
start = time.time()
subprocess.check_call(cmd, stdout=subprocess.DEVNULL)
end = time.time()
print(end-start)
PY
    fi
    par_real=$(cat /tmp/parallel_time.txt)
    if [ -z "$par_real" ] || awk "BEGIN{print ($par_real<=0)}" | grep -q 1; then
      par_real=0.000001
    fi
    par_mbs=$(awk -v b="$plain_size" -v s="$par_real" 'BEGIN{printf "%.1f", (b/1e6)/s}')
  fi

  echo "${sz},${crab_mbs},${lib_mbs},${bzip2_mbs},${par_mbs},${PAR_CMD},${THREADS}" >> "$OUT"

  # Print comparative summary for this size.
  if [ -n "$par_mbs" ]; then
    echo "  Results (${sz} MB): crabz2=${crab_mbs} MB/s, libbz2=${lib_mbs} MB/s, bzip2=${bzip2_mbs} MB/s, parallel=${par_mbs} MB/s ($PAR_CMD)"
    awk -v c=${crab_mbs} -v p=${par_mbs} 'BEGIN{printf "  Speedups: crabz2/parallel = %.2fx\n", (c/p)}'
  else
    echo "  Results (${sz} MB): crabz2=${crab_mbs} MB/s, libbz2=${lib_mbs} MB/s, bzip2=${bzip2_mbs} MB/s"
    awk -v c=${crab_mbs} -v l=${lib_mbs} -v b=${bzip2_mbs} 'BEGIN{printf "  Speedups: crabz2/libbz2 = %.2fx, crabz2/bzip2 = %.2fx\n", (c/l), (c/b)}'
  fi

  rm -f "$COMP_OUT"
done

echo "Wrote $OUT"
cat "$OUT"
