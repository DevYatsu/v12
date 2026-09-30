#!/usr/bin/env bash
# nextest gate for the `surface` worktree.
#
# `conformance/harness`'s `deferred_topics_match_the_pinned_corpus` test reads
# `CARGO_MANIFEST_DIR/../test262/test`, and `conformance/test262` is a *gitlink*
# (submodule path) that a git worktree cannot materialise. A symlink at that
# exact path makes `git status` error out, so the link is created and destroyed
# around the run and nothing else may run git while it exists.
#
# The gitlink directory has to be removed first: `ln -s` into an existing
# directory creates the link *inside* it instead of at the path.
set -uo pipefail
cd "$(dirname "$0")/.."
REAL=/Users/yanis/Programming/v12/conformance/test262
if [ -d conformance/test262 ] && [ ! -L conformance/test262 ]; then
  rmdir conformance/test262 2>/dev/null || true
fi
ln -s "$REAL" conformance/test262
cargo nextest run --workspace "$@"
status=$?
unlink conformance/test262
# Leave the gitlink slot present but empty, as a fresh worktree has it.
mkdir -p conformance/test262
exit $status
