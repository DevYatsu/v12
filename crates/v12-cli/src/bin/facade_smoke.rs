#![forbid(unsafe_code)]

//! Smoke test for the v12-api facade (ADR-005).
//!
//! `cargo run -p v12-cli --bin facade-smoke` evaluates a one-liner
//! through the facade and prints the result. The CLI's main path still
//! uses `v12-engine` for full feature parity (disassembly, GC stress
//! mode); the facade is the recommended path for *new* embedders.

use v12_api::{Context, V12Error};

fn main() {
    let mut ctx = Context::new();
    match ctx.eval::<()>(&std::env::args().nth(1).unwrap_or_else(|| "1+1".into())) {
        Ok(()) => println!("ok"),
        Err(V12Error::Compile(m)) => {
            eprintln!("compile error: {m}");
            std::process::exit(1);
        }
        Err(V12Error::Thrown(m)) => {
            eprintln!("uncaught: {m}");
            std::process::exit(1);
        }
        Err(V12Error::Host(m)) => {
            eprintln!("host error: {m}");
            std::process::exit(1);
        }
    }
    let drained = ctx.pump();
    if drained > 0 {
        println!("drained {drained} microtasks");
    }
}
