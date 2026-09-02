# Known failures — last scored 2026-09-02

> Latest verified run: `cargo run -p test262-runner -- --filter language/expressions --jobs 4 --format json`
> Totals: **11 190 tests, 4 047 pass / 6 960 fail / 183 skip, 36.8 % pass** over `language/expressions` (+ annexB).
> Treatment: `pass%` is over executable tests (`pass + fail`). Skips are not counted.
> After Step 8 (Number/Math globals + static registry, `24e838f`) and the harness
> sta.js/assert.js always-prepend fix. The full `language` run still times out in CI;
> the last completed full-language score was 8 919 / 24 446 / 427 skip, 36.5 % (Step 7b).
> Baseline was 19.9 % (4 858) on 2026-08-29. See `fix-log.md` for the burn-down log.

This file is the fix-it queue. Each bullet is a bucket — a single engine gap that, once closed, will flip a visible swath of red to green. Keep the buckets small and ordered by estimated lift.

## How to use

- The harness is the scoreboard. After a fix, re-run the filter for that bucket and paste the before/after into `fix-log.md`.
- Delete the bullet here when the bucket is green on `test/language`.
- Do not add new buckets without a filter that reproduces them: `cargo run -p test262-runner -- --filter <filter> --jobs 8 --verbose | head -n 50`.

## Buckets

### A. `callee is not a function` — 2 138 failures, two root causes identified (2026-09-02)

- **Symptom:** `TypeError: callee is not a function`. Top bucket on `language/expressions`. Two independent causes:

**A1. The `Function` intrinsic is not a global** — `typeof Function === "undefined"`, so Test262's `propertyHelper.js` throws at load on its third line of executable code (`var __join = Function.prototype.call.bind(Array.prototype.join);`). Every test declaring `includes: [propertyHelper.js]` dies before its body runs.
  - **Count:** class/elements 735, object/method-definition 66, class/gen-method-* etc. — roughly 800+ of the callee bucket.
  - **Repro:** `cargo run -p test262-runner -- --filter language/expressions/class/elements/after-same-line-gen-literal-names --jobs 1`; standalone: concatenate `harness/propertyHelper.js` + `verifyProperty({}, "x", {value: 1})` and run through `v12`.
  - **Fix location:** `crates/v12-interp/src/lib.rs` `GLOBAL_INTRINSIC_NAMES` (+ `intrinsic_slot`) and `crates/v12-engine/src/realm.rs` — materialize a `Function` constructor object (kind Function, `prototype` → the existing `function_proto` target, `length`/`name` own properties) and push it as a new intrinsic slot (bump `GLOBAL_VAR_OFFSET`). `Function.prototype.call.bind(...)` then resolves through the existing function-method surface.

**A2. Parameter-default register bug** — inside a function whose parameter list destructures with a default (`([arrow = () => {}]) => …`), a member read on the destructured binding (`arrow.name`) makes **every subsequent call in that body** throw `callee is not a function` (callee register clobbered by the member-read temp). Without the member read, calls resolve fine.
  - **Count:** class/dstr 376, object/dstr 95, async-generator/dstr 84, arrow/function/generators/assignment dstr ~110.
  - **Repro:** `var f = ([a = () => {}]) => { var n = a.x; return assert.sameValue(1, 2); }; f([]);` → throws; delete `var n = a.x;` → throws `Test262Error` (correct).
  - **Fix location:** `crates/v12-bccompiler/src/{expr,collect}.rs` — parameter-default lowering / register allocation for member expressions on destructured bindings.

### B. `Expected a undefined to be thrown but no exception was thrown at all` — 975 failures

- **Symptom:** negative tests (early SyntaxError/TypeError violations) execute successfully instead of throwing. The engine lacks the corresponding early-error validations.
- **Filter:** `cargo run -p test262-runner -- --filter language/expressions --jobs 4 --format json` then group by message; split by sub-suite (class/strict/eval-arguments…) before fixing.

### C. `dynamic import not supported in this context` — 324 failures

- **Symptom:** `import()` desugars to the registered `ModuleImport` native stub, which throws a proper TypeError. Needs the real dynamic-import path: module resolution + job-queue-backed promise.
- **Filter:** `--filter language/expressions/dynamic-import --jobs 4`.
- **Fix location:** `crates/v12-engine/src/builtins/mod.rs` (`ModuleImport` stub) + the ESM loader.

### D. Assertion-detail mismatches (SameValue / boolean) — ~800 failures combined

- **Symptom:** `Expected SameValue(«0», «1») to be true` 264, `Expected true but got false` 143, `Expected SameValue(«undefined», «23»)` 137, `Expected SameValue(«[object Object]», «23»)` 120, etc. Engine semantics gaps, one sub-suite at a time (value coercion, property attributes, Number formatting).
- **Note:** group by test path before fixing; this is a queue of small fixes, not one gap.

### E. Iterator/async semantics — ~300 failures combined

- **Symptom:** `abrupt completion closes iter` 205 (IteratorClose on abrupt completion), plus `yield*`/for-await gaps.
- **Fix location:** `crates/v12-interp/src/lib.rs` `op_iterator_close` + compiler lowering.

### F. Remaining type errors — ~430 failures combined

- **Symptom:** `TypeError: not a function` 174, `cannot set properties of null or undefined` 142, `right-hand side of 'instanceof' is not an object` 114. Mostly downstream of A/B gaps.

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
