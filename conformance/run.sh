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

if [[ ! -d "$TEST262_DIR/test" ]]; then
  echo "conformance/test262 checkout not found at $TEST262_DIR" >&2

  if [[ -f "$REPO_ROOT/.gitmodules" ]] && grep -q "test262" "$REPO_ROOT/.gitmodules" 2>/dev/null; then
    echo "Attempting: git submodule update --init --depth 1 conformance/test262" >&2
    if git -C "$REPO_ROOT" submodule update --init --depth 1 conformance/test262 2>&1; then
      echo "Submodule checkout succeeded." >&2
    else
      echo "Submodule update failed. Falling back to shallow clone." >&2
      rm -rf "$TEST262_DIR"
      git clone --depth 1 https://github.com/tc39/test262 "$TEST262_DIR"
    fi
  else
    echo "Attempting: git clone --depth 1 https://github.com/tc39/test262 $TEST262_DIR" >&2
    if ! git clone --depth 1 https://github.com/tc39/test262 "$TEST262_DIR" 2>&1; then
      echo "" >&2
      echo "Network clone failed. The runner can still execute with its minimal" >&2
      echo "harness polyfill, but many tests will be skipped (see conformance/README.md)." >&2
      echo "To retry manually:" >&2
      echo "  git clone --depth 1 https://github.com/tc39/test262 \"$TEST262_DIR\"" >&2
      echo "  or: git submodule add https://github.com/tc39/test262 conformance/test262" >&2
    fi
  fi
fi

if [[ -f "$TEST262_DIR/harness/assert.js" ]]; then
  echo "Using Test262 checkout at $TEST262_DIR ($(find "$TEST262_DIR/test" -name '*.js' | wc -l | tr -d ' ') tests, harness present)." >&2
elif [[ -d "$TEST262_DIR/test" ]]; then
  echo "Using Test262 checkout at $TEST262_DIR (harness missing — runner will use minimal polyfill)." >&2
else
  echo "No Test262 checkout found; proceeding with minimal polyfill (expect many skips)." >&2
fi

# --- Default args when none provided ---------------------------------------

if [[ $# -eq 0 ]]; then
  set -- --filter language --format human
  echo "No args provided — defaulting to: $*" >&2
fi

# --- Run -------------------------------------------------------------------

# The workspace `release` profile pins `panic = "abort"`, which defeats
# `catch_unwind` and lets one engine panic abort the whole suite. Conformance
# runs therefore use the default `dev` profile (no `--release`), which Cargo
# compiles with `panic = "unwind"`, so panicking tests report as
# `Fail: engine panic` and the run keeps going.
echo "Running: cargo run -p test262-runner -- $*" >&2
exec cargo run -p test262-runner -- "$@"
