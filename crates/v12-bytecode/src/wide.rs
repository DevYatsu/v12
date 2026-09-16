//! Stage 2: wide operands — `WideOp` payloads that exceed the three
//! 8-bit `Instr` slots.

use std::fmt;

use crate::{BytecodeError, Instr, Opcode};

// ---------------------------------------------------------------------------
// Stage 2: wide operands
// ---------------------------------------------------------------------------

/// Operations whose operands exceed the three 8-bit slots.
///
/// Encoding: the header is always `Opcode::Wide`; its `a` slot carries a
/// variant-specific byte (0 for most variants, the slot mask for
/// [`WideOp::RegExt`]) and the **low byte of its imm24 field** (i.e. slot
/// `c`) carries the variant discriminant. Payload words follow raw.
///
/// Register and slot fields are uniformly u16, packed two-per-word as
/// `(hi << 16) | lo` mirroring [`Instr::new_imm16`]'s big-endian convention.
/// This is what lifts the per-function 255-register / 255-slot / 255-function
/// ceilings: narrow instructions keep byte operands, anything overflowing
/// them escapes here (or through [`WideOp::RegExt`], which prefixes a narrow
/// instruction with high bytes for its register slots).
///
/// | Variant           | Header `a` slot | Payload words                                       |
/// |-------------------|-----------------|-----------------------------------------------------|
/// | `LoadConstW`      | 0               | `dst << 16 \| const_id >> 16`, `const_id & 0xFFFF`   |
/// | `LoadIntW`        | 0               | value low u32, value high u32 (i64), `dst`           |
/// | `GetEnvSlotW`     | 0               | `dst << 16 \| depth`, `slot`                         |
/// | `SetEnvSlotW`     | 0               | `src << 16 \| depth`, `slot`                         |
/// | `CallW`           | 0               | `dst << 16 \| func`, `argc`                          |
/// | `ConstructW`      | 0               | `dst << 16 \| func`, `argc`                          |
/// | `CopyObjectRestW` | 0               | `dst << 16 \| src`, `excl_base << 16 \| excl_count`  |
/// | `CopyArrayRestW`  | 0               | `dst << 16 \| src`, `start`                          |
/// | `RegExt`          | slot mask       | `a_hi << 16 \| b_hi << 8 \| c_hi`                    |
/// | `ClosureW`        | 0               | `dst << 16 \| function_index`                        |
/// | `NewEnvironmentW` | 0               | `depth << 16 \| slots`                               |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WideOp {
    LoadConstW {
        dst: u16,
        const_id: u32,
    },
    LoadIntW {
        dst: u16,
        value: i64,
    },
    GetEnvSlotW {
        dst: u16,
        depth: u16,
        slot: u16,
    },
    SetEnvSlotW {
        src: u16,
        depth: u16,
        slot: u16,
    },
    CallW {
        dst: u16,
        func: u16,
        argc: u16,
    },
    ConstructW {
        dst: u16,
        func: u16,
        argc: u16,
    },
    CopyObjectRestW {
        dst: u16,
        src: u16,
        excl_base: u16,
        excl_count: u16,
    },
    CopyArrayRestW {
        dst: u16,
        src: u16,
        start: u16,
    },
    /// High bytes for register operands of the *next* narrow instruction.
    ///
    /// The compiler emits `[RegExt][payload][narrow instr]` when one or more
    /// register operands of a narrow op exceed `u8::MAX`. `mask` (in the
    /// header's `a` slot) says which of the narrow instruction's `a`/`b`/`c`
    /// slots are registers extended here: bit 0 → `a`, bit 1 → `b`, bit 2 →
    /// `c`. Slots without their mask bit keep the narrow instruction's byte
    /// value (immediate slots are never masked, so e.g. `LoadConst`'s 16-bit
    /// const id survives untouched).
    ///
    /// The interpreter executes the header, payload, and narrow instruction
    /// as one logical op (3 words). The pair is always emitted atomically by
    /// the compiler, so no label or handler target can land between them.
    RegExt {
        mask: u8,
        a_hi: u8,
        b_hi: u8,
        c_hi: u8,
    },
    /// `Closure dst, fn#function_index` with a 16-bit function index — lifts
    /// the 255-functions-per-program ceiling of narrow `Closure`.
    ClosureW {
        dst: u16,
        function_index: u16,
    },
    /// `NewEnvironment depth, slots` with 16-bit operands — lifts the
    /// 255-environment-slots ceiling of narrow `NewEnvironment`.
    NewEnvironmentW {
        depth: u16,
        slots: u16,
    },
    GetPrivateW {
        dst: u16,
        obj: u16,
        class_id: u32,
        name_id: u32,
    },
    SetPrivateW {
        obj: u16,
        class_id: u32,
        name_id: u32,
        value: u16,
    },
    DefinePrivateW {
        obj: u16,
        class_id: u32,
        name_id: u32,
        value: u16,
    },
    HasPrivateW {
        dst: u16,
        obj: u16,
        class_id: u32,
        name_id: u32,
    },
}

