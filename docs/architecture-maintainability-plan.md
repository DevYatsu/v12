# v12 Architecture & Maintainability Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make v12 cheaper to change safely by restoring the enforcement tier that has silently stopped working, removing the one class of silent-wrong-value bug the compiler can still produce, and opening the builtins to unit testing.

**Architecture:** The crate graph is sound and should not be restructured. The defects are (a) **enforcement** — the gates meant to catch drift are broken or never invoked, (b) **exhaustiveness** — the compiler's first pass swallows unhandled syntax where its second pass deliberately refuses to, and (c) **seams** — builtins are reachable only end-to-end, so 0 of 30 builtin test files can be unit-tested. The plan restores gates **before** touching structure: a refactor without a working safety net is precisely how the current guard regressed unnoticed from 39 grandfathered sites to 121.

**Tech Stack:** Rust 1.90 (edition 2024), `cargo nextest`, `cargo clippy`, test262 via `conformance/run.sh`.

**Spec:** none — this is an audit output. Evidence for every claim is in §8; every number was measured on the commit named beside it.

**Location note:** written to `docs/architecture-maintainability-plan.md` rather than the skill's default `docs/superpowers/plans/` because 3 of the 5 files in that directory are untracked and one `git clean -fd` would destroy them (W4.3 fixes this).

---

## 0. State drift since the audit

This plan was first written against `8be05b0c`. It was re-verified against **`92e10484`** (18 further commits: `Function.prototype` linkage, iterator `limit`/`result`/`zip`, `JSON.rawJSON`, `SpeciesConstructor`, eval-determinism fixes). Everything that follows is measured on `92e10484`.

| Claim in the original draft | Was | **Is now** | Consequence |
|---|---|---|---|
| `cargo nextest run --workspace` | 1228 | **1291** passed, 2 skipped, 7.4 s | CONTEXT.md's `1 242` is stale by 49 |
| clippy warnings | 277 | **275** | `-D warnings` still fails: 15 errors in `v12-bccompiler` |
| `call_setup.rs` `error_value(` sites | 52 | **53** (cap 27) | Freeze guard still red |
| `call_setup.rs` size | 5 228 | **5 406** lines | Still the largest file in the repo |
| `cargo deny` | wired to nothing | still wired to nothing, **and the tool is not installed** | W0.6 re-scoped below |
| `cargo fmt --check` | not run | **23 files have diffs** | W0.7 re-scoped below |

**Two corrections to the user-facing brief I gave earlier:** the clippy warning cleanup is **not** in this tree (275 warnings remain), and `CONTEXT.md` now claims `1 242` rather than `1 115` — still wrong, by a different amount.

---

## 1. What is working — do not restructure it

Listed so the work below is not mistaken for a rewrite. Each is a real asset.

| Asset | Evidence (on `92e10484`) | Why it is good |
|---|---|---|
| Clean crate DAG | `v12-heap`, `v12-bytecode` have zero internal deps; `v12-bccompiler` never names `v12_interp`/`v12_engine` | Dependency inversion is correct at the seams |
| No global mutable execution state | zero `thread_local!`, zero `static mut`; one `static RNG_STATE` (`crates/v12-engine/src/builtins/math.rs:124`) | Every engine is isolatable in-process |
| Exactly one `unsafe` region | `crates/v12-heap/src/function.rs:80-116` | Auditable by inspection |
| Trait-object seams | `NativeRegistry` (`crates/v12-engine/src/engine/eval.rs:110`), `TierHooks` (`crates/v12-engine/src/jit_tier.rs:29`) | Engine↔interpreter does not leak representation |
| Exhaustive lowering **by design** | `crates/v12-bccompiler/src/stmt.rs:105-115` — comment states variants are listed instead of a guard "so a new oxc statement kind breaks compilation" | A deliberate, documented choice; the model for W1 |
| Opcode exhaustiveness | 73 `Opcode` variants, 73/73 have a top-level arm in `crates/v12-interp/src/execute.rs`, no catch-all | Adding an opcode cannot compile unimplemented |
| Fast gate | 1291 tests in 7.4 s | Cheap enough to run on every change — the reason Track 0 is viable |
| Layered conformance harness | `frontmatter.rs` / `harness.rs` / `runner.rs` / `report.rs` separated; per-test process isolation (`conformance/harness/src/runner.rs:686`); timeouts derived together (`Timeouts::from_window_ms`, `runner.rs:99`) | Deterministic measurement, already fixed and documented |

**Consequence:** no crate splits, no service layer, no plugin architecture, no trait-abstraction refactor of `Ctx`. Those are the tempting moves, and all are YAGNI violations against a codebase whose actual failure mode is unenforced drift.

---

## 2. The growth trend that frames everything

Measured across 25 days of development (`2df4e3a0` → `8be05b0c`), re-confirmed at `92e10484`:

| Metric | 2026-09-05 | 2026-09-30 | Change |
|---|---|---|---|
| Workspace `src` LOC | 39 358 | 73 025 | **+86 %** |
| Files >1500 LOC | 5 | 12 | **+7** |
| `v12-engine` src LOC | 6 135 | 24 736+ | **+303 %** |
| `v12-interp` src LOC | 8 335 | 18 626+ | +123 % |
| `v12-heap` src LOC | 6 887 | 8 493 | +23 % |
| `v12-jit-baseline`, `v12-jit-opt`, `v12-intl`, `v12-regex`, `v12-codegen`, `v12-api` | — | — | **0 %** |

