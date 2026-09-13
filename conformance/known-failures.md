# Known failures — last scored 2026-09-14

> Latest verified run: `./conformance/run.sh --filter language --jobs 8 --format json`
> Totals: **24 590 tests, 13 348 pass / 11 208 fail / 34 skip, 54.4 % pass** over `language` (+ annexB, intl402) — first full-`language` run to complete with zero timeouts/stalls since the CI-timeout note below.
> Treatment: `pass%` is over executable tests (`pass + fail`). Skips are not counted.
> After Step 8 (Number/Math globals + static registry, `24e838f`), the harness
> sta.js/assert.js always-prepend fix, the P0 commit (`1b0f73b`, incl. the
> with-statement `CompileError` that honestly costs ~108 false passes on this
> slice), and the P1 destructuring-default fix (2026-09-05). The full `language`
> run no longer times out (shape-lookup fix + TCO skip, 2026-09-14 — see `fix-log.md`);
> the last completed full-language score before that was 8 919 / 24 446 / 427 skip,
> 36.5 % (Step 7b). Baseline was 19.9 % (4 858) on 2026-08-29. See `fix-log.md` for the burn-down log.

This file is the fix-it queue. Each bullet is a bucket — a single engine gap that, once closed, will flip a visible swath of red to green. Keep the buckets small and ordered by estimated lift.

## How to use

- The harness is the scoreboard. After a fix, re-run the filter for that bucket and paste the before/after into `fix-log.md`.
- Delete the bullet here when the bucket is green on `test/language`.
- Do not add new buckets without a filter that reproduces them: `cargo run -p test262-runner -- --filter <filter> --jobs 8 --verbose | head -n 50`.

## Buckets

### A. `callee is not a function` — 2 138 failures, two root causes identified (2026-09-02)

- **Symptom:** `TypeError: callee is not a function`. Top bucket on `language/expressions`. Two independent causes:

**A1. ~~The `Function` intrinsic is not a global~~ — closed 2026-09-12 (`b18c160`, `76f6436`).** The original premise was stale: `Function` already worked as a global (`typeof Function === "function"`, `Function("a","return a+1")(1) === 2`). The real bug was that `Function.prototype` was allocated `Kind::Ordinary` and never linked to the `Function` ctor, so `function_method_surface` (gated on `Kind::Function`) never fired, `Function.prototype.call` read `undefined`, and Test262's `propertyHelper.js` died at line 31 (`Function.prototype.call.bind(Array.prototype.join)`). Fixed by making `function_proto` a `Kind::Function` object, linking the ctor via `install_ctor`, and implementing real `Function.prototype.bind` (`FunctionTarget::Bound` + state object + dispatch). On `class/elements`: `callee is not a function` 718 → 90; pass count flat (413) because the now-loading tests fail later on descriptor-attribute checks (`m descriptor should not be enumerable; m descriptor should be configurable` ×560) — a separate property-descriptor gap, not A1.
  - **Count:** class/elements 735 → 90 remaining (async-gen `yield*`/private contexts), object/method-definition etc. unblocked at load.
  - **Repro:** `verifyProperty({}, "x", { value: 1, writable: true, enumerable: true, configurable: true })` now runs the `<m descriptor should…>` assertion instead of throwing at load.
  - **Fix location:** `crates/v12-engine/src/realm.rs`, `crates/v12-engine/src/builtins/{ctx,mod}.rs`, `crates/v12-heap/src/{function,object}.rs`, `crates/v12-interp/src/call_setup.rs`.

**A2. Parameter-default register bug** — inside a function whose parameter list destructures with a default (`([arrow = () => {}]) => …`), a member read on the destructured binding (`arrow.name`) makes **every subsequent call in that body** throw `callee is not a function` (callee register clobbered by the member-read temp). Without the member read, calls resolve fine.
  - **Count:** class/dstr 376, object/dstr 95, async-generator/dstr 84, arrow/function/generators/assignment dstr ~110.
  - **Repro:** `var f = ([a = () => {}]) => { var n = a.x; return assert.sameValue(1, 2); }; f([]);` → throws; delete `var n = a.x;` → throws `Test262Error` (correct).
  - **Fix location:** `crates/v12-bccompiler/src/{expr,collect}.rs` — parameter-default lowering / register allocation for member expressions on destructured bindings.

### B. `Expected a undefined to be thrown but no exception was thrown at all` — 975 failures

- **Symptom:** negative tests (early SyntaxError/TypeError violations) execute successfully instead of throwing. The engine lacks the corresponding early-error validations.
- **Filter:** `cargo run -p test262-runner -- --filter language/expressions --jobs 4 --format json` then group by message; split by sub-suite (class/strict/eval-arguments…) before fixing.

### C. ~~`dynamic import not supported in this context` — 324 failures~~ — **closed** (f06e71c loader: 68.1 % on dynamic-import)

