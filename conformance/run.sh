#!/usr/bin/env bash
# conformance/run.sh — one-shot Test262 entry point for humans and CI.
#
# Usage:
#   ./conformance/run.sh                          # language, 4 jobs, human summary
#   ./conformance/run.sh --filter built-ins/Array --jobs 8 --verbose
#   ./conformance/run.sh --filter language --format tap --tap-out /tmp/t262.tap
#
# Any arguments are forwarded to test262-runner. The script ensures the
# Test262 checkout exists and then runs the runner.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TEST262_DIR="$REPO_ROOT/conformance/test262"

# --- Ensure checkout -------------------------------------------------------

GITLINK_ENTRY="$(git -C "$REPO_ROOT" ls-tree HEAD -- conformance/test262 2>/dev/null || true)"
read -r GITLINK_MODE GITLINK_TYPE EXPECTED_TEST262 GITLINK_PATH <<< "$GITLINK_ENTRY"
if [[ "$GITLINK_MODE" != 160000 || "$GITLINK_TYPE" != commit || -z "$EXPECTED_TEST262" ]]; then
  echo "No Test262 gitlink found at HEAD: conformance/test262" >&2
  exit 1
fi

TEST262_REPO_URL="https://github.com/tc39/test262.git"
if ! git -C "$TEST262_DIR" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "Test262 checkout not found at $TEST262_DIR; initializing pinned checkout." >&2
  mkdir -p "$TEST262_DIR"
  git -C "$TEST262_DIR" init
  if ! git -C "$TEST262_DIR" remote get-url upstream >/dev/null 2>&1; then
    git -C "$TEST262_DIR" remote add upstream "$TEST262_REPO_URL"
  fi
  if ! git -C "$TEST262_DIR" fetch --depth 1 upstream "$EXPECTED_TEST262"; then
    echo "Failed to fetch Test262 object $EXPECTED_TEST262 from $TEST262_REPO_URL." >&2
    echo "The checkout is pinned to the repository gitlink; refusing to use latest upstream." >&2
    exit 1
  fi
else
  if ! git -C "$TEST262_DIR" cat-file -e "$EXPECTED_TEST262^{commit}" >/dev/null 2>&1; then
    if ! git -C "$TEST262_DIR" remote get-url upstream >/dev/null 2>&1; then
      git -C "$TEST262_DIR" remote add upstream "$TEST262_REPO_URL"
    fi
    if ! git -C "$TEST262_DIR" fetch --depth 1 upstream "$EXPECTED_TEST262"; then
      echo "Failed to fetch Test262 object $EXPECTED_TEST262 from $TEST262_REPO_URL." >&2
      echo "The checkout is pinned to the repository gitlink; refusing to use latest upstream." >&2
      exit 1
    fi
  fi
fi

ACTUAL_TEST262="$(git -C "$TEST262_DIR" rev-parse HEAD 2>/dev/null || true)"
if [[ "$ACTUAL_TEST262" != "$EXPECTED_TEST262" ]]; then
  if ! git -C "$TEST262_DIR" checkout --detach "$EXPECTED_TEST262"; then
    echo "Failed to check out pinned Test262 object $EXPECTED_TEST262." >&2
    exit 1
  fi
  ACTUAL_TEST262="$(git -C "$TEST262_DIR" rev-parse HEAD)"
fi

if [[ "$ACTUAL_TEST262" != "$EXPECTED_TEST262" ]]; then
  echo "Test262 checkout is at $ACTUAL_TEST262; expected $EXPECTED_TEST262." >&2
  exit 1
fi

echo "Using Test262 checkout $TEST262_DIR at $ACTUAL_TEST262" >&2

# --- Default args when none provided ---------------------------------------

if [[ $# -eq 0 ]]; then
  set -- --filter language --format human
  echo "No args provided — defaulting to: $*" >&2
fi

# --- Run -------------------------------------------------------------------

echo "Running: cargo run --profile runner -p test262-runner -- $*" >&2
exec cargo run --profile runner -p test262-runner -- "$@"
