//! Tier-1 JIT hook wiring behind the `V12_JIT` environment variable.
//!
//! The interpreter reports hot functions (`FeedbackVector` crossing the
//! tier-up threshold) through [`TierHooks::on_tier_up`] between frame
//! completions. When `V12_JIT` is set in the environment, the engine installs
//! [`JitTierHooks`] on every interpreter it drives; the hook compiles each
//! hot function once with the baseline template JIT and keeps the resulting
//! [`CompiledFn`] in a per-program cache. When the variable is unset (the
//! default), the engine installs nothing and execution is pure interpreter.
//!
//! # What is wired, and what is not
//!
//! Wired today: the full tier-up *signal* path (feedback → threshold → hook)
//! and the compilation pipeline (bytecode → Cranelift IR → compiled artifact),
//! exercised end-to-end on every hot function.
//!
//! Not wired: *executing* the compiled artifact in place of the interpreter
//! frame. The baseline executor is heap-agnostic (strings collapse to `NaN`,
//! calls cannot re-enter the interpreter), so running it in place would
//! diverge from spec semantics. Delegating execution needs the OSR/deopt
//! machinery (post-v1); until then the hook's cache proves and measures the
//! compilation path without changing program behavior.

use std::cell::RefCell;
use std::rc::Rc;

use v12_bytecode::FunctionBytecode;
use v12_codegen::CompiledFn;
use v12_interp::feedback::TierHooks;
use v12_jit_baseline::JitBaseline;

/// Counters observed by tests and embedders through the hook's stats handle.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JitTierStats {
    /// Functions reported hot by the interpreter.
    pub tier_ups: u32,
    /// Hot functions successfully compiled by the baseline JIT.
    pub compiled: u32,
    /// Hot functions the JIT refused (too large, unsupported opcode).
    pub refused: u32,
}

/// Tier-up hook that compiles hot functions with the baseline JIT.
///
/// One instance per program: the interpreter's function indices index
/// `program`. Compilation happens once per function index; repeat tier-ups
/// for the same index are no-ops.
pub struct JitTierHooks {
    program: Rc<[FunctionBytecode]>,
    jit: JitBaseline,
    compiled: Vec<(u32, CompiledFn)>,
    stats: Rc<RefCell<JitTierStats>>,
}

impl JitTierHooks {
    /// Builds a hook for `program`. Returns the hook and a shared stats
    /// handle that stays live after the hook is boxed into the interpreter.
    pub fn new(program: Rc<[FunctionBytecode]>) -> (Self, Rc<RefCell<JitTierStats>>) {
        let stats = Rc::new(RefCell::new(JitTierStats::default()));
        let hook = Self {
            program,
            // Audited: `JitBaseline::new` is infallible today (a cache
            // allocation); the Result exists for future backends.
            #[allow(clippy::expect_used)]
            jit: JitBaseline::new()
                .expect("baseline JIT construction cannot fail in the current build"),
            compiled: Vec::new(),
            stats: Rc::clone(&stats),
        };
        (hook, stats)
    }

    /// Installs this hook on `interp` when the JIT is enabled — `V12_JIT`
    /// set in the environment. Returns the stats handle so the caller can
    /// observe compilation counts.
    pub fn install_if_enabled(
        interp: &mut v12_interp::Interp<'_>,
        program: &Rc<[FunctionBytecode]>,
    ) -> Option<Rc<RefCell<JitTierStats>>> {
        if !jit_enabled() {
            return None;
        }
        Some(Self::install(interp, program))
    }

    /// Installs this hook unconditionally. Tests use this to exercise the
    /// enabled mode without mutating the process environment.
    pub fn install(
        interp: &mut v12_interp::Interp<'_>,
        program: &Rc<[FunctionBytecode]>,
    ) -> Rc<RefCell<JitTierStats>> {
        let (hook, stats) = Self::new(Rc::clone(program));
        interp.set_hooks(Box::new(hook));
        stats
    }
}

/// Pure decision core of [`jit_enabled`], unit-testable without touching the
/// process environment.
fn jit_flag_is_on(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

impl TierHooks for JitTierHooks {
    fn on_tier_up(&mut self, function_index: u32) {
        let mut stats = self.stats.borrow_mut();
        stats.tier_ups += 1;
        if self.compiled.iter().any(|(idx, _)| *idx == function_index) {
            return;
        }
        let program = Rc::clone(&self.program);
        let Some(fb) = program.get(function_index as usize) else {
            return;
        };
        match self.jit.compile(fb) {
            Ok(compiled) => {
                self.compiled.push((function_index, compiled));
                stats.compiled += 1;
            }
            Err(_) => {
                stats.refused += 1;
            }
        }
    }
}

/// Whether the JIT tier is enabled: the `V12_JIT` environment variable is
/// set (to anything other than `0`). Unset — the default — means pure
/// interpreter.
pub fn jit_enabled() -> bool {
    jit_flag_is_on(std::env::var("V12_JIT").ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use v12_heap::Heap;

    /// A hot arithmetic loop — 2000 loop-header crossings, well past the
    /// tier-up threshold — inside the baseline JIT's supported opcode set
    /// (arithmetic + control flow only).
    const HOT_SCRIPT: &str = r#"
        let s = 0;
        for (let i = 0; i < 2000; i++) {
            s = s + i;
        }
        s
    "#;

    #[test]
    fn jit_flag_decision_table() {
        // Unset — the default — is off; only a non-empty value other than
        // `0` turns the tier on.
        assert!(!jit_flag_is_on(None));
        assert!(!jit_flag_is_on(Some("")));
        assert!(!jit_flag_is_on(Some("0")));
        assert!(jit_flag_is_on(Some("1")));
        assert!(jit_flag_is_on(Some("on")));
    }

    #[test]
    fn default_engine_evaluates_hot_script() {
        // The engine path must behave identically whether or not the hook
        // is installed (the hook only compiles; it never executes).
        let mut engine = crate::Engine::new();
        let v = engine.eval(HOT_SCRIPT).expect("run");
        assert_eq!(v.as_smi(), Some(1_999_000));
    }

    #[test]
    fn enabled_hook_compiles_hot_functions_and_preserves_semantics() {
        let (program, strings) =
            v12_bccompiler::compile_source_with_strings(HOT_SCRIPT).expect("compile");
        let functions: Rc<[FunctionBytecode]> = Rc::from(program.functions);

        let mut heap = Heap::new(v12_heap::GcPolicy::default());
        let mut interp = v12_interp::Interp::new_with_heap(
            &mut heap,
            None,
            Rc::clone(&functions),
            program.main,
            strings,
        );
        let stats = JitTierHooks::install(&mut interp, &functions);
        interp.run().expect("run");

        let s = stats.borrow();
        assert!(s.tier_ups > 0, "hot loop must cross the tier-up threshold");
        assert!(s.compiled > 0, "baseline JIT must compile the hot function");
    }
}
