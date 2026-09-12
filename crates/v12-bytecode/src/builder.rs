//! Stage 4: backpatching builder — labels, fixups, and `FunctionBuilder`.

use crate::{BytecodeError, Const, ConstantPool, FunctionBytecode, HandlerRange,
    Instr, Opcode, SpanPair};

// ---------------------------------------------------------------------------
// Stage 4: backpatching builder
// ---------------------------------------------------------------------------

/// A reference to a future instruction index.
///
/// Opaque by design: the builder owns pc assignment, so callers cannot hold
/// stale indices across emissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Label(u32);

/// One unresolved branch awaiting [`FunctionBuilder::bind`].
#[derive(Debug)]
struct Fixup {
    /// Index of the branch instruction to patch.
    at: u32,
    label: Label,
    /// `true` for `Jump` (24-bit target); conditional branches share their
    /// word with the condition register and carry a 16-bit target.
    wide_target: bool,
}

/// Incremental [`FunctionBytecode`] constructor with label backpatching.
///
/// Labels are allocated up front ([`FunctionBuilder::label`]), bound to "the
/// next emitted instruction" ([`FunctionBuilder::bind`]), and every branch
/// referencing them is rewritten in place during
/// [`FunctionBuilder::finish`] — the classic backpatching scheme, which lets
/// forward branches be emitted before their targets exist.
#[derive(Debug, Default)]
pub struct FunctionBuilder {
    name_hint: Option<String>,
    max_regs: u16,
    instrs: Vec<Instr>,
    spans: Vec<SpanPair>,
    consts: ConstantPool,
    handlers: Vec<HandlerRange>,
    labels: Vec<Option<u32>>,
    fixups: Vec<Fixup>,
    pub is_generator: bool,
    pub is_async: bool,
    pub is_arrow: bool,
}

impl FunctionBuilder {
    pub fn new(name_hint: Option<&str>) -> Self {
        Self {
            name_hint: name_hint.map(str::to_string),
            // Start at 1 so `validate`'s `max_regs > 0` holds for leaf thunks.
            max_regs: 1,
            ..Self::default()
        }
    }

    /// Grows the register budget; pass one more than the highest register
    /// index your emitted code touches.
    pub fn reserve_regs(&mut self, count: u16) {
        self.max_regs = self.max_regs.max(count);
    }

    /// Allocates an unbound label.
    pub fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() as u32 - 1)
    }

    /// Binds `label` to the index of the *next* emitted instruction.
    ///
    /// Panics on double binding: that is always a codegen bug, never a
    /// runtime condition.
    pub fn bind(&mut self, label: Label) {
        let slot = &mut self.labels[label.0 as usize];
        assert!(slot.is_none(), "label {label:?} bound more than once");
        *slot = Some(self.instrs.len() as u32);
    }

    /// Current emission pc; use it to compute [`HandlerRange`] bounds for
    /// [`FunctionBuilder::push_handler`].
    pub fn pc(&self) -> u32 {
        self.instrs.len() as u32
    }

    pub fn emit(&mut self, instr: Instr) {
        self.emit_spanned(instr, (0, 0));
    }

    /// Emits with a source span, keeping `spans` index-aligned with `instrs`.
    pub fn emit_spanned(&mut self, instr: Instr, span: SpanPair) {
        self.instrs.push(instr);
        self.spans.push(span);
    }

    /// Interns `constant`, deduplicating like [`ConstantPool::insert`].
    pub fn add_const(&mut self, constant: Const) -> Result<u16, BytecodeError> {
        self.consts.insert(constant)
    }

    pub fn push_handler(&mut self, handler: HandlerRange) {
        self.handlers.push(handler);
    }

    /// Emits a branch to `target`. Unconditional `Jump` stores a 24-bit
    /// absolute target; conditional branches spend their remaining slots on
    /// `cond_reg` and carry a 16-bit absolute target instead.
    pub fn emit_jump(&mut self, op: Opcode, cond_reg: u8, target: Label) {
        debug_assert!(
            matches!(op, Opcode::Jump | Opcode::JumpIfFalse | Opcode::JumpIfTrue),
            "emit_jump expects a branching opcode"
        );
        let wide_target = op == Opcode::Jump;
        let instr = if wide_target {
            Instr::new_imm24(op, 0)
        } else {
            Instr::new_imm16(op, cond_reg, 0)
        };
        let at = self.instrs.len() as u32;
        self.emit(instr);
        self.fixups.push(Fixup {
            at,
            label: target,
            wide_target,
        });
    }

    /// Resolves every label and assembles the final bytecode.
    ///
    /// Panics if any label was never bound: an unresolved label means some
    /// branch points at nothing, which is always a codegen bug rather than
    /// a runtime condition worth recovering from.
    pub fn finish(mut self) -> FunctionBytecode {
        let unbound = self.labels.iter().filter(|slot| slot.is_none()).count();
        assert!(unbound == 0, "{unbound} label(s) created but never bound");

        let fixups = std::mem::take(&mut self.fixups);
        for fixup in fixups {
            // Every label is bound by the check above, so this lookup always
            // resolves; the `if let` merely avoids an unwrap.
            if let Some(target_pc) = self.labels[fixup.label.0 as usize] {
                let instr = &mut self.instrs[fixup.at as usize];
                if fixup.wide_target {
                    instr.set_imm24(target_pc);
                } else {
                    debug_assert!(
                        target_pc <= u32::from(u16::MAX),
                        "conditional branch target {target_pc} exceeds the 16-bit window"
                    );
                    instr.set_imm16(target_pc as u16);
                }
            }
        }
        debug_assert_eq!(self.spans.len(), self.instrs.len());

        FunctionBytecode {
            name_hint: self.name_hint,
            function_name: None,
            max_regs: self.max_regs,
            instrs: self.instrs,
            consts: self.consts,
            handlers: self.handlers,
            spans: self.spans,
            pc_map: Vec::new(),
            is_strict: false,
            fixed_params: 0,
            has_rest: false,
            rest_reg: 0,
            expected_args: 0,
            needs_arguments: false,
            is_generator: self.is_generator,
            is_async: self.is_async,
            is_arrow: self.is_arrow,
        }
    }
}

