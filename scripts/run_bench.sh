#!/usr/bin/env bash
set -euo pipefail

# Automated benchmark runner for examples/compare.rs
# Produces bench_results.csv in the repo root.

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_DIR"

FEATURES="libbz2 parallel"
EXAMPLE="compare"
ITERS=3
OUT="bench_results.csv"

echo "input_mb,crab_avg_s,crab_mb_s,lib_avg_s,lib_mb_s,speedup" > "$OUT"

for sz in 1 5 10 50 100; do
  echo "Running gen:${sz} MB (iters=${ITERS})..."
  out=$(cargo run --release --example "$EXAMPLE" --features "$FEATURES" -- gen:${sz} $ITERS 2>&1)
  echo "$out" > /tmp/compare_out.txt
  crab_line=$(printf "%s\n" "$out" | grep "crabz2 average")
  lib_line=$(printf "%s\n" "$out" | grep "libbz2 average")

  crab_avg=$(printf "%s\n" "$crab_line" | sed -E 's/.*crabz2 average: ([0-9.]+)s.*/\1/')
  crab_mbs=$(printf "%s\n" "$crab_line" | sed -E 's|.*-> ([0-9.]+) MB/s.*|\1|')
  lib_avg=$(printf "%s\n" "$lib_line" | sed -E 's/.*libbz2 average: ([0-9.]+)s.*/\1/')
  lib_mbs=$(printf "%s\n" "$lib_line" | sed -E 's|.*-> ([0-9.]+) MB/s.*|\1|')

  speedup=$(awk -v l="$lib_avg" -v c="$crab_avg" 'BEGIN{printf "%.2f", l/c}')

  echo "${sz},${crab_avg},${crab_mbs},${lib_avg},${lib_mbs},${speedup}" >> "$OUT"
done

echo "Results written to $OUT"
cat "$OUT"
