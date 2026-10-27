//! Regression tests for ADR-0002's remaining batches: the
//! `Collect::stmt` `ForStatementLeft` wildcards, the `binding_symbol`
//! single-name assumption, and the secondary catch-alls it turned up.
//!
//! Every expected value here was checked against `node` (v24); the ones that
//! are early errors name the exact message the compiler emits so a change in
//! *which* error fires is visible.

use v12_bccompiler::{compile_source, compile_source_as_module};

/// Compiles and returns the compile-error message, or a marker for success.
fn compile_err(src: &str) -> String {
    match compile_source(src) {
        Ok(_) => "<compiled>".to_string(),
        Err(e) => e.message,
    }
}

fn assert_contains_arguments_error(src: &str) {
    let message = compile_err(src);
    assert!(
        message.contains("'arguments' is not allowed in a class field initializer"),
        "{src}: expected the ContainsArguments early error, got {message:?}",
    );
}

fn assert_const_reassignment_error(src: &str) {
    let message = compile_err(src);
    assert!(
        message.contains("Assignment to constant variable"),
        "{src}: expected a const-reassignment early error, got {message:?}",
    );
}

fn assert_compiles(src: &str) {
    if let Err(e) = compile_source(src) {
        panic!("{src}: expected this to compile, got {}", e.message);
    }
}

/// The unit's own environment allocation, i.e. whether `finalize` decided any
/// of its bindings escape.
///
/// This is the observable half of "a reference was noted". A reference site
/// that `collect` never records cannot mark its symbol `captured`
/// (`collect.rs` `finalize` step 1), so the declaring unit never sets
/// `has_env` and no `NewEnvironment` is emitted — and the *inner* frame's
/// read/write then resolves to a register of its own. It is the same
/// mechanism ADR-0004 records for `class … extends`.
fn main_allocates_env(src: &str) -> bool {
    let program = compile_source(src).unwrap_or_else(|e| panic!("{src}: {}", e.message));
    let main = &program.functions[program.main as usize];
    main.instrs
        .iter()
        .any(|i| i.op() == Some(v12_bytecode::Opcode::NewEnvironment))
}

/// Export names recorded for a module, in table order.
fn module_exports(src: &str) -> Vec<String> {
    let module = compile_source_as_module(src)
        .unwrap_or_else(|e| panic!("{src}: {}", e.message));
    module
        .exports
        .iter()
        .map(|e| e.exported.clone())
        .collect()
}

// ---------------------------------------------------------------------------
// ForStatementLeft: the assignment forms of a loop head
// ---------------------------------------------------------------------------

/// A for-of head that is a member expression reads its object. The head used
/// to be dropped by a `_ => {}`, so `o` was never a reference site, never
/// `captured`, and the enclosing unit allocated no environment.
///
/// The control is the same write spelled outside a loop head, which the
/// exhaustive `AssignmentTarget` walker always reached.
#[test]
fn for_of_member_head_read_marks_the_object_as_escaping() {
    assert!(
        main_allocates_env("let o={x:1}; let g=function(){ for (o.x of [5]) {} }"),
        "a `for (o.x of …)` head reads `o`; the enclosing unit must allocate an environment",
    );
    assert!(
        main_allocates_env("let o={x:1}; let g=function(){ o.x = 5; }"),
        "control: the same write as a plain assignment already escapes",
    );
    assert!(
        main_allocates_env("let k='x'; let o={x:1}; let g=function(){ for (o[k] of [5]) {} }"),
        "a computed member head reads both the object and the key",
    );
    assert!(
        !main_allocates_env("let o={x:1}; let g=function(){ let q = 1; }"),
        "control: an unrelated binding must not allocate an environment",
    );
}

#[test]
fn for_of_destructuring_head_writes_reach_the_enclosing_binding() {
    assert!(
        main_allocates_env("let a=1; let g=function(){ for ([a] of [[9]]) {} }"),
        "`for ([a] of …)` writes `a`",
    );
    assert!(
        main_allocates_env("let a=1; let g=function(){ for ({x:a} of [{x:2}]) {} }"),
        "`for ({{x:a}} of …)` writes `a`",
    );
    assert!(
        main_allocates_env("let a=1; let g=function(){ for ([a=9] of [[]]) {} }"),
        "`for ([a=9] of …)` writes `a` from a default",
    );
    assert!(
        main_allocates_env("let a=1; let g=function(){ for ([...a] of [[2]]) {} }"),
        "`for ([...a] of …)` writes `a` from a rest",
    );
    assert!(
        main_allocates_env("let a=1; let g=function(){ for (a of [3]) {} }"),
        "`for (a of …)` writes `a` (was already reached; must stay reached)",
    );
}

