# v12 code-quality refactor plan

Working document for the idiomatic-Rust / DRY / KISS refactor. Ordered by
priority (P0 correctness hazards → P4 mechanical fixes). Each item lists the
concrete files/lines and the intended change. Status column updated as work
lands. Baseline at plan start: 563 unit tests green; conformance
`--filter language` = 9,617 ok / 15,256 not ok (TAP snapshot at
`/tmp/t262-before.tap`).

**JIT decision (user directive):** the JIT is *wired into* execution behind a
`V12_JIT` environment variable; the variable is **unset by default**, so the
JIT stays off unless explicitly enabled.

---

## P0 — correctness hazards (divergence-caused)

| Status | Item |
|--------|------|
| done | **super spread call**: spread path in `bccompiler/expr.rs::call` mirrors non-spread receiver handling incl. `super(...)` (`CallApply` gets `this` = REG_THIS for super). |
| done | **`u16::try_from(idx).unwrap_or(0)` hazards**: silent truncation of function indices replaced with a real `CompileError` ("programs above 65535 functions are not supported") — `emit_closure_instr` (expr.rs, now `pub(crate)`), `class.rs` ctor/method closures, `unit.rs` func_idx. |
| done | **class.rs silent failure paths**: `load_const(...).unwrap()` → `?`; `format!("#{}")` + `unreachable!()` replaced by `static_key_text(...).ok_or_else(err)`; duplicated static/instance `DefinePrivateW` branches collapsed. |
| done | **jit-baseline wide-op validation**: `LoadConstW`/`LoadIntW`/`CallW` now validate register indices against `max_regs` (previously indexed `vars` unchecked). |
| done | **jit-baseline branch bounds**: `JumpIfFalse`/`JumpIfTrue` merged into one arm; both target *and* fallthrough block indices bounds-checked (previously `blocks[pc + 1]` unchecked → panic on malformed bytecode). |
| done | **jump-tail duplication**: 18 copies of the "jump to next block or exit" tail replaced by `jump_to_next(&mut builder, &blocks, next, exit_block)`. |
| done | **with-statement** (`bccompiler/stmt.rs`): silently compiled with scope extension ignored (wrong binding resolution) → now a `CompileError` ("with statements are not supported"). Costs ~24 accidentally-passing `language/statements/with` test262 tests (false passes from the scope-ignoring path); honest failure replaces silent misbehavior. |
| done | **legacy `.elements` store for arrays removed** (user directive): `elements_array` is the single element store for `Kind::Array`. New sanctioned accessors on `JsObject` (v12-heap/object.rs): `get_element`, `element_len`, `elements_snapshot`, `set_element`, `push_element`, `pop_element`, `delete_element`, `replace_elements`. Direct `.elements` field access remains only for non-array overloaded uses (arguments exotics, map/set entries, iterator/generator/promise slots). All former Array-kind `.elements` readers/writers (internal_methods get/has/define/set/delete, interp op_in/delete/array_element/array_set_element/apply/push, engine array builtins, join) now dispatch via the accessors — this also fixes the pre-existing divergence where `push` wrote only `elements_array` while `Object.*`/`in`/`delete` read the stale `.elements`. `JsObject::array` no longer mirrors elements into `.elements`. |
| done | **Object.defineProperty slot-0 overwrite** (`engine/builtins/object.rs`): the "skeleton" path overwrote `properties[0]` for any existing key and never bound the child shape for new keys. Now delegates to `internal_methods::ordinary_define_own_property` (made `pub(crate)`), which handles slots, kinds, and shape binding correctly. |
| done | **eval/compile error interning**: repeated `if ascii { latin1 } else { utf16 }` interning blocks (engine.rs ×8+) replaced by `string_value(heap, text)` / `heap.intern_text`. |

## P1 — DRY consolidation (same concept N copies)

