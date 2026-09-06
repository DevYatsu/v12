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

    grep -rFn --include='*.rs' -e "$pat" crates/ 2>/dev/null | cut -d: -f1 | sort | uniq -c | awk '{print $2" "$1}' | sort > "$tmp_hits" || true
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
