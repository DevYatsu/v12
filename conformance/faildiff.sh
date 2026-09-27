#!/usr/bin/env bash
# conformance/faildiff.sh — regression gate for a conformance lane.
#
# A pass-rate rise can hide new failures: a lane that fixes 40 tests and breaks
# 10 shows net +30. This script compares the per-test status of two runner
# JSON reports and classifies every change, so "did I regress anything?" has a
# mechanical answer instead of a judgement call.
#
# Usage:
#   conformance/faildiff.sh BEFORE.json AFTER.json
#   conformance/faildiff.sh BEFORE.json AFTER.json --topic built-ins/Array
#
# Produces:
#   FIXED     tests that went fail|skip -> pass
#   BROKE     tests that went pass -> fail        (must be 0 to commit)
#   REGRESSED tests that went pass -> skip
#   NEW_FAIL  tests that entered the fail set from pass, or appeared fresh
#   FIXED_BY_SKIP tests that went fail -> skip     (gating, not a real fix)
#
# Exit codes: 0 = no BROKE and no REGRESSED, 1 = regression present.

set -euo pipefail

BEFORE="${1:?usage: faildiff.sh BEFORE.json AFTER.json [--topic SUBSTR]}"
AFTER="${2:?usage: faildiff.sh BEFORE.json AFTER.json [--topic SUBSTR]}"
shift 2
TOPIC="${1:-}"
[ "${1:-}" = "--topic" ] && TOPIC="${2:-}"

for f in "$BEFORE" "$AFTER"; do
  [ -f "$f" ] || { echo "missing report: $f" >&2; exit 2; }
done

python3 - "$BEFORE" "$AFTER" "$TOPIC" <<'PY'
import json, sys, collections

before_path, after_path, topic = sys.argv[1], sys.argv[2], sys.argv[3]

def load(path):
    with open(path) as fh:
        return {r["path"]: r["status"] for r in json.load(fh)["results"]}

before, after = load(before_path), load(after_path)

if topic:
    def keep(p):
        parts = p.split("/")
        return topic in p
    before = {k: v for k, v in before.items() if keep(k)}
    after = {k: v for k, v in after.items() if keep(k)}

fixed, broke, regressed, new_fail, fixed_by_skip = [], [], [], [], []

for path, b in before.items():
    a = after.get(path)
    if a is None:
        continue  # test removed from the corpus; not a lane regression
    if b != "pass" and a == "pass":
        fixed.append(path)
    elif b == "pass" and a == "fail":
        broke.append(path)
    elif b == "pass" and a == "skip":
        regressed.append(path)
    elif b == "fail" and a == "skip":
        fixed_by_skip.append(path)

for path, a in after.items():
    if path in before:
        continue
    if a == "fail":
        new_fail.append(path)

def topic_of(p):
    parts = p.split("/")
    return parts[1] if len(parts) > 1 and parts[0] in ("built-ins", "annexB") else parts[0]

def report(name, paths):
    if not paths:
        return
    print(f"\n{name}: {len(paths)}")
    counts = collections.Counter(topic_of(p) for p in paths)
    for t, n in counts.most_common(12):
        print(f"   {n:5d}  {t}")
    for p in sorted(paths)[:10]:
        print(f"      - {p}")
    if len(paths) > 10:
        print(f"      ... and {len(paths) - 10} more")

print(f"before: {len(before)} tests, {sum(1 for v in before.values() if v == 'pass')} pass")
print(f"after:  {len(after)} tests, {sum(1 for v in after.values() if v == 'pass')} pass")
print(f"net pass delta: {sum(1 for v in after.values() if v == 'pass') - sum(1 for v in before.values() if v == 'pass'):+d}")

report("BROKE (pass -> fail)", broke)
report("REGRESSED (pass -> skip)", regressed)
report("NEW_FAIL (new to corpus, failing)", new_fail)
report("FIXED (fail -> pass)", fixed)
if fixed_by_skip:
    print(f"\nFIXED_BY_SKIP (fail -> skip): {len(fixed_by_skip)}  (gating, not a real fix)")

if broke or regressed:
    print("\nRESULT: REGRESSION — do not commit until these are explained or fixed.")
    sys.exit(1)
print("\nRESULT: clean — no pass regressed to fail or skip.")
PY
