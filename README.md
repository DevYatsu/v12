# v12

An experimental JavaScript engine for Rust apps.

v12 compiles JavaScript to a register-based bytecode and executes it on a
garbage-collected runtime implemented from scratch — no bindings to V8,
JavaScriptCore, or QuickJS. The goal is an embeddable engine that is fast to
start, small in memory, and correct enough to run real programs.

> **Status: early development — full pipeline works: compile → run → optimize.**
> The compiler, heap, interpreter, baseline JIT, embedding facade, the `v12`
> CLI, a Test262 harness, and a Tier-2 optimizer scaffold are implemented and
> tested. Generators, async functions, and Promise microtasks are wired end to
> end. Expect breaking changes.
>
> Measured on 2026-10-06 at `86268805` (test count) / `e925647d` (conformance):
> - `cargo nextest run --workspace` → **1 414 tests, 1 414 passed, 3 skipped**.
>   This is the canonical gate; see `CONTEXT.md`. The count drops when a crate's
>   tests go with it — it was 1 295 until the unused `v12-intl` crate (22 tests)
>   was deleted, and 1 387 → 1 379 when the node-differential table was
>   regenerated (17 test functions replaced by 9 plus one `#[ignore]`d, while the
>   assertion surface grew from ~90 rows to 351).
> - Test262 `language` → **77.6 % pass** (19 080 / 24 590; 41 skipped) — the
>   Phase 1 target is ≥60 %, so it is met. The committed baseline is
>   `conformance/baseline/language.json`; CI gates on **fail sets** via
>   `conformance/faildiff.sh`, not on pass rate.
> - Architecture gates, all four enforced in CI: `scripts/freeze-lint.sh`,
>   `scripts/unwrap-lint.sh`, `scripts/check-architecture.sh`, and the fail-set
>   gate above.
>
> These two numbers were stale for weeks (the README claimed 563 tests and
> 32.1 %) precisely because they were hand-maintained. If you change them, pin
> the commit and date as above — and regenerate the baseline only via
> `scripts/update-conformance-baseline.sh`, which requires a `faildiff`
> showing BROKE 0 to justify the raise.
>
> A conformance `net +0` is not evidence that a fix did nothing. Several
> verified, node-checked fixes moved the count by zero because test262 does not
> exercise the layer they were in. See `CONTEXT.md`.

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
ECMA-262 regular expressions, and [lasso][lasso] interns identifier strings.
`BigInt` and `Intl`/Temporal are hand-rolled or not yet implemented — the
engine carries storage for them but no arithmetic or formatting.

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

Run the test suite (1 414 tests, `cargo nextest run --workspace --no-fail-fast`):

```sh
# --no-fail-fast matters: without it nextest CANCELS the rest of the run on the
# first failure, so one failure hides the state of every test after it.
cargo nextest run --workspace --no-fail-fast
```

The architecture gates, all four enforced in CI:

```sh
./scripts/freeze-lint.sh          # no new sites in frozen pre-Ctx idioms
./scripts/unwrap-lint.sh          # no new unwrap/expect/panic in non-test code
./scripts/check-architecture.sh   # crate DAG + clippy budget
./conformance/faildiff.sh conformance/baseline/language.json after.json
```

The differential table that drove the array receiver work is **generated, not
transcribed**:

```sh
./scripts/gen-array-receiver-table.py           # regenerate (needs node)
./scripts/gen-array-receiver-table.py --check   # staleness; exits 1 if drifted
./scripts/table-divergence.py                   # count divergences WITHOUT the ledger
```

`scripts/table-divergence.py` exists because every measurement taken through the
ledger is a measurement the ledger could flatter. See
`docs/adr/0004-arraylike-index-access.md`.

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
- [ ] Test262 `language` conformance burn-down — 32.1 % → ≥60 % (queue in `ROADMAP.md`)
- [ ] Tier-2 driver wiring — second tier-up fire → `JitOpt::compile`, deopt backoff
- [ ] Built-in breadth — `Object.getOwnPropertyNames`, proper error classes/`Error.prototype`, remaining string methods

Performance targets: match or beat V8 on startup time and memory footprint;
interpreter performance in the class of modern production interpreters. Beating
optimizing-tier VMs on hot loops is explicitly out of scope for v1.

## License

v12's own code is dual-licensed under MIT or Apache-2.0, at your option.

Bundled dependencies carry their own licenses, which v12 does not modify —
all of them are permissive (MIT/Apache/BSD/Unicode-3.0/Zlib family; no
copyleft). Applications embedding v12 should still review the dependency
license list (`cargo deny check licenses`, configured in `deny.toml`) before
distributing binaries.

[regress]: https://crates.io/crates/regress
[lasso]: https://crates.io/crates/lasso
