# Builtins / Dispatch Clean Architecture Plan

## 0. Context & constraints
- Hybrid dispatch must keep working throughout migration:
  - Pure natives via `define_builtins!` (`crates/v12-engine/src/builtins/mod.rs:230-268`).
  - Callback methods via `Interp::run_callback_builtin` (`crates/v12-interp/src/call_setup.rs:1426-1459`).
- Slot-order contract is load-bearing and stays in sync:
  - `GLOBAL_INTRINSICS` (`crates/v12-bytecode/src/lib.rs:308-329`) + `intrinsic_slot` + guard (`crates/v12-interp/src/lib.rs:146-206`) + push order (`crates/v12-engine/src/realm.rs:47-88`).
- Recent drift = the risk: ad-hoc `length` wiring (`lib.rs:698-710`), per-call `wire_rest_array_identity` (`lib.rs:797-836`, `call.rs:90-96`), `string_prim_surface` branches (`property.rs:87-117`), frame `arguments` hacks (`lib.rs:740-791`, `globals.rs:120-154`), `syntax_error_value` (`registry.rs:265-297`).
- Gate every step: `cargo nextest run --workspace` green + affected test262 slices.

## 1. Target shape: `Ctx` + canonical signature

### 1.1 `Ctx` definition
`Ctx` is the sole context a builtin receives. It owns:
- `heap: &mut Heap` (exclusive borrow, never shared).
- `realm`: global handle + intrinsic-slot reader (`GLOBAL_INTRINSICS` index, never hardcoded `properties.get(1)`).
- `intrinsics`: constructor + prototype handles for error creation and `prototype` linking.
- `call`: re-entry for user callbacks (`call_inline` equivalent). Only callback-declared builtins get a `Ctx` with this capability.
- `roots`: allocation + rooting + `gc_protect` helpers (`alloc_obj`, stack-park pattern).
- `errors`: `type_error`, `range_error`, `syntax_error`, `reference_error` returning real error objects.
- `conv`: `require_object_coercible`, `to_primitive`, `to_number`, `to_string`, `to_length`, `to_index`.
- `props`: `define_data_prop`, `define_method`, `install_ctor` (see §3).

### 1.2 Canonical signature
```rust
pub type This = JsValue;
pub type Args<'a> = &'a [JsValue];
pub type BuiltinResult = Result<JsValue, Throw>;
pub type BuiltinFn = fn(&mut Ctx, This, Args) -> BuiltinResult;
```
`NativeHandler` (`registry.rs:45`, today `fn(&mut Heap, JsValue, &[JsValue])`) becomes this alias after migration. Arity lives in the declaration, not the body.

### 1.3 FORBIDDEN in builtin bodies
- Direct `heap.get(obj).properties[i]` / `properties.get(idx)` reads/writes.
- Direct `property_keys` pushes/resizes (current violations: `regexp.rs:321`, `iterator.rs:75`, `json.rs:231`, `object.rs:581`).
- Direct `heap.alloc` without rooting, direct `heap.intern_text` bypassing `Ctx`, direct `shape_of_mut` + `add_property` + `bind_shape` outside the install family.
- Raw `Value` poking: `as_smi` fallback chains instead of `ctx.to_number`, string flattening instead of `ctx.to_string`, `element_len` + `get_element` instead of `ctx.to_length` + accessor.
- Reaching into `Interp` fields (`stack`, `frames`, `natives`, `global`) from a builtin. Only `Ctx` methods are visible.

Justification: Law of Demeter + Single Responsibility. Heap layout changes often; one seam absorbs the change.

## 2. Dispatch layers

### 2.1 Three layers, one direction
1. **Registry (ID table).** `NativeId` (`crates/v12-native/src/id.rs`) stays the stable out-of-range callable index. `builtin_dispatch` (`builtins/mod.rs:245-256`) stays the jump-table match. No trait objects, no hash lookup on hot path.
2. **Call-setup routing.** Exactly one normalization point in `call_setup.rs`. `prepare_call`, `prepare_call_apply`, `prepare_construct`, `call_accessor_with`, `call_inline` all funnel native IDs through one `dispatch_native(ctx, id, this, args)` — tries `run_callback_builtin` first, then `NativeRegistry::call_native`.
3. **Per-builtin modules.** `builtins/array.rs`, `string.rs`, `object.rs` + siblings contain only spec logic. They take `&mut Ctx`. They never touch dispatch state.