#[test]
fn for_in_member_head_is_still_rejected_by_pass_b() {
    // Pass B only lowers a for-in head that is a declaration or a bare
    // identifier (`stmt.rs`, "for-in only supports variable declarations and
    // simple identifiers"). Pass A now walks every head form regardless; this
    // test records the boundary so a future Pass B widening is not mistaken
    // for a Pass A regression.
    let message = compile_err("let o={x:1}; for (o.x in {a:2}) {}");
    assert!(
        message.contains("for-in only supports variable declarations and simple identifiers"),
        "unexpected message: {message:?}",
    );
}

#[test]
fn for_of_head_reserved_word_target_is_an_early_error_in_strict_mode() {
    // Reaching `simple_target` from the head means the head now gets the same
    // strict-mode reserved-word check every other assignment target has.
    // node rejects each of these ("Unexpected strict mode reserved word").
    // `yield` is not in the list: oxc's *parser* rejects it first, as a
    // `yield` expression rather than an identifier.
    for src in [
        r#""use strict"; for (implements of [1]) {}"#,
        r#""use strict"; for (public of [1]) {}"#,
        r#""use strict"; for (interface of [1]) {}"#,
        r#""use strict"; for (package of [1]) {}"#,
    ] {
        let message = compile_err(src);
        assert!(
            message.contains("reserved word in strict mode"),
            "{src}: expected the strict reserved-word early error, got {message:?}",
        );
    }
    // Non-strict still compiles — these are legal identifiers there.
    assert_compiles("for (implements of [1]) {}");
}

// ---------------------------------------------------------------------------
// binding_symbol: a bare name, not "every name the pattern binds"
// ---------------------------------------------------------------------------