fn pack_hi_lo(hi: u16, lo: u16) -> u32 {
    (u32::from(hi) << 16) | u32::from(lo)
}

fn unpack_hi_lo(word: u32) -> (u16, u16) {
    ((word >> 16) as u16, word as u16)
}

impl WideOp {
    /// Discriminants are serialized in the header's low imm24 byte; fix them.
    pub const DISC_LOAD_CONST_W: u32 = 0;
    pub const DISC_LOAD_INT_W: u32 = 1;
    pub const DISC_GET_ENV_SLOT_W: u32 = 2;
    pub const DISC_SET_ENV_SLOT_W: u32 = 3;
    pub const DISC_CALL_W: u32 = 4;
    pub const DISC_COPY_OBJECT_REST_W: u32 = 5;
    pub const DISC_COPY_ARRAY_REST_W: u32 = 6;
    pub const DISC_REG_EXT: u32 = 7;
    pub const DISC_CLOSURE_W: u32 = 8;
    pub const DISC_NEW_ENVIRONMENT_W: u32 = 9;
    pub const DISC_CONSTRUCT_W: u32 = 10;
    pub const DISC_GET_PRIVATE_W: u32 = 11;
    pub const DISC_SET_PRIVATE_W: u32 = 12;
    pub const DISC_DEFINE_PRIVATE_W: u32 = 13;
    pub const DISC_HAS_PRIVATE_W: u32 = 14;

