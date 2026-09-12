# Known-failures fix plan — design (2026-09-12)

## Context & constraints

- Scoreboard: `conformance/known-failures.md` buckets A–F on `language/expressions` (~36.8%).
- Phase 1 gate: ≥60% on `test/language`, zero `engine panic`, jit vs no-jit JSON diff zero.
- Constraint from 2026-09-02 incident: never fan out parallel write lanes on the same files; no lane runs `git stash`/`reset`/`checkout` or workspace-wide `cargo fmt`. Fixer lanes run sequentially.
- After each phase: `cargo nextest run --workspace` + bucket filter before/after pasted into `fix-log.md`.

## Recommendation

Lift-ordered vertical slices: A1 → A2 → B → E → D → F sweep → C last.

## Phases

### 1. A1 Function.prototype + real bind (~718 in class/elements)

- Correction (2026-09-12 recon): the original premise is stale. `Function` already works as a global (`typeof Function === "function"`, `Function("a","return a+1")(1) === 2`, `globalThis.Function === Function`); it is installed via `install_native(heap, Some(global), "Function", NativeId::Function)` at `realm.rs:211`, not as an intrinsic slot. Adding an intrinsic slot is NOT the fix.
- Real problem: `Function.prototype` is `undefined`. `function_proto` is allocated ordinary at `realm.rs:157` and never linked to the Function constructor (the `install_ctor` loop at `realm.rs:168-179` covers only Object/Array/String/Number/Boolean/Symbol). So `function_method_surface` (`property.rs:375`, gated on `Kind::Function`) never fires for it, `Function.prototype.call` reads `undefined`, and `propertyHelper.js` line 31 `Function.prototype.call.bind(Array.prototype.join)` throws `TypeError: callee is not a function`. 718 of 1015 `class/elements` failures carry exactly that message.
- Second gap: `NativeId::FunctionBind` (`call_setup.rs:1215`) is a stub that returns the receiver unchanged. `propertyHelper.js` binds a *curried* call (`Function.prototype.call.bind(Object.prototype.hasOwnProperty)`), so `__hasOwnProperty(obj, name)` must dispatch through the bound target with a bound `this`. A stub cannot satisfy this.
- Change:
  1. Allocate `function_proto` as a `Kind::Function` object (callable returning `undefined`), and link the Function constructor to it via `install_ctor` after `realm.rs:211` (the constructor handle must be captured at install time).
  2. Add `FunctionTarget::Bound(Handle<JsObject>)` referencing a state object whose `elements` hold `[target, thisArg, boundArgs..]`; trace it in `FunctionTarget::trace` and update every exhaustive match site.
  3. Implement `FunctionBind` to allocate that state object plus a bound function object, and dispatch `Bound` in `prepare_call` and `prepare_call_apply` by delegating to `call_object(target, thisArg, bound ++ actual)`.
- Gate: `class/elements` filter (718 → near 0 for `callee`), propertyHelper standalone, nextest.

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
