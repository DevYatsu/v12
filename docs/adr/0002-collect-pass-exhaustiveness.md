# ADR-0002: Make compiler Pass A (`collect`) variant-exhaustive

**Status:** Proposed
**Date:** 2026-10-01
**Deciders:** user (pending)
**Relates to:** `docs/architecture-maintainability-plan.md` §3 F2, §4 Track 1 (W1.1/W1.2), §7 exit criteria
**Supersedes:** nothing
**Informed by:** ADR-0001 (oxc exact pin, cited from `Cargo.toml:39-42`; file still missing — W4.5)

---

## Context

### The two passes

`v12-bccompiler` walks the oxc AST twice.

**Pass A — `collect`** (`crates/v12-bccompiler/src/collect.rs`, 2627 lines) builds `Plans`:
declarations, references, const-bindings, early errors, and the `units` tree of nested
function/class bodies keyed by span. Everything downstream in Pass B looks a unit up in
`Plans` by span; there is no second source.

**Pass B — `lower`** (`crates/v12-bccompiler/src/stmt.rs`, `crates/v12-bccompiler/src/expr.rs`)
turns each unit into bytecode. `FnCtx::stmt` (`stmt.rs:105-257`) and `FnCtx::expr`
(`expr.rs:38-505`) list every oxc variant explicitly, by documented intent
(`stmt.rs:106-108`).

**A third walker exists.** `contains_arguments` (`collect.rs:215-329`) re-implements a
recursive AST traversal with its own `walk_expr` (`collect.rs:218-302`) and `walk_stmt`
(`collect.rs:313-327`), to implement the ES §15.7.1 `ContainsArguments` early error on
class field initializers. It has exactly one call site, `collect.rs:1091`.

### What oxc 0.147.0 actually does — two facts that decide this ADR

**Fact 1: nothing in `oxc_ast` is `#[non_exhaustive]`.**

```
$ grep -rn "non_exhaustive" ~/.cargo/registry/src/*/oxc_ast-0.147.0/ | wc -l
0
```

So a match on `Statement` or `Expression` without a wildcard *can* compile, and an
exhaustive match genuinely proves exhaustiveness to rustc. The premise in the task brief
("`oxc_ast` types are `#[non_exhaustive]` in some versions") is not true at the pinned
version. Nothing needs to be done about it beyond re-checking on every oxc bump, because
the pin is exact (`Cargo.toml:39-42`, all four crates `=0.147.0`).

**Fact 2: `INHERIT` is expanded to concrete variants at macro-expansion time.**

`Statement` is declared with oxc's `#[ast]` attribute
(`oxc_ast-0.147.0/src/ast/js.rs:1081`) and lists two marker variants:

