# Contributing to v12

Read this before touching `crates/`. Adding one `Array.prototype` method takes
**seven steps**. Only two of them are discoverable from the code; the other five
are convention, and two of those five fail *silently* rather than at compile
time (steps 3 and 4).

## Where things live

| Crate | Role |
| --- | --- |
| `v12-heap` | L0. `JsValue`, `JsObject`, shapes, the collector. **Zero internal deps.** |
| `v12-bytecode` | L0. `Opcode`, `FunctionBytecode`, `GLOBAL_INTRINSICS`. **Zero internal deps.** |
| `v12-native` | `NativeId`, the name→id method tables, `Throw`, the `NativeRegistry` trait. Deps: heap, bytecode. |
| `v12-bccompiler` | oxc AST → bytecode. Deps: bytecode. |
| `v12-interp` | The bytecode machine: dispatch, property access, calls, generators. Deps: heap, bytecode, native, bccompiler. |
| `v12-engine` | The realm, the globals, every builtin body, the concrete `NativeRegistry`. |
| `v12-api` | Embedder facade (`Context`). The only crate a host should import. |
| `v12-cli` | The `v12` binary. |
| `v12-codegen`, `v12-jit-baseline`, `v12-jit-opt` | JIT tiers. Wired behind `V12_JIT`; compiled code is not executed yet. |
| `test-support` | Shared test helpers. `mini.rs` is a third, partial dispatcher — read before using it. |
| `conformance/harness` | The test262 runner (`test262-runner`). |

The graph is a DAG with `v12-heap`/`v12-bytecode` at the bottom. Do not add an
edge that makes a cycle.

## Adding an `Array.prototype` method: the seven steps

### 1. Write the handler body

```rust
pub fn array_foo(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw>
```

**`&mut Ctx`, not `&mut Heap`.** The `Ctx` migration is complete for handler
bodies: every one of the 298 rows in the `define_builtins!` table dispatches
through `call_ctx` (279) or `callback_stub` (19) — there is no third form —
and the only remaining `pub fn …(&mut Heap, …)` in `builtins/` are realm
installers and pure helpers (`install_builtins`, `string_text`, `to_number`).
`NativeHandler = fn(&mut Heap, JsValue, &[JsValue])` is the **embedder
host-function** signature; `NativeRegistry::register` has no in-tree caller.
Private helpers inside a body may still take `&mut Heap` (`array.rs:41`
`array_length`), and most of `array.rs` does — that is fine.

### 2. Add a `NativeId` variant

`crates/v12-native/src/id.rs`. Discriminants are explicit and are the
serialized callable index, so **never renumber an existing variant**.

| Bands | Owning group (`id.rs` `//` comment) |
| --- | --- |
| `1000` / `1100` / `1200` | Object (`id.rs:16`) / Array (`:51`) / String (`:88`) |
| `1300`-`1699` | Number, Math, Boolean, Error (`:142`) |
| `1700` / `1800`-`1912` | Promise (`:247`) / Eval + function + console (`:239`) + Generator (`:383`) |
| `2000`-`2199` / `2200` | Map + Set (`:288`) / Iterator (`:325`) |
| `2300`-`2322` | RegExp and `%RegExp.prototype%` (`:340`, `:352`) |
| `2400` / `2500` / `2600` | Global + URI (`:234`, `:229`) / Symbol (`:312`) / JSON (`:224`) |
| `2700` / `2800` / `2900`-`2920` | Proxy (`:391`) / Date (`:398`) / Reflect + Iterator helpers (`:505`) |
| `3500`-`3599` | **Reserved exclusive "surface lane" band** (`:540`) — for parallel lanes, so two lanes cannot collide by construction. Currently 3502-3517. |

There is **no convention** for picking the number beyond "inside your group".

* Take the next unallocated number in your group's band. The Array band ends at
  1136, so 1135 is the current hole. Holes are normal and expected —
  1135, 1435-1439, 2201-2203, 2506-2599, 3000-3501. Inspect the band with
  `grep -oE "= [0-9]{4}," crates/v12-native/src/id.rs | sort -n | uniq`.
* **Collisions between two variants are prevented by rustc, not by convention.**
  A duplicate discriminant is error `E0081` (*discriminant value assigned more
  than once*), a hard error. Two lanes cannot silently collide on a number.
