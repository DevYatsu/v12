# v12-api

Convenient embedding facade for the v12 JavaScript engine.

`v12-api` is the **only** thing a host should import. It wraps the internal
`v12-engine` crate (heap, realm, interpreter, job queue) behind a small,
stable surface — one `Context` per engine, one realm, no `Send`/`Sync`.

## Quick start

```rust
use v12_api::Context;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut ctx = Context::new();

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

Run the full worked example with `cargo run -p v12-api --example calculator`.

## Concepts

- **`Context`** — one isolated execution environment: one engine, one realm,
  one heap. Not `Send`; drive it from the thread that created it.
- **`eval<T>(src)`** — evaluates a script and decodes the completion value
  into `T` (`f64`, `i32`, `i64`, `bool`, `String`, `Option<T>`, `Vec<T>`).
  Note: script completion values are currently `undefined` for plain
  expression statements (the engine's completion-value tracking is not yet
  wired for the top-level script).
- **`register_fn(name, closure)`** — installs a Rust closure as a global JS
  function. The closure receives `(&mut Heap, JsValue this, &[JsValue] args)`
  and returns `Result<JsValue, JsValue>`; an `Err` is thrown inside JS.
- **`call<T, A>(name, args)`** — calls a global JS function (defined by a
  prior `eval`) with marshalled arguments, decoding the result into `T`.
- **`pump()`** — drains the microtask queue (Promise reactions,
  `queueMicrotask`). Hosts with their own event loop call this on each tick.

## Error handling

All facade methods return `Result<_, V12Error>`, where:

- `V12Error::Compile(String)` — the front-end rejected the source.
- `V12Error::Thrown(String)` — the script threw; the payload is the
  stringified thrown value.
- `V12Error::Host(String)` — the embedder refused the call (e.g. source too
  large).

## Threading

`Context` and `Runtime` are `!Send + !Sync`, matching the engine's
single-mutator model. Create and drive each context from one thread.

## Current limitations

- Script completion values (ADR-004) are `undefined` for expression
  statements; `eval::<T>` is most useful with `call` or side effects.
- One realm per context (v1 single-realm constraint).
- `ToValue`/`FromValue` cover the primitive + `Option`/`Vec` set.
- Error constructors beyond `Error` are not fully wired as JS constructors.

For advanced use (raw heap access, JIT tier policy, module loading), depend
on `v12-engine` directly.
