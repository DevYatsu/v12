#!/usr/bin/env bash
# scripts/freeze-lint.sh — Phase 1 (Freeze) guard, docs/builtins-arch-plan.md §5 step 1.
#
# Freezes three pre-Ctx idioms so no NEW sites appear during migration:
#   PK_PUSH   `property_keys.push`   (target: only the Ctx install family, §3)
#   PROP_GET1 `properties.get(1)`    (target: Ctx realm/intrinsic reader, §1.1)
#   ERR_VAL   `error_value("`        (target: ctx.{type,range,syntax,reference}_error, §4.2)
#
# Grandfathered sites live in scripts/freeze-allowlist.txt as
# "<TAG> <file>:<line>" entries. Enforcement is file-membership + per-file
# count cap (line numbers drift with edits, so they are documentary only):
# a hit in a file not listed for its TAG fails, as does a hit count above
# the number of allowlist entries for that (TAG, file).
#
# TEST CODE IS OUT OF SCOPE. The three idioms are architecture debt in
# production paths; a hit in test code is not a violation, and counting it
# only dilutes the signal (it did: the guard went red on 2 test files while
# 14 real violations were missed). The rule is a property of the PATH, never
# a list of file names:
#   * any path component named `tests`  -> crates/*/tests/*.rs (integration)
#   * any basename named `tests.rs`     -> the `#[cfg(test)] mod tests;` file
# Implemented as `--exclude-dir=tests --exclude=tests.rs`.
# Deliberate consequences:
#   * `#[cfg(test)] mod tests { … }` INLINE inside a production file stays IN
#     scope: it is not a path, and parsing line ranges to exclude it would be
#     fragile enough to hide real sites. `crates/v12-interp/src/feedback.rs:360`
#     is such a file and currently has zero hits, so nothing is affected.
#   * `crates/v12-bccompiler/src/tests.rs` is excluded by the same rule even
#     though it holds no hits today; the rule must not depend on the tree.
#   * `crates/v12-engine/src/lib.rs` (host-API tests, `mod tests;`) stays IN
#     scope: its `tests` is a module name, not a path component. It has no
#     hits today, so this costs nothing and keeps the rule path-shaped.
#
# Usage: ./scripts/freeze-lint.sh        # exit 0 = frozen, 1 = new violation
# CI: .github/workflows/ci.yml, `architecture` job. No runtime behavior change.
#
# To retire a site after migrating it: delete its allowlist line (the lint
# then enforces the lower count automatically).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ALLOW="$ROOT/scripts/freeze-allowlist.txt"
cd "$ROOT"

fail=0

# check <TAG> <grep-pattern>
check() {
    local tag="$1" pat="$2"
    local tmp_hits tmp_allowed
    tmp_hits="$(mktemp)"
    tmp_allowed="$(mktemp)"
    trap 'rm -f "$tmp_hits" "$tmp_allowed"' RETURN

    grep -rFn --include='*.rs' --exclude-dir=tests --exclude='tests.rs' -e "$pat" crates/ 2>/dev/null | cut -d: -f1 | sort | uniq -c | awk '{print $2" "$1}' | sort > "$tmp_hits" || true
    # allowlist: "<TAG> <file>[:<line>]" -> "<file> <count>"
    awk -v t="$tag" '$1 == t { f=$2; sub(/:[0-9]+$/, "", f); print f }' "$ALLOW" | sort | uniq -c | awk '{print $2" "$1}' | sort > "$tmp_allowed"

    while read -r file count; do
        [ -z "$file" ] && continue
        local allowed
        allowed="$(awk -v f="$file" '$1 == f {print $2}' "$tmp_allowed")"
        if [ -z "$allowed" ]; then
            echo "FREEZE-VIOLATION [$tag]: new site in unlisted file: $file ($count hit(s))" >&2
            fail=1
        elif [ "$count" -gt "$allowed" ]; then
            echo "FREEZE-VIOLATION [$tag]: $file has $count hit(s), allowlisted $allowed" >&2
            fail=1
        fi
    done < "$tmp_hits"
}

check "PK_PUSH" 'property_keys.push'
check "PROP_GET1" 'properties.get(1)'
check "ERR_VAL" 'error_value("'

if [ "$fail" -eq 0 ]; then
    echo "freeze-lint: OK (no new sites)"
else
    echo "freeze-lint: FAILED — see violations above" >&2
fi
exit "$fail"
