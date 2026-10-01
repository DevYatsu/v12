# ADR-3 — Cranelift for both JIT tiers, with a split emission path

**Status:** Accepted (the backend choice; the emission split is *not*
implemented — see Consequences).

Cited from `Cargo.toml:44` ("JIT backend (ADR-3)") and in force since the
bootstrap commit.

> **Provenance of this record.** As with
> [`0001-oxc-exact-pin.md`](0001-oxc-exact-pin.md), the original ADR was
> `plan_idea.md` §5 (added `3ea57570`, 2026-08-27, deleted `8722ba90`) plus
> the buy/build table in §3. **Context**, **Decision**, and the first
> paragraph of **Consequences** are *recovered verbatim* from
> `git show 3ea57570:plan_idea.md` lines 279-291 and 82-83. Everything about
> today's crate contents is **[reconstructed]** — read off the working tree,
> not off a decision record. Numbering caveat: this is the `ADR-3` of the
> `plan_idea.md` series (which `Cargo.toml:44` cites); it is **not** the
> `ADR-003` of the later 2026-08-29 series, which is a different decision
> (`Engine` owns its `Heap`; `crates/v12-interp/src/lib.rs:1104`).

## Context

Cranelift is the maintained, safe, fast-compiling option, and user stack
maps (2024) give precise, moving-safe safepoints. Its weaknesses:
`cranelift-jit` is experimental, generated-code peak quality trails
LLVM/TurboFan, and dynamic-language production precedents are scarce.
**[recovered — `plan_idea.md:281-284`]**

The buy/build table row **[recovered — `plan_idea.md:82`]** gave the
rejected alternatives and why:

| | Chosen | Why | Rejected alternative |
|---|---|---|---|
| JIT backend (Tiers 1+2) | `cranelift-frontend` / `-codegen` / `-module` / `-native`, 0.135.x | Fast compilation, memory-safe, active. **User stack maps landed mid-2024**: every call is a safepoint, spilled refs reload after each safepoint — moving-collector-safe JIT frames are now supported | Hand-written assembler à la YJIT (YJIT abandoned Cranelift for exactly this control, at a cost of years); LLVM (compile latency kills a JIT) |

The code-emission row **[recovered — `plan_idea.md:83`]** separately
decided to *not* use `cranelift-jit` in production: `cranelift-jit` is
"verbatim extremely experimental" upstream, `JITModule` does not expose
stack maps, and Wasmtime maps compiled artifacts manually — so v12 would
"read function metadata ourselves, as Wasmtime does". That is the "split
emission path" in the title.

## Decision

Tier 1 is a template JIT (one Cranelift block per bytecode op,
Sparkplug-style). Tier 2 adds SSA + type feedback + speculative guards with
deopt. Dev runs on `cranelift-jit`; production loads emitted code through
our own mapper. **[recovered — `plan_idea.md:285-288`]**

The tier structure is still exactly this: `crates/v12-jit-baseline`
(1 439 LOC over four files) and `crates/v12-jit-opt`, with shared types in
`crates/v12-codegen` (`crates/v12-jit-baseline/src/lib.rs:22-25` re-exports
`CompiledFn`/`JitCache`/`JitError` from there). The template-JIT strategy is
visible verbatim in `crates/v12-jit-baseline/src/lib.rs:3-9` and
`compiler.rs:1-10`: one Cranelift block per bytecode instruction, arithmetic
baked as `box_number` immediates, heap-agnostic `extern "C" fn(u64,u64) ->
u64` helpers. **[reconstructed — read off the source.]**

**In force today** (`Cargo.toml:44-46`):

```toml
# JIT backend (ADR-3).
cranelift-frontend = "0.135"
cranelift-codegen  = "0.135"
```

Note these are **caret, not exact** — unlike oxc. Cranelift is a stable
1.0-series dependency with a semver policy, so it does not need ADR-1's
pin-exact treatment. That asymmetry is deliberate on the face of the file.
**[reconstructed]**

## Consequences

