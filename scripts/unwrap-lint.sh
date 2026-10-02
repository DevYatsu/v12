#!/usr/bin/env bash
# scripts/unwrap-lint.sh — W0.3: make the unwrap/expect/panic policy REAL.
# Companion to scripts/unwrap-allowlist.txt (docs/architecture-maintainability-plan.md
# §4, task W0.3).
#
# WHY THIS FILE EXISTS
#   The root Cargo.toml header claimed that an audited-site allowlist "blocks
#   new ones". No such script or list existed in any branch: the claim was
#   false, and `[workspace.lints.clippy]`'s unwrap_used / expect_used / panic
#   lints were warn-only with nothing consuming the warnings. This script is
#   the enforcement the comment promised.
#
# WHAT IT ENFORCES
#   Every `unwrap()`, `expect(`/`expect_err(`, `panic!(`, and `unreachable!(`
#   occurrence in non-test code must be accounted for in
#   scripts/unwrap-allowlist.txt, per (file, KIND):
#     * a hit in a (file, KIND) pair with NO allowlist entry  -> violation
#     * more hits in a (file, KIND) pair than entries           -> violation
#     * MORE entries than hits (a site that no longer exists) -> violation
#   The third rule is the difference from scripts/freeze-lint.sh, which
#   deliberately does not flag surplus entries — that omission is how a stale
#   PROP_GET1 entry silently inflated a cap (documented in W0.2). Here a
#   surplus entry is an ERROR: the allowlist is a ratchet and must shrink
#   monotonically. Deleting a site means deleting its line from the allowlist
#   in the same commit.
#
#   `:line` numbers in the allowlist are documentary (they drift with edits),
#   exactly as in scripts/freeze-allowlist.txt. Enforcement is per
#   (file, KIND) counts, so ordinary edits never produce a false failure; run
#   `--bless` to refresh the line numbers after a refactor.
#
# TEST CODE IS OUT OF SCOPE, BY PATH — the same rule scripts/freeze-lint.sh
# uses, deliberately reused rather than reinvented:
#   * any path component named `tests`  -> crates/*/tests/*.rs (integration)
#   * any basename named `tests.rs`     -> the `#[cfg(test)] mod tests;` file
#   * any basename matching `*_tests.rs` -> a sibling split-out test module,
#     e.g. `src/opspec_tests.rs`. Narrower than the two above, and included only
#     because it bit in practice: a lane wrote a correctly `#[cfg(test)]`-gated
#     module under that name and this gate flagged its 39 test assertions as
#     production panic sites. A gate that cries wolf on test code trains people
#     to route around it, which is worse. The rule stays name-shaped rather than
#     cfg-detection-shaped, so a PRODUCTION file named `foo_tests.rs` would be
#     excluded; accepted and documented rather than hidden.
# Implemented as `--exclude-dir=tests --exclude=tests.rs --exclude=*_tests.rs`.
# Deliberate consequences (identical to freeze-lint.sh):
#   * `#[cfg(test)] mod tests { … }` INLINE inside a production file stays IN
#     scope: it is not a path, and excluding it by line range would be fragile
#     enough to hide real sites. Those entries are marked `cfg-test` in the
#     allowlist's note column.
#   * `conformance/harness/src/*.rs` is workspace member code and stays IN
#     scope, like every other crate.
#   * Two entries are doc-comment occurrences (`//! ctx.eval(…).unwrap()` in a
#     doctest, `/// cells.guard_for(…).unwrap()`). They are counted because the
#     scan is textual and a file's cap must cover every hit it produces; they
#     are not panics.
#
# USAGE
#   ./scripts/unwrap-lint.sh            # check (exit 1 on violation)
#   ./scripts/unwrap-lint.sh --bless    # regenerate the allowlist from the tree
#   --bless REVIEW THE DIFF. It records whatever is in the tree, including new
#   sites; it is not an approval.
#
# CI: .github/workflows/ci.yml, `architecture` job, after scripts/freeze-lint.sh.
# No runtime behaviour change.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ALLOW="$ROOT/scripts/unwrap-allowlist.txt"
cd "$ROOT"

MODE="check"
if [ "$#" -gt 1 ]; then
    echo "usage: $0 [--bless]" >&2
    exit 2
elif [ "$#" -eq 1 ]; then
    MODE="$1"
    if [ "$MODE" != "--bless" ]; then
        echo "usage: $0 [--bless]" >&2
        exit 2
    fi
fi

# scan <KIND> <ERE> — occurrences as "path:line:match", one per occurrence.
#
# PORTABILITY: the patterns use `[(]` / `[!]`, never `\(` / `\)`. In POSIX ERE
# `\(` is a GROUPING escape, and BSD grep (macOS) implements it that way, so
# `\.expect\(\)` silently degrades to "literal .expect" and matches nothing
# while GNU grep (Linux CI) reads it as a literal paren. A pattern that means
# two different things on the developer's machine and on CI is exactly the bug
# class this repo keeps paying for — use a character class.
scan() {
    grep -rnE --include='*.rs' --exclude-dir=tests --exclude='tests.rs' \
        --exclude='*_tests.rs' \
        -o -e "$2" crates/ conformance/ 2>/dev/null || true
}

unwrap_pat='\.unwrap[(]'
expect_pat='\.expect[(]|\.expect_err[(]'
panic_pat='panic![(]'
unreachable_pat='unreachable![(]'

KINDS="unwrap expect panic unreachable"