- **Closed 2026-09-13:** real module loader (resolve/link/evaluate + job-backed `import()` promise) landed; see `fix-log.md` Step C. Remaining dynamic-import failures are `Array.prototype`-write/`import.meta`/missing-intrinsic gaps, not loader gaps.

### D. Assertion-detail mismatches (SameValue / boolean) — ~800 failures combined

- **Symptom:** `Expected SameValue(«0», «1») to be true` 264, `Expected true but got false` 143, `Expected SameValue(«undefined», «23»)` 137, `Expected SameValue(«[object Object]», «23»)` 120, etc. Engine semantics gaps, one sub-suite at a time (value coercion, property attributes, Number formatting).
- **Note:** group by test path before fixing; this is a queue of small fixes, not one gap.

### E. Iterator/async semantics — ~300 failures combined

- **Symptom:** `abrupt completion closes iter` 205 (IteratorClose on abrupt completion), plus `yield*`/for-await gaps.
- **Fix location:** `crates/v12-interp/src/lib.rs` `op_iterator_close` + compiler lowering.

### F. Remaining type errors — ~430 failures combined

- **Symptom:** `TypeError: not a function` 174, `cannot set properties of null or undefined` 142, `right-hand side of 'instanceof' is not an object` 114. Mostly downstream of A/B gaps.

### G. Top-level await in module bodies — `language/module-code/top-level-await/*`

- **Symptom:** `await` at module top level throws `SyntaxError: await outside async` — module mains compile with `is_async=false` (`v12-bccompiler/src/unit.rs` `UnitNode::Main`), so TLA never reaches the async machinery. Import promises of TLA modules reject; `async test did not complete`.
- **Fix location:** compile module mains as async (generator-backed) when the body contains await; `module_loader.rs::load_and_evaluate` must settle the module evaluation promise instead of taking the main's synchronous completion as the namespace. (The *crash* this used to cause — the live-frame leak + stack corruption — is fixed; see fix-log Step F.)
- **Filter:** `--filter language/module-code/top-level-await --jobs 1`.

## Done (moved out of the queue)

### ~~`unsupported expression` (12 625)~~ — closed as a bucket by Steps 1–8 (2026-09-02)

- The compiler-coverage mega-bucket was burned down through the Step 1–8 passes: collector walks (Step 2), Array/Object/Function builtins (Step 3a/b), for-of destructuring (Step 4), BigInt/tagged templates/`with` (Step 5), instanceof (Step 6), private fields (Step 7b/c), Number/Math globals (Step 8). Residual per-feature gaps live in buckets A–F above.

### D5. ~~Loose equality number↔string coercion~~ — **closed** (found by the differential suite, e4902d4)

- `loose_equals` (crates/v12-interp/src/ops.rs) was missing the number↔string arm of ES 7.2.14 (`1 == '1'` → `false`). Fixed: number↔string compares `ToNumber(string)` with the number, and boolean operands are coerced via ToNumber then re-dispatched (the old bool arm also panicked on bool↔number). Pinned differential test `known_gap_loose_equals_number_string_coercion` un-ignored and green.

### D1. ~~`in` / `instanceof` opcodes~~ — **closed** (262aed8 → verified f47ec78)

- Zero opcode/unbound errors remain across all 14 972 fails. Assignment slice gained ~+10 net via this and follow-ups. Gate predicted 65 % on the slice; actual plateau is 50.4 % because buckets A/B cap it.

### D2. ~~`collect.rs` overflow panic~~ — **closed** (262aed8 → successor bucket B)

- Zero `engine panic` results in the full run (was ~200+ distinct panics). Overflow path returns a clean compile error so negative tests can still pass.

### D3. ~~Global object & property model~~ — **closed** (aaa339b)

- `GetGlobal`/`SetGlobal`; no more "unbound variable" errors anywhere in the failure stream. Literals sit at 59.7 % with remaining losses owned by buckets A/B.

### D4. ~~Module / ESM skips~~ — **closed** (e534394 loader → 0466cb5 buckets)

- Module skips 721 → 0. `module-code` runs end-to-end: 755 total, 309 pass / 410 fail / 36 skip (43 %). Residual `export/import statements only valid in modules` ×214 is script-mode negative handling, tracked under bucket A.

## Re-run commands

```sh
# Full language gate (Phase 1 ≥60 %)
./conformance/run.sh --filter language --jobs 8

# Quick health check on the biggest current bucket
./conformance/run.sh --filter language/expressions/assignment --jobs 4 --verbose | head -n 100

# TAP + JSON artifacts for nightly
cargo run -p test262-runner -- --filter language --format tap,json --json-out /tmp/t262.json --tap-out /tmp/t262.tap
```

## Exit criteria

- Remove this file's buckets one by one into `fix-log.md`.
- Phase 1 gate: ≥60 % on `test/language`, zero `engine panic` cases, jit vs no-jit JSON diff is zero when the `jit` feature is on.
- Phase 2 gate: ≥85 % overall (`--filter` none) and Tier 1 default-on.
