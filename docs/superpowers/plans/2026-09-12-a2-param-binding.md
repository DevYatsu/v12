# A2 Param-Binding Prologue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Evaluate formal-parameter defaults and destructure parameter patterns in the function prologue, with a register layout that keeps the incoming-argument window (`r1..r{arity}`, rest at `r{arity+1}`) disjoint from pattern-inner locals.

**Architecture:** `collect` already walks every parameter binding leaf into `decl_order` (so capture analysis and ref-collection are correct), but `finalize` assigns registers in `decl_order` order starting at `r1`, which collides pattern leaves with incoming argument registers, and `compile_unit` emits only the body. The fix splits *formal arity* from *declared symbol count*: reserve the incoming window unconditionally in `finalize`, and emit a prologue that runs the existing `lower_default`/`lower_binding_pattern` lowerers against each incoming register.

**Tech Stack:** Rust, oxc AST, `v12-bccompiler` (collect/unit/stmt/model), `v12-bytecode`, `v12-interp` (unchanged ABI).

**Spec:** `docs/superpowers/specs/2026-09-12-known-failures-design.md` §2 (A2). Oracle design review: lane `ora-1` / `ses_f698ef4ceffe1klPaM1T3CYWcf`.

## Global Constraints

- Never run `git stash`/`reset`/`checkout` or workspace-wide `cargo fmt` (2026-09-02 incident rule). One sequential writer lane only.
- Tests are run with `cargo nextest run --workspace` (NOT `cargo test`).
- Scratch JS scripts use `console.log` (there is no `print` global in `v12-cli`).
- Accepted simplification: no parameter TDZ. `(a = b, b = 2)` reads `b`'s incoming register instead of throwing; document it, do not implement TDZ.
- The interpreter call ABI is frozen: `window[1..]` = formals, rest tail at `rest_reg` (`crates/v12-interp/src/call.rs`). No change there.

## File Structure

- `crates/v12-bccompiler/src/model.rs` — `UnitPlan` layout fields (`arity`, `formal_idents`, `rest_ident` replace `param_count`); `UnitPlan::new`.
- `crates/v12-bccompiler/src/collect.rs` — new `Collector::register_formals`; four call sites; `finalize` register/slot assignment.
- `crates/v12-bccompiler/src/stmt.rs` — `lower_binding_pattern` visibility.
- `crates/v12-bccompiler/src/unit.rs` — `emit_prologue` signature + lowering body; `compile_unit` param derivation; `finish()` field mapping.
- `crates/v12-bccompiler/src/tests.rs` — new bytecode-level tests.
- `conformance/fix-log.md` — score record (Task 2).

---

### Task 1: Prologue parameter lowering + incoming-window layout

**Files:**
- Modify: `crates/v12-bccompiler/src/model.rs:200-256`
- Modify: `crates/v12-bccompiler/src/collect.rs:182-194,231-240,458-467,510-519,1169-1211`
- Modify: `crates/v12-bccompiler/src/stmt.rs:388`
- Modify: `crates/v12-bccompiler/src/unit.rs:118,206-284`
- Test: `crates/v12-bccompiler/src/tests.rs` (append near line 1614)

**Interfaces:**
- Consumes: `Collector::binding_pattern(&mut self, p: &BindingPattern<'_>)` (`collect.rs:819`, already exists); `FnCtx::lower_binding_pattern(&mut self, pat: &BindingPattern<'_>, src: u16) -> Res<()>` (`stmt.rs:388`, made `pub(crate)`); `FnCtx::emit_set_env(&mut self, depth: u8, slot: u16, src: u16, span: Span)` (`model.rs:825`); `FnCtx::access(sym) -> VarAccess` (`model.rs:550`).
- Produces: `UnitPlan { pub arity: usize, pub formal_idents: Vec<Option<SymbolId>>, pub rest_ident: Option<SymbolId> }`; `Collector::register_formals(&mut self, idx: usize, params: &FormalParameters<'_>)`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/v12-bccompiler/src/tests.rs`:

```rust
// ---------------------------------------------------------------------------
// Bucket 7 — Parameter defaults & destructuring in the prologue
// ---------------------------------------------------------------------------

