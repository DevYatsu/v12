#!/usr/bin/env bash
# scripts/update-conformance-baseline.sh — regenerate the committed conformance
# baseline that scripts/../conformance/faildiff.sh compares against in CI.
#
# WHY THIS EXISTS
#   Until now CI compared nothing. It ran test262 and then propagated "exit 1 if
#   any test failed" (.github/workflows/ci.yml, old lines 68-72), which at 77.6 %
#   language conformance can NEVER be green — a permanently red gate teaches
#   reviewers to ignore it. The only tool that answers "did I regress anything?"
#   mechanically is conformance/faildiff.sh, and it was never invoked. A pass
#   rate can rise while new tests break; a fail SET cannot lie that way.
#
# WHEN TO RUN IT
#   Only when a conformance improvement is intended and verified. The baseline
#   is the definition of "no regression", so regenerating it to make a red gate
#   green defeats the entire gate. Procedure:
#     1. Establish the previous baseline's commit:  git log -1 --format=%H -- \
#        conformance/baseline
#     2. Run test262 at that commit and at HEAD, and compare the two fail sets:
#        conformance/faildiff.sh BEFORE.json AFTER.json
#     3. Only if BROKE and REGRESSED are 0, regenerate and commit.
#   If you are doing a refactor rather than a conformance improvement, do NOT
#   touch this file — a raise here must be justified by a faildiff in the same
#   commit message.
#
# USAGE
#   ./scripts/update-conformance-baseline.sh [filter]     # default: language
#
# The output is reduced to the four fields faildiff.sh actually reads (path,
# status, duration_ms, stalled). The full runner report keeps `message` and the
# summary block; faildiff.sh never reads either, so committing them would add
# ~3 MB of bytes that no gate consumes. Diagnostics for a BROKE come from the
# AFTER artifact CI uploads, which does carry messages.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

FILTER="${1:-language}"
SUITE="${FILTER%%/*}"
OUT="conformance/baseline/${SUITE}.json"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "==> running test262 --filter ${FILTER} (this builds the 'runner' profile; ~15 min cold)"
# --format tap,json is required together: `--format json --json-out F` alone
# silently writes nothing (CONTEXT.md).
./conformance/run.sh \
  --filter "$FILTER" \
  --jobs 4 \
  --format tap,json \
  --json-out "$TMP/report.json" \
  --tap-out "$TMP/report.tap"

if [ ! -s "$TMP/report.json" ]; then
  echo "update-conformance-baseline: FAIL — no JSON report produced" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUT")"
python3 - "$TMP/report.json" "$OUT" <<'PY'
import json, sys
src, dst = sys.argv[1], sys.argv[2]
d = json.load(open(src))
keep = [{k: r[k] for k in ("path", "status", "duration_ms", "stalled") if k in r}
        for r in d["results"]]
json.dump({"timeout_ms": d.get("timeout_ms"), "results": keep},
          open(dst, "w"), separators=(",", ":"))
n = len(keep)
passed = sum(1 for r in keep if r.get("status") == "pass")
print(f"  wrote {dst}: {n} tests, {passed} pass ({100.0 * passed / n:.1f} %)")
PY

echo "==> self-check: the new baseline must diff clean against itself"
./conformance/faildiff.sh "$OUT" "$OUT"

cat <<EOF

Next steps:
  1. git add ${OUT}
  2. In the commit message, paste the faildiff that justifies the raise:
       conformance/faildiff.sh <old-baseline> ${OUT}
     It must report BROKE 0 and REGRESSED 0.
EOF