* What is **not** checked is reuse of a *retired* id — 3500/3501 were vacated by
  the well-known-symbol removal (`id.rs:548`) — and nothing pins all 383
  discriminants (the tests in `id.rs` pin 14). Both are your eyeball.

### 3. Wire the name → id mapping

`crates/v12-native/src/methods.rs`, in the `builtin_methods!` block, under the
receiver `Kind` arm (`Array =>` for array methods). The macro generates
`lookup_method`, an O(1) kind-jump-table + name switch.

This is a **fast path, not the only path**, and the asymmetry matters:

* Skip it → `[1].foo` still resolves through the real prototype chain (the
  surface returns `None` and falls through), but loses the cached-function path.
* Include it → the surface **shadows** `Array.prototype`, so a user reassigning
  that name on the array is ignored. That is accepted for `sort` and is exactly
  why `Kind::RegExp` has no surface arm at all.

### 4. Register it in `define_builtins!`

`crates/v12-engine/src/builtins/mod.rs`, in the right target group
(`ArrayProto` for a prototype method, `Array` for a static).

```rust
"foo" (1) => ArrayFoo => |heap, this, args| call_ctx(array::array_foo, heap, this, args),
```

* The `(len)` is the ES `length` property, and it is **duplicated** in
  `builtin_length` (the declared-arity table in the same file). A `debug_assert_eq!`
  in `Ctx::define_method` pins the two, so a mismatch fails in any debug run —
  which is what CI runs. Update both.
* `call_ctx` is the forward adapter: it builds a `Ctx` carrying the primary
  realm global and calls your `&mut Ctx` body. Never write the closure by hand.
* Use `=> callback_stub` instead if the method takes a user callback (step 5).
* The group is `Target { … }`; a bare `Variant => handler` after the `;` emits
  a dispatch arm with no install, for natives not visible from JS.

### 5. If it takes a callback, intercept at the interpreter seam

`callback_stub` in the table is a placeholder that throws; the real body is the
interpreter's. Add the id to `Interp::run_callback_builtin`, then implement the
iteration in `run_array_callback` (or the family helper for your receiver).

**Why a native cannot just re-enter the interpreter:**
`NativeRegistry::call_native` receives only `(&mut Heap, this, args, id)`. It has
no handle on the machine's value stack, function table, or cross-program table,
so it cannot perform a `Call` — a `NativeHandler` is a pure heap function by
construction. The seam therefore *splits* the work: the interpreter performs
every call and hands the engine only pure-heap work, over a **capability
carrier**. `NewPromiseCapability` is the reference example — the registry returns
`[promise, resolve, reject]` and the executor call stays in the interpreter,
which is what makes it synchronous.

### 6. Check the freeze guard

`./scripts/freeze-lint.sh` fails on three grandfathered idioms:

| Tag | Idiom | Use instead |
| --- | --- | --- |
| `PK_PUSH` | `property_keys.push` | `ctx.define_data_prop` / `define_data_prop_with_attrs` |
| `PROP_GET1` | `properties.get(1)` | a shape-keyed read |
| `ERR_VAL` | `error_value("` | `ctx.type_error` / `ctx.range_error` / `ctx.syntax_error` |

Enforcement is **file membership plus a per-file count cap** (`grep -F` over
`crates/`, excluding `tests/` paths and `tests.rs`); the `:line` numbers in
`scripts/freeze-allowlist.txt` are documentary only. **`builtins/array.rs` has
zero entries today**, so any hit there is a hard failure. Adding an allowlist
line to make the guard green is a ratchet regression, not a fix — delete the
entry when you migrate a site.

### 7. Test it, then check test262

* Integration test: `crates/v12-engine/tests/<topic>.rs`, `Engine::new()` +
  `eval` + `to_display_string`. No engine test calls a handler directly today —
  `eval` is the only seam.
* `./conformance/run.sh --filter language` — the default is `--format human`.
* **Never `--format json --json-out F` alone.** With no `tap` and no `human` in
  `--format`, the runner prints JSON to stdout and silently never writes the
  file. Use `--format tap,json --json-out F`.
* Compare **fail sets**, never pass rates:
  `conformance/faildiff.sh BEFORE.json AFTER.json`. A net rise can hide new
  failures; a net zero can hide a one-for-one swap.

## Gates that must be green before you push