    /// Serializes to a header word plus the documented payload words.
    ///
    /// Header layout: `Opcode::Wide`, variant-specific byte in slot `a`,
    /// discriminant in the imm24 field's low byte (slot `c`); slot `b` stays
    /// zero.
    pub fn encode(self) -> Vec<Instr> {
        let hdr = |a: u8, disc: u32| Instr::new(Opcode::Wide, a, (disc >> 8) as u8, disc as u8);
        match self {
            Self::LoadConstW { dst, const_id } => vec![
                hdr(0, Self::DISC_LOAD_CONST_W),
                Instr(pack_hi_lo(dst, (const_id >> 16) as u16)),
                Instr(const_id & 0xFFFF),
            ],
            Self::LoadIntW { dst, value } => {
                let bits = value as u64; // two's complement split low/high
                vec![
                    hdr(0, Self::DISC_LOAD_INT_W),
                    Instr(bits as u32),
                    Instr((bits >> 32) as u32),
                    Instr(u32::from(dst)),
                ]
            }
            Self::GetEnvSlotW { dst, depth, slot } => vec![
                hdr(0, Self::DISC_GET_ENV_SLOT_W),
                Instr(pack_hi_lo(dst, depth)),
                Instr(u32::from(slot)),
            ],
            Self::SetEnvSlotW { src, depth, slot } => vec![
                hdr(0, Self::DISC_SET_ENV_SLOT_W),
                Instr(pack_hi_lo(src, depth)),
                Instr(u32::from(slot)),
            ],
            Self::CallW { dst, func, argc } | Self::ConstructW { dst, func, argc } => {
                let disc = match self {
                    Self::CallW { .. } => Self::DISC_CALL_W,
                    _ => Self::DISC_CONSTRUCT_W,
                };
                vec![
                    hdr(0, disc),
                    Instr(pack_hi_lo(dst, func)),
                    Instr(u32::from(argc)),
                ]
            }
            Self::CopyObjectRestW {
                dst,
                src,
                excl_base,
                excl_count,
            } => vec![
                hdr(0, Self::DISC_COPY_OBJECT_REST_W),
                Instr(pack_hi_lo(dst, src)),
                Instr(pack_hi_lo(excl_base, excl_count)),
            ],
            Self::CopyArrayRestW { dst, src, start } => vec![
                hdr(0, Self::DISC_COPY_ARRAY_REST_W),
                Instr(pack_hi_lo(dst, src)),
                Instr(u32::from(start)),
            ],
            Self::RegExt {
                mask,
                a_hi,
                b_hi,
                c_hi,
            } => vec![
                hdr(mask, Self::DISC_REG_EXT),
                Instr((u32::from(a_hi) << 16) | (u32::from(b_hi) << 8) | u32::from(c_hi)),
            ],
            Self::ClosureW {
                dst,
                function_index,
            } => vec![
                hdr(0, Self::DISC_CLOSURE_W),
                Instr(pack_hi_lo(dst, function_index)),
            ],
            Self::NewEnvironmentW { depth, slots } => vec![
                hdr(0, Self::DISC_NEW_ENVIRONMENT_W),
                Instr(pack_hi_lo(depth, slots)),
            ],
            Self::GetPrivateW {
                dst,
                obj,
                class_id,
                name_id,
            } => vec![
                hdr(0, Self::DISC_GET_PRIVATE_W),
                Instr(pack_hi_lo(dst, obj)),
                Instr(class_id),
                Instr(name_id),
            ],
            Self::SetPrivateW {
                obj,
                class_id,
                name_id,
                value,
            } => vec![
                hdr(0, Self::DISC_SET_PRIVATE_W),
                Instr(pack_hi_lo(obj, value)),
                Instr(class_id),
                Instr(name_id),
            ],
            Self::DefinePrivateW {
                obj,
                class_id,
                name_id,
                value,
            } => vec![
                hdr(0, Self::DISC_DEFINE_PRIVATE_W),
                Instr(pack_hi_lo(obj, value)),
                Instr(class_id),
                Instr(name_id),
            ],
            Self::HasPrivateW {
                dst,
                obj,
                class_id,
                name_id,
            } => vec![
                hdr(0, Self::DISC_HAS_PRIVATE_W),
                Instr(pack_hi_lo(dst, obj)),
                Instr(class_id),
                Instr(name_id),
            ],
        }
    }