```rust
// oxc_ast-0.147.0/src/ast/js.rs:1106-1108
    // `Declaration` and `ModuleDeclaration` variants added here by `#[ast]` macro
    INHERIT(Declaration<'a>),
    INHERIT(ModuleDeclaration<'a>),
```

`oxc_ast_macros` documents and implements that expansion:

```rust
// oxc_ast_macros-0.147.0/src/ast.rs:34-35
    /// `#[ast]` macro removes the `INHERIT` marker variants from the enum, and inserts the
    /// variants of these enums in their place. Empty if the enum doesn't inherit any variants.
```

and does it literally, before rustc ever sees the item:

```rust
// oxc_ast_macros-0.147.0/src/ast.rs:113-122
            let inherited = &INHERITED_ENUMS[index as usize];
            // Insert the inherited enum's variants, spanned to the `INHERIT` marker variant they replace
            item.variants.extend(
                inherited
                    .variants
                    .iter()
                    .map(|variant| make_inherited_variant(variant, inherited.doc, marker_span)),
            );
```

So `Statement::FunctionDeclaration` and `Expression::StaticMemberExpression` are ordinary
variants of `Statement` and `Expression`. A new variant added to `Declaration`,
`ModuleDeclaration` or `MemberExpression` propagates into the flattened enum and breaks
every exhaustive match on it — Pass A's included, once Pass A is exhaustive.

> **This corrects `docs/architecture-maintainability-plan.md:173`**, which reads: *"oxc's
> `INHERIT` macro means 'exhaustive' must be spelled variant by variant … so the compiler's
> help is weaker than it first appears."* The **ergonomics** are worse (you must re-type
> arms on a bump, and `rustc`'s "not covered" note points at a variant you did not write).
> The **guarantee** is not weaker at all: it is identical to a directly-declared variant.
> The plan's premise for choosing a weaker mechanism is unfounded.

**Flattened variant counts** (computed from the vendored source by resolving `INHERIT`
recursively):

| Enum | Direct | Inherited from | Flattened |
|---|---|---|---|
| `Statement` (`js.rs:1085`) | 18 | `Declaration` (10, `js.rs:1161`), `ModuleDeclaration` (8) | **36** |
| `Expression` (`js.rs:80`) | 40 | `MemberExpression` (3, `INHERIT` at `js.rs:172`) | **44** |

### The measured gap, per walker

Method: extract the flattened variant list from the vendored oxc source, then extract the
`Statement::`/`Expression::` mentions inside each match's line range.

| Walker | Match range | Covered | Wildcard |
|---|---|---|---|
| Pass A `Collect::stmt` | `collect.rs:734-892` | **25 / 36** (17 in-match + 8 via the `as_module_declaration()` prefilter at `:721-733`) | `_ => {}` at **`:891`** |
| Pass A `Collect::expr` | `collect.rs:1610-1920` | **28 / 44** | `_ => {}` at **`:1918`** |
| Pass A `contains_arguments::walk_expr` | `collect.rs:218-302` | **24 / 44** | `_ => false` at **`:300`** |
| Pass A `contains_arguments::walk_stmt` | `collect.rs:313-327` | **7 / 36** | `_ => false` at **`:325`** |
| Pass B `FnCtx::stmt` | `stmt.rs:105-257` | **36 / 36** | **none** |
| Pass B `FnCtx::expr` | `expr.rs:38-505` | **44 / 44** | **none** |

**Pass B's exhaustiveness claim is real.** Both Pass B matches name every variant and
contain no wildcard anywhere in their bodies (the 23 wildcards in `stmt.rs`/`expr.rs` all
belong to other functions — destructuring targets, helper predicates, `Option` returns).
It satisfies §7's Track 1 exit criterion ("adding an oxc AST variant cannot compile unless
Pass A handles it") for exactly one of the two passes today.

### Which of the missing variants are missing *on purpose*

The 11 variants `Collect::stmt` drops, and the 16 `Collect::expr` drops, split cleanly:

**Intentional — the variant cannot carry a binding, a reference, or a nested unit (26):**

- `Statement`: `BreakStatement`, `ContinueStatement`, `EmptyStatement`, `DebuggerStatement`
- `Expression`: the 6 literals (`BooleanLiteral`, `NullLiteral`, `NumericLiteral`,
  `BigIntLiteral`, `RegExpLiteral`, `StringLiteral`), `ThisExpression`, `Super`,
  `ImportMeta`, `NewTarget`
- TypeScript and JSX, which the compiler never parses: `TSTypeAliasDeclaration`,
  `TSInterfaceDeclaration`, `TSEnumDeclaration`, `TSExternalModuleDeclaration`,
  `TSNamespaceDeclaration`, `TSGlobalDeclaration`, `TSImportEqualsDeclaration`,
  `TSExportAssignment`, `TSNamespaceExportDeclaration` (the last two are already written
  out at `collect.rs:729-730`), `TSAsExpression`, `TSSatisfiesExpression`,
  `TSTypeAssertion`, `TSNonNullExpression`, `TSInstantiationExpression`,
  `V8IntrinsicExpression`, `JSXElement`, `JSXFragment`

Note `TSExportAssignment` and `TSNamespaceExportDeclaration` are already explicit — proof
that listing the type-only variants individually is the established local convention, not
a new burden.

**Genuine — the variant *can* contain a reference or a nested function and is silently
skipped (2, both in `contains_arguments`):**

- `ChainExpression` and `ImportExpression`, both absent from `collect.rs:218-302`.

This is not theoretical. `ContainsArguments` traverses the whole initializer, so both must
be searched, and `collect.rs:300` makes the early error miss them. Reproduced on `43038962`
with `./target/debug/v12`:

```
class C { x = arguments; }                  → SyntaxError   (correct)
class C { x = (0, arguments); }              → SyntaxError   (correct)
class C { x = (arguments ?? 1); }            → SyntaxError   (correct)
class C { x = (()=>arguments)?.f; }          → accepted      WRONG
class C { x = import(arguments); }           → accepted      WRONG
```

Node rejects both of the last two. This is a shipped, observable conformance defect and a
direct instance of the failure class F2 describes — found by reading the variant gap, not
by any failing test.

`collect::stmt`'s `ForStatementLeft` wildcards at `:802`/`:833` were the other candidate
(`ForStatementLeft` flattens to 8 variants including the two destructuring patterns), so I
probed them directly: `for (const [a,b] of xs)` and `for (const {k} of o)` both compute
correctly, including through a captured closure (`3,7` and `a,b` as expected). Pass B's
`assign_for_of_value` lowers those independently, so this is a latent hazard, not a live
bug. It is listed below as an optional third batch, not as required work.

### Why this must be fixed now, not at the next oxc bump

The mutation check, run on a `git archive` copy of `43038962` in `/tmp` (the working tree
was not touched):

**Today — deleting an entire statement kind from Pass A is undetectable:**

```
# remove `Statement::SwitchStatement` from collect.rs:877-885
$ cargo check -p v12-bccompiler
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 36.80s
```

Builds clean. `switch` cases stop producing `ListKind::Control` bodies, every `case`
clause loses its discriminant and test expressions, and rustc reports nothing — only an
incidental `warning: variant 'Control' is never constructed` (`collect.rs:344`), a
side-effect of this particular mutation, not a guard.

**After the change proposed below — the same deletion is a compile error:**

```
$ cargo check -p v12-bccompiler
error[E0004]: non-exhaustive patterns: `&Statement::SwitchStatement(_)` not covered
```

That is the entire difference between the two options, measured.

---

## Decision

**Convert Pass A's three `Statement`/`Expression` matches to explicit variant lists,
matching Pass B. Do not add an enumeration test.**

Three edits, all in `crates/v12-bccompiler/src/collect.rs`:

1. `Collect::stmt` — replace `_ => {}` at **`:891`** with 19 named alternatives: the 4
   control-flow statements and the 15 declaration/module variants that the
   `as_module_declaration()` prefilter at `:721-733` intercepts at runtime. All are
   collected into one `=> {}` arm with a comment saying what they are (nothing to collect).
2. `Collect::expr` — replace `_ => {}` at **`:1918`** with the 16 leaf/type-only
   alternatives, one arm.
3. `contains_arguments::walk_expr` — replace `_ => false` at **`:300`** with the same 16
   alternatives, plus `ChainExpression` and `ImportExpression` given **real traversal
   arms** (not `_ => false`), which fixes the `ContainsArguments` defect demonstrated
   above. `walk_stmt` at `:325` follows the same pattern.

The remaining 15 catch-alls in `collect.rs` are on enums where a wildcard is defensible
(`PropertyKey` `:185`, `ClassElement` `:206`/`:958`/`:1014`/`:1074`, `MethodDefinitionKind`
`:1120`, `ExportSpecifier`-ish `:1295`, `ExportDefaultDeclarationKind` `:1353`/`:1382`,
`ArrayExpressionElement` `:1901`, `AssignmentTarget` `:2003`,
`SimpleAssignmentTarget` `:2040`, `BindingPattern` `:2164`/`:2247`). They are **out of
scope for this ADR**; §"What this does not fix" says why each is a different question.

### Why (a) and not the alternatives

| Option | Verdict | Reason |
|---|---|---|
| **(a) Explicit variant lists** | **Chosen** | Compile-time, on par with Pass B, zero new machinery, no new dependency, ~40 net lines. Satisfies §7's exit criterion literally. |
| (b) Enumeration test | Rejected | Weaker (test-time, and only if someone runs the suite — the suite is not run by CI today, `ci.yml:35`), strictly more machinery, and **redundant**: rustc already answers the question at build time with a better error message. Fails *fail-fast*. |
| (c) Shared macro/trait for both passes | Rejected | The two passes cannot share arms — Pass A records declarations, Pass B emits bytecode, and they already diverge on 12 variants (e.g. `ChainExpression` is walked in both, `SwitchStatement` is collected in one and lowered in the other, `ParenthesizedExpression` is transparent in both). A `macro_rules!` would generate a match whose bodies are still written twice, i.e. (a) plus indirection. Fails *KISS*. |
| (d) Something the API makes available | Rejected | `oxc_ast::AstType`/`AstKind` (`oxc_ast-0.147.0/src/generated/ast_kind.rs:11` `AST_TYPE_MAX: u8 = 191`, 192 variants) *is* a usable enumeration source, so (b) is technically real — but it enumerates node **kinds** (structs), not `Statement`/`Expression` **variants**, and oxc exposes no public mapping between them. `oxc_ast` also does not re-export `oxc_ast_visit` (`oxc_ast-0.147.0/src/lib.rs:41-42` only link it in docs), so a visitor-based rewrite would add a dependency to replace a guarantee rustc already provides. Fails *YAGNI*. |

The plan asked for a decision between fail-fast and cheap-bump-proof. **Fail-fast wins, and
the cost of being wrong is one `git checkout`.** The bump cost (a) imposes is re-typing a
handful of arms in a file whose `rustc` error names the exact variant — which is the
review you wanted anyway. The bump cost (b) imposes is *zero*, and it buys a guarantee that
fires after the code has already compiled, in a test suite that no CI job currently runs.

### Concrete first batch (W1.2)

One commit, `crates/v12-bccompiler/src/collect.rs` only, **+40 / −3**:

| Site | Change |
|---|---|
| `collect.rs:891` | `_ => {}` → 19 named alternatives → `{}` |
| `collect.rs:1918` | `_ => {}` → 16 named alternatives → `{}` |
| `collect.rs:300` | `_ => false` → 16 named alternatives → `false`, **plus** `ChainExpression` → recurse on `c.expression`, `ImportExpression` → recurse on `i.source` and `i.options` |
| `collect.rs:325` | `_ => false` → remaining `Statement` alternatives → `false` |

**Verification (all four must be run; the first two are the point):**

1. *Positive mutation* — delete the `Statement::SwitchStatement` arm; expect
   `error[E0004]: non-exhaustive patterns: &Statement::SwitchStatement(_) not covered`.
2. *Positive mutation* — delete the `Expression::ChainExpression` arm; expect the same
   error naming `ChainExpression`. (Confirmed for `SwitchStatement` on a `/tmp` copy of
   `43038962`; both arms were compiled green in the same experiment.)
3. *Positive test* — add to `crates/v12-bccompiler/src/tests.rs`:
   `class C { x = (()=>arguments)?.f }` and `class C { x = import(arguments) }` must both
   report the `ContainsArguments` early error. Both fail on `43038962` (verified above) and
   must pass after.
4. *No regression* — `cargo nextest run --workspace --no-fail-fast`, and `faildiff` on
   `conformance/baseline/language.json` (W0.4 must be landed first for this to be a gate;
   until then it is a manual step). Batch 1 is the only batch permitted before W0.4 lands.

Batchable later, each independently green: (2) the 15 secondary catch-alls, with
`ClassElement` and `SimpleAssignmentTarget` first since both can carry a nested unit;
(3) `ForStatementLeft` at `:802`/`:833`; (4) the `binding_symbol`/`ident_name_of_binding`
asymmetry described below.

### Consequences

**Positive**

- §7's Track 1 exit criterion becomes true for real: an oxc variant — direct or inherited
  — cannot be added without Pass A naming it.
- The `ContainsArguments` defect for `ChainExpression` / `ImportExpression` is fixed, with
  two tests that fail today.
- The `Collect::stmt` / `Collect::expr` asymmetry documented at
  `architecture-maintainability-plan.md:93` closes: 25→36 / 28→44, matching Pass B's
  36/36 and 44/44.

**Negative**

- Every future oxc bump touches three more match arms in this file. This is the accepted
  cost and is the reason the plan's `INHERIT` premise had to be corrected rather than
  believed.
- Diff noise on an unrelated oxc bump will look like a compiler change. Mitigate with the
  commit-message convention already in use: state that the commit is mechanical and name
  the variant(s) oxc added.
- 26 of the 40 new arms encode "this variant is irrelevant". If v12 ever grows a
  JavaScript-proposal variant (e.g. decorators, explicit resource management), that arm
  becomes the place a reviewer looks — which is the point, but it is ceremony until then.

### What this decision does **not** fix

Stated explicitly, because the temptation after landing it will be to call F2 closed.

- **The 26 pre-existing gaps are closed by assertion, not by new behaviour.** They were
  verified irrelevant (they cannot carry a binding or reference, or the compiler does not
  parse them). This ADR adds no traversal to them. If that analysis is wrong for some
  variant, this ADR will not find it — only a test will.
- **The 15 secondary catch-alls stay.** Two are worth a follow-up:
  - `collect.rs:2164` `binding_symbol` returns `None` for any non-`BindingIdentifier`
    pattern, while the recursive `collect_const_bindings` (`:1493-1517`) exists and *is*
    used for plain declarations at `:1451` but **not** for the for-in/for-of paths at
    `:790`/`:822`. That is a real asymmetry. I could **not** isolate it as a live bug: the
    const-reassignment check does not fire for `const [y] = [1]; y = 2` either, so the
    const-TDZ gap is larger than the asymmetry. Fixing that gap is not this ADR.
  - `collect.rs:802`/`:833` (`ForStatementLeft`) drop 6 variants, two of which are
    destructuring patterns that can hold bindings. Probed and currently correct; left
    wildcarded on purpose, because closing it without a test would be an untested change
    to a path that works.
- **`contains_arguments` stays a third walker.** This ADR makes it exhaustive; it does not
  remove it. It duplicates `collect::expr`'s traversal with a *different* boundary rule —
  `contains_arguments` stops at any `FunctionExpression` (`:221`) and at class heritage
  (`:222-225`), whereas `mark_arguments` (`:492-505`) stops only at arrows. Deleting the
  walker would be a **behaviour change**, not a refactor, and needs its own
  conformance-verified batch.
- **The environment-depth model.** `env_depth` / `env_depth_between` / `env_bias` are
  untouched. That is W1.3/W1.4, and it is the permanent home of the `class … extends`
  family.
- **The `class … extends` bug and the per-iteration-`let` aliasing bug** recorded in
  `CONTEXT.md` are untouched. Exhaustiveness stops *new* variants from being silently
  dropped; it does not make the existing scope model correct.
- **CI still does not run the test suite or clippy** (`ci.yml:32`, `:35`). If W0.1/W0.4 do
  not land, the two tests in verification step 3 are not gated by anything, and this
  ADR's guarantee reduces to the compiler's — which is still the stronger half, but the
  weaker half is gone.

### Open questions for the decider

1. Should the 15 secondary catch-alls be in scope now, or explicitly deferred as a
   follow-up? Recommendation: defer; `ClassElement` and `SimpleAssignmentTarget` first.
2. Should `contains_arguments` be deleted in a follow-up rather than made exhaustive?
   Recommendation: yes, but only after W0.4 makes `faildiff` a gate, because it is a
   behaviour change.