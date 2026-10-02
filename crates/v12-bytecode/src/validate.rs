//! Role-directed operand validation — the enforcement half of [`crate::opspec`].
//!
//! # Why this module exists
//!
//! [`crate::opspec`] records what each operand slot *is*. Until now nothing
//! read that record at runtime-of-the-compiler: `FunctionBytecode::validate`
//! checked handler nesting, `max_regs != 0` and handler targets, and a function
//! could be emitted with `SetEnvSlot` naming register 900 in a 4-register frame
//! and validate cleanly. The interpreter then indexed `self.stack[base + 900]`,
//! which is a read or write outside the frame's window — silent for a value in
//! another frame's registers, a memory-safety question for the stack slice.
//!
//! So this is the pass that makes the table *load-bearing*: every instruction
//! is checked against its [`crate::OpSpec`], per slot, and the operand kinds
//! that imply a bound are checked against the function's own declared shape.
//!
//! # What is checked, and what each check does *not* claim
//!
//! | check | rejects | explicitly does not reject |
//! |---|---|---|
//! | register range | a register operand `>= max_regs`, including one widened by a `RegExt` prefix and including the *positional* operands that extend past their base (a `DefineAccessor` pair's setter, a `NewArray` element window) | the operand's role *kind* — see below; a call header window, which needs `CALL_HEADER_REGS` and is W2.4's to expose |
//! | environment slot | a `GetEnvSlot`/`SetEnvSlot` slot at or past the size of an environment **this function allocated** at that depth | any access at a depth naming an inherited environment, and the environment *depth* itself — see "Why there is no depth bound" |
//! | branch target | a `Jump`/`JumpIf*` target past the end of the stream | a target *equal* to the length, which is the implicit `return undefined` completion point |
//! | constant id | a `LoadConst` id past the function's pool (the interpreter panics on this) | a `NameId` past the program's string table, which is not in a `FunctionBytecode` |
//! | flag domain | a `Role::Flag` operand that is not `0` or `1` | anything about a `Count`, `Start` or `Imm8` immediate's *value* |
//! | instruction stream | an unassigned opcode byte at an instruction boundary (every dispatcher panics on it) and a truncated or doubled `Wide`/`RegExt` sequence | a payload word's contents, which are raw data and not instructions |
//!
//! # Why there is no depth bound, and no balance check either
//!
//! The obvious check — "a `SetEnvSlot` depth must not exceed the environment
//! nesting the function allocated" — is **not implementable**, and the reason
//! is worth stating because the plan asks for it by name.
//!
//! A frame's environment chain at any pc is: the environments this function
//! pushed, then the chain the closure captured at its creation point. The
//! second half is not in the `FunctionBytecode`: it lives in the *enclosing*
//! function's environment objects. So a function may legitimately address a
//! depth far past anything it allocated itself. Measured on `3df63e3d` with
//! `v12 --disasm`:
//!
//! ```text
//! function o(){ let a=1; function m(){ let b=2; function inner(){ let c=3;
//!                return ()=>c+b+a; } return inner(); } return m(); }
//! ```
//!
//! compiles `<arrow>4` with **no `new_environment` instruction at all** and
//! `get_env_slot … depth=0`, `depth=1`, `depth=2` in its body — the arrow
//! pushes nothing and reaches three environments up, every one of them
//! allocated by a function it knows nothing about. A `depth <= pushes` gate
//! rejects that, and no fixed bonus is sound either: add another nesting level
//! and the same arrow needs `depth=3`.
//!
//! The related balance check — "no `PopEnvironment` without a live push" — is
//! unsound for a second reason: this walk is **linear in pc order**, and a loop
//! re-pushes on every iteration, so correct bytecode retires more
//! environments than it allocates in pc order. The counterexample is
//! `v12-engine`'s own `nested_for_statements_pop_exactly_the_exited_environments`
//! (labeled `continue outer` in two nested `for (let …)` loops): its `<fn>:f`
//! has three `new_environment` and four `pop_environment` in pc order, and
//! every pop is correctly paired at run time — the pop at pc 85 is the inner
//! loop's exit edge, reached only from inside the loop where that
//! environment is live, and the outer loop re-pushes at pc 14 on the next
//! iteration.
//!
//! Both were written and both were removed rather than kept with an exemption,
//! because an exemption keyed on the opcode would tolerate the same mistake in
//! every *other* function, and a check that has to be switched off for real
//! bytecode is not a gate. What survives is the slot check, which is anchored
//! on the environments this function allocated and **skips** — rather than
//! assumes — any access at a depth naming an inherited one. The linear walk
//! under-counts the live chain exactly in the loop case above (it sees an empty
//! stack where the frame has one), and an under-count only ever *skips* a
//! check, so the slot check cannot mis-fire on it.
//!
//! Making either check decidable needs a control-flow graph, not a walk: a
//! forward dataflow over logical pcs with a max-lattice merge would resolve
//! the loop back-edges that defeat both. That is a real analysis and out of
//! scope here.
//!
//! # The other limit: role kind
//!
//! For the 53 all-register opcodes the disassembler proves the slot *count*,
//! not what each slot is *for*, so a `Key`/`Obj` swap is undetectable from
//! inside `v12-bytecode` — the only other statements of the contract live in
//! `v12-bccompiler`'s emitters and `v12-interp`'s dispatch arms, and depending
//! on either from here would invert the dependency direction the table exists
//! to establish. [`crate::KNOWN_DISASSEMBLER_LABEL_DIVERGENCE`] and the
//! emit-site citations in `opspec.rs` are what those rest on, and
//! `role_kinds_are_citation_backed_for_unlabelled_register_operands` in
//! `tests.rs` keeps the size of that surface visible.
//!
//! What *is* checked for those 53 is everything the role implies about
//! *magnitude*: which slots are registers at all (so a `Count` immediate is
//! never range-checked while a `Key` register is), the window a positional
//! operand reaches, and the immediates a role bounds. That is the honest
//! ceiling, and the table is what draws it: `Call`'s slot `c` is a `Count`
//! and `CallApply`'s is an `ArgsArr` register, so the same slot is checked for
//! one of the eight documented divergences and skipped for the other.

