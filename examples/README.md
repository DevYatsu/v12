# v12 examples

Runnable JavaScript programs covering the language subset v12 executes today (Tier 1).

Run any file:

```sh
cargo run -p v12-cli -- examples/01-basics.js
./target/debug/v12 examples/01-basics.js        # after `cargo build -p v12-cli`
./target/debug/v12 --disasm examples/01-basics.js  # bytecode listing instead of running
```

REPL (arrow keys + Up for history):

```sh
cargo run -p v12-cli
```

Notes:
- `let`/`const`/`var` work; `null` literal is not supported yet — use `undefined`.
- `new` is not supported yet — construct objects/arrays via literals.
- Each REPL line is an independent script (bindings don't persist across lines).

Files:

| File | Covers |
|---|---|
| `01-basics.js` | literals, arithmetic, comparisons, typeof, coercion |
| `02-variables.js` | block scoping, shadowing, const |
| `03-functions.js` | declarations, expressions, arrows, recursion |
| `04-control-flow.js` | if/else, while/do-while/for, break/continue, labels |
| `05-objects.js` | literals, get/set/delete, prototype, Object.* |
| `06-arrays.js` | literals, push/pop, length, holey reads |
| `07-strings.js` | concat, String methods, interning |
| `08-exceptions.js` | throw, try/catch/finally, nested handlers |
| `09-closures.js` | counter factory, shared env, arrow capture |
| `10-builtins.js` | Object/Array/String/Number/Math/Error |