**Peak hot-loop performance is capped relative to LLVM-tier backends —
accepted for v1; revisit only if Tier 2 plateaus below target. Budget the
majority of JIT effort for ICs/OSR/deopt, which Cranelift does not provide.**
**[recovered — `plan_idea.md:288-291`]**

**[reconstructed]** Four consequences are readable off the current tree and
are *not* in the original record:

1. **The split emission path was never built, and the crates that would
   have implemented it were deleted.** `Cargo.toml` at bootstrap (`fbbe6284`)
   pinned five cranelift crates — `frontend`, `codegen`, `module`, `native`,
   `jit`. Commit `560d0b03` ("P2 delete dead/speculative JIT machinery",
   2026-09-05) removed `-module`, `-native` and `-jit`, and deleted the
   doc-only `mmap.rs`. Only `frontend` + `codegen` remain, **both
   `optional = true`** behind the `jit` feature (`crates/v12-jit-baseline/Cargo.toml:17-27`),
   so interpreter-only builds do not link Cranelift at all — which was
   plan principle 4 ("Reversibility") applied to the backend dependency.
   The consequence is that ADR-3's *title clause* ("with a split emission
   path") describes a decision that has since been narrowed by deletion:
   there is no own-mapper crate, and no code emission at all. See 2.

2. **No tier emits machine code.** `crates/v12-jit-baseline/src/compiler.rs`
   builds Cranelift IR (`FunctionBuilder`, `InstBuilder`) but the crate
   contains no `finalize_definitions` / `Context::for_function` call —
   `grep -rn 'finalize_definitions\|Context::for_function' crates/v12-jit-baseline/src/
   crates/v12-jit-opt/src/` returns nothing. And the engine never executes
   what it compiles: `crates/v12-engine/src/jit_tier.rs` compiles on tier-up
   and caches the `CompiledFn`, and `CONTEXT.md:107` records that the hook
   "never *executes* compiled code", because the baseline executor is
   heap-agnostic. So Cranelift today buys **IR construction plus a tier-up
   counter**, nothing more. Any cost/benefit argument about Cranelift in
   2026 is currently an argument about a component that is not wired into
   the execution path.

3. **Tier 2 is a stub with no Cranelift dependency at all.**
   `crates/v12-jit-opt/Cargo.toml` lists only v12 crates. `560d0b03` deleted
   its `build_ssa_ir` (whose output was discarded at the only call site) and
   every Cranelift helper, keeping a speculative tier-2 API behind per-item
   `#[allow(dead_code)]`. ADR-3's "Tier 2 adds SSA + type feedback" is
   therefore aspirational.

4. **Enabling the JIT is an env-var decision, not a build decision.**
   `jit` is in `default` (`crates/v12-jit-baseline/Cargo.toml:24`), so
   `--all-features` is a no-op for it; the runtime switch is `V12_JIT`
   (`crates/v12-engine/src/jit_tier.rs:127-131`). Unset = pure interpreter.

**[reconstructed]** If the decision were re-litigated today, the honest
question is not "Cranelift vs LLVM" but "does Cranelift survive contact
with item 2 above". A backend that never emits code has no measured
compile latency, no measured code quality, and no exercised stack-map
integration with the collector — so ADR-3's core justification (fast
compilation, moving-safe safepoints) is currently *unverified in this
repo*. That is a gap in the decision record's evidence, not a refutation
of it.

## Verification

```sh
git show 3ea57570:plan_idea.md | sed -n '279,291p'   # ADR-3 verbatim
git show 3ea57570:plan_idea.md | sed -n '82,83p'     # buy/build + emission rows
sed -n '44,46p' Cargo.toml                            # the pin, in force
git show fbbe6284:Cargo.toml | grep cranelift         # five crates at bootstrap
git log -1 --format=%B 560d0b03                       # the three-crate deletion
cat crates/v12-jit-baseline/Cargo.toml                # both deps optional, default=["jit"]
grep -rn 'finalize_definitions\|Context::for_function' crates/v12-jit-*/src/   # no hits
```