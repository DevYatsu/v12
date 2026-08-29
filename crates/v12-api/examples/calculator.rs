//! Minimal embedder: spawn a context, expose a Rust host function,
//! evaluate a script, and call back into JS.
use v12_api::Context;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut ctx = Context::new();

    // Host function visible to JS as `log(...)`.
    ctx.register_fn("log", |heap, _this, args| {
        let mut out = String::new();
        for (i, a) in args.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            if let Some(s) = <String as v12_api::FromValue>::from_value(heap, *a) {
                out.push_str(&s);
            } else if let Some(n) = <f64 as v12_api::FromValue>::from_value(heap, *a) {
                out.push_str(&n.to_string());
            } else {
                out.push_str("[object]");
            }
        }
        println!("[js] {out}");
        Ok(v12_engine::JsValue::undefined())
    })?;

    // Evaluate a script that calls the host function.
    ctx.eval::<()>(
        "function add(a, b) { return a + b; } \
         log('sum is', add(2, 40));",
    )?;

    // Call back into JS from the host.
    let sum: f64 = ctx.call("add", &[1.5, 2.5])?;
    println!("[host] add(1.5, 2.5) = {sum}");

    Ok(())
}