The largest file in the repository is `crates/v12-interp/src/call_setup.rs` at **5 406 lines**. It was created by commit `2df4e3a0` — *"refactor(interp): P3 split lib.rs by concern"* — and is larger than the 5 787-line `lib.rs` that split set out to reduce. **The split moved complexity; it did not remove it, and nothing tracked that.**

Complexity is concentrating where the module boundaries are weakest (the engine) while the low-level crates stayed flat. Any plan that does not add *enforcement* will be outrun within a month.

---

## 3. Findings, ranked by risk × effort

### F1 — The enforcement tier is non-functional. **Highest priority.**

- `.github/workflows/ci.yml:32` runs `./scripts/check-architecture.sh`. **The file does not exist and never has** — zero commits in any branch. `Cargo.toml:17` names a companion `scripts/check-architecture.py`; also absent.
- Consequence: the `architecture` job fails at that step, and `ci.yml:35` (`cargo clippy --workspace --all-features -- -D warnings`) has no `if: always()`, so **clippy has never run in CI**. The `unwrap`/`expect`/`panic` policy that `Cargo.toml:14-22` calls "blocked by an audited-site allowlist" is fiction.
- `./scripts/freeze-lint.sh` — which *does* exist — **exits 1 on `HEAD`**, with 16 violation lines. Site counts went **up**: `call_setup.rs` has 53 `error_value(` hits against a cap of 27; `property.rs` newly violates with 16; 10 further files violate because they were never allowlisted.
- Part of that is the script's own bug: it counts test code, so `crates/v12-interp/src/tests.rs` and `crates/v12-interp/tests/error_value_gc.rs` are flagged. Do **not** allowlist those — fix the glob.
- `cargo clippy --workspace --all-features` currently emits **275 warnings**; `-- -D warnings` already fails with 15 errors in `v12-bccompiler` (e.g. `collapsible_if` at `crates/v12-bccompiler/src/unit.rs:395`). So `-D warnings` cannot simply be switched on — see W0.1.
- `--all-features` (`ci.yml:21,26`) is a **no-op**: `jit` is in `default` (`crates/v12-engine/Cargo.toml:26`, `crates/v12-jit-baseline/Cargo.toml:21`). The CI comment claiming it "catches gated tests" is false, and no `--no-default-features` run exists — so the interpreter-only build is untested. (It compiles cleanly; verified, 25.8 s.)
- `deny.toml` (42 lines, incl. an LGPL-3.0 exception for `malachite`) is wired into nothing, and **`cargo-deny` is not even installed**.
- No coverage, mutation, or fuzz tooling. `proptest` is a dev-dep used by exactly one test (`crates/v12-interp/src/feedback.rs:524`). `bench/` is markdown only.
- `conformance/faildiff.sh` — the only real regression gate — is **never invoked by CI**. The `conformance` job instead propagates "exit 1 if any test failed" (`ci.yml:69`, `conformance/harness/src/main.rs:444-448`), which at 77.5 % `language` conformance can **never** be green. A permanently red gate trains reviewers to ignore it.
- **Net effect: a refactor can silently cost thousands of test262 passes while CI stays green.**

### F2 — The compiler's first pass is silent where its second is exhaustive.

`collect.rs` (Pass A) ends its AST walks in catch-alls — 23 sites, **10 of them fully silent** (`_ => {}` / `_ => continue`): `crates/v12-bccompiler/src/collect.rs:802, 833, 891, 1074, 1295, 1918, 2003, 2040`. It also carries a **third** independent mini-walker, `contains_arguments` (`collect.rs:214-300`), with its own `_ => false` at `:300`.

By contrast `FnCtx::stmt` / `FnCtx::expr` (Pass B) list every variant explicitly, *by documented intent* (`crates/v12-bccompiler/src/stmt.rs:105-115`).

Coverage confirms the asymmetry: `Statement::` appears 24× in `collect.rs` vs 42× in `stmt.rs`; `Expression::` 59× vs 60×.

**Failure mode:** a new oxc variant compiles fine, is swallowed by Pass A, and produces **no declaration, no reference record, no nested unit** — a silent wrong value. Function-like variants fail loudly (`expr.rs:2242` "internal: nested function missing from plans"); everything else is silent. This is the exact class of the documented `class … extends` bug, where a missing scope operation yielded `function` where the spec says `1`. There is no checklist and no test enumerating syntax constructs — `grep` for `trybuild`/`compile_fail` in `v12-bccompiler` returns zero.

### F3 — The dispatch ABI has no operand contract and is implemented four times.

73 `Opcode` variants with a hand-written `impl TryFrom<u8>` (`crates/v12-bytecode/src/opcode.rs:181`); the stability test (`opcode.rs:449`) pins only **13 of 73** literal values. `FunctionBytecode::validate` (`crates/v12-bytecode/src/lib.rs:308`) checks handler nesting, bounds, and `max_regs != 0` — it does **not** validate operand *roles*.

Operand order is inconsistent between adjacent opcodes: `GetEnvSlot` is `(a=dst, b=depth, c=slot)` (`crates/v12-bccompiler/src/model.rs:1452`, read `env_read(rb, rc)` at `crates/v12-interp/src/execute.rs:833`) while `SetEnvSlot` is `(a=depth, b=slot, c=src)` (`model.rs:1469`, read `env_write(ra, rb)` with `stack[rc]` at `execute.rs:837-839`). `GetProperty`/`DeleteProperty` are `(dst, obj, key)` while `SetProperty` is `(obj, key, val)` — the destination moves position between neighbouring arms. `NewEnvironment`'s `depth` operand is emitted (`model.rs:1677`) and discarded (`execute.rs:776`).

