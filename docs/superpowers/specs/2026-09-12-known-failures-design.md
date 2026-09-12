# Known-failures fix plan — design (2026-09-12)

## Context & constraints

- Scoreboard: `conformance/known-failures.md` buckets A–F on `language/expressions` (~36.8%).
- Phase 1 gate: ≥60% on `test/language`, zero `engine panic`, jit vs no-jit JSON diff zero.
- Constraint from 2026-09-02 incident: never fan out parallel write lanes on the same files; no lane runs `git stash`/`reset`/`checkout` or workspace-wide `cargo fmt`. Fixer lanes run sequentially.
- After each phase: `cargo nextest run --workspace` + bucket filter before/after pasted into `fix-log.md`.

## Recommendation

Lift-ordered vertical slices: A1 → A2 → B → E → D → F sweep → C last.

## Phases

### 1. A1 Function global (~800)

- Problem: `Function` absent from `GLOBAL_INTRINSICS` and `GLOBAL_ACCESS_INTRINSICS` (`v12-bytecode/src/lib.rs:308`), no `intrinsic_slot` arm (`v12-interp/src/lib.rs:146`). Bare `Function` (incl. `typeof Function`, `propertyHelper.js` line 3) is a `CompileError`. `globalThis.Function` resolves via shape path only.
- Change: add `Function` slot, bump `GLOBAL_VAR_OFFSET`, materialize ctor in `realm.rs:47-88` with prototype/length/name links (reuse `function_construct` seam).
- Gate: `class/elements` filter + propertyHelper standalone + nextest.

### 2. A2 param-default registers (~600)

- Problem: `stmt.rs:388-419` duplicates `lower_default` inline instead of calling `model.rs:672`; bump-only `new_temp` with no temp-scope lets member reads on destructured bindings clobber the callee window.
- Change: route binding defaults through shared `lower_default`; add temp-scope/callee-window guard (`expr.rs:1676-1790`, `unit.rs:233`).
- Gate: `var f = ([a = () => {}]) => { var n = a.x; … }; f([])` repro + `class/dstr`, `object/dstr` filters.

### 3. B negatives (~975 + ~2044 frontmatter)

- Problem: missing early-error validations silently accept; `eval.rs:89-95` flattens `Compile` to string so phase/type is lost; `.name` erasure on expected ctors; strict-mode writes silently drop (`property.rs:657`), undeclared reads yield `undefined` (`engine.rs:612`) instead of ReferenceError.
- Change: preserve Compile/Thrown distinction for `handle_thrown`; real error objects with own name/message/constructor; early-error slices in order class → object/assignment-target → strict-mode → instanceof/BigInt/Symbol.
- Gate: group-by-message×path before/after.

### 4. E iterators (~300)

- Problem: `jump_out` emits plain `Jump` with no `IteratorClose` (`stmt.rs:979`, `with_loop` has no close slot); `op_iterator_close` swallows return() result/throw (`object_ops.rs:359`); yield* has no close/return/throw delegation (`expr.rs:250`); for-await has no lowering.
- Change: emit close on abrupt, propagate return() errors, add yield* delegation, lower for-await.
- Gate: abrupt-close filter.

### 5. D coercion (~800, four slices)

- Problem: `to_number` maps objects to NaN with no valueOf (`ops.rs:50`); loose object↔primitive is false with no ToPrimitive (`ops.rs:375`); ±0/NaN vs `Object.is` mismatches; Number formatting/property-attribute gaps.
- Change: one sub-suite at a time, grouped by message×path.
- Gate: SameValue/boolean message groups.

### 6. F sweep

- Expect most of the 430 to evaporate after A/B. Only independent leg: `0xFFFFFFFF` sentinel → "not a function" (`lib.rs:1424`) as missing-builtin signal.

### 7. C loader (324) last

- Problem: `module_import` stub returns rejected promise (`builtins/mod.rs:839`); no ModuleMap/resolve/link/evaluate; `eval_module_source` uses dummy namespace and ignores `_base`.
- Change: ModuleMap + resolve/link/evaluate, promise via `JobQueue`/`drain_checkpoint` (`eval.rs:312`). First satisfy negative-phase + promise shape; full ESM per `language-coverage-plan §1b row 11`.

## Verification

Per phase: one sequential @fixer lane, `cargo nextest run --workspace`, bucket filter before/after into `fix-log.md`, delete bullet when green on `test/language`.
