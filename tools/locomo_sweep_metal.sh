#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# LoCoMo candidate sweep + LongMemEval on a large dedicated host.
# Expects: repo at /root/hyphae (built release binary), datasets at
# /root/datasets/{locomo10.json,longmemeval_s_cleaned.json}.
set -uo pipefail

REPO=/root/hyphae
BIN=$REPO/target/release/hyphae
DATA=/root/datasets
OUT=/root/memory-results
mkdir -p "$OUT"
cd "$REPO"

run_locomo() {
  local name="$1"; shift
  echo "== LoCoMo candidate: $name =="
  python3 tools/long_term_memory_benchmarks.py \
    --binary "$BIN" \
    --benchmark locomo \
    --dataset "$DATA/locomo10.json" \
    --output "$OUT/locomo-$name.receipt.json" \
    --progress "$OUT/locomo-$name.trace.jsonl" \
    "$@" > "$OUT/locomo-$name.log" 2>&1 && echo "$name OK" || echo "$name FAILED"
}

# Baseline (frozen reference config).
run_locomo baseline &

# The published "enriched" family: multi-view + weighted RRF.
run_locomo enriched \
  --locomo-view timestamp --locomo-view timestamp-previous \
  --rrf-weight 2 --rrf-weight 1 &

# Enriched + analyzers (stop+stem).
run_locomo enriched-analyzed \
  --locomo-view timestamp --locomo-view timestamp-previous \
  --rrf-weight 2 --rrf-weight 1 \
  --analyzer-english-stop --analyzer-english-stem &

# Enriched + analyzers + tuned BM25.
run_locomo enriched-analyzed-k1b \
  --locomo-view timestamp --locomo-view timestamp-previous \
  --rrf-weight 2 --rrf-weight 1 \
  --analyzer-english-stop --analyzer-english-stem \
  --bm25-k1-micros 800000 --bm25-b-micros 400000 &

wait

# Second wave (4 more candidates in parallel).
run_locomo enriched-centered \
  --locomo-view timestamp --locomo-view timestamp-previous --locomo-view centered \
  --rrf-weight 2 --rrf-weight 1 --rrf-weight 1 \
  --analyzer-english-stop --analyzer-english-stem &

run_locomo enriched-sliceb \
  --locomo-view timestamp --locomo-view timestamp-previous \
  --rrf-weight 2 --rrf-weight 1 \
  --analyzer-english-stop --analyzer-english-stem \
  --slice-b &

run_locomo enriched-sliceb-cover \
  --locomo-view timestamp --locomo-view timestamp-previous \
  --rrf-weight 2 --rrf-weight 1 \
  --analyzer-english-stop --analyzer-english-stem \
  --slice-b --session-cover 4 &

run_locomo enriched-k1b-12-75 \
  --locomo-view timestamp --locomo-view timestamp-previous \
  --rrf-weight 2 --rrf-weight 1 \
  --analyzer-english-stop --analyzer-english-stem \
  --bm25-k1-micros 1200000 --bm25-b-micros 750000 &

wait

echo "== LoCoMo summary =="
for receipt in "$OUT"/locomo-*.receipt.json; do
  name=$(basename "$receipt" .receipt.json)
  python3 - "$receipt" "$name" << 'PYEOF'
import json, sys
receipt = json.load(open(sys.argv[1]))
overall = receipt.get("retrieval", {}).get("overall", {})
keys = [k for k in overall if "@10" in k]
summary = " ".join(f"{k}={overall[k]}" for k in sorted(keys))
print(f"{sys.argv[2]}: {summary}")
PYEOF
done

echo "=== SWEEP COMPLETO ==="