### 2.2 What moves where
- Delete the five duplicated dispatch copies: `prepare_call` (`call_setup.rs:48-346`), `prepare_call_apply` (`1090-1096`), `call_accessor_with` (`493-500`), `call_inline` (`587-689`), `prepare_construct` constructor seam (`1211-1242`).
- Keep `NativeId::Eval`, `Function`, `FunctionCall/Apply/Bind`, generator natives as explicit router arms (need stack/program tables; not builtins).
- Keep `callback_stub` entries only until the shim phase ends; then replace with real `Ctx` handlers marked `needs_call`.
- Move `callback_len`, `callback_elem`, `callback_dense_bound` out of `Interp` into `Ctx` array helpers. Builtins stop branching on `Kind::Array` directly.

### 2.3 Install-time metadata
Arity, `name`, `length`, constructor behavior, `prototype` linkage live in the `define_builtins!` declaration. `install_builtins` expands each entry into one `install_builtin_fn` call. No per-call wiring survives: per-call `wire_rest_array_identity` dies, `install_function_length` via `set_property` dies, lazy `prototype` creation in `prepare_construct` dies for builtins.

## 3. Property installation contract

### 3.1 One helper family
- `ctx.define_data_prop(obj, key, value, attrs)` — shape-descriptor based. Wraps `add_property` + `bind_shape` + storage push as one atomic step.
- `ctx.define_method(obj, name, id, length)` — allocates `Kind::Function`, installs `length` + `name`, then `define_data_prop` with `Attrs::BUILTIN`.
- `ctx.install_ctor(global, name, ctor_id, proto, length)` — allocates ctor + prototype, installs `constructor` back-link, installs `prototype` with spec attrs, publishes both.

`builtin_install_prop` (`mod.rs:54-72`) + `install_native` (`mod.rs:81-95`) + `install_value` (`mod.rs:104-114`) collapse into this family. `__builtin_emit_install!` arms route through it.

### 3.2 Attribute defaults
- Methods, statics, constants, `prototype` links: `Attrs::BUILTIN` (writable + configurable, not enumerable) per `shape.rs:76-78`.
- Function `length`: `{ writable: false, enumerable: false, configurable: true }`.
- Function `name`: `{ writable: false, enumerable: false, configurable: true }`.
- Constructor `prototype`: `{ writable: false, enumerable: false, configurable: false }`.
- `caller` + `arguments` on builtin functions: thrower, never plain data.
- No caller passes raw bits; helper takes an enum (`Method`, `Length`, `Name`, `CtorProto`). Spec default is compiler-enforced.

### 3.3 Source of truth
Shape descriptors are authoritative. The parallel `property_keys` vec is a mirror only. New code never reads `property_keys` to enumerate; enumeration uses `heap.get(shape).descriptors`.

### 3.4 Invariants + debug assertions
- `install_builtin_fn` asserts: descriptor slot == storage index, attrs == declared enum, `length` == declared arity, no duplicate key install.
- `install_ctor` asserts: `ctor.prototype` == installed `prototype` object, `proto.constructor` == ctor, both directions rooted.
- Realm construction asserts: global `properties[0..INTRINSIC_COUNT]` order == `GLOBAL_INTRINSICS` order. Existing `intrinsic_slot_guard` stays; add realm-side `debug_assert` on push order (`realm.rs:82-88`).
- `set_property` on a fresh builtin function is a bug. Fresh installs use only the install family.

## 4. Error + conversion helpers