| Gate | Command | State on `a965bb6e` |
| --- | --- | --- |
| Tests | `cargo nextest run --workspace --no-fail-fast` | 1 273 passed, 2 skipped — **not re-measured here**; recorded at `CONTEXT.md:84` (measured 2026-10-01, `eaa495b5`) |
| Freeze | `./scripts/freeze-lint.sh` | exit 0 |
| unwrap/expect/panic | `./scripts/unwrap-lint.sh` | exit 0 |
| Architecture | `./scripts/check-architecture.sh` | exit 0 |
| Conformance | `conformance/faildiff.sh conformance/baseline/language.json <after>.json` | enforced in CI |

* **Use `cargo nextest`, never `cargo test`.** Always pass `--no-fail-fast`:
  without it nextest *cancels* the rest of the run on the first failure, and a
  lane once reported "1 failure" when 1 113 of 1 156 tests had never executed.
* The 2 skips are `#[ignore]`d spec-correct assertions in
  `tests/lone_surrogates.rs`, parked until the non-lossy UTF-16 read path lands.
  A third skip means you added one.
* Check-architecture runs clippy **report-only**. `-D warnings` is not a gate;
  do not add it.
* Never regenerate `conformance/baseline/*.json` except via
  `scripts/update-conformance-baseline.sh`, and only with a faildiff in the same
  commit proving **BROKE 0 and REGRESSED 0**. Raising the baseline to turn a red
  gate green defeats the gate.

## House rules

Commit verified work immediately. Never `git stash`, `reset`, or `checkout`.
Never fan out parallel lanes that write the same files. Never run a
workspace-wide `cargo fmt`.

Why: on 2026-09-02 parallel fixer lanes shared one working tree, one ran
`git stash` + `reset` "to preserve WIP", and the uncommitted diff plus its fix-log
backfill were gone. Recovery rebuilt them spec-first over a day of work. The
commit is the only backup that exists.

## Verified against

Re-check these after they move; the numbers above were read at `a965bb6e`.

```
crates/v12-engine/src/builtins/array.rs:3            header (cites a deleted doc)
crates/v12-engine/src/builtins/array.rs:41,68        &mut Heap helper, &mut Ctx body
crates/v12-engine/src/builtins/mod.rs:28             pub use ctx::{BuiltinFn, Ctx, call_ctx, …}
crates/v12-engine/src/builtins/mod.rs:99             builtin_length (declared arity)
crates/v12-engine/src/builtins/mod.rs:1327-1340      ArrayProto group, row shape
crates/v12-engine/src/builtins/mod.rs:1352           callback_stub row
crates/v12-engine/src/builtins/mod.rs:1803           fn callback_stub
crates/v12-engine/src/builtins/ctx.rs:217,222,538    type_error, range_error, define_data_prop
crates/v12-engine/src/builtins/ctx.rs:575            debug_assert length == builtin_length
crates/v12-engine/src/builtins/ctx.rs:1107           pub fn call_ctx
crates/v12-engine/src/builtins/registry.rs:49        NativeHandler = fn(&mut Heap, …)
crates/v12-interp/src/call_setup.rs:1570,1869        dispatch_native → run_callback_builtin
crates/v12-interp/src/call_setup.rs:1966             run_callback_builtin
crates/v12-interp/src/call_setup.rs:2995             run_array_callback
crates/v12-interp/src/property.rs:1076,1086          array_instance_surface, fall-through
crates/v12-interp/src/lib.rs:2058                    method_native
crates/v12-native/src/id.rs:15,52,540                enum, ArrayPush = 1100, 3500 band
crates/v12-native/src/id.rs:16-505                   the `//` group comments that own each band
crates/v12-native/src/id.rs:548                      3500/3501 retired
crates/v12-native/src/methods.rs:28,109,121,205      builtin_methods!, RegExp note, Array arm, macro
crates/v12-native/src/registry.rs:30,95              call_native signature, "needs the machine"
Cargo.toml:28-34                                     workspace lints (unwrap/expect/panic = warn)
conformance/harness/src/main.rs:366-383              --json-out only honoured with tap or human
conformance/faildiff.sh:2                            classifications
conformance/run.sh:4-7                               default filter/format
scripts/freeze-lint.sh:2-37                         the three tags + the matching rule
scripts/freeze-allowlist.txt                         "<TAG> <file>:<line> # reason"
.github/workflows/ci.yml:30,33,37,85                the four gates
CONTEXT.md:81,84,117                                 nextest rule, the count, the incident
```