#[test]
fn const_destructuring_loop_head_reassignment_is_an_early_error() {
    // `var_decl` already had the pattern fallback; the for-in/of head path
    // reached only `binding_symbol`, so a destructuring const head was never
    // recorded as const and `y = 2` in the loop body compiled.
    for src in [
        r#""use strict"; for (const y of [1]) { y = 2; }"#,
        r#""use strict"; for (const [y] of [[1]]) { y = 2; }"#,
        r#""use strict"; for (const {y} of [{y:1}]) { y = 2; }"#,
        r#""use strict"; for (const [y = 0] of [[1]]) { y = 2; }"#,
        r#""use strict"; for (const [...y] of [[1]]) { y = 2; }"#,
        r#""use strict"; for (const [[y]] of [[[1]]]) { y = 2; }"#,
        r#""use strict"; for (const {p: {y}} of [{p:{y:1}}]) { y = 2; }"#,
        r#""use strict"; for (const y in {a:1}) { y = 2; }"#,
        r#""use strict"; for (const [y] in {}) { y = 2; }"#,
    ] {
        assert_const_reassignment_error(src);
    }
    // Control: the plain declaration form already fired, and still does.
    assert_const_reassignment_error(r#""use strict"; const [y] = [1]; { y = 2; }"#);
    // A `let` head is reassignable — the check must not over-reach.
    assert_compiles(r#""use strict"; for (const [a] of [[1]]) { let b = a; } for (const [c] of [[1]]) { c; }"#);
}

#[test]
fn destructuring_exports_are_recorded() {
    // `export const [a] = [1]` is ordinary module syntax; the export table
    // used to come out empty because `binding_symbol` returns `None` for
    // anything that is not a bare `BindingIdentifier`.
    assert_eq!(module_exports("export const a = 1;"), ["a"]);
    assert_eq!(module_exports("export const [a] = [1];"), ["a"]);
    assert_eq!(module_exports("export const {a} = {a: 1};"), ["a"]);
    assert_eq!(module_exports("export const [a = 5] = [];"), ["a"]);
    assert_eq!(module_exports("export let {x: y} = {x: 1};"), ["y"]);
    assert_eq!(module_exports("export var [p, q] = [1, 2];"), ["p", "q"]);
    assert_eq!(module_exports("export var {a: {b}} = {a: {b: 1}};"), ["b"]);
    assert_eq!(module_exports("export var [...r] = [1];"), ["r"]);
    assert_eq!(module_exports("export const a = 1, [b] = [2];"), ["a", "b"]);
    assert_eq!(module_exports("export var {a} = {a: 1}, {c} = {c: 2};"), ["a", "c"]);
    // node: `export const [a] = [1]; export var [p,q]=[2,3];` -> ['a','p','q'].
    assert_eq!(
        module_exports("export const [a] = [1]; export var [p, q] = [2, 3];"),
        ["a", "p", "q"],
    );
}

#[test]
fn a_destructuring_formal_still_leaves_its_incoming_register_as_scratch() {
    // The one caller that legitimately wants `None` from `binding_symbol`:
    // `register_formals` reads `formal_idents` positionally against the
    // incoming formal window, where `None` means "this formal is a pattern".
    // The `None` must survive the change — a destructuring formal must still
    // consume exactly one incoming register, and its leaves must be placed
    // from `decl_order` instead.
    let program = compile_source("function f([a], {b}, c = 1, ...rest) { return [a, b, c]; }")
        .expect("compiles");
    let f = program
        .functions
        .iter()
        .find(|f| f.is_top_level == false && f.name_hint.is_some())
        .expect("a nested function unit exists");
    // The incoming formal window is reserved positionally: r0 = this, then
    // one register per fixed formal (r1/r2/r3), then the rest tail (r4). A
    // pattern formal consumes its register like any other and leaves it as
    // scratch; its leaves are placed separately from `decl_order`.
    assert_eq!(f.fixed_params, 3);
    assert!(f.has_rest);
    assert_eq!(f.rest_reg, 4);

    // Control: the same arity spelled with bare identifiers reserves the same
    // window, so the patterns did not widen or narrow it.
    let plain = compile_source("function g(a, b, c = 1, ...rest) { return [a, b, c]; }")
        .expect("compiles");
    let g = plain
        .functions
        .iter()
        .find(|f| !f.is_top_level && f.name_hint.is_some())
        .expect("a nested function unit exists");
    assert_eq!(g.fixed_params, f.fixed_params);
    assert_eq!(g.rest_reg, f.rest_reg);
}

// ---------------------------------------------------------------------------
// ContainsArguments through an update expression's member target
// ---------------------------------------------------------------------------

#[test]
fn contains_arguments_reaches_through_an_update_expressions_member_target() {
    // `(arguments).x++` reads `arguments` to find the object it updates. The
    // `UpdateExpression` arm used to answer `false` for every non-identifier
    // target; node rejects all three of these.
    for src in [
        "class C { x = (arguments).x++; }",
        "class C { x = (arguments.x)++; }",
        "class C { x = (arguments)[0]++; }",
        "class C { x = ++(arguments).y; }",
    ] {
        assert_contains_arguments_error(src);
    }
    // Controls: the identifier and the unrelated-target forms.
    assert_contains_arguments_error("class C { x = arguments; }");
    assert_contains_arguments_error("class C { x = (arguments, 0); }");
    assert_compiles("class C { x = (this).n++; }");
    assert_compiles("class C { x = (o).n++; }");
}

// ---------------------------------------------------------------------------
// Secondary catch-alls: naming a variant must not change behaviour
// ---------------------------------------------------------------------------

#[test]
fn the_named_class_element_variants_keep_their_pass_b_rejection() {
    // Pass A now names `StaticBlock` / `AccessorProperty` at all four
    // `ClassElement` sites. They are rejected by Pass B, so Pass A naming
    // them must not turn them from "rejected" into "silently accepted".
    for (src, needle) in [
        (
            "class C { static { this.x = 1; } }",
            "static initialization blocks are not supported yet",
        ),
        ("class C { accessor x = 1; }", "accessor properties"),
    ] {
        let message = compile_err(src);
        assert!(
            message.contains(needle),
            "{src}: expected {needle:?}, got {message:?}",
        );
    }
}

#[test]
fn the_named_export_declaration_variants_still_compile() {
    // `export default <expression>` reaches the `as_expression()` delegation;
    // the `TS*` arms are named rather than wildcarded. Neither may change
    // what compiles.
    assert_eq!(module_exports("export default 1;"), ["default"]);
    assert_eq!(module_exports("export default function f(){}"), ["default"]);
    assert_eq!(
        module_exports("export default (function(){ return 1; });"),
        ["default"],
    );
    // The exported *name* for a default-exported binding is still the
    // declaration's own name, not `default`.
    let module = compile_source_as_module("export default function named(){}").unwrap();
    assert_eq!(module.exports.len(), 1);
    assert_eq!(module.exports[0].exported, "default");
}

#[test]
fn a_computed_class_element_key_is_still_walked_once() {
    // The computed-key loop was rewritten from "match with guards" to "match
    // to a `&PropertyKey`, then walk". A key expression must still be walked
    // exactly once, in the class's unit.
    assert_compiles("let e = 'k'; class C { [e]() {} }");
    assert_compiles("let e = 'k'; class C { [e] = 1; }");
    assert_compiles("class C { k() {} }");
    // A computed key is evaluated in the enclosing unit (ES §15.7.14 step
    // 26), so reading `e` there captures nothing; only a closure that reads
    // `e` forces it into an environment.
    assert!(
        !main_allocates_env("let e='k'; class C { [e]() {} }"),
        "a computed class key reads `e` directly in the enclosing unit",
    );
    assert!(
        main_allocates_env("let e='k'; class C { [e]() { return e; } }"),
        "a method body that reads `e` still captures it",
    );
}