### 4.1 Central conversions via `Ctx`
- `ctx.require_object_coercible(v)` — throws `TypeError` on `undefined`/`null`.
- `ctx.to_primitive(v, hint)` — honors `Symbol.toPrimitive`, then `valueOf`/`toString`. Needs call capability; pure builtins take the no-call subset only.
- `ctx.to_number(v)`, `ctx.to_string(v)`, `ctx.to_length(v)`, `ctx.to_index(v)` — replace `helpers::to_number` (`helpers.rs:163-185`) + `value_text` (`helpers.rs:75-84`) + ad-hoc length reads (`array.rs:13`, `regexp.rs:297`).
- `ctx.this_object(v, method, kind)` — replaces `helpers::as_object` (`helpers.rs:20-41`).

### 4.2 Real error objects
- Today: `Throw::type_error` (`throw.rs:25-27`) interns plain `"TypeError: msg"`; `error_value` (`call_setup.rs:1001-1011`) builds `Kind::Error` ad hoc; `syntax_error_value` (`registry.rs:265-297`) hand-installs `name`/`message`/`constructor`.
- Target: `ctx.type_error/range_error/syntax_error/reference_error(msg)` construct `Kind::Error` with own `name`, `message`, `constructor` wired to the realm intrinsic (`TypeError`/`RangeError`/`SyntaxError` slots at `lib.rs:308-329`).
- `Throw::Value` then always carries an object. `assert.throws(SyntaxError)` passes without special cases. Plain-string throws disappear except at the outermost `Throw::Message` boundary, resolved through `Ctx` before crossing into JS.

## 5. Migration path (phased, each step gated)

1. **Freeze.** No new `property_keys` pushes, no new `*_surface` arms (`property.rs:17-80`), no new `wire_*` helpers, no new stringly `error_value` call sites. CI `grep` lint for `property_keys.push`, `properties.get(1)`, `error_value("`.
2. **Introduce `Ctx` + adapter shim.** Add `Ctx` wrapping `&mut Heap` + global + intrinsics + pending-job sink. Add `BuiltinFn` alias + blanket adapter from `BuiltinFn` to legacy `NativeHandler` so existing `fn(&mut Heap, …)` bodies compile unchanged. Gate: workspace nextest green.
3. **Migrate file-by-file.** Order: `helpers.rs` conversions first, then `math.rs` (pure, no `this`), `number.rs`, `boolean.rs`, `object.rs`, non-callback `array.rs`, `string.rs`, `json.rs`, `map.rs`, `iterator.rs`, `regexp.rs`, `promise.rs`, `global.rs`, `error.rs`, `symbol.rs`. Each file: change bodies to `&mut Ctx`, replace direct heap pokes with `ctx` calls, keep dispatch IDs unchanged. Gate per file: nextest + matching test262 slice (`Array`/`Math`/`Number`/`Object`/`String`/`JSON`/`Boolean`/`global` baselines).
4. **Collapse install paths.** Extend `define_builtins!` entries with `length`. Route all installs through `define_method` + `install_ctor`. Delete `install_native` + `builtin_install_prop` + per-site `prototype` installs (`realm.rs:164-181`). Move rest-array identity + `arguments` object creation to construction time (shape + bind once, no per-call wiring). Gate: nextest + `Function.prototype`, `Array`, `arguments` slices.
5. **Collapse call helpers.** Funnel five dispatch copies into one `dispatch_native`. Move `RealmEval` + `eval` + `function_construct` out of builtins into the router. Delete `eval_stub` + `function_stub` + `console_log` duplicates (`mod.rs:592-655`). Gate: full nextest + realm/eval slices.
6. **Enforcement.** Turn lints into hard errors: forbid `property_keys` mutation outside install family, forbid `error_value(` outside `Ctx`, forbid `wire_rest_array_identity`, add `debug_assert` coverage for `length` + `name` + `prototype` on every installed builtin. Gate: full nextest 570/570 + recorded test262 slices, no regressions.