    /// Decodes a wide op from the front of `words`, returning the op and its
    /// total word count (header included). Errors cover truncation, a
    /// non-Wide header, and unknown discriminants.
    pub fn try_decode(words: &[Instr]) -> Result<(Self, usize), BytecodeError> {
        let Some(header) = words.first() else {
            return Err(BytecodeError::TruncatedWide { word: 0 });
        };
        if header.op() != Some(Opcode::Wide) {
            return Err(BytecodeError::WideHeaderNotWide);
        }
        // Closure keeps payload indexing tied to the documented layout.
        let payload = |i: usize| -> Result<u32, BytecodeError> {
            words
                .get(i)
                .map(|w| w.0)
                .ok_or(BytecodeError::TruncatedWide { word: i })
        };
        match u32::from(header.c()) {
            Self::DISC_LOAD_CONST_W => {
                let (dst, const_hi) = unpack_hi_lo(payload(1)?);
                let const_lo = payload(2)?;
                Ok((
                    Self::LoadConstW {
                        dst,
                        const_id: (u32::from(const_hi) << 16) | const_lo,
                    },
                    3,
                ))
            }
            Self::DISC_LOAD_INT_W => {
                let lo = u64::from(payload(1)?);
                let hi = u64::from(payload(2)?);
                let dst = payload(3)?;
                Ok((
                    Self::LoadIntW {
                        dst: dst as u16,
                        value: ((hi << 32) | lo) as i64,
                    },
                    4,
                ))
            }
            Self::DISC_GET_ENV_SLOT_W => {
                let (dst, depth) = unpack_hi_lo(payload(1)?);
                let slot = payload(2)? as u16;
                Ok((Self::GetEnvSlotW { dst, depth, slot }, 3))
            }
            Self::DISC_SET_ENV_SLOT_W => {
                let (src, depth) = unpack_hi_lo(payload(1)?);
                let slot = payload(2)? as u16;
                Ok((Self::SetEnvSlotW { src, depth, slot }, 3))
            }
            Self::DISC_CALL_W | Self::DISC_CONSTRUCT_W => {
                let (dst, func) = unpack_hi_lo(payload(1)?);
                let argc = payload(2)? as u16;
                let op = if u32::from(header.c()) == Self::DISC_CALL_W {
                    Self::CallW { dst, func, argc }
                } else {
                    Self::ConstructW { dst, func, argc }
                };
                Ok((op, 3))
            }
            Self::DISC_COPY_OBJECT_REST_W => {
                let (dst, src) = unpack_hi_lo(payload(1)?);
                let (excl_base, excl_count) = unpack_hi_lo(payload(2)?);
                Ok((
                    Self::CopyObjectRestW {
                        dst,
                        src,
                        excl_base,
                        excl_count,
                    },
                    3,
                ))
            }
            Self::DISC_COPY_ARRAY_REST_W => {
                let (dst, src) = unpack_hi_lo(payload(1)?);
                let start = payload(2)? as u16;
                Ok((Self::CopyArrayRestW { dst, src, start }, 3))
            }
            Self::DISC_REG_EXT => {
                let ext = payload(1)?;
                Ok((
                    Self::RegExt {
                        mask: header.a(),
                        a_hi: (ext >> 16) as u8,
                        b_hi: (ext >> 8) as u8,
                        c_hi: ext as u8,
                    },
                    2,
                ))
            }
            Self::DISC_CLOSURE_W => {
                let (dst, function_index) = unpack_hi_lo(payload(1)?);
                Ok((
                    Self::ClosureW {
                        dst,
                        function_index,
                    },
                    2,
                ))
            }
            Self::DISC_NEW_ENVIRONMENT_W => {
                let (depth, slots) = unpack_hi_lo(payload(1)?);
                Ok((Self::NewEnvironmentW { depth, slots }, 2))
            }
            Self::DISC_GET_PRIVATE_W => {
                let (dst, obj) = unpack_hi_lo(payload(1)?);
                let class_id = payload(2)?;
                let name_id = payload(3)?;
                Ok((
                    Self::GetPrivateW {
                        dst,
                        obj,
                        class_id,
                        name_id,
                    },
                    4,
                ))
            }
            Self::DISC_SET_PRIVATE_W => {
                let (obj, value) = unpack_hi_lo(payload(1)?);
                let class_id = payload(2)?;
                let name_id = payload(3)?;
                Ok((
                    Self::SetPrivateW {
                        obj,
                        class_id,
                        name_id,
                        value,
                    },
                    4,
                ))
            }
            Self::DISC_DEFINE_PRIVATE_W => {
                let (obj, value) = unpack_hi_lo(payload(1)?);
                let class_id = payload(2)?;
                let name_id = payload(3)?;
                Ok((
                    Self::DefinePrivateW {
                        obj,
                        class_id,
                        name_id,
                        value,
                    },
                    4,
                ))
            }
            Self::DISC_HAS_PRIVATE_W => {
                let (dst, obj) = unpack_hi_lo(payload(1)?);
                let class_id = payload(2)?;
                let name_id = payload(3)?;
                Ok((
                    Self::HasPrivateW {
                        dst,
                        obj,
                        class_id,
                        name_id,
                    },
                    4,
                ))
            }
            other => Err(BytecodeError::UnknownWideDiscriminant {
                discriminant: other,
            }),
        }
    }
}

