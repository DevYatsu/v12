#!/usr/bin/env bash
# Conformance baseline/after runner for the `surface` lane.
#   usage: run-conformance.sh <outdir>
set -euo pipefail
OUT="${1:?usage: run-conformance.sh <outdir>}"
ROOT=/Users/yanis/Programming/v12/conformance/test262
W=/Users/yanis/Programming/v12/.worktrees/surface
mkdir -p "$OUT"
cd "$W"
for f in Set Promise Iterator Symbol Error Object String language; do
  echo "=== $f"
  ./target/debug/test262-runner --test262-root "$ROOT" --filter "$f" --jobs 8 \
    --format human,tap,json --tap-out "$OUT/$f.tap" --json-out "$OUT/$f.json" \
    > "$OUT/$f.human" 2>&1 || true
  tail -1 "$OUT/$f.human" || true
done
echo ALLDONE