## 6. What NOT to do (YAGNI guards)
- No generic framework over `BuiltinFn`. No proc macros beyond the existing `define_builtins!` extension. One `match` table + one install family is enough.
- No trait-object dispatch. `NativeHandler` stays a `fn` pointer. Runtime `handlers` map (`registry.rs:27`) stays for host closures only.
- No bytecode slot-contract rewrite. `GLOBAL_INTRINSICS` order, `GLOBAL_VAR_OFFSET`, `intrinsic_slot` + guard, `realm.rs:47-88` push order stay as-is. Only readers centralize into `Ctx`.
- No full spec-conversion rewrite in this pass. `to_primitive` with user-code re-entry arrives only where test262 demands it; subset helpers stay until then.
- No wrapper objects for string/number/boolean primitives in this pass. `string_prim_surface` shrinks to method-table reads only; full wrapper semantics wait for their own ADR.

## 7. Current-state recon (grounding, 2026-09-06)
- Defs: `define_builtins!` single-sources dispatch+install; `NativeId` explicit `u32` = serialized callable; `GLOBAL_INTRINSICS` order = global.properties prefix order (`GLOBAL_VAR_OFFSET` bias); `intrinsic_slot` jump table + const guard enforces it.
- Dispatch: `FunctionTarget::Bytecode(idx<len)` = frame push; `idx>=len` + `NativeId` match = interp seam (generators/callbacks/Eval/Function/Call/Apply) else `natives.call_native` (compile table → stateful intercept → handlers map); property reads synthesize `Bytecode(id)` fn objects via surfaces + `lookup_method`.
- Ctx today: NO `Ctx` struct — builtins take `(&mut Heap, this:JsValue, args:&[JsValue])`; stateful extras ad-hoc (promise `&pending` Rc, regexp/string `&regex_cache`); `JobCtx` only for microtasks, never builtins.
- Props: `builtin_install_prop` (shape + `Attrs::BUILTIN`) / `install_native` (alloc fn + install) / `install_value` (dispatch-once constants); realm `wire_callable`/`wire_prototype`; interp `install_function_length` / `wire_rest_array_identity` / string length branch.
- Sloppiness top 5: (a) dual install paths (descriptor-less intrinsic prefix vs shape-bound props + offset bias); (b) dead/placeholder installs (`ErrorProto`/`RegExp`/`Map`/`Set` → `None`, `u32::MAX` placeholders, ad-hoc `Function`); (c) stringly/key_is probe chains + hardcoded slot indexes (`properties[0/1/2/4]`, Promise `props[10]`); (d) callback split-brain (engine `callback_stub` unreachable + interp seam, `NativeHandler` lacks re-entry); (e) saturating/direct-heap hacks (arity clamps, direct `properties[]` I/O bypassing shapes, liberal `add_root`).

## 8. Deferred / follow-ups (recorded in Phase 6, 2026-09-06; all resolved 2026-09-07)

- `wire_rest_array_identity` — DONE (`facc95a`): deleted by folding, not
  wiring. Rest arrays build like `NewArray` literals (shape + `Kind::Array` +
  bind, no per-call proto/constructor fixup). Only observable delta:
  `rest.constructor` reads `undefined`, identical to array literals.
- Stub deletion (`eval_stub` / `function_stub` / `console_log`) — DONE
  (`5ddae9b`): three direct `call_native` callers rerouted through
  `dispatch_native`; bare entries + fallback impls deleted. `callback_stub`
  stays (carries installs for seam-intercepted builtins).
- Regexp-cache carrier + string methods — DONE (`facc95a`): cache is a `Ctx`
  capability (`with_regex_cache`); all 7 fns are canonical `BuiltinFn`.
- Length-arity audit — DONE (`5ddae9b`): 149 `(len)` arities landed. Unblocked
  by an attrs-aware shape-transition fix (`shape.rs`/`gc.rs` key transitions
  by `(key, attrs)`). Known gap: callback-path installs (`[].map.length`,
  `Object.keys.length`) still report `undefined` (pre-existing split-brain).
- Real error objects (§4.2) — DONE (`269e9d2`): `Throw::Value` always carries
  `Kind::Error`; JSON plain-string bucket 11 → 0. Remaining follow-up:
  thrown-error `constructor` identity needs dispatch to supply a global to
  `Ctx` (thrown `thrown.constructor !== SyntaxError` residuals).
- `call_accessor_with` reroute — DONE (`cdb1bb7`): OOR path funnels through
  `dispatch_native`; unknown indices keep the historical `undefined`.