Four dispatch implementations exist: `crates/v12-interp/src/execute.rs:143`, `crates/test-support/src/mini.rs:134`, `crates/v12-jit-baseline/src/compiler.rs:297`, and the `crates/v12-bccompiler/src/peephole.rs:247` `word_width` table. All exhaustive (good), none sharing operand definitions.

The call ABI is duplicated too: `CALL_HEADER_REGS = 2` (`model.rs:29`) documents `[callee][this][args]`, while `call_setup.rs:301` and `:344` hardcode `callee_slot + 2`. `super_env_depth` is copied verbatim from `expr.rs:2250` to `unit.rs:79-91`, acknowledged in a comment as "mirrors".

**Failure mode:** a new opcode cannot be silently unimplemented, but it *can* be decoded with wrong operand roles and no test fails.

### F4 — Builtins are not unit-testable.

All **30** `crates/v12-engine/tests/*.rs` files construct `Engine::new()` + `eval`. **Zero** build a heap and call a builtin handler directly. So the entire `&mut Ctx` signature migration is unverifiable except through conformance — which is how that refactor could stall for weeks without detection (F1).

Root cause is a real bug: `crates/v12-engine/src/builtins/helpers.rs:26,48,64,134` construct throwaway `Ctx::new(heap, None, None)`, so anything routed through a shim silently loses realm/intrinsic linkage. And of 156 functions taking `&mut Ctx`, **130 (83 %)** touch no field but `heap` — `Ctx` is currently a disguised `&mut Heap`, which is why the signature change bought nothing.

Duplication a shared seam would remove, counted per builtin file: **194** hand-rolled coercion sites, **75** argument-count checks, **129** throw sites, **39** species lookups, **73** result-object builds. There is no `Ctx::to_integer_or_infinity` at all (`array.rs:27 to_length` is a private duplicate; `date.rs:218` documents a second local one), and no `Ctx::species_constructor`.

Two crates are effectively untested: `v12-native` (1 703 LOC, 2 tests — including `throw.rs:44 parse_error_text`, which string-parses error prefixes) and `v12-codegen` (285 LOC, 2 tests).

### F5 — Decision records lost, published metrics false, planning history untracked.

Commit `8722ba90` ("refactor: roadmap written down", 2026-09-17) deleted four planning documents at once: `docs/builtins-arch-plan.md` (142 L), `docs/refactor-plan.md` (84 L), `docs/language-coverage-plan.md` (81 L), `docs/builtins-plan.md` (49 L) — **356 lines of design rationale**. Fourteen sites in `crates/*/src` still cite the deleted `docs/builtins-arch-plan.md` §5.3, so the citation graph dangles.

ADR-1 (oxc exact-pin) and ADR-3 (cranelift) are cited by bare number from `Cargo.toml:28` and `Cargo.toml:35` with **no ADR file existing** — `docs/adr/` is absent.

Published metrics are false, and re-verified on `92e10484`: `README.md:13-18` claims "563 tests" and Test262 `language` at **32.1 %** (7 561/23 580); measured is **1 291 tests**. `CONTEXT.md:84` now claims "1 242 passed"; measured is 1 291 — stale by 49. Both are the front door.

Untracked and lost to one `git clean -fd`: `ROADMAP.md` (133 L), `.orchestrator-notes.md` (258 L), 3 of 5 `docs/superpowers/plans/*` (1 763 L) — **~2 150 lines**. Note `docs/superpowers/plans/2026-09-27-test262-builtins-coverage.md` (2 159 L) *is* tracked: the directory was partially committed.

Spec traceability: **155** spec-citing comment lines out of 16 333 comment lines (**0.95 %**), and zero `tc39.es` links in `crates/*/src`. Traceability is bimodal — `promise.rs` (35 citations) and `regexp.rs` (15) are safe to change; `array.rs` has **1** citation in 847 lines and it is about the refactor, not the spec.

Onboarding: adding an `Array.prototype` method requires **7 steps, of which 2 are documented**. `CONTRIBUTING.md` (W4.6) now covers all 7. Two corrections to the original audit, both verified on `a965bb6e`: native-`Id` **collisions are impossible** — a duplicate discriminant is rustc error `E0081` — so the real hazard is *retired-id* reuse (3500/3501) plus the fact that only 14 of 383 discriminants are pinned by tests; and there is an **8th undocumented step** the audit missed, `builtin_length` (`crates/v12-engine/src/builtins/mod.rs:99`), where arity is declared a second time and held in line only by a `debug_assert_eq!` (`crates/v12-engine/src/builtins/ctx.rs:575`).

---

## 4. Sequenced work items

Track 0 is a hard prerequisite: every later track's verification depends on gates that currently do not run.

### Track 0 — Restore enforcement

