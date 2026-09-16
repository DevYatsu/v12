//! The dispatch loop: `Interp::execute` and its decode helpers.
//!
//! Split out of the interpreter root by concern; the run loop is the
//! hottest and largest single function in the engine.

use std::time::Instant;

use v12_bytecode::{BytecodeError, Const, Instr, Opcode, WideOp};
use v12_heap::{Handle, JsObject, JsValue, Kind, V12Str};

use super::{CallOutcome, DEADLINE_CHECK_INTERVAL, Interp, JSException, env_display_push};
use crate::feedback::{Lattice, TYPE_NAMES};
use crate::generator::Suspendable;
use crate::ops;

/// Decodes the instruction at `pc` into
/// `(op, a, b, c, word_width, narrow_word)`.
///
/// - Narrow op: operands are the instruction's own byte slots, width 1.
/// - `Wide` header with discriminant [`WideOp::DISC_REG_EXT`]: merges the
///   prefix's high bytes into the following narrow instruction's register
///   slots per the prefix mask; header + payload + narrow instruction
///   execute as one logical op of width 3.
/// - Any other `Wide` header: returned as [`Opcode::Wide`] (width 1) so
///   the wide arm decodes the payload itself.
///
/// The returned `narrow_word` is the instruction whose *immediates*
/// (imm16/imm24, unmerged byte slots) apply; it equals the header word
/// for plain narrow ops and the third word for `RegExt` pairs.
pub(crate) fn decode_instr(instrs: &[Instr], pc: usize) -> (Opcode, u16, u16, u16, usize, Instr) {
    let instr = instrs[pc];
    let Some(op) = instr.op() else {
        panic!("corrupt bytecode: unassigned opcode byte at pc {pc}");
    };
    if op != Opcode::Wide {
        return (
            op,
            u16::from(instr.a()),
            u16::from(instr.b()),
            u16::from(instr.c()),
            1,
            instr,
        );
    }
    if u32::from(instr.c()) == WideOp::DISC_REG_EXT {
        let (wide, _) = WideOp::try_decode(&instrs[pc..]).expect("malformed wide opcode sequence");
        let WideOp::RegExt {
            mask,
            a_hi,
            b_hi,
            c_hi,
        } = wide
        else {
            unreachable!("discriminant matched REG_EXT")
        };
        let narrow = instrs.get(pc + 2).copied().unwrap_or_else(|| {
            panic!("corrupt bytecode: RegExt prefix at pc {pc} without its narrow instruction")
        });
        let narrow_op = narrow.op().unwrap_or_else(|| {
            panic!(
                "corrupt bytecode: RegExt prefix at pc {pc} not followed by a narrow instruction"
            )
        });
        let merge = |slot: u8, hi: u8, bit: u8| -> u16 {
            if mask & bit != 0 {
                (u16::from(hi) << 8) | u16::from(slot)
            } else {
                u16::from(slot)
            }
        };
        return (
            narrow_op,
            merge(narrow.a(), a_hi, 1),
            merge(narrow.b(), b_hi, 2),
            merge(narrow.c(), c_hi, 4),
            3,
            narrow,
        );
    }
    (Opcode::Wide, 0, 0, 0, 1, instr)
}

/// Destination register and total word width of the Call/Construct header
/// parked at `pc` while a callee frame runs, plus whether it is a
/// `Construct` (which needs the spec's return-value adjustment).
///
/// Handles the narrow forms, the wide `CallW`/`ConstructW` escapes, and the
/// `RegExt`-prefixed narrow form (parked pc is the prefix header).
///
/// Returns `Err` on corrupt bytecode instead of panicking — callers turn it
/// into a JS `TypeError` (or, for the Await resume path, fall back to
/// undefined advancement) rather than unwinding the native stack.
pub(crate) fn decode_parked_call(
    instrs: &[Instr],
    pc: usize,
) -> Result<(bool, u16, usize), BytecodeError> {
    let instr = instrs
        .get(pc)
        .copied()
        .ok_or(BytecodeError::TruncatedWide { word: pc })?;
    match instr.op() {
        Some(Opcode::Wide) => match WideOp::try_decode(&instrs[pc..]) {
            Ok((WideOp::CallW { dst, .. }, width)) => Ok((false, dst, width)),
            Ok((WideOp::ConstructW { dst, .. }, width)) => Ok((true, dst, width)),
            Ok((WideOp::RegExt { mask, a_hi, .. }, _)) => {
                // The narrow call/construct follows the 2-word prefix.
                let narrow = instrs
                    .get(pc + 2)
                    .copied()
                    .ok_or(BytecodeError::InvalidFunction {
                        reason: "RegExt prefix without its narrow instruction".to_string(),
                    })?;
                let dst = if mask & 1 != 0 {
                    (u16::from(a_hi) << 8) | u16::from(narrow.a())
                } else {
                    u16::from(narrow.a())
                };
                let is_construct = narrow.op() == Some(Opcode::Construct);
                Ok((is_construct, dst, 3))
            }
            other => Err(BytecodeError::InvalidFunction {
                reason: format!("call parked on malformed wide header: {other:?}"),
            }),
        },
        _ => Ok((
            instr.op() == Some(Opcode::Construct),
            u16::from(instr.a()),
            1,
        )),
    }
}

