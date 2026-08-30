# v12

An experimental JavaScript engine for Rust apps.

v12 compiles JavaScript to a register-based bytecode and executes it on a
garbage-collected runtime implemented from scratch — no bindings to V8,
JavaScriptCore, or QuickJS. The goal is an embeddable engine that is fast to
start, small in memory, and correct enough to run real programs.

> **Status: early development — full pipeline works: compile → run → optimize.**
> The compiler, heap, interpreter, baseline JIT, embedding facade, the `v12`
> CLI, a Test262 harness, and a Tier-2 optimizer scaffold are implemented and
> tested (563 tests, `cargo nextest run --workspace`). Generators, async
> functions, and Promise microtasks are wired end to end. Test262 `language`
> conformance is at **32.1 % pass** (7 561 / 23 580 executable tests, 427
> skipped) — the Phase 1 target is ≥60 %. Expect breaking changes.

## Why another engine?

Existing options for running JS inside a Rust process are bindings to large C++
engines (V8 via `rusty_v8`, JavaScriptCore via `javascriptcore-rs`) or pure-Rust
interpreters that prioritize conformance breadth over execution speed (boa).

v12 takes a third path: a native-Rust engine designed for speed from the first
commit —

- **NaN-boxed values** in a single machine word, with small integers stored inline
- **Hidden classes** (shapes) with transition trees, inline caches, and an
  elements-kind lattice, the machinery that makes property access fast in
  production engines
- **A handle-based heap**: objects live in arenas addressed by 32-bit indices,
  so garbage collection never scans the native stack and never moves data under
  a running mutator
- **Cranelift-backed JIT tiers** — a baseline template tier today, a speculative optimizing tier landing now