- [ ] **W0.1 — Make the `architecture` job run, honestly.** Write `scripts/check-architecture.sh` and remove the `check-architecture.py` reference at `Cargo.toml:17` in the same commit.
  **Do NOT add `-D warnings` in this task.** Measured: `cargo clippy --workspace --all-features` emits **275 warnings**, and `-- -D warnings` already fails (15 errors in `v12-bccompiler`). Wiring `-D warnings` on day one trades one red gate for another. Land the script **report-only** (run clippy, print the count, exit 0) so the debt becomes *visible* in CI logs; flipping to `-D warnings` is a separate task once the 275 are cleared **and** W0.3's allowlist exists — because `[workspace.lints.clippy]` sets `unwrap_used`/`expect_used`/`panic` to `warn` (`Cargo.toml:19-22`), so `-D warnings` also promotes the 114 audited sites to build errors.
  Verify: `./scripts/check-architecture.sh; echo $?` → 0; the script exists in the commit; `ci.yml` has no reference to a nonexistent path.
- [ ] **W0.2 — Re-ratchet `freeze-lint.sh`. ✅ Done (verified on `92e10484`).** The test-code glob bug is fixed (`--exclude-dir=tests --exclude='tests.rs'`, a **path** rule documented with its deliberate consequences), all caps raised to measured reality with **zero slack**, and every previously-unlisted file given an explicit entry with a per-file rationale. Guard is green (exit 0) and still bites — verified by probe against an unlisted file and an over-cap file. Entries went 67 → 133; old caps totalled 67 against **129 real sites** (62 unaccounted), not the 16 violation lines the guard reported. A **stale** `PROP_GET1` entry for `call_setup.rs:1773` was deleted (already migrated; the script does not flag surplus entries, so it was silently inflating that cap). Allowlist entries now carry the literal error text per site, so the file describes the debt rather than counting it.
  **Correction to the original text of this task:** porting `call_setup.rs`'s 53 sites to `ctx.{type,range,syntax,reference}_error` **is not executable** — `Ctx` lives in `v12-engine`, which *depends on* `v12-interp` (`crates/v12-engine/Cargo.toml:11`); `crates/v12-interp/Cargo.toml` has no `v12-engine` dependency, so `Ctx` is unreachable from the interpreter. The reachable target is the same pair `Ctx::make_error` already uses — `v12_native::{parse_error_text, error_object}` (`crates/v12-native/src/throw.rs:44,57`). That becomes **W0.2b** below; only after it exists can the caps ratchet down.
- [x] **W0.2b — Add `Interp::{type,range,syntax,reference}_error`. ✅ Done (`2f52598c`, +45/-0).** Also added `error_value_kind(default_kind, text)`, the *validating* sibling, because `error_value`'s permissive split and `parse_error_text`'s validating split are not interchangeable. `error_value` deliberately keeps its permissive behaviour and is **not** delegated; a scan of all 223 call sites found exactly one non-`KNOWN_KINDS` prefix (`ScriptRuntimeError`), and a mutation-checked test pins the permissive path.
- [x] **W0.2c — Port all ERR_VAL sites in batches. ✅ Done (`bb579572`, `a965bb6e`, `12a8a979`).** **216 sites ported; workspace ERR_VAL 119 → 1.** Per file: `call_setup.rs` 79, `property.rs` 75, `object_ops.rs` 25, `lib.rs` 12, `generator_async.rs` 7, `execute.rs` 5 of 6, `call_capability.rs` 5, `eval_context.rs` 3, `ops.rs` 2, `engine/module_loader.rs` 2, `globals.rs` 1. Total allowlist entries 133 → 15.
  **The remaining 1 is correct, not unfinished:** `crates/v12-interp/src/execute.rs:174` uses prefix `ScriptRuntimeError`, which is not in `KNOWN_KINDS`, so porting it would set `e.name` to `"TypeError"` — observable from JavaScript. Its allowlist block is marked `DELIBERATE EXCEPTION` and `Interp::error_value` is **retained** for it. Forcing the count to 0 would have been the exact unsoundness this task existed to fix: a cap that isn't true is worse than a red guard, because it is a green lie.
  **Every historical cap was a lower bound.** The freeze lint's pattern only matches the single-line `error_value("` form, so multi-line and trailing-comma calls were invisible — `property.rs` held 75 sites against a recorded 16 (4.7×), `lib.rs` 12 against 8, `globals.rs` 1 against 0. A receiver split across a line break (`self\n  .error_value(`) defeats the lint *and* naive grep; three sites had that shape. Conversely `grep -c error_value(` over-reports: 3 hits in `registry.rs` are `syntax_error_value(`, a substring false positive. **Never size a batch from an allowlist number; count by paren-matching.** The allowlist header now records this.
  **The Ctx-vs-Interp choice is per-site, not per-file.** `engine/module_loader.rs` uses `interp.error_value` from `ensure_deferred`/`validate_export_stars` (no `Ctx` in scope) *and* `ctx.type_error` at :185/:206 inside `handle_import`.
  Each batch proved unobservability three ways: per-file `(name, message)` multisets byte-identical to `HEAD`; `faildiff` BROKE 0 / REGRESSED 0 / NEW_FAIL 0 / FIXED 0 / MARGINAL 0 at 19 080 pass = 77.7 %; and per-test verdict diffs across all 24 590 tests showing 0 against both the baseline and the prior batch.