/// The Tier-0 interpreter over one compiled program's bytecode.
///
/// Deliberately decoupled from the compiler crate: callers pass the
/// function table, the entry index, and the string table that
/// `Const::Str32` ids resolve through (as produced by
/// `v12_bccompiler::compile_source_with_strings`).
impl Interp<'_> {
    /// The single iterative dispatch loop. Each arm either advances the
    /// current frame's pc, redirects it, pushes/pops frames, or raises a
    /// pending exception for delivery at the top of the next iteration.
    pub(crate) fn execute(&mut self) -> Result<(), JSException> {
        // Cross-boundary canonical-form guard: every value on the stack must
        // be canonical (spare bits zero, assigned tag) at dispatch entry.
        // Forged words from embedders/JIT helpers fail here in debug builds
        // instead of corrupting type predicates downstream.
        debug_assert!(
            self.stack.iter().all(|v| v.is_canonical()),
            "non-canonical value on the stack at dispatch entry"
        );
        // Thrown value awaiting delivery to a handler (or escape).
        let mut pending: Option<JsValue> = None;

        'drive: loop {
            if let Some(exc) = pending.take() {
                // Either lands in a handler (pc rewritten, value delivered)
                // or pops frames; escaping the bottom frame ends the run.
                self.unwind(exc)?;
            }

            // Cooperative deadline: sample the wall clock periodically so a
            // runaway bytecode loop (no IO/await to yield on) cannot block the
            // host. Force-returns the timeout out of `execute` so it escapes
            // every user `try/catch` (they all live in this same loop), rather
            // than being swallowed and letting the loop resume its spin.
            self.deadline_ticks = self.deadline_ticks.wrapping_add(1);
            if (self.deadline_ticks & (DEADLINE_CHECK_INTERVAL - 1)) == 0
                && let Some(dl) = self.deadline
                && Instant::now() >= dl
            {
                self.deadline_exceeded = true;
                return Err(JSException(
                    self.error_value("ScriptRuntimeError: execution deadline exceeded"),
                ));
            }

            // Snapshot hot frame state: arms call back into `self` and must
            // not hold borrows across those calls. Resolve the frame's
            // function table (its program) once per iteration.
            let (fn_idx, program, pc, base, max_regs) = {
                let f = self.frames.last().expect("execute() requires a frame");
                (f.fn_idx, f.program, f.pc, f.base, f.max_regs)
            };
            let funcs = self.functions_for_program(program);

            // Falling off the instruction stream is implicit `return
            // undefined` (the documented completion ABI).
            let Some(&instr) = funcs[fn_idx as usize].instrs.get(pc) else {
                if self.complete_frame(JsValue::undefined())? {
                    return Ok(());
                }
                continue 'drive;
            };
            let Some(_op) = instr.op() else {
                panic!("corrupt bytecode: unassigned opcode byte at {fn_idx}:{pc}");
            };
            // Decode operands: narrow ops expose their byte slots directly;
            // a `RegExt` prefix merges high bytes into the following narrow
            // instruction's register slots and executes as one 3-word op
            // (see `WideOp::RegExt`).
            let (op, ra, rb, rc, op_width, narrow) = {
                let instrs = &funcs[fn_idx as usize].instrs;
                decode_instr(instrs, pc)
            };

            macro_rules! throw_js {
                ($v:expr) => {{
                    pending = Some($v);
                    continue 'drive;
                }};
            }
            macro_rules! attempt {
                ($e:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(JSException(v)) => throw_js!(v),
                    }
                };
            }

            match op {
                // ------------------------------------------------------
                // Data movement and constants
                // ------------------------------------------------------
                Opcode::Move => {
                    self.stack[base + usize::from(ra)] = self.stack[base + usize::from(rb)];
                    self.set_pc(pc + op_width);
                }
                Opcode::LoadInt => {
                    let v = i8::from_be_bytes([narrow.c()]);
                    self.stack[base + usize::from(ra)] = ops::box_number(f64::from(v));
                    self.set_pc(pc + op_width);
                }
                Opcode::LoadConst => {
                    let value =
                        attempt!(self.const_value(fn_idx, u32::from(narrow.imm16()), program));
                    self.stack[base + usize::from(ra)] = value;
                    self.set_pc(pc + op_width);
                }
                Opcode::Wide => {
                    let words = &funcs[fn_idx as usize].instrs[pc..];
                    let (wide, width) =
                        WideOp::try_decode(words).expect("malformed wide opcode sequence");
                    match wide {
                        WideOp::LoadIntW { dst, value } => {
                            // i64 → f64 is lossy past 2⁵³; identical to the
                            // reference behavior for the constants emitted.
                            self.stack[base + usize::from(dst)] = ops::box_number(value as f64);
                        }
                        WideOp::LoadConstW { dst, const_id } => {
                            let value = attempt!(self.const_value(fn_idx, const_id, program));
                            self.stack[base + usize::from(dst)] = value;
                        }
                        WideOp::GetEnvSlotW { dst, depth, slot } => {
                            let v = attempt!(self.env_read(depth, slot));
                            self.stack[base + usize::from(dst)] = v;
                        }
                        WideOp::SetEnvSlotW { src, depth, slot } => {
                            let v = self.stack[base + usize::from(src)];
                            attempt!(self.env_write(depth, slot, v));
                        }
                        WideOp::CallW { dst, func, argc } => {
                            match attempt!(self.prepare_call(base, max_regs, func, argc)) {
                                CallOutcome::Pushed => continue 'drive,
                                CallOutcome::Value(v) => {
                                    let caller_base = base;
                                    self.stack[caller_base + usize::from(dst)] = v;
                                    self.set_pc(pc + width);
                                }
                            }
                            continue 'drive;
                        }
                        WideOp::ConstructW { dst, func, argc } => {
                            match attempt!(self.prepare_construct(base, max_regs, func, argc)) {
                                CallOutcome::Pushed => continue 'drive,
                                CallOutcome::Value(v) => {
                                    let caller_base = base;
                                    self.stack[caller_base + usize::from(dst)] = v;
                                    self.set_pc(pc + width);
                                }
                            }
                            continue 'drive;
                        }
                        WideOp::ClosureW {
                            dst,
                            function_index,
                        } => {
                            self.gc_protect();
                            let env = self.frames.last().expect("frame").env;
                            let h = self.alloc_closure(function_index.into(), env);
                            self.stack[base + usize::from(dst)] = JsValue::object(h);
                        }
                        WideOp::NewEnvironmentW { depth: _, slots } => {
                            // The static `depth` operand duplicates the
                            // dynamic parent chain (see crate docs); only the
                            // slot count matters here, matching the narrow op.
                            self.gc_protect();
                            let parent = self.frames.last().expect("frame").env;
                            let h = self
                                .heap
                                .alloc(JsObject::environment(usize::from(slots), parent));
                            // Display head-shift (O(1)): the new head's
                            // parent is the old head by construction.
                            let frame = self.frames.last_mut().expect("frame");
                            env_display_push(&mut frame.env_display, h);
                            frame.env = Some(h);
                        }
                        WideOp::CopyObjectRestW {
                            dst,
                            src,
                            excl_base,
                            excl_count,
                        } => {
                            let src_v = self.stack[base + usize::from(src)];
                            let excl_vals_vec = if excl_count == 0 {
                                Vec::new()
                            } else {
                                let start = base + usize::from(excl_base);
                                let end = start + usize::from(excl_count);
                                self.stack[start..end].to_vec()
                            };
                            let dst_val = attempt!(self.op_copy_object_rest(src_v, &excl_vals_vec));
                            self.stack[base + usize::from(dst)] = dst_val;
                        }
                        WideOp::CopyArrayRestW { dst, src, start } => {
                            let src_v = self.stack[base + usize::from(src)];
                            let dst_val = attempt!(self.op_copy_array_rest(src_v, start));
                            self.stack[base + usize::from(dst)] = dst_val;
                        }
                        // RegExt is decoded by `decode_instr` before dispatch
                        // (it merges the wide register halves into the narrow
                        // operand); reaching this arm means corrupt bytecode.
                        WideOp::RegExt { .. } => {
                            panic!("corrupt bytecode: bare RegExt reached dispatch")
                        }
                        WideOp::GetPrivateW {
                            dst,
                            obj,
                            class_id,
                            name_id,
                        } => {
                            let obj_v = self.stack[base + usize::from(obj)];
                            let v = attempt!(self.private_get(obj_v, class_id, name_id));
                            self.stack[base + usize::from(dst)] = v;
                        }
                        WideOp::SetPrivateW {
                            obj,
                            class_id,
                            name_id,
                            value,
                        } => {
                            let obj_v = self.stack[base + usize::from(obj)];
                            let val = self.stack[base + usize::from(value)];
                            attempt!(self.private_set(obj_v, class_id, name_id, val));
                        }
                        WideOp::DefinePrivateW {
                            obj,
                            class_id,
                            name_id,
                            value,
                        } => {
                            let obj_v = self.stack[base + usize::from(obj)];
                            let val = self.stack[base + usize::from(value)];
                            attempt!(self.private_define(obj_v, class_id, name_id, val));
                        }
                        WideOp::HasPrivateW {
                            dst,
                            obj,
                            class_id,
                            name_id,
                        } => {
                            let obj_v = self.stack[base + usize::from(obj)];
                            let present = self.private_has(obj_v, class_id, name_id);
                            self.stack[base + usize::from(dst)] =
                                v12_heap::JsValue::from_bool(present);
                        }
                    }
                    self.set_pc(pc + width);
                }

                // ------------------------------------------------------
                // Arithmetic
                // ------------------------------------------------------
                Opcode::Add => {
                    let l = attempt!(self.to_primitive_default(self.stack[base + usize::from(rb)]));
                    let r = attempt!(self.to_primitive_default(self.stack[base + usize::from(rc)]));
                    self.gc_protect();
                    let v = attempt!(ops::add(self.heap, l, r));
                    self.stack[base + usize::from(ra)] = v;
                    let lat = Lattice::from_value(v, None);
                    self.feedback
                        .entry(fn_idx)
                        .or_default()
                        .record_type(pc as u32, lat);
                    self.set_pc(pc + op_width);
                }
                Opcode::Sub => {
                    let ln = attempt!(self.to_number_value(self.stack[base + usize::from(rb)]));
                    let rn = attempt!(self.to_number_value(self.stack[base + usize::from(rc)]));
                    let v = ops::box_number(ln - rn);
                    self.stack[base + usize::from(ra)] = v;
                    let lat = Lattice::from_value(v, None);
                    self.feedback
                        .entry(fn_idx)
                        .or_default()
                        .record_type(pc as u32, lat);
                    self.set_pc(pc + op_width);
                }
                Opcode::Mul => {
                    let ln = attempt!(self.to_number_value(self.stack[base + usize::from(rb)]));
                    let rn = attempt!(self.to_number_value(self.stack[base + usize::from(rc)]));
                    let v = ops::box_number(ln * rn);
                    self.stack[base + usize::from(ra)] = v;
                    let lat = Lattice::from_value(v, None);
                    self.feedback
                        .entry(fn_idx)
                        .or_default()
                        .record_type(pc as u32, lat);
                    self.set_pc(pc + op_width);
                }
                Opcode::Div | Opcode::Mod | Opcode::Pow => {
                    let l = self.stack[base + usize::from(rb)];
                    let r = self.stack[base + usize::from(rc)];
                    let ln = attempt!(self.to_number_value(l));
                    let rn = attempt!(self.to_number_value(r));
                    let n = match op {
                        Opcode::Div => ops::box_number(ln / rn),
                        Opcode::Mod => ops::box_number(ln % rn),
                        _ => ops::js_pow(ln, rn),
                    };
                    self.stack[base + usize::from(ra)] = n;
                    let lat = Lattice::from_value(n, None);
                    self.feedback
                        .entry(fn_idx)
                        .or_default()
                        .record_type(pc as u32, lat);
                    self.set_pc(pc + op_width);
                }

                // ------------------------------------------------------
                // Bitwise operations and shifts (ES ToInt32/ToUint32)
                // ------------------------------------------------------
                Opcode::BitAnd | Opcode::BitOr | Opcode::BitXor => {
                    let ln = attempt!(self.to_number_value(self.stack[base + usize::from(rb)]));
                    let rn = attempt!(self.to_number_value(self.stack[base + usize::from(rc)]));
                    let (a, b) = (ops::to_int32(ln), ops::to_int32(rn));
                    let n = match op {
                        Opcode::BitAnd => a & b,
                        Opcode::BitOr => a | b,
                        _ => a ^ b,
                    };
                    self.stack[base + usize::from(ra)] = ops::box_number(f64::from(n));
                    self.set_pc(pc + op_width);
                }
                Opcode::Shl | Opcode::Shr | Opcode::UShr => {
                    let ln = attempt!(self.to_number_value(self.stack[base + usize::from(rb)]));
                    let rn = attempt!(self.to_number_value(self.stack[base + usize::from(rc)]));
                    let shift = ops::to_uint32(rn) & 31;
                    let n = match op {
                        Opcode::Shl => ops::to_int32(ln) << shift,
                        Opcode::Shr => ops::to_int32(ln) >> shift,
                        // Unsigned shift reinterprets the int32 bits as u32.
                        _ => (ops::to_int32(ln) as u32 >> shift) as i32,
                    };
                    self.stack[base + usize::from(ra)] = ops::box_number(f64::from(n));
                    self.set_pc(pc + op_width);
                }

                // ------------------------------------------------------
                // Equality, comparison, unary operators
                // ------------------------------------------------------
                Opcode::Eq | Opcode::Ne => {
                    let l = self.stack[base + usize::from(rb)];
                    let r = self.stack[base + usize::from(rc)];
                    let eq = attempt!(self.loose_equals(l, r));
                    self.write_bool(base, ra, eq ^ (op == Opcode::Ne));
                    self.set_pc(pc + op_width);
                }
                Opcode::StrictEq | Opcode::StrictNe => {
                    let l = self.stack[base + usize::from(rb)];
                    let r = self.stack[base + usize::from(rc)];
                    let eq = ops::strict_equals(self.heap, l, r);
                    self.write_bool(base, ra, eq ^ (op == Opcode::StrictNe));
                    self.set_pc(pc + op_width);
                }
                Opcode::Lt | Opcode::Le | Opcode::Gt | Opcode::Ge => {
                    let l = self.stack[base + usize::from(rb)];
                    let r = self.stack[base + usize::from(rc)];
                    let ord = attempt!(self.compare(op, l, r));
                    self.write_bool(base, ra, ord);
                    self.set_pc(pc + op_width);
                }
                Opcode::Neg => {
                    let n = -attempt!(self.to_number_value(self.stack[base + usize::from(rb)]));
                    self.stack[base + usize::from(ra)] = ops::box_number(n);
                    self.set_pc(pc + op_width);
                }
                Opcode::ToNumber => {
                    // ES ToNumber (unary `+`): result is a number value.
                    let n = attempt!(self.to_number_value(self.stack[base + usize::from(rb)]));
                    self.stack[base + usize::from(ra)] = ops::box_number(n);
                    self.set_pc(pc + op_width);
                }
                Opcode::ToPropertyKey => {
                    // ES ToPropertyKey: materialize the key so a key object's
                    // `toString`/`valueOf` side effects run at this pc.
                    let k =
                        attempt!(self.to_property_key_value(self.stack[base + usize::from(rb)]));
                    self.stack[base + usize::from(ra)] = k;
                    self.set_pc(pc + op_width);
                }
                Opcode::BitNot => {
                    let n = attempt!(self.to_number_value(self.stack[base + usize::from(rb)]));
                    self.stack[base + usize::from(ra)] =
                        ops::box_number(f64::from(!ops::to_int32(n)));
                    self.set_pc(pc + op_width);
                }
                Opcode::Not => {
                    let truthy = ops::to_boolean(self.heap, self.stack[base + usize::from(rb)]);
                    self.write_bool(base, ra, !truthy);
                    self.set_pc(pc + op_width);
                }
                Opcode::TypeOf => {
                    let v = self.stack[base + usize::from(rb)];
                    self.gc_protect();
                    let tag = self.type_tag(v);
                    let name = attempt!(self.typeof_name(tag));
                    self.stack[base + usize::from(ra)] = JsValue::string(name);
                    self.set_pc(pc + op_width);
                }
                Opcode::In => {
                    let key_v = self.stack[base + usize::from(rb)];
                    let obj_v = self.stack[base + usize::from(rc)];
                    self.gc_protect();
                    let present = attempt!(self.op_in(key_v, obj_v));
                    self.write_bool(base, ra, present);
                    self.set_pc(pc + op_width);
                }
                Opcode::InstanceOf => {
                    let lhs_v = self.stack[base + usize::from(rb)];
                    let rhs_v = self.stack[base + usize::from(rc)];
                    self.gc_protect();
                    let result = attempt!(self.op_instanceof(lhs_v, rhs_v));
                    self.write_bool(base, ra, result);
                    self.set_pc(pc + op_width);
                }

                // ------------------------------------------------------
                // Control flow
                // ------------------------------------------------------
                Opcode::Jump => {
                    self.set_pc(narrow.imm24() as usize);
                }
                Opcode::JumpIfFalse | Opcode::JumpIfTrue => {
                    let truthy = ops::to_boolean(self.heap, self.stack[base + usize::from(ra)]);
                    let taken = truthy ^ (op == Opcode::JumpIfFalse);
                    self.set_pc(if taken {
                        usize::from(narrow.imm16())
                    } else {
                        pc + op_width
                    });
                }
                Opcode::JumpIfNullish => {
                    // Optional-chaining short-circuit: jump when the value is
                    // null or undefined.
                    let v = self.stack[base + usize::from(ra)];
                    let nullish = v.is_null() || v.is_undefined();
                    self.set_pc(if nullish {
                        usize::from(narrow.imm16())
                    } else {
                        pc + op_width
                    });
                }
                Opcode::LoopHeader => {
                    self.note_loop(fn_idx);
                    self.set_pc(pc + op_width);
                }

                // ------------------------------------------------------
                // Calls, returns, throws
                // ------------------------------------------------------
                Opcode::Call => {
                    let argc = rc;
                    match attempt!(self.prepare_call(base, max_regs, rb, argc)) {
                        CallOutcome::Pushed => continue 'drive,
                        CallOutcome::Value(v) => {
                            self.stack[base + usize::from(ra)] = v;
                            self.set_pc(pc + op_width);
                        }
                    }
                    continue 'drive;
                }
                Opcode::Construct => {
                    let argc = rc;
                    match attempt!(self.prepare_construct(base, max_regs, rb, argc)) {
                        CallOutcome::Pushed => continue 'drive,
                        CallOutcome::Value(v) => {
                            self.stack[base + usize::from(ra)] = v;
                            self.set_pc(pc + op_width);
                        }
                    }
                    continue 'drive;
                }
                Opcode::GetNewTarget => {
                    let new_target = self.frames.last().expect("frame").new_target;
                    self.stack[base + usize::from(ra)] = new_target.unwrap_or(JsValue::undefined());
                    self.set_pc(pc + op_width);
                }
                Opcode::Return => {
                    let v = self.stack[base + usize::from(ra)];
                    if self.complete_frame(v)? {
                        return Ok(());
                    }
                    continue 'drive;
                }
                Opcode::Throw => {
                    throw_js!(self.stack[base + usize::from(ra)]);
                }

                // ------------------------------------------------------
                // Property access
                // ------------------------------------------------------
                Opcode::GetProperty => {
                    let obj_v = self.stack[base + usize::from(rb)];
                    let key_v = self.stack[base + usize::from(rc)];
                    self.gc_protect();
                    let v = attempt!(self.get_property(fn_idx, pc as u32, obj_v, key_v));
                    self.stack[base + usize::from(ra)] = v;
                    let lat = Lattice::from_value(v, v.as_object().map(|h| self.shape_of(h)));
                    self.feedback
                        .entry(fn_idx)
                        .or_default()
                        .record_type(pc as u32, lat);
                    self.set_pc(pc + op_width);
                }
                Opcode::SetProperty => {
                    let obj_v = self.stack[base + usize::from(ra)];
                    let key_v = self.stack[base + usize::from(rb)];
                    let value = self.stack[base + usize::from(rc)];
                    // Guard: null/undefined base throws TypeError per ES 9.1.9 / 13.14.3.
                    // Thrown through the unwind path (not a bare `return Err`):
                    // a bare return escapes `execute` leaving this frame live,
                    // and callers that truncate the stack afterwards then
                    // corrupt the machine (the register-window OOB class).
                    if obj_v.is_null() || obj_v.is_undefined() {
                        let exc = self
                            .error_value("TypeError: cannot set properties of null or undefined");
                        throw_js!(exc);
                    }
                    self.gc_protect();
                    attempt!(self.set_property(obj_v, key_v, value));
                    self.set_pc(pc + op_width);
                }
                Opcode::DefineMethod => {
                    let obj_v = self.stack[base + usize::from(ra)];
                    let key_v = self.stack[base + usize::from(rb)];
                    let value = self.stack[base + usize::from(rc)];
                    self.gc_protect();
                    attempt!(self.op_define_method(obj_v, key_v, value));
                    self.set_pc(pc + op_width);
                }
                Opcode::DeleteProperty => {
                    let obj_v = self.stack[base + usize::from(rb)];
                    let key_v = self.stack[base + usize::from(rc)];
                    let deleted = attempt!(self.delete_property(obj_v, key_v));
                    self.write_bool(base, ra, deleted);
                    self.set_pc(pc + op_width);
                }

                // ------------------------------------------------------
                // Allocation and environments
                // ------------------------------------------------------
                Opcode::NewObject => {
                    self.gc_protect();
                    let h = self.heap.alloc(JsObject::default());
                    self.stack[base + usize::from(ra)] = JsValue::object(h);
                    self.set_pc(pc + op_width);
                }
                Opcode::NewArray => {
                    let first = base + usize::from(rb);
                    let len = usize::from(rc);
                    self.gc_protect();
                    let elements = self.stack[first..first + len].to_vec();
                    let shape = self.array_shape();
                    let h = self.heap.alloc(JsObject::array(elements));
                    // Publish the shape onto the fresh object before anything
                    // else can allocate (the shape is pinned in
                    // `array_shape`, satisfying the allocation contract).
                    self.bind_shape(h, shape);
                    self.link_array_proto(h);
                    self.stack[base + usize::from(ra)] = JsValue::object(h);
                    self.set_pc(pc + op_width);
                }
                Opcode::Closure => {
                    self.gc_protect();
                    let env = self.frames.last().expect("frame").env;
                    let h = self.alloc_closure(rb.into(), env);
                    self.stack[base + usize::from(ra)] = JsValue::object(h);
                    self.set_pc(pc + op_width);
                }
                Opcode::NewEnvironment => {
                    let slots = usize::from(rb);
                    self.gc_protect();
                    // Environments are always fresh objects, mirroring the
                    // reference interpreter in `v12-bccompiler/tests.rs`: the
                    // global object lives *outside* the environment chain
                    // (top-level `var`s route through `SetGlobal`/`GetGlobal`),
                    // so aliased environments cannot collide with user global
                    // properties that occupy the same physical storage.
                    let parent = self.frames.last().expect("frame").env;
                    let h = self.heap.alloc(JsObject::environment(slots, parent));
                    // Display head-shift (O(1)), mirroring the wide op.
                    let frame = self.frames.last_mut().expect("frame");
                    env_display_push(&mut frame.env_display, h);
                    frame.env = Some(h);
                    self.set_pc(pc + op_width);
                }
                Opcode::GetEnvSlot => {
                    let v = attempt!(self.env_read(rb, rc));
                    self.stack[base + usize::from(ra)] = v;
                    self.set_pc(pc + op_width);
                }
                Opcode::SetEnvSlot => {
                    let v = self.stack[base + usize::from(rc)];
                    attempt!(self.env_write(ra, rb, v));
                    self.set_pc(pc + op_width);
                }
                Opcode::SetPrototype => {
                    let obj_v = self.stack[base + usize::from(rb)];
                    let proto_v = self.stack[base + usize::from(rc)];
                    attempt!(self.op_set_prototype(obj_v, proto_v));
                    self.set_pc(pc + op_width);
                }
                Opcode::GetIterator => {
                    // ES GetIterator: `iter = @@iterator(rhs)`.
                    let src_v = self.stack[base + usize::from(rb)];
                    self.gc_protect();
                    let iter = attempt!(self.op_get_iterator(src_v));
                    self.stack[base + usize::from(ra)] = iter;
                    self.set_pc(pc + op_width);
                }
                Opcode::GenResumeMode => {
                    // Loads the resumed generator's pending-mode slot: 0 =
                    // normal `next()` resume, 1 = `return(v)` completion (the
                    // resume value is the return value). Reads the generator
                    // attached to the current frame.
                    let dst = ra;
                    let v = self
                        .frames
                        .last()
                        .and_then(|f| f.generator)
                        .and_then(|g| self.heap.get(g).properties.get(5).copied())
                        .unwrap_or_else(JsValue::undefined);
                    self.stack[base + usize::from(dst)] = v;
                    self.set_pc(pc + op_width);
                }
                Opcode::IteratorNext => {
                    // ES IteratorNext: `result = iter.next()`.
                    let iter_v = self.stack[base + usize::from(rb)];
                    self.gc_protect();
                    let result = attempt!(self.op_iterator_next(iter_v));
                    self.stack[base + usize::from(ra)] = result;
                    self.set_pc(pc + op_width);
                }
                Opcode::IteratorClose => {
                    let iter_v = self.stack[base + usize::from(ra)];
                    self.gc_protect();
                    attempt!(self.op_iterator_close(iter_v));
                    self.set_pc(pc + op_width);
                }
                Opcode::CopyArrayRest => {
                    let src_v = self.stack[base + usize::from(rb)];
                    let start = rc;
                    let dst_val = attempt!(self.op_copy_array_rest(src_v, start));
                    self.stack[base + usize::from(ra)] = dst_val;
                    self.set_pc(pc + op_width);
                }
                Opcode::CheckIsArray => {
                    let v = self.stack[base + usize::from(ra)];
                    attempt!(self.op_check_is_array(v));
                    self.set_pc(pc + op_width);
                }
                Opcode::CallApply => {
                    // RegExt-merged operands; `instr.a()` would read the wide
                    // header's mask byte for prefixed sites.
                    let callee = rb;
                    let dst = ra;
                    let args_reg = rc;
                    let this_v = self.stack[base + usize::from(callee) + 1];
                    let callee_v = self.stack[base + usize::from(callee)];
                    let args_v = self.stack[base + usize::from(args_reg)];
                    self.gc_protect();
                    let result =
                        attempt!(self.prepare_call_apply(base, max_regs, callee_v, this_v, args_v));
                    match result {
                        CallOutcome::Pushed => continue 'drive,
                        CallOutcome::Value(v) => {
                            self.stack[base + usize::from(dst)] = v;
                            self.set_pc(pc + op_width);
                        }
                    }
                    continue 'drive;
                }
                Opcode::CopyObjectRest => {
                    // Narrow form with single excluded key in c (or 0).
                    let src_v = self.stack[base + usize::from(rb)];
                    let excl_vec = if rc == 0 {
                        Vec::new()
                    } else {
                        let start = base + usize::from(rc);
                        self.stack[start..start + 1].to_vec()
                    };
                    let dst_val = attempt!(self.op_copy_object_rest(src_v, &excl_vec));
                    self.stack[base + usize::from(ra)] = dst_val;
                    self.set_pc(pc + op_width);
                }
                Opcode::ArrayAppend => {
                    let dst_v = self.stack[base + usize::from(ra)];
                    let src_v = self.stack[base + usize::from(rb)];
                    attempt!(self.op_array_append(dst_v, src_v));
                    self.set_pc(pc + op_width);
                }
                Opcode::MergeObject => {
                    let dst_v = self.stack[base + usize::from(rb)];
                    let src_v = self.stack[base + usize::from(rc)];
                    attempt!(self.op_merge_object(dst_v, src_v));
                    self.set_pc(pc + op_width);
                }
                Opcode::DefineAccessor => {
                    let obj_v = self.stack[base + usize::from(ra)];
                    let key_v = self.stack[base + usize::from(rb)];
                    let pair_base = base + usize::from(rc);
                    let getter_v = self
                        .stack
                        .get(pair_base)
                        .copied()
                        .unwrap_or(JsValue::undefined());
                    let setter_v = self
                        .stack
                        .get(pair_base + 1)
                        .copied()
                        .unwrap_or(JsValue::undefined());
                    attempt!(self.op_define_accessor(obj_v, key_v, getter_v, setter_v));
                    self.set_pc(pc + op_width);
                }
                Opcode::GetGlobal => {
                    // `ra` is the RegExt-merged destination; `instr.a()` would
                    // read the wide header's mask byte for prefixed sites.
                    let dst = ra;
                    let const_id = u32::from(narrow.imm16());
                    let val = attempt!(self.op_get_global(const_id, program));
                    self.stack[base + usize::from(dst)] = val;
                    self.set_pc(pc + op_width);
                }
                Opcode::GetGlobalLenient => {
                    let dst = ra;
                    let const_id = u32::from(narrow.imm16());
                    let val = attempt!(self.op_get_global_lenient(const_id, program));
                    self.stack[base + usize::from(dst)] = val;
                    self.set_pc(pc + op_width);
                }
                Opcode::SetGlobal => {
                    let src = ra;
                    let const_id = u32::from(narrow.imm16());
                    let val = self.stack[base + usize::from(src)];
                    // Guard: global base must be an object (invariant; defensive).
                    if let Some(global) = self.global {
                        let global_v = JsValue::object(global);
                        if global_v.is_null() || global_v.is_undefined() {
                            let exc = self.error_value(
                                "TypeError: cannot set properties of null or undefined",
                            );
                            throw_js!(exc);
                        }
                    }
                    attempt!(self.op_set_global(const_id, val, program));
                    self.set_pc(pc + op_width);
                }

                Opcode::CreateGenerator => {
                    // No longer emitted by compiler; generator creation is handled in prepare_call.
                    // Keep stub for manual bytecode: create dormant generator capturing current frame state after this pc.
                    let dst = ra;
                    let src = rb;
                    // Bounds-checked: OOB stack read yields undefined (JS semantics for array OOB is undefined; for register window treat OOB as undefined rather than panic)
                    let func_idx = self
                        .stack
                        .get(base + usize::from(src))
                        .and_then(|v| v.as_smi())
                        .map(|v| v as u32)
                        .unwrap_or(fn_idx);
                    self.gc_protect();
                    let h = self.heap.alloc(JsObject {
                        kind: Kind::Generator,
                        properties: smallvec::smallvec![
                            ops::box_number(f64::from(func_idx)),
                            ops::box_number(f64::from((pc + op_width) as u32)),
                            ops::box_number(0.0),
                        ],
                        elements: {
                            let end = base + usize::from(max_regs);
                            if end <= self.stack.len() {
                                self.stack[base..end].to_vec()
                            } else if base <= self.stack.len() {
                                // Pad with undefined up to requested window rather than panic (handler/wide op edge)
                                let mut v = self.stack[base..].to_vec();
                                v.resize(usize::from(max_regs), JsValue::undefined());
                                v
                            } else {
                                vec![JsValue::undefined(); usize::from(max_regs)]
                            }
                        },
                        prototype: self.frames.last().and_then(|f| f.env),
                        ..JsObject::default()
                    });
                    self.heap.add_root(JsValue::object(h));
                    // Bounds-checked write: extend stack if needed rather than panic
                    {
                        let idx = base + usize::from(dst);
                        if idx >= self.stack.len() {
                            self.stack.resize(idx + 1, JsValue::undefined());
                        }
                        self.stack[idx] = JsValue::object(h);
                    }
                    self.set_pc(pc + op_width);
                }
                Opcode::SuspendYield => {
                    // Suspend generator: save register window and resume pc, then exit inner execute.
                    // yield* delegation is lowered by the compiler to a generic iterator loop of SuspendYield
                    // (see crates/v12-bccompiler/src/expr.rs YieldExpression delegate path).
                    self.gc_protect();
                    let dst = ra;
                    let yielded = self
                        .stack
                        .get(base + usize::from(dst))
                        .copied()
                        .unwrap_or(JsValue::undefined());
                    if self.frames.last().and_then(|f| f.generator).is_none() {
                        let exc = self.error_value("SyntaxError: yield outside generator");
                        throw_js!(exc);
                    }
                    let resume_pc = pc + op_width;
                    self.suspend(dst, yielded, resume_pc)?;
                    return Ok(());
                }
                Opcode::Await => {
                    self.gc_protect();
                    let src = rb;
                    let dst = ra;
                    // Bounds-checked stack read: OOB array element is undefined in JS semantics
                    let arg = self
                        .stack
                        .get(base + usize::from(src))
                        .copied()
                        .unwrap_or(JsValue::undefined());
                    // Both guards throw through the unwind path: a bare
                    // `return Err` would escape `execute` leaving this frame
                    // live, and a caller that then truncates the stack (e.g.
                    // `call_object`) corrupts every later resume (the
                    // register-window OOB class). Top-level await in a module
                    // main hits the second guard until module mains compile
                    // as async functions.
                    if self.frames.last().and_then(|f| f.generator).is_none() {
                        let exc = self.error_value("SyntaxError: await outside async");
                        throw_js!(exc);
                    };
                    let r#gen = self
                        .frames
                        .last()
                        .and_then(|f| f.generator)
                        .expect("generator checked above");
                    let fn_idx_of_frame = fn_idx;
                    // Async *functions* park their caller at the async-call
                    // header (the caller-advance below delivers the return
                    // promise there). Async *generators* resume through
                    // `resume_generator` like yield — after `suspend` pops
                    // the body frame, the frame below is NOT parked at a
                    // call, so decoding one there corrupts the caller's pc.
                    let frame_is_async_fn = {
                        let funcs = self.functions_for_program(fn_idx_of_frame);
                        funcs
                            .get(fn_idx_of_frame as usize)
                            .is_some_and(|f| f.is_async && !f.is_generator)
                    };
                    let async_promise = if self.heap.get(r#gen).properties.len() > 4 {
                        self.heap.get(r#gen).properties[4]
                    } else {
                        JsValue::undefined()
                    };
                    let has_promise = async_promise.as_object().is_some();
                    let (promise, is_rejected, payload) = self.promise_resolve_for_await(arg);
                    self.heap.add_root(payload);
                    if let Some(ph) = promise.as_object() {
                        self.heap.add_root(JsValue::object(ph));
                    }
                    let resume_pc = pc + op_width;
                    let _rgen = self.suspend(u16::from(dst), arg, resume_pc)?;
                    self.pending_awaits.push_back((r#gen, payload, is_rejected));
                    self.top_result = None;
                    if !frame_is_async_fn {
                        // Generator-body await: behave like SuspendYield —
                        // the resume machinery picks the frame back up.
                        return Ok(());
                    }
                    // Advance caller past its Call header: async call returns Promise if available else undefined (task 7)
                    if let Some(caller) = self.frames.last_mut() {
                        let instrs = &self.functions[caller.fn_idx as usize].instrs;
                        let caller_pc = caller.pc;
                        if let Ok((_, cdst, width)) = decode_parked_call(instrs, caller_pc) {
                            let caller_base = caller.base;
                            let idx = caller_base + usize::from(cdst);
                            if idx >= self.stack.len() {
                                self.stack.resize(idx + 1, JsValue::undefined());
                            }
                            if has_promise {
                                self.stack[idx] = async_promise;
                            } else {
                                // No promise slot yet (legacy path) – keep undefined for backward compat
                                self.stack[idx] = JsValue::undefined();
                            }
                            caller.pc += width;
                        }
                    } else {
                        return Ok(());
                    }
                    let _ = promise;
                    continue 'drive;
                }
            }
            // Dispatch tail. On stable Rust the `match op` above compiles to
            // LLVM's jump-table/switch lowering: the `#[repr(u8)]` opcodes are
            // the table indices, so this is already an O(1) indirect dispatch
            // for the dense prefix (1..=4, 10..=62). A hand-written
            // computed-goto (`goto *dispatch[op]`) is the classic interpreter
            // win over match-dispatch, but it requires `asm!` (unsafe) or
            // nightly; the bytecode layout is deliberately table-ready for it.
            //
            // MIGRATION PATH: when the `become` keyword stabilizes (Rust
            // tail-call optimization), rewrite this loop as tail calls —
            // `become dispatch(op, ...)` — and the compiler will keep it in a
            // tight loop without the indirect-jump table. Keep this comment
            // next to the dispatch tail so the migration point is documented
            // in place.
        }
    }

    pub(crate) fn set_pc(&mut self, pc: usize) {
        self.frames
            .last_mut()
            .expect("dispatch requires a frame")
            .pc = pc;
    }

    pub(crate) fn write_bool(&mut self, base: usize, reg: u16, b: bool) {
        self.stack[base + usize::from(reg)] = JsValue::from_bool(b);
    }

    // ------------------------------------------------------------------
    // Constants
    // ------------------------------------------------------------------

    pub(crate) fn const_value(
        &mut self,
        fn_idx: u32,
        id: u32,
        program: u32,
    ) -> Result<JsValue, JSException> {
        let funcs = self.functions_for_program(program);
        let konst = funcs[fn_idx as usize]
            .consts
            .get(id as u16)
            .unwrap_or_else(|| panic!("constant k{id} out of range in fn {fn_idx}"));
        match konst {
            Const::F64(v) => Ok(ops::box_number(v)),
            Const::Str32(str_id) => {
                if let Some(&h) = self.const_strings.get(&(program, str_id)) {
                    return Ok(JsValue::string(h));
                }
                // Interning allocates, so republish roots first: values
                // created since the last gc_protect point (e.g. the operand
                // of a preceding Closure) are otherwise invisible to the
                // collector.
                self.gc_protect();
                let strings = self.strings_for_program(program);
                let text: String = strings
                    .get(str_id as usize)
                    .unwrap_or_else(|| panic!("Str32({str_id}) missing from the string table"))
                    .clone();
                let h = self.heap.intern_text(&text);
                self.const_strings.insert((program, str_id), h);
                Ok(JsValue::string(h))
            }
            // `null` is a singleton distinct from `undefined`.
            Const::Null => Ok(JsValue::null()),
            Const::BigIntId(str_id) => {
                // Preserve BigInt identity via heap BigInt object (magnitude from decimal text).
                // Minimal decode: text from string table, parse decimal into bytes.
                let strings = self.strings_for_program(program);
                let text = strings
                    .get(str_id as usize)
                    .cloned()
                    .unwrap_or_else(|| "0".to_string());
                // Strip sign/prefix already normalized; store as utf8 bytes magnitude placeholder.
                let sign = text.starts_with('-');
                let body = text.trim_start_matches('-').trim_start_matches('+');
                // Simple decimal -> little-endian bytes via u128 fallback; for large values store utf8 bytes
                if let Ok(v) = body.parse::<u128>() {
                    let mut bytes = v.to_le_bytes().to_vec();
                    while bytes.len() > 1 && *bytes.last().unwrap() == 0 {
                        bytes.pop();
                    }
                    if v == 0 {
                        bytes = vec![];
                    }
                    let h = self.heap.alloc(v12_heap::V12BigInt {
                        sign,
                        magnitude_le: bytes,
                    });
                    self.heap.add_root(JsValue::bigint(h));
                    Ok(JsValue::bigint(h))
                } else {
                    let h = self.heap.alloc(v12_heap::V12BigInt {
                        sign,
                        magnitude_le: body.as_bytes().to_vec(),
                    });
                    self.heap.add_root(JsValue::bigint(h));
                    Ok(JsValue::bigint(h))
                }
            }
            Const::BigU64(v) => {
                let mut bytes = v.to_le_bytes().to_vec();
                while bytes.len() > 1 && *bytes.last().unwrap() == 0 {
                    bytes.pop();
                }
                if v == 0 {
                    bytes = vec![];
                }
                let h = self.heap.alloc(v12_heap::V12BigInt {
                    sign: false,
                    magnitude_le: bytes,
                });
                self.heap.add_root(JsValue::bigint(h));
                Ok(JsValue::bigint(h))
            }
        }
    }

    pub(crate) fn typeof_name(&mut self, tag: usize) -> Result<Handle<V12Str>, JSException> {
        if let Some(h) = self.typeof_names[tag] {
            return Ok(h);
        }
        let h = self.heap.intern_text(TYPE_NAMES[tag]);
        self.typeof_names[tag] = Some(h);
        Ok(h)
    }

    /// `typeof` classification, indexing [`TYPE_NAMES`].
    ///
    /// Internal markers (`hole`, `empty`) are never legitimate JavaScript
    /// values; if one leaks here (e.g. an array-hole escape), classify it as
    /// `undefined` — the observable analogue — instead of crashing the run.
    pub(crate) fn type_tag(&self, v: JsValue) -> usize {
        if v.is_hole() || v.is_empty() {
            return 0;
        }
        if v.is_undefined() {
            0
        } else if v.is_boolean() {
            1
        } else if v.is_smi() || v.is_f64() {
            2
        } else if v.is_string() {
            3
        } else if v.is_bigint() {
            4
        } else if v.is_symbol() {
            5
        } else if v.is_null() || v.is_object() {
            // null historically types as "object"; functions split out here.
            let function = v
                .as_object()
                .is_some_and(|h| self.heap.get(h).kind == Kind::Function);
            if function { 7 } else { 6 }
        } else {
            panic!("non-canonical value cannot be typed: {:#x}", v.bits())
        }
    }

    // ------------------------------------------------------------------
    // Calls
    // ------------------------------------------------------------------
}
