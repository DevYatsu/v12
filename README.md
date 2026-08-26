# v12

An experimental JavaScript engine for Rust apps.

v12 compiles JavaScript to a register-based bytecode and executes it on a
garbage-collected runtime implemented from scratch — no bindings to V8,
JavaScriptCore, or QuickJS. The goal is an embeddable engine that is fast to
start, small in memory, and correct enough to run real programs.

> **Status: early development.** The compiler front end and the memory-safe
> heap are working and tested; the interpreter and JIT tiers are being built
> on top of them. Embedding APIs do not exist yet. Expect breaking changes.

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
- **Cranelift-backed JIT tiers** (in progress) instead of hand-written assembly

Where a best-in-class crate exists, v12 uses it rather than rebuilding it:
[oxc](https://oxc.rs) parses and analyzes all JavaScript, [regress][] implements
ECMA-262 regular expressions, [malachite][malachite] powers `BigInt`,
[ICU4X][icu] and [temporal_rs][temporal] back internationalization and dates,
and [lasso][lasso] interns identifier strings.

## Workspace layout

| Crate | Role |
|---|---|
| `v12-bytecode` | Register ISA (fixed-width 32-bit words), constant pool, exception handler tables, disassembler |
| `v12-heap` | Values, typed handles, hidden classes, elements storage, strings, mark-sweep garbage collector |
| `v12-bccompiler` | oxc AST → bytecode compiler (scopes, closures, exceptions, peephole pass) |
| `v12-interp` | Tier-0 bytecode interpreter *(in development)* |
| `v12-jit-baseline` | Tier-1 template JIT backed by Cranelift *(planned)* |
| `v12-jit-opt` | Tier-2 speculative optimizing JIT *(planned, post-v1)* |
| `v12-regex` | ES-semantics wrapper over `regress` |
| `v12-intl` | `Intl`/Temporal primitives over ICU4X and `temporal_rs` |
| `v12-engine` | Built-ins, realms, job queue, embedding API *(planned)* |
| `v12-cli` | The `v12` binary: REPL and script runner *(planned)* |

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

Run the test suite (231 tests across the five implemented crates):

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
4. **Safe Rust by default.** Every crate carries `#![forbid(unsafe_code)]`;
   the JIT's executable-memory layer will be the single audited exception.

## Roadmap

- [x] Bytecode ISA, constant pool, exception tables, disassembler
- [x] Heap: values, handles, hidden classes, elements kinds, strings, GC
- [x] Compiler front end: statements, closures, exceptions, peephole pass
- [ ] Tier-0 interpreter (in development)
- [ ] Built-ins: Object / Array / String / Promise / Map / Set
- [ ] Tier-1 baseline JIT with inline caches and OSR
- [ ] Test262 conformance harness and benchmark gating
- [ ] Embedding API and the `v12` CLI
- [ ] Tier-2 speculative optimizer

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