use crate::error::BytecodeError;
use crate::opspec::spec;
use crate::{FunctionBytecode, Opcode, Role, WideOp};

/// A `Role::Flag` operand must be one of these. The flag is a behavioural
/// switch read as `== 0` / `== 1`, not a number: `IteratorClose`'s completion
/// path (`rb == 0` selects the throw path) and `Await`'s async-from-sync form
/// (`rc == 1` selects the extra await). Any other value takes one of the two
/// paths while being neither, so the domain is checked rather than assumed.
const FLAG_VALUES: [u8; 2] = [0, 1];

/// Slot counts of the environments `fb` allocated, innermost last.
///
/// Deliberately excludes the environment a frame *inherits* from its closure's
/// capture point: that object is not described by this `FunctionBytecode`, and
/// counting a guess would make every check that reads this vector a gate that
/// lies. A depth at or past `envs.len()` therefore names an inherited
/// environment and is not checked.
type EnvStack = Vec<u32>;

/// Validates every instruction of `fb` against its [`crate::OpSpec`].
///
/// Called by [`FunctionBytecode::validate`]; not a public entry point of its
/// own, because a partial operand check is exactly the kind of thing a caller
/// might reach for by mistake.
pub(crate) fn check_operand_contract(fb: &FunctionBytecode) -> Result<(), BytecodeError> {
    let instrs = &fb.instrs;
    let max_regs = u32::from(fb.max_regs);
    let end = instrs.len() as u32;
    let mut envs: EnvStack = Vec::new();
    // A `RegExt` prefix defers to the *next* narrow instruction, so the walk
    // has to carry it across one step. `emit_regs` writes the pair atomically,
    // so a second prefix or a prefix followed by a wide op is malformed.
    let mut pending_regext: Option<WideOp> = None;

    let mut pc = 0usize;
    while pc < instrs.len() {
        let at = pc as u32;
        let instr = instrs[pc];
        let Some(op) = instr.op() else {
            return Err(BytecodeError::UnassignedOpcode {
                pc: at,
                byte: (instr.0 >> 24) as u8,
            });
        };

        if op == Opcode::Wide {
            let (wide, width) = WideOp::try_decode(&instrs[pc..])?;
            if let WideOp::RegExt { .. } = wide {
                if pending_regext.is_some() {
                    return Err(BytecodeError::TruncatedWide { word: pc });
                }
                pending_regext = Some(wide);
                pc += width;
                continue;
            }
            if pending_regext.is_some() {
                // `decode_instr` would read the header word as a narrow
                // instruction and merge the high bytes into whatever opcode
                // byte the header's payload happened to leave there.
                return Err(BytecodeError::TruncatedWide { word: pc });
            }
            check_wide(at, wide, fb, &mut envs, max_regs)?;
            pc += width;
            continue;
        }

        let prefix = pending_regext.take();
        let Some(op_spec) = spec(op) else {
            // Unreachable for a decoded opcode: `spec` is populated at exactly
            // the discriminants `TryFrom<u8>` accepts, and `tests.rs` asserts
            // that pairing. Returning rather than asserting keeps a future
            // table edit from turning into a panic on the validation path.
            return Err(BytecodeError::UnassignedOpcode {
                pc: at,
                byte: op as u8,
            });
        };

        // Resolve each slot once, merging a `RegExt` prefix's high bytes into
        // the slots its mask claims. A slot the mask does not claim keeps its
        // narrow byte even if the prefix carries a high byte for it, which is
        // what `Interp::decode_instr` does.
        let bytes = [instr.a(), instr.b(), instr.c()];
        let mut operands = [0u16; 3];
        for (index, byte) in bytes.iter().enumerate() {
            operands[index] = match prefix {
                Some(WideOp::RegExt {
                    mask,
                    a_hi,
                    b_hi,
                    c_hi,
                }) if mask & (1 << index) != 0 => {
                    let hi = match index {
                        0 => a_hi,
                        1 => b_hi,
                        _ => c_hi,
                    };
                    (u16::from(hi) << 8) | u16::from(*byte)
                }
                _ => u16::from(*byte),
            };
        }

        for (index, reg) in operands.iter().enumerate() {
            if !op_spec.index_is_register(index) {
                // `Unused` and every immediate role: deliberately unconstrained.
                // A dead operand is allowed to hold garbage (the table records
                // that it *is* garbage), and an immediate's value is the
                // emitter's business, checked below only where the role itself
                // states a domain (`Flag`).
                continue;
            }
            check_register(at, op, op_spec.role_at_index(index).name(), *reg, max_regs)?;
        }

        check_positional_windows(at, op, &operands, max_regs)?;

        for (index, byte) in bytes.iter().enumerate() {
            if matches!(op_spec.role_at_index(index), Role::Flag) && !FLAG_VALUES.contains(byte) {
                return Err(BytecodeError::FlagNotBoolean {
                    pc: at,
                    op,
                    value: *byte,
                });
            }
        }

        if op == Opcode::LoadConst {
            let id = u32::from(instr.imm16());
            if id as usize >= fb.consts.len() {
                return Err(BytecodeError::ConstIdOutOfRange {
                    pc: at,
                    op,
                    id,
                    pool: fb.consts.len(),
                });
            }
        }

        let target = match op {
            Opcode::Jump => Some(instr.imm24()),
            Opcode::JumpIfFalse | Opcode::JumpIfTrue | Opcode::JumpIfNullish => {
                Some(u32::from(instr.imm16()))
            }
            _ => None,
        };
        if let Some(target) = target.filter(|&t| t > end) {
            return Err(BytecodeError::BranchTargetOutOfRange {
                pc: at,
                op,
                target,
                instrs: instrs.len(),
            });
        }

        // Environment allocation and access, read from the table rather than
        // from hardcoded slot indices: `NewEnvironment`'s slot count and
        // `GetEnvSlot`/`SetEnvSlot`'s (depth, slot) pair are located by role,
        // so the check follows the table if W2.4 moves a divergence instead of
        // silently checking the operands a transposed emitter would no longer
        // write.
        if op == Opcode::NewEnvironment
            && let Some(count) = role_slot(op_spec, Role::Slot)
        {
            envs.push(u32::from(operands[count]));
        }
        // `PopEnvironment` retires the innermost allocation. No balance check:
        // see the module docs.
        if op == Opcode::PopEnvironment {
            envs.pop();
        }
        if let (Some(depth_at), Some(slot_at)) = (
            role_slot(op_spec, Role::Depth),
            role_slot(op_spec, Role::Slot),
        ) {
            // `GetEnvSlot` is (dst, depth, slot) and `SetEnvSlot` is
            // (depth, slot, src) — the first of the eight documented
            // operand-order divergences, recorded rather than normalised. Both
            // are checked in the order they are emitted today, which is the
            // point: a half-moved normalisation moves the table and not the
            // emitters, and this reads the operands where the *table* says they
            // are, so it fails.
            check_env_access(
                at,
                op,
                u32::from(operands[depth_at]),
                u32::from(operands[slot_at]),
                &envs,
            )?;
        }

        pc += 1;
    }

    if pending_regext.is_some() {
        // A prefix with no narrow instruction to extend: the interpreter
        // panics on exactly this shape.
        return Err(BytecodeError::TruncatedWide { word: instrs.len() });
    }
    Ok(())
}

