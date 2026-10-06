#!/usr/bin/env bash
# Merge the sharded cargo-mutants baseline (shards 1-6) into one outcome set.
# Tie-break rule: when a mutant appears in both `caught` and `missed` (shards
# that ran before killing-test commits record stale `missed` entries), prefer
# `caught`. See plans/2026-10-06-cargo-mutants-oom-safe-sharded-execution-plan.md.
set -euo pipefail
root=reports/mutants
out="$root/2026-10-06-audio-core-baseline-merged"
mkdir -p "$out"
: > "$out/caught.txt"; : > "$out/missed.txt"; : > "$out/timeout.txt"; : > "$out/unviable.txt"
for i in 0 1 2 3 4 5; do
  d="$root/2026-10-06-baseline-shard$i/mutants.out"
  [ -d "$d" ] || { echo "shard $i missing — skipping"; continue; }
  cat "$d/caught.txt"  >> "$out/caught.txt"
  cat "$d/missed.txt"  >> "$out/missed.txt"
  cat "$d/timeout.txt" >> "$out/timeout.txt"
  cat "$d/unviable.txt" >> "$out/unviable.txt"
done
sort -u "$out/caught.txt"  -o "$out/caught.txt"
sort -u "$out/timeout.txt" -o "$out/timeout.txt"
sort -u "$out/unviable.txt" -o "$out/unviable.txt"
# missed = everything seen as missed that is NOT in caught or timeout
sort -u "$out/missed.txt" -o "$out/missed.txt"
comm -23 "$out/missed.txt" "$out/caught.txt"  > "$out/missed.tmp"
comm -23 "$out/missed.tmp" "$out/timeout.txt" > "$out/missed.txt"
rm "$out/missed.tmp"
echo "== merged baseline ($out) =="
c=$(wc -l < "$out/caught.txt"); m=$(wc -l < "$out/missed.txt")
t=$(wc -l < "$out/timeout.txt"); u=$(wc -l < "$out/unviable.txt")
echo "caught: $c  missed: $m  timeout: $t  unviable: $u"
echo "decided: $((c + m + t))  score: $(awk "BEGIN{printf \"%.1f%%\", ($c + $t) / ($c + $m + $t) * 100}")"
