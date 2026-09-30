#!/usr/bin/env bash
# scripts/check-architecture.sh — design-boundary guardrails
# (docs/architecture-maintainability-plan.md, task W0.1).
#
# WHY THIS FILE EXISTS
#   .github/workflows/ci.yml referenced ./scripts/check-architecture.sh, which
#   had never existed in any branch (0 commits). The `architecture` job failed
#   at that step, and because the following clippy step has no `if: always()`,
#   clippy never ran in CI at all. This script makes the step real.
#
# WHAT IT ENFORCES
#   1. unsafe discipline — `[workspace.lints.rust] unsafe_code = "deny"`, with
#      exactly one audited exception (the HostClosure raw-pointer handle in
#      crates/v12-heap/src/function.rs).
#   2. clippy across every crate and feature combination.
#
# DELIBERATELY REPORT-ONLY (not `-D warnings`)
#   Measured on 92e10484: `cargo clippy --workspace --all-features` emits 275
#   warnings, and `-- -D warnings` ALREADY FAILS with 15 errors in
#   v12-bccompiler (e.g. collapsible_if at crates/v12-bccompiler/src/unit.rs:395).
#   Turning on `-D warnings` today would trade one permanently-red gate for
#   another. This script therefore prints the warning count and exits 0, so the
#   debt becomes VISIBLE in CI logs instead of absent.
#
#   Flip to `-D warnings` only when BOTH hold:
#     (a) the clippy warning count reaches 0, and
#     (b) scripts/unwrap-allowlist.txt exists (task W0.3).
#   (b) matters because [workspace.lints.clippy] sets unwrap_used/expect_used/
#   panic to "warn" — so `-D warnings` also promotes the 114 audited
#   unwrap/expect/panic sites into build errors. Fixing (a) without (b) turns
#   every one of them red at once.
#
# Usage:  ./scripts/check-architecture.sh
# CI:     .github/workflows/ci.yml, `architecture` job.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "==> clippy: reporting warnings (report-only; see header for the -D warnings gate)"
# Scope deliberately matches the CI step this script replaces
# (`cargo clippy --workspace --all-features`): lib+bins only. Adding
# --all-targets pulls in test code and reports 1063 warnings instead of 275,
# which would make the budget below meaningless.
set +e
clippy_out="$(cargo clippy --workspace --all-features 2>&1)"
clippy_status=$?
set -e
echo "$clippy_out"

if [ "$clippy_status" -ne 0 ]; then
  echo "check-architecture: FAIL — clippy exited $clippy_status (a hard error, not a warning)"
  exit "$clippy_status"
fi

warnings="$(printf '%s\n' "$clippy_out" | grep -cE '^warning: ' || true)"
echo
echo "check-architecture: clippy OK with ${warnings} warning(s)."
if [ "$warnings" -gt 0 ]; then
  echo "check-architecture: NOTE — ${warnings} warning(s) outstanding. This gate is report-only by"
  echo "  design until the count reaches 0 AND scripts/unwrap-allowlist.txt exists."
  echo "  Budget: 275 warnings as of 92e10484. Do not raise this number."
fi

# The single audited unsafe site. If it disappears, this guard tells you rather
# than letting the justification in Cargo.toml go stale.
audited_unsafe="crates/v12-heap/src/function.rs"
if [ ! -f "$audited_unsafe" ]; then
  echo "check-architecture: FAIL — audited unsafe site $audited_unsafe is gone;"
  echo "  drop the scoped #[allow(unsafe_code)] and update Cargo.toml's comment."
  exit 1
fi

echo "check-architecture: OK"
