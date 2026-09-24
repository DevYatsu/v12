#!/usr/bin/env bash
# Record a reproducible Test262 coverage baseline under target/.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN="$ROOT/conformance/run.sh"
LABEL="${1:-manual}"
OUT="$ROOT/target/test262-coverage/$LABEL"
mkdir -p "$OUT"

"$RUN" --test262-root "$ROOT/conformance/test262" --filter 'language/*' --list \
  | sed '$d' >"$OUT/list.txt"
printf 'test262=%s\n' "$(git -C "$ROOT/conformance/test262" rev-parse HEAD)" \
  >"$OUT/summary.txt"
printf 'language_files=%s\n' "$(wc -l <"$OUT/list.txt" | tr -d ' ')" \
  >>"$OUT/summary.txt"

for spec in \
  'language/eval-code' \
  'annexB/language/eval-code' \
  'language/arguments-object' \
  'language/module-code' \
  'built-ins/Array' \
  'built-ins/Object' \
  'built-ins/Promise'; do
  safe="${spec//\//_}"
  set +e
  "$RUN" --filter "$spec" --jobs 4 --format human \
    >"$OUT/$safe.human" 2>"$OUT/$safe.stderr"
  human_status=$?
  "$RUN" --filter "$spec" --jobs 4 --format tap \
    --tap-out "$OUT/$safe.tap" \
    >"$OUT/$safe.stdout" 2>"$OUT/$safe.stderr.tap"
  tap_status=$?
  set -e
  printf '%s human=%s tap=%s\n' "$spec" "$human_status" "$tap_status" \
    >>"$OUT/summary.txt"
done