/// The bytecode text of the first non-main function in `src`.
fn fn0_text(src: &str) -> String {
    let (prog, _) = compile_source_with_strings(src).expect("compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
    format!("{}", prog.functions[1])
}

#[test]
fn param_default_lowers_in_prologue() {
    // `function k(a = 1){ return a; }` must test `a` against undefined
    // in the prologue and select the default when it is.
    let text = fn0_text("function k(a = 1){ return a; }");
    assert!(text.contains("strict_eq"), "expected default test in:\n{text}");
    assert!(text.contains("jump_if_false"), "expected default branch in:\n{text}");
}

#[test]
fn param_pattern_destructures_in_prologue() {
    // `function d([a]){ return a; }` must read element 0 off the incoming
    // array register (GetProperty), not alias it.
    let text = fn0_text("function d([a]){ return a; }");
    assert!(text.contains("get_property"), "expected pattern read in:\n{text}");
}

#[test]
fn simple_params_layout_is_unchanged() {
    // The all-simple-identifier fast path must not grow the prologue.
    let (prog, _) = compile_source_with_strings("function f(a, b){ return a + b; }").expect("compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
    let text = format!("{}", prog.functions[1]);
    assert!(!text.contains("strict_eq"), "no default test expected in:\n{text}");
    assert_eq!(prog.functions[1].fixed_params, 2);
    assert_eq!(prog.functions[1].rest_reg, 0);
}

#[test]
fn pattern_formal_then_rest_register_abi() {
    // Formal 0 is the pattern (incoming r1, reserved as scratch); the rest
    // array therefore lands at r2, NOT r3.
    let (prog, _) = compile_source_with_strings("function f([a], ...r){ return r.length; }")
        .expect("compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
    assert_eq!(prog.functions[1].fixed_params, 1);
    assert!(prog.functions[1].has_rest);
    assert_eq!(prog.functions[1].rest_reg, 2);
}

#[test]
fn dflt_params_length_stops_at_first_default() {
    let (prog, _) = compile_source_with_strings("function f(a, b = 1, c){}").expect("compile");
    assert_eq!(prog.functions[1].expected_args, 1);
}

#[test]
fn generator_with_default_compiles() {
    let (prog, _) = compile_source_with_strings("function* g(a = 1){ yield a; }").expect("compile");
    for f in &prog.functions {
        f.validate().expect("validate");
    }
    assert!(format!("{}", prog.functions[1]).contains("strict_eq"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p v12-bccompiler param_default_lowers_in_prologue param_pattern_destructures_in_prologue pattern_formal_then_rest_register_abi`
Expected: FAIL — `strict_eq`/`get_property` absent from the prologue; `rest_reg` is 3 for the pattern-rest case.

- [ ] **Step 3: Replace the `UnitPlan` layout fields**

In `crates/v12-bccompiler/src/model.rs`, replace the `param_count` field (lines 211-212) with:

```rust
    /// Number of top-level formal parameters (excluding a rest parameter).
    /// `r{i+1}` receives formal `i` by the call ABI (`r0` is `this`).
    pub arity: usize,
    /// For each formal (`len() == arity`): the top-level binding identifier,
    /// or `None` when the formal is a destructuring pattern whose incoming
    /// register is reserved as scratch for the prologue destructure.
    pub formal_idents: Vec<Option<SymbolId>>,
    /// The rest parameter's top-level binding identifier when it is a simple
    /// identifier (`...a`). `None` for a pattern rest or no rest.
    pub rest_ident: Option<SymbolId>,
```

In `UnitPlan::new` (line ~247) replace `param_count: 0,` with:

```rust
            arity: 0,
            formal_idents: Vec::new(),
            rest_ident: None,
```

- [ ] **Step 4: Add `Collector::register_formals`**

In `crates/v12-bccompiler/src/collect.rs`, add `FormalParameters` to the `oxc_ast::ast` import list (line 23-27), then add this method immediately after `binding_pattern` (ends line 847):

```rust
    /// Registers one function's formal parameters: walks every binding leaf
    /// (so captures and default-RHS references are collected) and records the
    /// top-level arity separately from the leaf count.
    fn register_formals(&mut self, idx: usize, params: &FormalParameters<'_>) {
        for p in &params.items {
            self.binding_pattern(&p.pattern);
        }
        if let Some(rest) = &params.rest {
            self.binding_pattern(&rest.rest.argument);
        }
        let formal_idents = params
            .items
            .iter()
            .map(|p| binding_symbol(&p.pattern))
            .collect();
        let rest_ident = params
            .rest
            .as_ref()
            .and_then(|r| binding_symbol(&r.rest.argument));
        self.plans.units[idx].arity = params.items.len();
        self.plans.units[idx].formal_idents = formal_idents;
        self.plans.units[idx].rest_ident = rest_ident;
        self.plans.units[idx].has_rest = params.rest.is_some();
        self.plans.units[idx].expected_args = expected_args(&params.items);
    }
```

- [ ] **Step 5: Replace the four hand-rolled param blocks**

`collect.rs:182-194` (function unit) becomes:

```rust
        // Params register first; they occupy the incoming-argument window.
        self.register_formals(idx, &f.params);
```

`collect.rs:231-240` (arrow unit) becomes:

```rust
        self.register_formals(idx, &a.params);
```

`collect.rs:455-471` (class constructor) becomes:

```rust
        if let Some(m) = ctor_el {
            // The explicit constructor is a Function; register its params and
            // walk its body, and note references inside it.
            self.register_formals(idx, &m.value.params);
            if let Some(body) = m.value.body.as_deref() {
                self.stmt_list(&body.statements);
            }
        }
```

`collect.rs:510-519` (class method) becomes:

```rust
                self.register_formals(midx, &m.value.params);
```

- [ ] **Step 6: Reserve the incoming window in `finalize`**

In `crates/v12-bccompiler/src/collect.rs`, replace the per-unit body loop `for i in 0..decl_count { ... }` (lines 1174-1201) with the following. Keep the `slot`/`reg` declarations above it and the `this_slot`/`env_slot_count`/`locals_end`/bounds-check block below it unchanged.

```rust
        let (arity, has_rest, rest_ident) = {
            let u = &plans.units[ui];
            (u.arity, u.has_rest, u.rest_ident)
        };
        // 1. Reserve the incoming formal window unconditionally: `r{i+1}`
        //    carries formal `i` from the call ABI. A simple-identifier formal
        //    takes that register (or an env slot when captured — the register
        //    is still consumed because the ABI writes it). A pattern formal
        //    leaves it as scratch for the prologue destructure.
        for i in 0..arity {
            let this_reg = reg;
            reg = reg.checked_add(1).ok_or_else(|| CompileError {
                message: "too many functions/constants".into(),
                span: Some((0, 0)),
            })?;
            let Some(sym) = plans.units[ui].formal_idents.get(i).copied().flatten() else {
                continue;
            };
            if !homes.get(&sym).is_some_and(|h| *h == ui) {
                continue;
            }
            if plans.captured.contains(&sym) {
                plans.units[ui].env_slots.insert(sym, slot);
                plans.units[ui].vars.insert(sym, VarLoc::Env(slot));
                slot = slot.checked_add(1).ok_or_else(|| CompileError {
                    message: "too many functions/constants".into(),
                    span: Some((0, 0)),
                })?;
            } else {
                plans.units[ui].vars.insert(sym, VarLoc::Reg(this_reg));
            }
        }
        // 2. Reserve the rest register at `r{arity+1}` (the ABI tail).
        if has_rest {
            let rest_reg = reg;
            reg = reg.checked_add(1).ok_or_else(|| CompileError {
                message: "too many functions/constants".into(),
                span: Some((0, 0)),
            })?;
            if let Some(sym) = rest_ident
                && homes.get(&sym).is_some_and(|h| *h == ui)
            {
                if plans.captured.contains(&sym) {
                    plans.units[ui].env_slots.insert(sym, slot);
                    plans.units[ui].vars.insert(sym, VarLoc::Env(slot));
                    slot = slot.checked_add(1).ok_or_else(|| CompileError {
                        message: "too many functions/constants".into(),
                        span: Some((0, 0)),
                    })?;
                } else {
                    plans.units[ui].vars.insert(sym, VarLoc::Reg(rest_reg));
                }
            }
        }
        // 3. Everything else (pattern leaves, body declarations, named
        //    function-expression self-bindings) above the reserved window.
        let decl_count = plans.units[ui].decl_order.len();
        for i in 0..decl_count {
            let sym = plans.units[ui].decl_order[i];
            // Formals/rest already placed above.
            if plans.units[ui].vars.contains_key(&sym) {
                continue;
            }
            let is_home = homes.get(&sym).is_some_and(|h| *h == ui);
            if !is_home {
                continue;
            }
            // Top-level `var`/`function` bindings alias the global object
            // (scripts only; modules keep their own scope).
            if ui == 0 && !plans.is_module && plans.global_vars.contains(&sym) {
                plans.units[ui].vars.insert(sym, VarLoc::Global);
                continue;
            }
            if plans.captured.contains(&sym) {
                plans.units[ui].env_slots.insert(sym, slot);
                plans.units[ui].vars.insert(sym, VarLoc::Env(slot));
                slot = slot.checked_add(1).ok_or_else(|| CompileError {
                    message: "too many functions/constants".into(),
                    span: Some((0, 0)),
                })?;
            } else {
                plans.units[ui].vars.insert(sym, VarLoc::Reg(reg));
                reg = reg.checked_add(1).ok_or_else(|| CompileError {
                    message: "too many functions/constants".into(),
                    span: Some((0, 0)),
                })?;
            }
        }
```

Delete the now-unused `let decl_count = plans.units[ui].decl_order.len();` that sat at old line 1174 (it is re-declared in step 3 above).

- [ ] **Step 7: Make `lower_binding_pattern` crate-visible**

In `crates/v12-bccompiler/src/stmt.rs:388`, change:

```rust
    fn lower_binding_pattern(&mut self, pat: &BindingPattern<'_>, src: u16) -> Res<()> {
```

to:

```rust
    pub(crate) fn lower_binding_pattern(&mut self, pat: &BindingPattern<'_>, src: u16) -> Res<()> {
```

- [ ] **Step 8: Rewrite the prologue and `finish()` mapping**

In `crates/v12-bccompiler/src/unit.rs`, extend the `oxc_ast::ast` import (line 5-7) with `BindingPattern, FormalParameters`. Replace the call at line 118 with the param-deriving form:

```rust
    let params: Option<&FormalParameters<'_>> = match &node {
        UnitNode::Fn(f) => Some(&f.params),
        UnitNode::Arrow(a) => Some(&a.params),
        UnitNode::Method(f) => Some(&f.params),
        UnitNode::Class(c) => c.body.body.iter().find_map(|el| match el {
            oxc_ast::ast::ClassElement::MethodDefinition(m)
                if m.kind == MethodDefinitionKind::Constructor =>
            {
                Some(&m.value.params)
            }
            _ => None,
        }),
        UnitNode::Main(_) => None,
    };
    emit_prologue(&mut cx, idx, params, self_symbol)?;
```

Replace `emit_prologue` (lines 236-284) with:

```rust
fn emit_prologue(
    cx: &mut FnCtx<'_, '_, '_, '_>,
    idx: usize,
    params: Option<&FormalParameters<'_>>,
    self_symbol: Option<SymbolId>,
) -> Result<(), CompileError> {
    let (has_env, env_slots, this_slot, arity) = {
        let plan = &cx.comp.plans.units[cx.unit];
        (plan.has_env, plan.env_slot_count, plan.this_slot, plan.arity)
    };

    if has_env {
        let ancestor_envs = if cx.unit == 0 {
            0
        } else {
            cx.comp.plans.env_depth_between(cx.unit, 0)
        };
        cx.emit_new_env(ancestor_envs, env_slots, oxc_span::Span::default());
    }

    if let Some(ps) = params {
        // Formal `i` arrives in `r{i+1}` (`r0` is `this`). A simple identifier
        // is already in place unless captured (then copy into the env); a
        // pattern runs the shared destructuring lowerer against its incoming
        // register.
        for (i, p) in ps.items.iter().enumerate() {
            let incoming = i as u16 + 1;
            let loc = match &p.pattern {
                BindingPattern::BindingIdentifier(id) => id
                    .symbol_id
                    .get()
                    .and_then(|sym| cx.comp.plans.units[cx.unit].vars.get(&sym).copied()),
                _ => {
                    cx.lower_binding_pattern(&p.pattern, incoming)?;
                    None
                }
            };
            if let Some(VarLoc::Env(slot)) = loc {
                cx.emit_set_env(0, slot, incoming, oxc_span::Span::default());
            }
        }

        if let Some(rest) = &ps.rest {
            let rest_reg = arity as u16 + 1;
            let loc = match &rest.rest.argument {
                BindingPattern::BindingIdentifier(id) => id
                    .symbol_id
                    .get()
                    .and_then(|sym| cx.comp.plans.units[cx.unit].vars.get(&sym).copied()),
                pattern => {
                    cx.lower_binding_pattern(pattern, rest_reg)?;
                    None
                }
            };
            if let Some(VarLoc::Env(slot)) = loc {
                cx.emit_set_env(0, slot, rest_reg, oxc_span::Span::default());
            }
        }
    }

    if let Some(slot) = this_slot {
        cx.emit_set_env(0, slot, REG_THIS, oxc_span::Span::default());
    }

    if let Some(sym) = self_symbol {
        let idx16 = u16::try_from(idx).map_err(|_| CompileError {
            message: "programs above 65535 functions are not supported".into(),
            span: None,
        })?;
        let dst = cx.new_temp();
        cx.emit_closure(dst, idx16, oxc_span::Span::default());
        let access = cx.access(sym);
        cx.store_access(access, dst, oxc_span::Span::default());
    }
    Ok(())
}
```

Replace the `finish()` mapping (lines 213-222) with:

```rust
    fb.fixed_params = plan.arity as u16;
    fb.rest_reg = if plan.has_rest {
        plan.arity as u16 + 1
    } else {
        0
    };
```

- [ ] **Step 9: Run the tests and the workspace gate**

Run: `cargo nextest run -p v12-bccompiler`
Expected: PASS, including the six new tests.

Run: `cargo nextest run --workspace`
Expected: PASS (570+ tests). If `simple_params_layout_is_unchanged` fails on `fixed_params`, the formal-ident collection is wrong — inspect `register_formals`.

- [ ] **Step 10: Commit**

```bash
git add crates/v12-bccompiler/src/model.rs crates/v12-bccompiler/src/collect.rs crates/v12-bccompiler/src/stmt.rs crates/v12-bccompiler/src/unit.rs crates/v12-bccompiler/src/tests.rs
git commit -m "feat(bccompiler): lower param defaults/patterns in prologue; reserve incoming arg window"
```

---

### Task 2: Runtime verification, score, and fix-log record

**Files:**
- Modify: `conformance/fix-log.md`

**Interfaces:**
- Consumes: Task 1's prologue lowering.
- Produces: a documented score delta; nothing code-facing.

- [ ] **Step 1: Verify defaults at runtime**

Write `/tmp/a2_defaults.js`:

```javascript
function k(a = 1) { return a; }
function h(a = 1, b = a + 1) { return b; }
function d([a]) { return a; }
function r([a], ...rest) { return [a, rest.length]; }
function prior(a, b = a + 1) { return b; }
console.log(k());        // 1
console.log(k(7));       // 7
console.log(h());        // 2
console.log(d([9]));     // 9
console.log(r([4], 1, 2)); // [4,2]
console.log(prior(5));   // 6
```

Run: `cargo run -q -p v12-cli --bin v12 -- /tmp/a2_defaults.js`
Expected: `1`, `7`, `2`, `9`, `[4,2]`, `6`.

- [ ] **Step 2: Score the targeted test262 filters**

Run each and record pass/total:

```bash
cargo run -q -p test262-runner --bin test262-runner -- --filter 'language/expressions/function/dflt-params' --jobs 8
cargo run -q -p test262-runner --bin test262-runner -- --filter 'language/expressions/arrow-function/dflt-params' --jobs 8
cargo run -q -p test262-runner --bin test262-runner -- --filter 'language/expressions/object/method-definition' --jobs 8
cargo run -q -p test262-runner --bin test262-runner -- --filter 'language/expressions/function' --jobs 8
cargo run -q -p test262-runner --bin test262-runner -- --filter 'language/expressions/arrow-function' --jobs 8
```

Baseline before A2: function 74/264, arrow-function 156/343, object/method-definition 149/303, `function/dflt-params*` 3/9, `arrow-function/dflt-params*` 3/9, `*/dflt-params-ref-prior.js` 0/13.

- [ ] **Step 3: Re-run the full `language` slice**

Run: `./conformance/run.sh --filter language --jobs 8`
Record total/pass/fail and the percentage. Baseline after A1: 9722/24873 = 39.1%.

- [ ] **Step 4: Append the fix-log entry**

Append to `conformance/fix-log.md`:

```markdown
## Step A2 — parameter defaults & destructuring in the prologue (2026-09-12)

- Root cause: formal parameters were never lowered (`compile_unit` emitted only
  the body); `UnitPlan.param_count` counted pattern leaves, so pattern-inner
  symbols collided with the incoming argument registers `r1..`.
- Fix: `register_formals` records top-level `arity`/`formal_idents`/`rest_ident`;
  `finalize` reserves the incoming window (`r1..r{arity}`, rest at `r{arity+1}`)
  before assigning pattern leaves and body locals; `emit_prologue` runs the
  shared `lower_default`/`lower_binding_pattern` against each incoming register.
- Accepted gap: no parameter TDZ (`(a = b, b = 2)` reads `b`'s register).
- Scores: <fill in from Steps 2-3 — before → after>.
- Gate: `cargo nextest run --workspace` <count> passed.
```

- [ ] **Step 5: Commit**

```bash
git add conformance/fix-log.md
git commit -m "docs(conformance): record A2 param-binding results"
```

---

## Self-Review

- **Spec coverage:** §2 A2 (defaults + patterns + ABI collision) → Task 1 Steps 3-8. Runtime + score + log → Task 2. ✓
- **Placeholders:** none; every code step carries the full replacement. Score numbers are filled by running Task 2 (unavoidable measured data, not a design placeholder).
- **Type consistency:** `arity: usize`, `formal_idents: Vec<Option<SymbolId>>`, `rest_ident: Option<SymbolId>` used identically in `model.rs`, `collect.rs` (`register_formals`, `finalize`), and `unit.rs` (`emit_prologue`, `finish`). `lower_binding_pattern` keeps its `(&BindingPattern, u16) -> Res<()>` signature. `fb.fixed_params = plan.arity` and `fb.rest_reg = plan.arity + 1` match the interpreter ABI in `crates/v12-interp/src/call.rs`.