/// The wide-op half of [`check_operand_contract`]: the same register and
/// constant-id bounds, read from the decoded payload instead of byte slots.
fn check_wide(
    at: u32,
    wide: WideOp,
    fb: &FunctionBytecode,
    envs: &mut EnvStack,
    max_regs: u32,
) -> Result<(), BytecodeError> {
    // A wide op's operands are `u16` fields, so a `RegExt` prefix is never
    // involved and no merging is needed.
    let mut failure = None;
    wide.each_register(|name, reg| {
        if failure.is_none() && u32::from(reg) >= max_regs {
            failure = Some(BytecodeError::RegisterOutOfRange {
                pc: at,
                op: Opcode::Wide,
                operand: format!("{name} (wide)"),
                reg: u32::from(reg),
                max_regs,
            });
        }
    });
    if let Some(err) = failure {
        return Err(err);
    }

    if let WideOp::LoadConstW { const_id, .. } = wide
        && const_id as usize >= fb.consts.len()
    {
        return Err(BytecodeError::ConstIdOutOfRange {
            pc: at,
            op: Opcode::Wide,
            id: const_id,
            pool: fb.consts.len(),
        });
    }
    if let Some(slots) = wide.env_allocation() {
        envs.push(slots);
    }
    if let Some((depth, slot)) = wide.env_access() {
        check_env_access(at, Opcode::Wide, depth, slot, envs)?;
    }
    Ok(())
}

