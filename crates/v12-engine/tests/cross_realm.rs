//! Cross-realm integration tests: `$262.createRealm` builds a second realm
//! on the shared heap; the tests mirror the Test262 `language` patterns the
//! runner now executes (cross-realm eval, identity, and slot aliasing).

use v12_engine::Engine;

fn to_text(engine: &mut Engine, v: Result<v12_heap::JsValue, v12_heap::JsValue>) -> String {
    match v {
        Ok(v) => engine.to_display_string(v),
        Err(thrown) => format!("threw: {}", engine.to_display_string(thrown)),
    }
}

fn eval_ok(engine: &mut Engine, source: &str) -> String {
    let result = engine.eval(source);
    let text = to_text(engine, result);
    assert!(!text.starts_with("threw:"), "eval failed: {text}");
    text
}

#[test]
fn create_realm_exposes_global_and_eval() {
    let mut engine = Engine::new();
    engine
        .install_create_realm_function("__v12CreateRealm__")
        .expect("install");
    // `typeof other` is an object; `other.eval` is callable; eval runs in
    // the other realm (its `x` stays out of this realm's global).
    let out = eval_ok(
        &mut engine,
        r#"
        var other = __v12CreateRealm__().global;
        var otherEval = other.eval;
        otherEval('var x = 23;');
        [
          typeof other,
          typeof otherEval,
          typeof x,
          String(other.x),
        ].join('|');
    "#,
    );
    assert_eq!(out, "object|function|undefined|23");
}

#[test]
fn cross_realm_identity_is_real_identity() {
    let mut engine = Engine::new();
    engine
        .install_create_realm_function("__v12CreateRealm__")
        .expect("install");
    // Two realms produce distinct globals; each realm's global IS the
    // object handed out (so property identity checks hold).
    let out = eval_ok(
        &mut engine,
        r#"
        var r1 = __v12CreateRealm__();
        var r2 = __v12CreateRealm__();
        [
          String(r1.global === r1.global),
          String(r1.global === r2.global),
          String(r1.global.globalThis === r1.global),
        ].join('|');
    "#,
    );
    assert_eq!(out, "true|false|true");
}

#[test]
fn cross_realm_eval_sees_target_realm_intrinsics() {
    let mut engine = Engine::new();
    engine
        .install_create_realm_function("__v12CreateRealm__")
        .expect("install");
    // Reads of intrinsic slots off the other realm's global (shapeless
    // prefix) resolve, and eval'd code resolves ITS realm's intrinsics.
    let out = eval_ok(
        &mut engine,
        r#"
        var other = __v12CreateRealm__().global;
        [
          String(typeof other.eval),
          String(typeof other.Math),
          String(typeof other.TypeError),
          other.eval('typeof TypeError'),
          other.eval('Math.floor(1.5)'),
        ].join('|');
    "#,
    );
    assert_eq!(out, "function|object|function|function|1");
}

#[test]
fn cross_realm_function_constructor_compiles_a_real_function() {
    let mut engine = Engine::new();
    engine
        .install_create_realm_function("__v12CreateRealm__")
        .expect("install");
    // `new (other.Function)(body)` returns a callable closure (its program
    // registers in the caller's cross-program table), callable from here.
    let out = eval_ok(
        &mut engine,
        r#"
        var other = __v12CreateRealm__().global;
        var f = new (other.Function)('return 40 + 2;');
        [
          String(typeof f),
          String(f()),
        ].join('|');
    "#,
    );
    assert_eq!(out, "function|42");
}

#[test]
fn debug_cross_realm_var_write() {
    let mut engine = Engine::new();
    engine
        .install_create_realm_function("__v12CreateRealm__")
        .expect("install");
    let out = eval_ok(
        &mut engine,
        r#"
        var other = __v12CreateRealm__().global;
        var otherEval = other.eval;
        otherEval('var x = 23;');
        [
          String(otherEval('x')),
          String(typeof other.x),
          String(other['x']),
        ].join('|');
    "#,
    );
    eprintln!("DEBUG: {out}");
}

#[test]
fn debug_isolate_eval_write_target() {
    let mut engine = Engine::new();
    engine
        .install_create_realm_function("__v12CreateRealm__")
        .expect("install");
    let out = eval_ok(
        &mut engine,
        r#"
        var otherEval = __v12CreateRealm__().global.eval;
        otherEval('var x = 23;');
        eval('var y = 23;');
        [typeof x, typeof y, String(x), String(y)].join('|');
    "#,
    );
    eprintln!("DEBUG: {out}");
}