- [ ] **W0.2d — Add `Interp::internal_error`.** There is **no** `internal_error` wrapper, only `type`/`range`/`syntax`/`reference`. The 4 `InternalError` sites (3 in `lib.rs`, 1 in `call_capability.rs`) therefore went to `error_value_kind("InternalError", msg)` as a stopgap — same `(name, message)` pair, lint hit retired, but the stringly kind is still at the call site. A real wrapper closes it.
- [ ] **W0.2e — `PK_PUSH` migration.** 14 entries remain (`property.rs` 3, `object_ops.rs` 1, `module_namespace.rs` 3, `promise.rs` 1, `date.rs` 1, plus `ctx.rs`/`object.rs`/`json.rs`/`string.rs`/`heap/object.rs`). These are `property_keys.push` → `define_data_prop_with_attrs` (`ctx.rs:482`) and have **no** typed wrapper; each remaining site is individually justified in the allowlist (internal slots, fixed-slot records, spec-mandated export order). Lower-effort than W0.2c and lower-value; do it only after Tracks 1-3.
- [x] **W0.3 — Make the unwrap/expect/panic policy real. ✅ Done (uncommitted; verified on `e925647d` + this diff).** `scripts/unwrap-allowlist.txt` + `scripts/unwrap-lint.sh` (wired into `ci.yml` after `freeze-lint.sh`, both green, guard verified to bite on an added site, a deleted entry, and a stale entry).
  **The audit's inventory was wrong by 5×, and the reason matters:** its per-file numbers (`promise.rs` 19, `engine.rs` 17, `ctx.rs` 13, `realm.rs` 12, `module_loader.rs` 9, `regexp.rs` 9) all match a scan of **`v12-engine` alone** — 114 sites. The workspace has **566** (`expect` 291 incl. 4 `expect_err`, `unwrap` 218, `panic!` 32, `unreachable!` 25), and the 114 was 1 crate of 13. `v12-interp` alone has 132. Any "114 audited sites" claim (this plan, `check-architecture.sh`, root `Cargo.toml`) understated the debt 5-fold.
  Budget: clippy 275 → **301**. All +25 lints are *coverage* (`test-support` +16, `v12-cli`'s `check` bin +5, `conformance/harness`'s `test262-runner` +4 — crates that never opted in; `v12-api`/`v12-intl`/`v12-regex` gained 0 because their sites are all in inline `#[cfg(test)]`), −1 lint from the `ctx.rs` work below, +3 summary lines. `conformance/harness` is a **6th** unlinted member the task text missed; it keeps its stricter `[lints.rust] unsafe_code = "forbid"`, so it gets `[lints.clippy]` spelled out instead of `[lints] workspace = true` (Cargo rejects both).
  Triage: `ctx.rs:829` and `:858` were both **unreachable by construction** (each reads back a descriptor the same function wrote 2–5 lines earlier, on the caller's immediately preceding `define_data_prop_with_attrs`), and neither has a spec-visible error to throw — they run during builtin installation with no JS frame. So both became `debug_assert!`s rather than `panic!` + throw; debug coverage is unchanged (the whole 1295-test suite runs with `debug_assertions` on and passes). `module_loader.rs`'s 9 are **not** host-triggerable: `read_and_compile` turns a missing file into a JS throw, a missing child record into `"Unlinked module import"`, and every record in `modules` provably has ≥1 namespace, so all 9 are internal-invariant assertions. **Untraced:** only these 9 were assessed; `v12-interp/src/execute.rs` (8 `panic!("corrupt bytecode: …")`) and `v12-heap/src/{macros,string}.rs` (5) are the same class of production invariant panic and are the obvious next triage batch.
  Verify: nextest 1295/2 in both feature configs; `freeze-lint.sh` 0; `check-architecture.sh` 0 at budget 301; `unwrap-lint.sh` 0; `clippy -D warnings` still fails only on the same 17 pre-existing lints (15 `v12-bccompiler` + 2 `v12-heap`, the latter pre-existing and not mentioned in this plan).
- [ ] **W0.4 — Replace the un-passable conformance gate.** Delete the exit-1 propagation (`ci.yml:68-72`) and add a `faildiff` step comparing against a committed baseline.
  Verify: on a deliberately-regressing branch the gate goes red; on `HEAD` it is green.
- [ ] **W0.5 — Add `--no-default-features` to CI.** Confirmed compiling today (`cargo check --workspace --no-default-features` → exit 0, 25.8 s), so this is free. Correct the misleading `--all-features` comment at `ci.yml:19-20`.
  Verify: both feature configurations build in CI.

### Track 0 — re-scoped out, with reasons

| Was | Why not now |
|---|---|
| W0.6 — wire `cargo deny` | **`cargo-deny` is not installed.** Adding a CI step that has never been executed risks a red gate for an unrelated reason (an unvetted `deny.toml` entry). Do it as: install via `taiki-e/install-action`, run `cargo deny check` **locally first**, fix what it reports, *then* add the CI step. W4.1 makes the `malachite` LGPL exception moot. |
| W0.7 — add `cargo fmt --check` | **23 files have diffs**, so adding the check turns CI red immediately. Also note the standing rule that no lane may run a workspace-wide `cargo fmt` (`CONTEXT.md` incident log). Ship `rustfmt.toml` now; add the check only after a deliberate, separately-reviewed formatting commit. |

### Track 1 — Make Pass A exhaustive (silent-bug class)

- [ ] **W1.1 — Design spike (½ day, no code).** oxc's `INHERIT` macro means "exhaustive" must be spelled variant by variant. Decide: convert `collect::stmt`/`collect::expr` to explicit variant lists, or add an assertion test that enumerates the `INHERIT` leaves and fails when a new one appears. Write the decision as `docs/adr/0002-collect-pass-exhaustiveness.md`. **A decision, not a task — do not code before it exists.**
- [ ] **W1.2 — Land the mechanism chosen in W1.1.** Verify: deleting one `Statement::` arm from `collect.rs` fails the build or a test; restoring it passes.
- [ ] **W1.3 — Unit-test the environment-depth model.** `Plans::env_depth` (`model.rs:453`), `env_depth_between` (`:465`), `this_home` (`:497`), and `env_bias` accounting (`:1694-1726`) are **not unit-testable today**. `env_depth_between` returns `0` on a non-ancestor chain (`model.rs:477`) — a silent wrong answer. This is the permanent home of the `class … extends` bug: the compiler's depth is a static unit-graph walk, the interpreter's `env_at` (`crates/v12-interp/src/lib.rs:1673`) a runtime chain walk cached in `env_display`, reconciled only by `env_bias`. Construct `Plans` directly in `src/tests.rs` and pin the `0`-fallback behaviour.
- [ ] **W1.4 — Test `env_display` / `walk_env` agreement.** `ENV_DISPLAY_CAP = 8` (`lib.rs:429`) has no compile-time counterpart — the compiler's depth is unbounded `u8`. The code comments depend on these agreeing; nothing tests it.

### Track 2 — One operand contract

- [ ] **W2.1 — Add an `OpSpec` table to `v12-bytecode`.** `pub struct OpSpec { pub a: Role, pub b: Role, pub c: Role, pub width: Width }`, one entry per opcode, as the single source of operand roles. Derive `crates/v12-bccompiler/src/peephole.rs:247 word_width` from it. **Additive — do not rewrite any dispatcher in this task.**
- [ ] **W2.2 — Derive `TryFrom<u8>` from a `const ALL: [Opcode; N]` table.** Replaces the hand-written 73-arm decoder (`opcode.rs:181`) and removes the `ALL_OPS` duplication (`opcode.rs:366`). Fix the stability test to pin all 73 discriminants, not 13.
- [ ] **W2.3 — Assert `validate()` against `OpSpec`.** Extend `FunctionBytecode::validate` (`crates/v12-bytecode/src/lib.rs:308`) to check operand roles — e.g. that a `SetEnvSlot` slot fits the environment the function allocated. Remove the dead `NewEnvironment.depth` operand (`model.rs:1677`, discarded at `execute.rs:776`) as part of this.
- [ ] **W2.4 — Delete the duplicated call-ABI literals.** Route `call_setup.rs:301` and `:344` through `CALL_HEADER_REGS` (`model.rs:29`). Extract the verbatim `super_env_depth` copy (`unit.rs:79-91`) into one shared function.

### Track 3 — Open the builtins to unit testing

- [ ] **W3.1 — Fix the `helpers.rs` realm hole first.** `helpers.rs:26,48,64,134` build `Ctx::new(heap, None, None)`, so every caller loses realm linkage silently. This is a **correctness bug**, not style, and a prerequisite: unit tests asserting realm behaviour would otherwise encode the bug.
- [ ] **W3.2 — Add the missing `Ctx` primitives.** `Ctx::to_integer_or_infinity` and `Ctx::species_constructor`; delete the private `array.rs:27 to_length` and the second local copy documented at `date.rs:218`. Convert **one** coercion family per commit from the 194 counted sites, ratcheting the count down. Each commit independently shippable and conformance-verified.
- [ ] **W3.3 — Give `Ctx` a real constructor for tests.** One `Ctx::for_test(heap)`-style helper in `test-support`. Convert **one** builtin file (recommend `number.rs` — 26 coercion sites, no callback seam) to have its first directly-called unit tests. Verify: a test constructs a heap, calls the builtin directly, and asserts a result — with no `Engine`.
- [ ] **W3.4 — Consolidate test boilerplate.** `fn eval_display` is byte-identical in **10** files (~7 LOC each); ~150-250 LOC of near-duplicate helpers across 45 files. `crates/v12-bytecode/tests/common/mod.rs` (279 LOC) is a second private helper module unreachable from other crates. Promote one shared helper into `test-support` and delete the copies.
- [ ] **W3.5 — Decide the fate of `test-support/src/mini.rs` (753 LOC).** It is a **third** dispatch implementation over the same 73 opcodes and a parallel semantics model used by only 5 files, of which **zero** are `v12-engine` tests. Either promote it to a real differential oracle (and use it more widely) or delete it. Leaving it half-used is the worst option — it silently encodes divergent semantics. Recommend: promote and wire into Track 1's `env_depth` tests.
- [ ] **W3.6 — Test `v12-native` and `v12-codegen`.** ~10 tests, starting with `throw.rs:44 parse_error_text`, `throw.rs:57 error_object`, `promise_combinator.rs:40 combinator_kind`, `crates/v12-codegen/src/cache.rs:119,134,139`.

### Track 4 — Dependency and documentation debt

- [ ] **W4.1 — Delete `v12-intl` and `malachite` from `v12-engine`.** `crates/v12-engine/Cargo.toml:15-16`. **Zero** use sites in engine source (verified by grep), yet `icu 2.3` + `temporal_rs 0.2` + 5 `malachite-*` crates are built into the engine, the CLI, `v12-api`, **and the test262 runner** — so every conformance process pays for them. Delete the two lines and `crates/v12-intl/`. Also delete unused `lasso` (`crates/v12-cli/Cargo.toml:21`) and the consumerless `criterion` / `bumpalo` workspace pins. Verify: `cargo check --workspace --all-features` passes and `cargo tree -i icu` returns no match.
- [ ] **W4.2 — Move the `Math.random` seed onto `Heap`/`Engine`.** `crates/v12-engine/src/builtins/math.rs:124 static RNG_STATE: AtomicU64` is the only process-global mutable execution state in the workspace — unresettable, shared across realms and engines, and therefore unisolatable by construction, unlike everything else.
- [ ] **W4.3 — Track the untracked planning history.** `git add ROADMAP.md .orchestrator-notes.md docs/superpowers/plans/`. ~2 150 lines currently lost to one `git clean -fd`. Also add `snip` (0-byte mode-700 scratch file) to `.gitignore`, and delete `.ignore` — it is a negation-only file for a path `.gitignore:12` already excludes.
- [ ] **W4.4 — Fix the false metrics and stop the re-rot.** Correct `README.md:13-18` (563 → 1 291 tests; 32.1 % → current) and `CONTEXT.md:84` (1 242 → 1 291). Then either generate the counts in CI or make the as-of date visually dominant. Do **not** delete the `CONTEXT.md` incident log — it records the workspace-reset and RegExp-timing incidents, the two most valuable engineering lessons in the repo. Split it: terminology stays, dated history moves to `conformance/fix-log.md` (already 1 426 lines, append-only).
- [ ] **W4.5 — Write the two missing ADRs and repoint the dangling citations.** Create `docs/adr/0001-oxc-exact-pin.md` and `docs/adr/0003-cranelift-backend.md`; replace the 14 sites citing the deleted `docs/builtins-arch-plan.md` §5.3 with a pointer to `ROADMAP.md` §2 plus the surviving `ctx.rs` doc comments.
- [x] **W4.6 — Add `CONTRIBUTING.md`. ✅ Done.** All 7 onboarding steps documented, with a "verified against" footer of `path:line` citations that a script re-checks (all 63 resolve). Four corrections to this plan's original text, all verified on `a965bb6e`: (1) the `Ctx`-vs-`Heap` discrepancy this task was meant to fix **does not exist** — 155 handler bodies take `&mut Ctx` and the 10 on `&mut Heap` are all installers/helpers, so `builtins/array.rs:3`'s header is *correct*, and only its `docs/builtins-arch-plan.md §5.3` citation dangles (defer to W4.5); (2) `run_callback_builtin` is at `call_setup.rs:1966`, not `:1847`; (3) the "program table" this task mentioned **does not exist** — the real sites are the seam router at `:1977-1989` and the `match` inside `run_array_callback` at `:3073`; (4) the `NativeId` band list in this plan was wrong and is replaced by the one derived from `id.rs`'s own group comments.

### Track 5 — Explicitly deferred (recorded, not scheduled)

| Deferred | Why |
|---|---|
| Make `v12-heap`'s `JsObject` fields private (`object.rs:173-252`; engine constructs literals at `promise.rs:195,246,409,416`) | Correct and worth doing (~40 engine sites), but it is a **representation** change — the highest-risk mechanical edit in the repo. Do it only after Track 0 proves gates work; it needs the conformance `faildiff` net that W0.4 creates. |
| Split `v12-engine` (24.7 kLOC, 418 `pub fn`, 29 `pub mod`) | The P3 precedent (`2df4e3a0`) shows splitting produced the largest file in the repo. Revisit after Tracks 1-2. |
| Extract `v12-jit-types` so the JIT crates stop linking the whole interpreter via `crates/v12-codegen/src/lib.rs:41-45` | Real leak, small payoff (4 call sites in `jit-opt/compile.rs:388`). Do it when the JIT actually executes compiled code, post-v1. |
| Replace `v12-jit-opt`'s 22 `#[allow(dead_code)]` | Intentional "speculative tier-2 API", honestly labelled. Keep. |
| Property-based testing beyond the single `feedback.rs` test | Only meaningful once Tracks 1-2 give the property something to assert. Revisit with W1.3. |
| Coverage percentage gate | Start with `cargo-llvm-cov` reporting, no threshold. A threshold set before the refactor would encode today's gaps as the goal. |

---

## 5. Why this order

1. **Enforcement before structure.** The freeze guard regressed from 39 sites to 121 *because nothing ran it in CI* — the missing script at `ci.yml:32` meant clippy never ran, and the red `freeze-lint` result sat in a job that had already failed. Restructuring first repeats this: Track 1's exhaustiveness change touches the compiler's most delicate pass, Track 2's `OpSpec` table touches all four dispatchers. Neither is safe without a green gate and a working `faildiff`.
2. **W3.1 before W3.2/W3.3.** Fixing the `helpers.rs` realm hole first means new unit tests assert correct behaviour rather than encoding a known bug.
3. **W2.1 additive before W2.3 enforcing.** The `OpSpec` table must exist and be trusted before `validate()` starts rejecting bytecode.
4. **W4.3 first of all** — it is minutes of work and pure loss prevention.

## 6. Verification protocol per task

Every task reports all four, or it is not done:

1. `cargo nextest run --workspace` → record the **delta**, not just the total (baseline `92e10484`: 1291 passed, 2 skipped).
2. `./scripts/freeze-lint.sh` → exit 0, with the allowlist monotonically smaller than the previous commit.
3. `./scripts/check-architecture.sh` → exit 0 (W0.1 onward).
4. For any conformance-affecting change: `conformance/faildiff.sh BEFORE.json AFTER.json`, pasted into the commit message. **Any pass regression blocks the commit.** No pass-rate comparison.

## 7. Exit criteria

- **Track 0 done:** a fresh clone on a clean branch is fully green, and clippy's warning count is *visible in CI* rather than absent. `-D warnings` remains off until the 275 reach 0.
- **Track 1 done:** adding an oxc AST variant cannot compile unless Pass A handles it.
- **Track 3 done:** at least one builtin is covered by a test that never constructs an `Engine`.

## 8. Evidence appendix — how to re-check every claim

All commands verified on `92e10484`.

```bash
# F1: gates
./scripts/freeze-lint.sh; echo "exit=$?"      # → exit 1, 16 FREEZE-VIOLATION lines
ls scripts/check-architecture.sh                # → No such file
git log --all --oneline -- scripts/check-architecture.sh   # → zero commits
sed -n '32p;35p' .github/workflows/ci.yml      # → missing script, then clippy with no if: always()
grep -A2 '^\[features\]' crates/v12-engine/Cargo.toml       # → default = ["jit"]
cargo clippy --workspace --all-features 2>&1 | grep -cE '^warning'   # → 275
cargo clippy --workspace --all-features -- -D warnings      # → fails, 15 errors in v12-bccompiler
cargo deny --version                           # → "no such command: deny"
cargo fmt --all --check 2>&1 | grep -cE '^Diff in'          # → 23
cargo nextest run --workspace                  # → 1291 run: 1291 passed, 2 skipped, 7.4s

# F2: exhaustiveness asymmetry
grep -cE '^\s+_ =>' crates/v12-bccompiler/src/collect.rs     # → 23
sed -n '105,115p' crates/v12-bccompiler/src/stmt.rs         # → "proves exhaustiveness to the compiler"
grep -c 'Statement::' crates/v12-bccompiler/src/collect.rs crates/v12-bccompiler/src/stmt.rs  # → 24 vs 42

# F3: operand contract
awk '/^pub enum Opcode/,/^}/' crates/v12-bytecode/src/opcode.rs | grep -cE '^    [A-Z]'  # → 73
sed -n '181p' crates/v12-bytecode/src/opcode.rs            # → hand-written TryFrom<u8>
sed -n '833p;837,839p' crates/v12-interp/src/execute.rs     # → GetEnvSlot vs SetEnvSlot operand order
grep -c 'self\.stack\[' crates/v12-interp/src/execute.rs    # → 134 unchecked writes

# F4: testability
ls crates/v12-engine/tests/*.rs | wc -l                          # → 30
grep -l 'Engine::new' crates/v12-engine/tests/*.rs | wc -l       # → 30 (zero direct-handler tests)
grep -n 'Ctx::new' crates/v12-engine/src/builtins/helpers.rs    # → 26, 48, 64, 134 — all (heap, None, None)

# F5: lost records + false metrics
git show --stat 8722ba90 | grep 'docs/'    # → 4 plan docs deleted, 356 lines
sed -n '13,18p' README.md                  # → "563 tests", "32.1 %"
grep -rn 'builtins-arch-plan' crates/*/src | wc -l   # → 14 dangling citations
git status --short                         # → ROADMAP.md, .orchestrator-notes.md, 3 plans untracked

# §2 growth trend
git ls-tree -r --name-only 2df4e3a0 -- crates | grep '/src/.*\.rs$' \
  | while read f; do git show 2df4e3a0:$f | wc -l; done | awk '{s+=$1}END{print s}'   # → 39358
find crates -name '*.rs' -not -path '*/tests/*' -not -name 'tests.rs' | xargs wc -l | tail -1  # → 73025
wc -l crates/v12-interp/src/call_setup.rs    # → 5406, the largest file in the repo
git log --format='%h %s' --diff-filter=A -- crates/v12-interp/src/call_setup.rs  # → 2df4e3a0, "P3 split lib.rs"
```

## 9. Self-review

- **Spec coverage:** no external spec; every finding in §3 maps to a task in §4 (F1→W0.*, F2→W1.*, F3→W2.*, F4→W3.*, F5→W4.*). §2 is context. §4 "re-scoped out" and Track 5 list six findings with reasons rather than silently dropping them.
- **Placeholder scan:** no `TBD`/`TODO`. W1.1 is a genuine decision gate — the oxc `INHERIT` mechanism admits two defensible designs, and choosing without the spike is how the previous attempt would fail; it produces an ADR, not code. W0.6/W0.7 are named as "was" with the measurement that blocked them.
- **Type/name consistency:** `Ctx::to_integer_or_infinity` and `Ctx::species_constructor` are introduced in W3.2 and consumed by W3.3; `OpSpec` is introduced in W2.1 and consumed by W2.2/W2.3; `Ctx::for_test` is introduced in W3.3 and used by W3.4.
- **Untraced:** `cargo deny` has still never been executed, so W0.6's re-scope is based on the tool being absent, not on what it would report; the `OpSpec` design was not benchmarked against the `mini.rs` and `jit-baseline` dispatchers, so W2.1 may need a third representation for width-only dispatch; the `Ctx` conversion of all 130 heap-only call sites was not costed per file; `cargo fmt --all --check` was sampled by counting `Diff in` lines, not by reviewing the diffs.