Where a best-in-class crate exists, v12 uses it rather than rebuilding it:
[oxc](https://oxc.rs) parses and analyzes all JavaScript, [regress][] implements
ECMA-262 regular expressions, [malachite][malachite] powers `BigInt`,
[ICU4X][icu] and [temporal_rs][temporal] back internationalization and dates,
and [lasso][lasso] interns identifier strings.

## Embedding

`v12-api` is the embedder-facing facade — the only crate a host should import.
It wraps the engine behind a small, stable surface: one `Context` per engine,
one realm, no `Send`/`Sync`.

```rust
use v12_api::Context;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut ctx = Context::new();

    // Rust closure exposed to JS as a global function.
    ctx.register_fn("log", |heap, _this, args| {
        let msg = args
            .iter()
            .filter_map(|v| <String as v12_api::FromValue>::from_value(heap, *v))
            .collect::<Vec<_>>()
            .join(" ");
        println!("[js] {msg}");
        Ok(v12_engine::JsValue::undefined())
    })?;

    ctx.eval::<()>("log('hello from', 1 + 1);")?;

    ctx.eval::<()>("function add(a, b) { return a + b; }")?;
    let sum: f64 = ctx.call("add", &[1.5, 2.5])?;
    assert_eq!(sum, 4.0);
    Ok(())
}
```

See `crates/v12-api/README.md` for the full API surface and the runnable
`calculator` example.

## Workspace layout

| Crate | Role |
|---|---|
| `v12-api` | Embedder facade: `Context` (`eval`/`register_fn`/`call`/`pump`), `Runtime`, `V12Error` |
| `v12-bytecode` | Register ISA (fixed-width 32-bit words), constant pool, `Program`, exception handler tables, disassembler |
| `v12-heap` | Values, typed handles, hidden classes, elements storage, strings, mark-sweep garbage collector |
| `v12-native` | Unified native dispatch: the `NativeId` enum, typed `NativeSig` signatures, and std `From`/`TryFrom` conversions shared by the interpreter and engine |
| `v12-bccompiler` | oxc AST → bytecode compiler (scopes, closures, exceptions, generators/async, peephole pass) |
| `v12-interp` | Tier-0 bytecode interpreter (iterative loop, handler tables, generator suspension, tier-up feedback) |
| `v12-codegen` | Shared JIT seams: compiled-function cache, deopt maps, tier policy |
| `v12-jit-baseline` | Tier-1 template JIT backed by Cranelift (feature-gated, deopt map) |
| `v12-jit-opt` | Tier-2 speculative optimizer — type lattice, guards, SSA, inlining, loop versioning *(driver wiring pending)* |
| `v12-regex` | ES-semantics wrapper over `regress` |
| `v12-intl` | `Intl`/Temporal primitives over ICU4X and `temporal_rs` |
| `v12-engine` | Built-ins (Object/Array/String/Number/Math/Error/Promise/Map/Set/RegExp), single realm, microtask queue, `Engine::eval` API, ESM module compilation |
| `v12-cli` | The `v12` binary: REPL (rustyline — history + arrows) and script runner |

## Try the pieces that exist today

Compile a script down to bytecode and disassemble it:

```rust
use v12_bccompiler::compile_source;

let program = compile_source(r#"
    function counter() {
        let n = 0;
        return () => ++n;
    }
"#)?;

// Every function validates its own invariants: handler-table nesting,
// jump targets, register bounds.
for function in &program.functions {
    println!("{function}"); // human-readable disassembly
}
```

Run a script, including generators, async functions, and promises:

```sh
echo 'function* g(){ yield 1; yield 2; } console.log([...g()]);' | cargo run --bin v12
echo 'Promise.resolve(2).then(v => console.log("got " + v));' | cargo run --bin v12
```

Run the test suite (563 tests, `cargo nextest run --workspace`):

```sh
cargo nextest run --workspace
```

## Design principles

1. **Reuse before rebuild.** A custom component must justify itself against the
   best maintained crate; every hand-rolled piece documents why nothing
   off-the-shelf qualified.
2. **Correctness before speed.** The interpreter must pass Test262 before any
   JIT lands, and every optimization must express its assumptions as a check
   that fails closed.
3. **Narrow interfaces, frozen early.** The bytecode ISA and the value/heap
   layout were specified bit-exactly before dependent work started.
4. **Safe Rust by default.** Engine-core crates opt into `[workspace.lints]`
   with `unsafe_code` denied and `unwrap`/`expect`/`panic` warned; the two
   audited exceptions (the `HostClosure` raw-pointer handle and the JIT's
   executable-memory layer) carry scoped `#[allow]`s.

## Roadmap

- [x] Bytecode ISA, constant pool, exception tables, disassembler
- [x] Heap: values, handles, hidden classes, elements kinds, strings, GC
- [x] Compiler front end: statements, closures, exceptions, peephole pass
- [x] Tier-0 interpreter — handler-table unwinding, GC-rooted frames, differential vs reference interpreter
- [x] Built-ins — Object/Array/String/Number/Math/Error/Promise + single realm, microtask queue, `Engine::eval` API
- [x] Generators & async — `function*`/`yield`, `async`/`await`, `yield*`, Promise reactions through `run_jobs`
- [x] Tier-1 baseline JIT — Cranelift template (feature-gated, deopt pc_map)
- [x] Embedding facade — `v12-api`: `Context` (eval/register_fn/call/pump), `Runtime`, `V12Error`
- [x] `v12` CLI — script runner + REPL (rustyline: history + arrows)
- [x] Test262 harness — parallel runner, TAP/JSON/human output, per-suite gating (`conformance/run.sh`)
- [x] Tier-2 speculative optimizer — type lattice, guards, SSA, inlining, loop versioning
- [x] ESM modules — `compile_source_as_module`, import/export linkage, `Engine` module compilation
- [x] Unified native dispatch — `NativeId` enum, typed `NativeSig`, structural built-in method lookup (O(1))
- [x] Built-in breadth — Map/Set, RegExp runtime, `for-of`/iterator protocol, error objects
- [ ] Test262 `language` conformance burn-down — 32.1 % → ≥60 % (queue in `conformance/known-failures.md`)
- [ ] Tier-2 driver wiring — second tier-up fire → `JitOpt::compile`, deopt backoff
- [ ] Built-in breadth — `Object.getOwnPropertyNames`, proper error classes/`Error.prototype`, remaining string methods

Performance targets: match or beat V8 on startup time and memory footprint;
interpreter performance in the class of modern production interpreters. Beating
optimizing-tier VMs on hot loops is explicitly out of scope for v1.

## License

v12's own code is dual-licensed under MIT or Apache-2.0, at your option.

Bundled dependencies carry their own licenses, which v12 does not modify:
most are MIT/Apache; the `BigInt` implementation ([malachite]) is
LGPL-3.0-only — applications embedding v12 should review the dependency
license list (`cargo deny check licenses`) before distributing binaries.

[regress]: https://crates.io/crates/regress
[malachite]: https://crates.io/crates/malachite
[icu]: https://crates.io/crates/icu
[temporal]: https://crates.io/crates/temporal_rs
[lasso]: https://crates.io/crates/lasso
