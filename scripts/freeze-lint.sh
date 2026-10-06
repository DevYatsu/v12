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

# check <TAG> <grep-pattern> [scoped-file ...]
#
# With no file list the rule scans every crate. With a file list it scans exactly
# those files, which is what ARRAY_DIRECT_ACCESS wants: the invariant is about the
# ARRAY builtin path, not about the whole tree, and a repo-wide scan for this
# pattern would fire on ordinary heap access in code that is not doing array-like
# index/length work at all.
check() {
    local tag="$1" pat="$2"
    shift 2
    local -a scope=("$@")
    local tmp_hits tmp_allowed
    tmp_hits="$(mktemp)"
    tmp_allowed="$(mktemp)"
    trap 'rm -f "$tmp_hits" "$tmp_allowed"' RETURN

    if [ "${#scope[@]}" -gt 0 ]; then
        # -E is REQUIRED here and deliberately absent below. The scoped rule's
        # pattern uses alternation and a bracket class, which are literals in BRE.
        # The three unscoped patterns must stay BRE: "properties.get(1)" relies on
        # "(" being a LITERAL, and under -E it would become a group and match
        # "properties.get1" instead.
        grep -EHn -e "$pat" "${scope[@]}" 2>/dev/null | cut -d: -f1 | sort | uniq -c | awk '{print $2" "$1}' | sort > "$tmp_hits" || true
    else
        grep -rFn --include='*.rs' --exclude-dir=tests --exclude='tests.rs' -e "$pat" crates/ 2>/dev/null | cut -d: -f1 | sort | uniq -c | awk '{print $2" "$1}' | sort > "$tmp_hits" || true
    fi
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

# ARRAY_DIRECT_ACCESS — the anti-drift half of ADR-0006.
#
# ADR-0004 existed because Array.prototype had SEVEN parallel implementations of
# index/length access on a generic receiver (read_index, write_index, dense_bound,
# array_len, array_length, sync_length, pop_read, pop_delete). Each did its own
# element access with its own coercion and its own hole handling, and each diverged
# from node differently. Batches 1-8 deleted all seven in favour of
# internal_methods::dispatch_get/dispatch_has/dispatch_set.
#
# Nothing stops the eighth. This rule watches the MECHANISM rather than the names:
# a new helper that acquires the element store or property bag directly instead of
# delegating to dispatch_* is a re-implementation of [[Get]]/[[HasProperty]], which
# is exactly the defect class. Names were rejected as the signal deliberately --
# the sanctioned set alone is 18 helpers in array.rs and 12 in call_setup.rs, and
# callback_len KEEPS its name after its body was rewritten, so a name rule would
# need a 30-entry allowlist to catch names nobody has chosen yet.
#
# THE PATTERN MUST COVER BOTH SHAPES, and a first attempt proved why. Matching only
# `.properties[` / `.elements[` catches the LEAST likely form: the deleted readers
# all did `let props = heap.properties(o)` and then indexed the local. A probe that
# did exactly that passed the gate. So the rule matches the ACQUISITION
# `.properties(` / `.elements(` as well as the indexing `.properties[` /
# `.elements[`. Verified by negative test: a plausible eighth reader
# (`fn read_index_at(..) { let props = heap.properties(o); props[k].value }`) fails
# the gate on the acquisition form.
#
# Baseline: array.rs is at ZERO (proof the seven are genuinely gone), object_ops.rs
# has 1 and call_setup.rs has 21, all indexing-form sites in the callback seam where
# hand-rolled access is deliberate. Those 22 are grandfathered; only additions fail.
check "ARRAY_DIRECT_ACCESS" '\.(properties|elements)[(\[]' \
    crates/v12-engine/src/builtins/array.rs \
    crates/v12-interp/src/object_ops.rs \
    crates/v12-interp/src/call_setup.rs

if [ "$fail" -eq 0 ]; then
    echo "freeze-lint: OK (no new sites)"
else
    echo "freeze-lint: FAILED — see violations above" >&2
fi
exit "$fail"
