# ROADMAP — v12 future work (single queue)

The only forward-looking doc. `CONTEXT.md` holds the ubiquitous language,
`conformance/fix-log.md` holds the done-history. Everything else planning-related
was deleted; history lives in git.

> Scoreboard (last verified 2026-09-18 on master `0642abb`): nextest gate
> **640 passed / 0 skipped**; clippy 0 errors; fmt clean.
> **Phase-1 gate MET**: full `language` **17 123 pass / 7 426 fail / 41 skip =
> 69.8 %** with **0 engine panics** (`./run.sh --filter language --jobs 1`;
> panic-hiding `catch_unwind` means the proof is the absence of a `panicked at`
> line — verified 0 on stderr+stdout). The long-standing `execute.rs:588` panic
> is closed by `lane/interp-panic` (`6049d06`).
> Landed 2026-09-18 (all merged + independently re-measured on master):
> `built-ins/Reflect` **0 / 154 → 109 / 154 = 71.2 %** (`0f290e8`),
> `language/module-code` **416 → 431 / 599 = 72.0 %** (`016f60e`),
> `built-ins/String` **658 → 681 / 1 341 = 50.8 %** (`b50c04c`),
> object literals now inherit `%Object.prototype%` (`af93856`:
> `language/expressions` +9, `language/statements` +1, `built-ins/Object` +6),
> `language/statements/class` **2 430 → 2 766 / 4 369 = 63.3 %** and
> `language/expressions` **7 194 → 7 707 / 11 102 = 69.5 %** (`0642abb`,
> `lane/class-fields`: instance fields as own properties + `NamedEvaluation`;
> +336 / +513 with 0 regressions).
> Verified slices: `language/expressions` 7 707 / 11 128 = 69.5 %,
> `language/statements/class` 2 766 / 4 369 = 63.3 %,
> `built-ins/Date` 471 / 618 = 76.2 %, `built-ins/JSON` 81 / 165 = 49.1 %.
> Several entries below note doc-vs-reality drift; re-verify the symptom before
> fixing.

## 1. Conformance burn-down (Phase 1 gate: `language` ≥ 60 %, zero engine panics)

Work one bucket at a time. After each fix: re-run the bucket filter, paste
before/after into `conformance/fix-log.md`, check the box here when green on
`test/language`. Re-verify the symptom first — `fix-log.md` claims several
closures that `known-failures.md` (deleted 2026-09-17, content merged here)
still listed as open.

- [x] **A2 — parameter-default register bug.** Verified **CLOSED** on master
  `2011965` (repro below returns `1`); closed by the nested-destructuring-defaults
  work in `1e662a4`. Entry was stale drift.
  Repro: `var f = ([a = () => {}]) => { var n = a.x; return 1; }; f([]);`
  The `class/dstr` (672) and `language/statements/class/elements` (914) mass is
  therefore NOT this bug — see the active root-cause investigation instead.
- [ ] **B — missing early-error validations (~975).** Negative tests execute
  instead of throwing. Group by message × sub-suite before fixing
  (class/strict/eval-arguments…). Filter: `--filter language/expressions`.
  Partially landed: `lane/early-errors` (`636aa56`, merged `6fe228e`) added the
  import() options walk + a first validation batch; re-measure the residue.
- [ ] **D — assertion-detail mismatches (~800).** `SameValue` / boolean gaps:
  value coercion, property attributes, Number formatting. Queue of small fixes,
  one sub-suite at a time — not one gap.
- [ ] **E — iterator/async semantics (~300).** `abrupt completion closes iter`
  (205, `IteratorClose` on abrupt completion) plus `yield*` / for-await gaps.
  Location: `op_iterator_close` in `crates/v12-interp/src/lib.rs` + compiler
  lowering. Partially landed via `lane/e-iterator-close`.
- [ ] **F — remaining type errors (~430).** `not a function` (174), `cannot set
  properties of null or undefined` (142), bad `instanceof` RHS (114). Mostly
  downstream of A/B — sweep last.
- [ ] **G — top-level await in module bodies.** Module mains compile with
  `is_async=false` (`UnitNode::Main`), so TLA throws `await outside async`.
  Fix: compile module mains as async when the body contains await; settle the
  evaluation promise in `module_loader.rs::load_and_evaluate`.
  Filter: `--filter language/module-code/top-level-await --jobs 1`.
