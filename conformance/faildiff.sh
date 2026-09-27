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
#   MARGINAL  status changes by tests that were near the kill window in either
#             run (killed, or within MARGINAL_FRACTION of it)
#
# Why MARGINAL exists: a test that runs at 95% of the kill window can flip
# pass->fail on a loaded machine with no code change at all. Counting that as
# BROKE is crying wolf (it buries a real regression); ignoring it is worse
# (a real regression among slow tests becomes invisible). So it is reported
# on its own, never counted as a regression, and never silently dropped.
#
# Exit codes: 0 = no BROKE and no REGRESSED (MARGINAL only warns),
#             1 = regression present, 2 = usage/IO error.

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

# A test that spent at least this fraction of the kill window is "near" it: its
# verdict is a coin flip against machine load, so a status change is not
# evidence about the code. A test the parent actually killed (stalled=true) is
# near it by definition. At the default window 0.8 * 35 000 = 28 000 ms, i.e.
# just under the 30 000 ms cooperative deadline: "reached the deadline, or came
# close enough that load decided it". It is well above the 2 ms median of the
# stable population, so ordinary fast tests can never be marginal.
MARGINAL_FRACTION = 0.8

# More than this many marginal tests means the measurement itself is weak
# (usually: the window is too tight for the slice, or the box is overloaded).
MARGINAL_LOUD = 5

def load(path):
    with open(path) as fh:
        doc = json.load(fh)
    # timeout_ms and stalled are the runner's kill-window disclosures; a report
    # from an older runner has neither, in which case marginal detection is
    # simply off (and said to be off below).
    window = doc.get("timeout_ms")
    rows = {}
    for r in doc["results"]:
        rows[r["path"]] = {
            "status": r["status"],
            "duration_ms": r.get("duration_ms") or 0,
            "stalled": bool(r.get("stalled", False)),
        }
    return rows, window

def near_kill(row, window):
    if row["stalled"]:
        return True
    if not window:
        return False
    return row["duration_ms"] >= MARGINAL_FRACTION * window

before, before_window = load(before_path)
after, after_window = load(after_path)

if topic:
    def keep(p):
        parts = p.split("/")
        return topic in p
    before = {k: v for k, v in before.items() if keep(k)}
    after = {k: v for k, v in after.items() if keep(k)}

def marginal(p):
    """True if either run's row for `p` sat near that run's kill window."""
    b, a = before.get(p), after.get(p)
    return (b is not None and near_kill(b, before_window)) or \
           (a is not None and near_kill(a, after_window))

fixed, broke, regressed, new_fail, fixed_by_skip, marginal_flips = [], [], [], [], [], []

def classify(path, b, a):
    """Buckets a status transition, routing kill-window tests to MARGINAL."""
    if b is None and a == "fail":
        new_fail.append(path)
    elif b != "pass" and a == "pass":
        fixed.append(path)
    elif b == "pass" and a == "fail":
        broke.append(path)
    elif b == "pass" and a == "skip":
        regressed.append(path)
    elif b == "fail" and a == "skip":
        fixed_by_skip.append(path)
    else:
        return
    if marginal(path):
        # A transition touching the kill window says nothing about the code;
        # pull it out of the verdict buckets and report it separately.
        for bucket in (fixed, broke, regressed, new_fail, fixed_by_skip):
            if path in bucket:
                bucket.remove(path)
        marginal_flips.append(path)

for path, b in before.items():
    a = after.get(path)
    if a is None:
        continue  # test removed from the corpus; not a lane regression
    classify(path, b["status"], a["status"])

for path, a in after.items():
    if path in before:
        continue
    classify(path, None, a["status"])

def topic_of(p):
    parts = p.split("/")
    return parts[1] if len(parts) > 1 and parts[0] in ("built-ins", "annexB") else parts[0]

def report(name, paths, note=None):
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
    if note:
        print(f"      {note}")

print(f"before: {len(before)} tests, {sum(1 for v in before.values() if v['status'] == 'pass')} pass")
print(f"after:  {len(after)} tests, {sum(1 for v in after.values() if v['status'] == 'pass')} pass")
print(f"net pass delta: {sum(1 for v in after.values() if v['status'] == 'pass') - sum(1 for v in before.values() if v['status'] == 'pass'):+d}")
print(f"kill window: before={before_window or 'unreported'} ms, after={after_window or 'unreported'} ms")
if before_window and after_window and before_window != after_window:
    print("warning: the two runs used different kill windows; timings are not comparable")

report("BROKE (pass -> fail)", broke)
report("REGRESSED (pass -> skip)", regressed)
report("NEW_FAIL (new to corpus, failing)", new_fail)
report("FIXED (fail -> pass)", fixed)
if fixed_by_skip:
    print(f"\nFIXED_BY_SKIP (fail -> skip): {len(fixed_by_skip)}  (gating, not a real fix)")

if not (before_window or after_window):
    print("\nnote: neither report carries timeout_ms (pre-fix runner); "
          "MARGINAL detection is disabled and any slow-test flip counts as real.")
report("MARGINAL (status change near the kill window)", marginal_flips,
       note=f"not counted as regressions; re-run this slice with a larger "
            f"--test-timeout-ms to get a verdict")

# Loudness: marginal tests mean the measurement, not the code, is suspect.
if len(marginal_flips) > MARGINAL_LOUD:
    print(f"\nWARNING: {len(marginal_flips)} status changes sat near the kill window "
          f"(> {MARGINAL_LOUD}). This run's verdict is weak: treat it as a tie, re-run "
          f"with a larger --test-timeout-ms, and do not draw conclusions from it.")
elif marginal_flips:
    print(f"\nnote: {len(marginal_flips)} status change(s) sat near the kill window "
          f"(<= {MARGINAL_LOUD}); not treated as regressions, but worth a look.")

if broke or regressed:
    print("\nRESULT: REGRESSION — do not commit until these are explained or fixed.")
    sys.exit(1)
print("\nRESULT: clean — no pass regressed to fail or skip.")
PY