| Status | Item |
|--------|------|
| done | **`helpers::intern_text` deleted** — identical to `Heap::intern_text`; all call sites (error.rs, object.rs, mod.rs) call the heap method directly. |
| done | **`Interp` program-table churn**: `Interp::new`/`new_with_heap`/`register_program` take `impl Into<Rc<[T]>>` so callers can hand over `Rc` clones; `engine.rs` no longer `.to_vec()`s `Rc<[FunctionBytecode]>`/`Rc<[String]>` back into Vecs (was a full deep copy per eval/call_global/run_jobs). |
| done | **eval twins unified**: `eval_inner` (script) and `eval_module_source` (module) share `Engine::run_compiled(global, functions, main, strings, natives)` (retain → run → drain checkpoint → capture completion). `Engine::error_to_value` flattens `EngineError` → legacy `JsValue` error for `eval_direct`/module paths. |
| done | **intrinsic table consolidation** (2026-09-05): the realm-installed 18-name list is now `v12_bytecode::GLOBAL_INTRINSICS` (the only copy; the 4 hand-synced duplicates are gone); realm and interp import it, and `GLOBAL_VAR_OFFSET` is derived as `GLOBAL_INTRINSICS.len()` in v12-bytecode (interp imports it), so the slot-order contract can no longer drift. The compiler's intentionally longer superset moved to `v12_bytecode::GLOBAL_ACCESS_INTRINSICS` (re-exported by `model.rs` as `GLOBAL_INTRINSICS`). All 87 `NATIVE_*: NativeId = NativeId::X` alias consts deleted (v12-interp ×39 incl. duplicates, v12-engine builtins ×48); remaining call sites use `v12_native::NativeId::Variant` directly. |
| done | **bccompiler helpers** (2026-09-05): `FnCtx::global_name_id(name)` replaces the 15 `str_id_of(strings.get_or_intern(..))` global-interning sites (non-global interns — private keys, BigInt const ids — keep direct `str_id_of`); `FnCtx::with_loop(break, cont, name, body)` replaces all 7 `loops.push/pop` pairs incl. the labeled-statement and switch arms; `FnCtx::lower_default(src, default, span)` is the single `x === undefined ? default : x` lowering (binding patterns, for-of `bind_with_default`, and — newly — assignment-target destructuring); the stmt.rs for-of copies `read_index_for_of`/`assign_simple_for_of` are deleted in favor of `pub(crate)` expr.rs `read_index`/`assign_simple`. **Correctness fix riding along:** the expr.rs assignment-target destructuring walker silently skipped array-element defaults (`[a = 1] = []` bound `a` as the raw read) and ignored object-property defaults (`{x = 5} = {}`); both now apply the default via `lower_default`. Remaining: the full merge of the stmt.rs binding-pattern walkers with the expr.rs assignment-target walker (different oxc types; both now share `lower_default`, `read_index`, `assign_simple`) is deferred. |
| done | **realm.rs** (2026-09-05): `wire_callable` replaces the 8 near-identical callable-wiring blocks (String/Error/Boolean/Map/Set/RegExp/eval/Number) and `wire_prototype` the constructor→prototype links; alloc+root blocks reuse `helpers::alloc_obj` (`alloc_root`); the dead `intern_key` (results discarded at both call sites) is deleted. Map/Set construction merge landed in builtins/map.rs: stride-generic `entries`/`construct`/`has`/`delete` helpers shared by both collections (~60 lines). |
| done | **string.rs helpers** (2026-09-05): `this_string` replaces the 6 `as_string().ok_or_else(TypeError)` prologues; `as_regexp` + `replace_first_occurrence` collapse the doubled non-regexp fallbacks in match/replace/search/split; `collect_match_spans` is the single exec-loop (match/replace/split previously each hand-rolled it; zero-width guard, groups, global break included). |
| done | **HostFn type alias** (builtins/mod.rs): `pub type HostFn = dyn FnMut(…)`; `HostClosure(Rc<RefCell<HostFn>>)` — clippy type_complexity gone. |
| done | **proxy trap stub macro** (internal_methods.rs): `proxy_trap_stub!` generates the 8 `Err(TypeError: … trap not implemented)` stubs (set_prototype_of, prevent_extensions, get_own_property, define_own_property, has_property, get, set, delete); non-throwing stubs stay hand-written. |
| done | **bool→JsValue**: `const JsValue::from_bool(bool)` added on the heap value type (instead of an engine-only helper — the interpreter has 4 sites too); all ~14 manual `if b { true_() } else { false_() }` sites (map, number, boolean, array, object, regexp, iterator, interp ×3) now call it. |
| done | **jit-baseline op-arm tables** (2026-09-05): the 12 binary-operator arms (Add…StrictNe) in the Cranelift table and in the exec closure are each generated by a local `binop!` macro calling shared `emit_binop` / inline bodies; Neg (unary) stays explicit. Cross-crate sharing with jit-opt was evaluated and deliberately skipped: jit-opt's arms diverge (feedback guards, placeholder `fn_add`) and its `build_ssa_ir` path is P2-deletion material. |