impl fmt::Display for WideOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::LoadConstW { dst, const_id } => write!(f, "load_const_w r{dst}, k{const_id}"),
            Self::LoadIntW { dst, value } => write!(f, "load_int_w r{dst}, #{value}"),
            Self::GetEnvSlotW { dst, depth, slot } => {
                write!(f, "get_env_slot_w r{dst}, depth={depth} slot={slot}")
            }
            Self::SetEnvSlotW { src, depth, slot } => {
                write!(f, "set_env_slot_w r{src}, depth={depth} slot={slot}")
            }
            Self::CallW { dst, func, argc } => write!(f, "call_w r{dst}, r{func}, argc={argc}"),
            Self::ConstructW { dst, func, argc } => {
                write!(f, "construct_w r{dst}, r{func}, argc={argc}")
            }
            Self::CopyObjectRestW {
                dst,
                src,
                excl_base,
                excl_count,
            } => write!(
                f,
                "copy_object_rest_w r{dst}, r{src}, excl_base=r{excl_base} count={excl_count}"
            ),
            Self::CopyArrayRestW { dst, src, start } => {
                write!(f, "copy_array_rest_w r{dst}, r{src}, start={start}")
            }
            Self::RegExt {
                mask,
                a_hi,
                b_hi,
                c_hi,
            } => write!(
                f,
                "reg_ext mask={mask:#04x} a_hi={a_hi:#04x} b_hi={b_hi:#04x} c_hi={c_hi:#04x}"
            ),
            Self::ClosureW {
                dst,
                function_index,
            } => write!(f, "closure_w r{dst}, fn#{function_index}"),
            Self::NewEnvironmentW { depth, slots } => {
                write!(f, "new_environment_w depth={depth}, slots={slots}")
            }
            Self::GetPrivateW {
                dst,
                obj,
                class_id,
                name_id,
            } => write!(
                f,
                "get_private_w r{dst}, r{obj}, class={class_id} name={name_id}"
            ),
            Self::SetPrivateW {
                obj,
                class_id,
                name_id,
                value,
            } => write!(
                f,
                "set_private_w r{obj}, class={class_id} name={name_id} r{value}"
            ),
            Self::DefinePrivateW {
                obj,
                class_id,
                name_id,
                value,
            } => write!(
                f,
                "define_private_w r{obj}, class={class_id} name={name_id} r{value}"
            ),
            Self::HasPrivateW {
                dst,
                obj,
                class_id,
                name_id,
            } => write!(
                f,
                "has_private_w r{dst}, r{obj}, class={class_id} name={name_id}"
            ),
        }
    }
}

#[cfg(test)]
mod wide_tests {
    use super::*;