#[cfg(test)]
mod builder_tests {
    use super::*;

    #[test]
    fn forward_jump_patches_to_bound_pc() {
        let mut b = FunctionBuilder::new(None);
        let done = b.label();
        b.emit_jump(Opcode::JumpIfFalse, 0, done);
        b.emit(Instr::new(Opcode::Move, 1, 2, 0));
        b.bind(done);
        b.emit(Instr::new(Opcode::Return, 0, 0, 0));

        let fb = b.finish();
        assert_eq!(fb.instrs[0].op(), Some(Opcode::JumpIfFalse));
        assert_eq!(fb.instrs[0].a(), 0);
        assert_eq!(fb.instrs[0].imm16(), 2, "forward target is the Move's pc");
    }

    #[test]
    fn backward_jump_patches_to_loop_head() {
        let mut b = FunctionBuilder::new(None);
        let top = b.label();
        b.bind(top); // loop head at pc 0
        b.emit(Instr::new_imm24(Opcode::LoopHeader, 0));
        b.emit(Instr::new(Opcode::Add, 0, 0, 1));
        b.emit_jump(Opcode::JumpIfTrue, 2, top);

        let fb = b.finish();
        assert_eq!(fb.instrs[2].imm16(), 0, "backward target is the loop head");
    }

    #[test]
    fn unconditional_jump_uses_full_imm24_target() {
        let mut b = FunctionBuilder::new(None);
        let end = b.label();
        b.emit_jump(Opcode::Jump, 0, end);
        b.emit(Instr::new(Opcode::Move, 0, 0, 0));
        b.bind(end);

        let fb = b.finish();
        assert_eq!(fb.instrs[0].imm24(), 2, "end binds after the skipped Move");
    }

    #[test]
    fn diamond_control_flow_resolves_and_validates() {
        // if (r0) { r1 = 1 } else { r1 = 2 }; return r1
        let mut b = FunctionBuilder::new(Some("diamond"));
        b.reserve_regs(2);
        let else_l = b.label();
        let end_l = b.label();
        b.emit_jump(Opcode::JumpIfFalse, 0, else_l); // 0
        b.emit(Instr::new(Opcode::LoadInt, 1, 0, 1)); // 1: then
        b.emit_jump(Opcode::Jump, 0, end_l); // 2
        b.bind(else_l);
        b.emit(Instr::new(Opcode::LoadInt, 1, 0, 2)); // 3: else
        b.bind(end_l);
        b.emit(Instr::new(Opcode::Return, 1, 0, 0)); // 4

        let fb = b.finish();
        assert_eq!(fb.instrs[0].imm16(), 3, "false-branch lands on else");
        assert_eq!(fb.instrs[2].imm24(), 4, "then-branch skips the else");
        assert_eq!(fb.instrs.len(), 5);
        assert_eq!(fb.name_hint.as_deref(), Some("diamond"));
        assert!(fb.validate().is_ok());

        let text = format!("{fb}");
        for needle in [
            "jump_if_false r0, -> 3",
            "load_int r1, #1",
            "jump -> 4",
            "return r1",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    #[should_panic(expected = "never bound")]
    fn finish_panics_on_unbound_label() {
        let mut b = FunctionBuilder::new(None);
        let nowhere = b.label();
        b.emit_jump(Opcode::Jump, 0, nowhere);
        b.finish();
    }

    #[test]
    #[should_panic(expected = "bound more than once")]
    fn bind_twice_panics() {
        let mut b = FunctionBuilder::new(None);
        let l = b.label();
        b.bind(l);
        b.bind(l);
    }

    #[test]
    fn builder_consts_share_indices() {
        let mut b = FunctionBuilder::new(None);
        let k1 = b.add_const(Const::BigU64(7)).unwrap();
        let k2 = b.add_const(Const::BigU64(7)).unwrap();
        let k3 = b.add_const(Const::Str32(1)).unwrap();
        assert_eq!(k1, k2);
        assert_ne!(k1, k3);
    }

    #[test]
    fn emit_spanned_keeps_spans_parallel() {
        let mut b = FunctionBuilder::new(None);
        b.emit(Instr::new(Opcode::Move, 0, 1, 0));
        b.emit_spanned(Instr::new(Opcode::Return, 0, 0, 0), (10, 20));

        let fb = b.finish();
        assert_eq!(fb.spans, vec![(0, 0), (10, 20)]);
        assert_eq!(fb.spans.len(), fb.instrs.len());
    }

    #[test]
    fn handler_pushed_via_builder_validates() {
        let mut b = FunctionBuilder::new(Some("try"));
        b.reserve_regs(3);
        let start = b.pc(); // 0
        b.emit(Instr::new(Opcode::NewObject, 0, 0, 0)); // may throw
        let end = b.pc(); // 1
        b.emit(Instr::new(Opcode::Return, 0, 0, 0));
        b.push_handler(HandlerRange {
            start,
            end,
            target: 2,
            stack_depth: 1,
        });
        b.emit(Instr::new(Opcode::Throw, 0, 0, 0)); // handler target at pc 2

        let fb = b.finish();
        assert_eq!(fb.handlers.len(), 1);
        assert!(fb.validate().is_ok());
    }
}