# ---------------------------------------------------------------- bless mode
if [ "$MODE" = "--bless" ]; then
    tmp_hits="$(mktemp)"
    for kind in $KINDS; do
        eval "pat=\$${kind}_pat"
        scan "$kind" "$pat" | awk -v k="$kind" -F: '{print $1" "k" "$2}' >> "$tmp_hits"
    done
    sort -k1,1 -k2,2 -k3,3n "$tmp_hits" -o "$tmp_hits"

    # Best-effort label for inline `#[cfg(test)]` blocks (notes only, never
    # enforcement — see scripts/cfg-test-lines.awk).
    cfg_test_pairs="$(mktemp)"
    find crates conformance -name '*.rs' -not -path '*/tests/*' -not -name tests.rs \
        | sort | xargs awk -f scripts/cfg-test-lines.awk > "$cfg_test_pairs"

    total="$(wc -l < "$tmp_hits" | tr -d ' ')"
    {
        cat <<'HEADER'
# scripts/unwrap-allowlist.txt — audited unwrap/expect/panic inventory.
# Enforced by scripts/unwrap-lint.sh (W0.3); see that script's header for the
# policy, the path-based test exclusion, and why :line is documentary while
# per-(file, KIND) counts are not.
#
# POLICY
#   This file is the AUDITED SET. It exists so that the warn-level
#   unwrap_used/expect_used/panic lints in [workspace.lints.clippy] can
#   eventually become errors: every entry below is debt that was looked at and
#   accepted, and the guard fails if a site appears that is not here. New sites
#   must NOT be added to this file to make a gate green — replace the unwrap.
#
# FORMAT
#   "<path>:<line>  <KIND>  <note>"
#     KIND is one of: unwrap | expect | panic | unreachable
#     note is the trimmed source line, truncated; `cfg-test` marks an inline
#     #[cfg(test)] mod tests block (in scope by the path rule, see the script).
#
# RATCHET POLICY
#   Counts only ever go DOWN. scripts/unwrap-lint.sh fails on surplus entries,
#   so removing a site forces its line to be deleted here in the same commit.
#
HEADER
        echo "# TOTALS: ${total} sites"
        awk '{ split($1, a, "/"); crate=a[2]; if (crate == "harness") crate="conformance/harness"; n[crate]++; kind[$3]++ }
             END { for (c in n) printf "#   %-24s %d\n", c, n[c] }' "$tmp_hits" | sort
        awk '{ kind[$2]++ } END { for (k in kind) printf "#   %-24s %d\n", k, kind[k] }' "$tmp_hits" | sort
        echo "#"
        echo "# --- entries ---"
        while read -r path kind line; do
            src="$(sed -n "${line}p" "$path" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]][[:space:]]*/ /g' | cut -c1-88)"
            if grep -qxF "$path:$line" "$cfg_test_pairs"; then
                src="[cfg-test] $src"
            fi
            printf '%s:%s  %s  %s\n' "$path" "$line" "$kind" "$src"
        done < "$tmp_hits"
    } > "$ALLOW"
    rm -f "$tmp_hits" "$cfg_test_pairs"
    echo "unwrap-lint: blessed $ALLOW ($total sites)"
    exit 0
fi

# ---------------------------------------------------------------- check mode
if [ ! -f "$ALLOW" ]; then
    echo "unwrap-lint: FAIL — $ALLOW is missing; run '$0 --bless' and review the diff." >&2
    exit 1
fi

fail=0

# Allowlist entries -> "path kind count" (section/format lines are comments).
entries="$(mktemp)"
sites="$(mktemp)"
trap 'rm -f "$entries" "$sites"' EXIT

awk '$1 !~ /^#/ && NF >= 2 { split($1, a, ":"); f=a[1]; print f, $2 }' "$ALLOW" \
    | sort | uniq -c | awk '{print $2" "$3" "$1}' | sort > "$entries"

for kind in $KINDS; do
    eval "pat=\$${kind}_pat"
    scan "$kind" "$pat" | awk -v k="$kind" -F: '{print $1" "k}' \
        | sort | uniq -c | awk '{print $2" "$3" "$1}' | sort >> "$sites"
done

while read -r file kind count; do
    [ -z "$file" ] && continue
    allowed="$(awk -v f="$file" -v k="$kind" '$1 == f && $2 == k {print $3}' "$entries")"
    if [ -z "$allowed" ]; then
        echo "UNWRAP-VIOLATION [$kind]: unlisted site in $file ($count hit(s))" >&2
        fail=1
    elif [ "$count" -gt "$allowed" ]; then
        echo "UNWRAP-VIOLATION [$kind]: $file has $count hit(s), allowlisted $allowed" >&2
        fail=1
    fi
done < "$sites"

while read -r file kind allowed; do
    [ -z "$file" ] && continue
    count="$(awk -v f="$file" -v k="$kind" '$1 == f && $2 == k {print $3}' "$sites")"
    if [ -z "$count" ]; then
        echo "UNWRAP-VIOLATION [$kind]: stale allowlist entry for $file ($allowed entr(ies), 0 sites) —" \
             "delete the line(s); the allowlist must shrink" >&2
        fail=1
    elif [ "$allowed" -gt "$count" ]; then
        echo "UNWRAP-VIOLATION [$kind]: $file lists $allowed entr(ies) for $count site(s) —" \
             "delete the surplus; the allowlist must shrink" >&2
        fail=1
    fi
done < "$entries"

if [ "$fail" -eq 0 ]; then
    echo "unwrap-lint: OK ($(awk '{s+=$3} END {print s+0}' "$entries") audited non-test site(s), no new ones)"
else
    echo "unwrap-lint: FAILED — see violations above" >&2
fi
exit "$fail"