    #[test]
    fn wide_ops_roundtrip_through_encode_decode() {
        let ops = [
            WideOp::LoadConstW {
                dst: 3,
                const_id: 0xDEAD_BEEF,
            },
            WideOp::LoadConstW {
                dst: 300,
                const_id: 7,
            },
            WideOp::LoadIntW { dst: 1, value: -5 },
            WideOp::LoadIntW {
                dst: 9,
                value: i64::MIN,
            },
            WideOp::LoadIntW {
                dst: 300,
                value: 1000,
            },
            WideOp::GetEnvSlotW {
                dst: 2,
                depth: 0x1234,
                slot: 0x5678,
            },
            WideOp::GetEnvSlotW {
                dst: 300,
                depth: 0,
                slot: 300,
            },
            WideOp::SetEnvSlotW {
                src: 9,
                depth: 7,
                slot: u16::MAX,
            },
            WideOp::CallW {
                dst: 0,
                func: 4,
                argc: 1000,
            },
            WideOp::CallW {
                dst: 300,
                func: 260,
                argc: 300,
            },
            WideOp::ConstructW {
                dst: 301,
                func: 302,
                argc: 256,
            },
            WideOp::CopyObjectRestW {
                dst: 1,
                src: 2,
                excl_base: 3,
                excl_count: 7,
            },
            WideOp::CopyArrayRestW {
                dst: 4,
                src: 5,
                start: 258,
            },
            WideOp::RegExt {
                mask: 0b011,
                a_hi: 1,
                b_hi: 2,
                c_hi: 0,
            },
            WideOp::ClosureW {
                dst: 300,
                function_index: 257,
            },
            WideOp::NewEnvironmentW {
                depth: 0,
                slots: 300,
            },
        ];
        for op in ops {
            let words = op.encode();
            assert_eq!(
                words[0].op(),
                Some(Opcode::Wide),
                "{op:?} header must be Wide"
            );
            let (back, width) = WideOp::try_decode(&words).unwrap();
            assert_eq!(back, op);
            assert_eq!(width, words.len());
        }
    }

    #[test]
    fn wide_word_layout_matches_docs() {
        let lc = WideOp::LoadConstW {
            dst: 6,
            const_id: 0xABCD_1234,
        }
        .encode();
        assert_eq!(lc.len(), 3);
        // Register/id pairs pack hi|lo; the const id splits hi, lo.
        assert_eq!(lc[1].0, 0x0006_ABCD);
        assert_eq!(lc[2].0, 0x1234);

        let env = WideOp::GetEnvSlotW {
            dst: 0x00AA,
            depth: 0x00BB,
            slot: 0x00CC,
        }
        .encode();
        assert_eq!(env.len(), 3);
        assert_eq!(env[1].0, 0x00AA_00BB);
        assert_eq!(env[2].0, 0x00CC);

        let call = WideOp::CallW {
            dst: 0x11,
            func: 0x22,
            argc: 0x3344,
        }
        .encode();
        assert_eq!(call.len(), 3);
        assert_eq!(call[1].0, 0x0011_0022);
        assert_eq!(call[2].0, 0x3344);

        let li = WideOp::LoadIntW { dst: 0, value: 1 }.encode();
        assert_eq!(li.len(), 4);
        assert_eq!((li[1].0, li[2].0, li[3].0), (1, 0, 0)); // low, high, dst

        // RegExt: mask rides in the header `a` slot, high bytes in payload 1.
        let rex = WideOp::RegExt {
            mask: 0b101,
            a_hi: 0x12,
            b_hi: 0x34,
            c_hi: 0x56,
        }
        .encode();
        assert_eq!(rex.len(), 2);
        assert_eq!(rex[0].a(), 0b101);
        assert_eq!(u32::from(rex[0].c()), WideOp::DISC_REG_EXT);
        assert_eq!(rex[1].0, 0x0012_3456);
    }

    #[test]
    fn wide_decode_rejects_truncated_and_unknown() {
        assert!(WideOp::try_decode(&[]).is_err());

        let hdr_only = [Instr::new_imm24(Opcode::Wide, WideOp::DISC_CALL_W)];
        assert!(WideOp::try_decode(&hdr_only).is_err());

        let not_wide = [Instr::new(Opcode::Add, 0, 0, 0)];
        assert!(WideOp::try_decode(&not_wide).is_err());

        let unknown = [Instr::new_imm24(Opcode::Wide, 99), Instr(0)];
        assert!(WideOp::try_decode(&unknown).is_err());
    }
}