- [ ] **Promise surface.** `Promise.all` / `race` / `finally`, async-generator
  `.next()` promise semantics, timers, async iteration. (Constructor, `.catch`,
  adoption chains, and the async-verdict runner path already landed.)
- [ ] **Module export values.** Loader (resolve/link/evaluate + job-backed
  `import()`) landed; if namespace objects still come back empty, the module-env
  capture path in `run_compiled` is the gap.

## 2. Builtins architecture (`Ctx` migration)

Target: one `Ctx` context per builtin (`heap`, realm/intrinsics, call
capability, roots, errors, conversions, install family); `BuiltinFn =
fn(&mut Ctx, This, Args) -> BuiltinResult`. Guards: Law of Demeter, Single
Responsibility. No trait-object dispatch, no `GLOBAL_INTRINSICS` slot-contract
rewrite, no wrapper objects for primitives in this pass.

1. Freeze: no new `property_keys` pushes, `*_surface` arms, `wire_*` helpers,
   or stringly `error_value` call sites.
2. `Ctx` + adapter shim (legacy `NativeHandler` bodies compile unchanged).
   Gate: nextest green.
3. Migrate file-by-file: `helpers` conversions → `math` → `number` →
   `boolean` → `object` → non-callback `array` → `string` → `json` → `map` →
   `iterator` → `regexp` → `promise` → `global` → `error` → `symbol`.
   Gate per file: nextest + matching `built-ins/X` slice.
4. Collapse install paths: `length` in `define_builtins!`, all installs through
   `define_method` + `install_ctor`; move rest-array identity + `arguments`
   creation to construction time.
5. Collapse call helpers: five dispatch copies into one `dispatch_native`.
6. Enforcement: hard-error lints + `debug_assert` coverage for `length`/`name`/
   `prototype` on every installed builtin.
- Residuals: callback-path installs still report `undefined` for `length`
  (`[].map.length`, `Object.keys.length`); thrown-error `constructor` identity
  needs dispatch to supply a global to `Ctx`.

## 3. Engine milestones

- [ ] Tier-2 driver wiring: second tier-up fire → `JitOpt::compile`, deopt
  backoff. (Tier-1 stays behind `V12_JIT`, unset = pure interpreter.)
- [ ] Built-in breadth: `Object.getOwnPropertyNames`, proper error
  classes / `Error.prototype` remainder (`EvalError`/`URIError` globals,
  `Error.cause`, `Error.isError`), remaining string methods.
- [ ] `Proxy` remainder: traps beyond `has` + `Reflect`. `built-ins/Proxy`
  measured **94 / 311** on 2026-09-17 (doc previously said 71/311 — drift).
  `Reflect` itself is **absent** (`built-ins/Reflect` 0/154) and is being added
  by a lane; the two overlap, so land `Reflect` first.
- [ ] Full holed-descriptor reader sweep; array indexed elements in
  own-property queries; derived-class field ordering after `super()`;
  static blocks; accessor `defineProperty` (`get`/`set`); computed/symbol
  method `name`; parameter TDZ.

## 4. Explicit non-goals (honest gaps, do not re-open silently)

- Identifier **writes** inside a `with` body resolve statically
  (`language/statements/with` 74 pass / 107 fail) — documented YAGNI boundary.
- `language/destructuring` resizable-arraybuffer test stays skipped (out of v1
  scope: no `SharedArrayBuffer`/`Atomics`, single-threaded mutator).
- Single realm, Annex B included, Tier 2 post-v1 (see `CONTEXT.md`).

## 5. Gates & workflow

- `cargo nextest run --workspace` green after every step.
- Conformance reads use default `--format human`; machine-comparable ok-lists
  go to a file (`--tap-out`), never stdout.
- `cargo clippy --workspace --all-targets`: 0 errors (unwrap/expect/panic
  policy set accepted). `cargo fmt --check` clean.
- No lane runs `git stash` / `reset` / `checkout` or workspace-wide `cargo fmt`;
  commit verified work immediately; never fan out parallel write lanes on the
  same files.
- Re-run commands:
  `./conformance/run.sh --filter language --jobs 8` (Phase 1 gate),
  `./conformance/run.sh --filter language/expressions/assignment --jobs 4` (quick health check).