## P2 — delete dead/speculative machinery

| Status | Item |
|--------|------|
| todo | JIT: `build_ssa_ir` (jit-opt compile.rs:451-795, result discarded at 946), `TierCompiler` trait (v12-codegen:66), `OptCompiler` external API, `ClashCounter` (guard.rs:199-242), `DeoptMap::live_regs`, mmap.rs doc module, dead `cranelift-jit` dep; scope the blanket `#![allow(dead_code)]` on jit-opt modules to individual items. |
| todo | heap: unused `inline_props`/`overflow`/`prop_slot`/`set_prop_slot` (also untraced — hazard), `EnginePromise` trait (object.rs:653), `alloc_array_with_roots`, `JsObject::new`. |
| todo | engine: `install_core` (builtins/mod.rs:638), `translate_value` (engine.rs:922), `new_pending_promise` alias + `EnginePromiseFactory` trait, `eval`/`eval_unwrap_value` dup (drop one, keep `eval`). |
| todo | v12-native: ~300 lines dead (`native_table!`, `typed_wrapper!`, `NativeSig`, `RuntimeRegistry`, `BUILTIN_METHODS`). |
| todo | bccompiler: `script_linkage_error`, `GLOBAL_INTRINSICS` re-export, `Compiler.strict`. |

## P2b — wire the JIT behind `V12_JIT` (user directive)

| Status | Item |
|--------|------|
| todo | Investigate the tier seam: interp has `set_hooks` / tier hooks; jit-baseline's executor is heap-agnostic and bails with `UnsupportedOpcode`. |
| todo | Wire: when `std::env::var("V12_JIT")` is set (default **unset/off**), install the baseline JIT hook on the interp/engine path; when unset, pure interpreter. No behavior change by default; unit test covers both modes. |

## P3 — structural splits (giant files/functions)

| Status | Item |
|--------|------|
| todo | v12-bytecode lib.rs (2,226 lines) → opcode.rs / wide.rs / builder.rs / analysis.rs. |
| todo | v12-interp lib.rs (5,787 lines; `execute` ~830 lines) → split by concern. |
| todo | engine.rs (1,617 lines) → eval.rs / host_fn.rs / display.rs. |
| todo | builtins/mod.rs role split; `define_opcodes!` macro maintains 5 parallel opcode tables — collapse. |

## P4 — mechanical clippy fixes

| Status | Item |
|--------|------|
| todo | `collapsible_if` ×5 (interp 1116, 1215, 1293, 1294, 2867). |
| todo | array.rs sort: `sort_by` with per-comparison `value_text` allocation → `sort_by_cached_key`. |
| todo | `let_and_return` / `redundant_closure` / `needless_borrow` / `unnecessary_sort_by` in array.rs. |
| todo | `map_err(Throw::Value)` normalizations, `then_some().is_some()` (jit compile.rs:399-405), identity match (compile.rs:970-974), `HashMap<u32, bool>` → `HashSet` (guard.rs), `format!("{}", v)` → `to_string()` (expr.rs:1903), underscore params instead of `let _ =`. |

## Verification protocol

1. `cargo nextest run --workspace` — 563 tests must stay green after every step.
2. Conformance before/after: `./conformance/run.sh --filter language --format tap --tap-out /tmp/t262-<name>.tap`; diff ok-lists (`comm` on sorted test names). Known accepted delta so far: **−24** (`with` false passes → honest compile error), **+1** (delete/elements fix).
3. `cargo clippy --workspace --all-targets` — 0 errors; deliberate-policy warnings (`unwrap_used`/`expect_used`/`panic` in audited sites) are acceptable.
4. Update `conformance/fix-log.md` per project convention when conformance moves.
