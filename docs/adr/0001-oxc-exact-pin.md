# ADR-1 — Adopt oxc wholesale for the front end, pinned to exact versions

**Status:** Accepted.

Cited from `Cargo.toml:37` ("Front end (ADR-1)") and in force since the
bootstrap commit.

> **Provenance of this record.** The original ADR was written in
> `plan_idea.md` (a 474-line design document, added `3ea57570`, 2026-08-27),
> which was deleted by `8722ba90` (2026-09-17, "refactor: roadmap written
> down"). `docs/adr/` did not exist. Everything in **Context**, **Decision**
> and the first two sentences of **Consequences** below is *recovered
> verbatim or near-verbatim* from `git show 3ea57570:plan_idea.md` §5
> (lines 246-257) and §3 (line 79). Anything added since is marked
> **[reconstructed]** and is inference from the dependency set, not record.

Note the numbering: `plan_idea.md` numbered these `ADR-1`…`ADR-5`, while the
later `docs/superpowers/plans/2026-08-29-architecture-plan.md` (also deleted
by `8722ba90`) used `ADR-001`…`ADR-007` for a *different* set of decisions
that survive as live comments (`ADR-003` at `crates/v12-interp/src/lib.rs:652`,
`ADR-007` at `crates/v12-engine/src/engine.rs:483`). The two series overlap
numerically and do not correspond. This file continues the `ADR-1`… series
that `Cargo.toml` cites; it is not the plan's `ADR-001`
(`crates/v12-interp/src/lib.rs:731`).

## Context

The previous revision planned a hand-written lexer/parser/AST with oxc as
a differential-testing oracle. Under the reuse-before-rebuild principle this
spends ~10% of total engineering re-deriving the ecosystem's best-tested
component. **[recovered — `plan_idea.md:246-250`]**

The buy/build table row that motivated it **[recovered —
`plan_idea.md:79`]** named the rejected alternatives and why:

| | Chosen | Why | Rejected alternative |
|---|---|---|---|
| Parser / AST / scopes | `oxc_parser` + `oxc_ast` + `oxc_semantic`, **pinned `=0.147.0`** | Fastest spec-compliant Rust JS parser; passes all Test262 parser tests + 99% Babel/TS; error recovery; continuously fuzzed; scope/symbol resolution included | Hand-written parser (weeks-to-months to reach parity we get day 1); `swc_ecma_parser` (heavier `swc_common` infra, monthly major bumps, weaker conformance claim) |

A related note **[recovered — `plan_idea.md:88`]**: oxc uses its own bump
allocator internally (via `allocator-api2`), so v12's own arena types do not
cross that boundary.

## Decision

Depend on `oxc_parser`/`oxc_ast`/`oxc_semantic`; `v12-bccompiler` walks
the oxc AST directly; spans come free via `oxc_span`.
**[recovered — `plan_idea.md:251-253`]**

The **pinning policy** is the operative part and is in force at
`Cargo.toml:37-42`:

```toml
# Front end (ADR-1): weekly 0.x releases with breaking changes — pin exact,
# upgrade deliberately.
oxc_parser  = "=0.147.0"
oxc_ast     = "=0.147.0"
oxc_semantic = "=0.147.0"
oxc_span    = "=0.147.0"
```

`=` (exact requirement) rather than `^` (caret): a caret on a `0.x` crate
still admits the next `0.148.0`, and ADR-1's stated cost is "breaking AST
changes". Exact pins make an oxc upgrade a *deliberate* commit that can be
reviewed and fail loudly, instead of an incidental `Cargo.lock` refresh.

## Consequences

**Weekly 0.x releases with breaking AST changes → pin exact versions,
upgrade deliberately.** **[recovered — `plan_idea.md:254-255`]**

**AST is shaped for tooling (spans everywhere, no parent links); the
bytecode compiler adapts to it rather than the reverse — accepted coupling,
this is the least reversible decision in the plan.** **[recovered —
`plan_idea.md:255-257`]** That coupling is still the mechanism by which a
new oxc AST variant would reach `v12-bccompiler`; see the *known gap* below.

**Eliminates five planned crates (lexer, parser, AST, semantic, span) and
an entire workstream.** **[recovered — `plan_idea.md:257`]** Consistent with
today's tree: `crates/` holds no parser or AST crate, and
`v12-bccompiler` is the only consumer of oxc.

**[reconstructed]** *The pin has never been moved.* `git log --all -p --
Cargo.toml | grep -E '^[-+].*oxc_(parser|ast|semantic|span) ='` returns four
`+` lines and zero `-` lines: `=0.147.0` was introduced by the bootstrap
commit `fbbe6284` (2026-08-26) and has never been changed. So the ADR's
"upgrade deliberately" clause has, to date, meant *never*. Whether that is
correct depends on oxc's breaking-change rate, which this repo has never
had to absorb; treat it as an untested policy, not a proven one.

### Known gap: `oxc_allocator` is not in `[workspace.dependencies]`

`crates/v12-bccompiler/Cargo.toml:10` declares

```toml
oxc_allocator = "=0.147.0"
```

**inline**, while the other four oxc crates go through
`[workspace.dependencies]` (`Cargo.toml:39-42`). It is used at
`crates/v12-bccompiler/src/lib.rs:80` (`use oxc_allocator::Allocator`) and
`crates/v12-bccompiler/src/tests.rs:14`.

Consequence **[reconstructed from the dependency graph, verified against
`Cargo.lock`]**: an oxc upgrade that edits `Cargo.toml:39-42` will *not* by
itself reach `oxc_allocator`. `Cargo.lock` currently resolves 15 `oxc_*`
crates, 14 of them at `0.147.0` (`oxc_index` is unrelated, at `5.0.0`),
including the five transitive ones (`oxc_ast_macros`, `oxc_ast_visit`,
`oxc_data_structures`, `oxc_diagnostics`, `oxc_ecmascript`, `oxc_estree`,
`oxc_regular_expression`, `oxc_str`, `oxc_syntax`) — those follow
automatically. `oxc_allocator` does not, because it is a *direct* dependency
declared outside the workspace table.

The failure mode is **loud, not silent**: `oxc_parser` 0.148 depends on
`oxc_allocator ^0.148`, so leaving `=0.147.0` in place forces cargo to
resolve *two* `oxc_allocator` versions, and `Parser::new(&allocator)` at
`crates/v12-bccompiler/src/lib.rs:82` then receives the wrong `Allocator`
type — a compile error, not a silently-skipped crate. So the gap costs one
extra edit and one confusing error message per upgrade, not a hidden
version skew. **[reconstructed — the two-version resolution is standard cargo
behaviour for a semver-incompatible `oxc_allocator`; it was not reproduced
here, because doing so requires actually bumping oxc.]**

Fix when someone next upgrades: add `oxc_allocator = "=0.147.0"` to
`Cargo.toml` `[workspace.dependencies]` and change the crate to
`oxc_allocator.workspace = true`, in the same commit as the version bump.

## Verification

Every claim above re-checks with:

```sh
git show 3ea57570:plan_idea.md | sed -n '246,257p'   # ADR-1 verbatim
git show 3ea57570:plan_idea.md | sed -n '79p'        # buy/build table row
sed -n '37,42p' Cargo.toml                           # the pin, in force
grep -n oxc_allocator crates/v12-bccompiler/Cargo.toml # line 10, inline
grep -A1 'name = "oxc_' Cargo.lock | grep -E 'name|version' | paste - - -  # all 0.147.0
```