/// Index of the one operand slot holding `role`, if the opcode has it.
///
/// Two slots can share a role ([`Role::Imm16`] spans `b`/`c`,
/// [`Role::Imm24`] all three), so this returns the *first* and callers must
/// only ask for roles that occur once — `Depth` and `Slot` both do.
fn role_slot(spec: crate::OpSpec, role: Role) -> Option<usize> {
    (0..3).find(|&i| spec.role_at_index(i) == role)
}

/// Rejects a register at or past the frame's register file.
fn check_register(
    at: u32,
    op: Opcode,
    operand: &str,
    reg: u16,
    max_regs: u32,
) -> Result<(), BytecodeError> {
    if u32::from(reg) < max_regs {
        return Ok(());
    }
    Err(BytecodeError::RegisterOutOfRange {
        pc: at,
        op,
        operand: operand.to_string(),
        reg: u32::from(reg),
        max_regs,
    })
}

/// The positional operands that reach *past* their base register.
///
/// The table names the base; the operand count is what makes it a window. Both
/// are positional rather than stored, so neither is visible in the role — they
/// are read from the instruction the roles belong to.
///
/// * `NewArray` reads the contiguous element window `[b, b + c)`.
/// * `DefineAccessor` reads a packed `(getter, setter)` pair, so the setter at
///   `c + 1` is read too.
///
/// The `Call`/`Construct` header window (`[callee][this][args…]`, see
/// [`crate::Role::Callee`]) is **not** checked: its size is
/// `CALL_HEADER_REGS + argc`, and that constant lives in `v12-bccompiler`.
/// Duplicating it here to make the check possible would create a second
/// statement of the call ABI that nothing cross-checks — the exact hazard this
/// crate's table exists to remove. W2.4 routes `call_setup.rs` through the one
/// `CALL_HEADER_REGS`; the check becomes writable in the same change.
fn check_positional_windows(
    at: u32,
    op: Opcode,
    operands: &[u16; 3],
    max_regs: u32,
) -> Result<(), BytecodeError> {
    let (base, count, name) = match op {
        // `b` = element base, `c` = element count.
        Opcode::NewArray => (operands[1], operands[2], "element window b + c"),
        // `c` = pair base; the setter sits at `c + 1`.
        Opcode::DefineAccessor => (operands[2], 1, "accessor pair base + 1"),
        _ => return Ok(()),
    };
    let last = u32::from(base) + u32::from(count);
    if last < max_regs {
        return Ok(());
    }
    Err(BytecodeError::RegisterOutOfRange {
        pc: at,
        op,
        operand: name.to_string(),
        reg: last,
        max_regs,
    })
}

/// Rejects an environment slot at or past the size of the environment it names.
///
/// Skipped, and *not* assumed sound, when `depth` names an environment this
/// function did not allocate: the inherited chain is invisible here, so there
/// is nothing to compare the slot against. See the module docs.
fn check_env_access(
    at: u32,
    op: Opcode,
    depth: u32,
    slot: u32,
    envs: &[u32],
) -> Result<(), BytecodeError> {
    let depth = depth as usize;
    let Some(from_end) = depth.checked_add(1).and_then(|d| envs.len().checked_sub(d)) else {
        return Ok(());
    };
    let env_slots = envs[from_end];
    if slot < env_slots {
        return Ok(());
    }
    Err(BytecodeError::EnvSlotOutOfRange {
        pc: at,
        op,
        depth: depth as u32,
        slot,
        env_slots,
    })
}
