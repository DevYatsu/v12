//! Cross-checks for the operand-contract table (`opspec.rs`).
//!
//! # What is actually being checked
//!
//! [`crate::opspec::OpSpec`] and the disassembler's [`crate::fmt_operands`]
//! are two *independent* statements about the same thing: which of an
//! `Instr`'s `a`/`b`/`c` slots an opcode actually uses.
//!
//! * The table was derived from the **construction sites** in
//!   `v12-bccompiler` (`model.rs`'s `emit_reg*` families, plus the direct
//!   `emit_reg3` calls in `expr.rs` / `stmt.rs` / `class.rs` / `unit.rs`) and
//!   the **read sites** in `v12-interp/src/execute.rs`.
//! * `fmt_operands` was derived from the listing it produces.
//!
//! Comparing them therefore catches a table entry that names a slot the
//! disassembler never shows, a slot the table calls dead that the
//! disassembler renders, and — the failure mode that sank a previous attempt
//! at this table — an entry whose *width* is wrong. `Add` is ternary: it
//! renders three registers, so an entry that gave it a two-operand shape
//! would leave slot `c` unaccounted for and fail here.
//!
//! # Why the coverage is not self-reported
//!
//! The opcode list is derived by scanning the full `0..=255` byte range
//! against `Opcode`'s decoder, so a new variant cannot be absent from the
//! sweep. The hand-maintained `ALL_OPS` list this file used to contrast itself
//! against is gone (W2.2): it stopped at `ToPropertyKey` = 75, so it was both
//! incomplete and — as the tests in `opcode.rs` iterated it — evidence of
//! nothing. The frozen transcript that replaced it lives in `opcode.rs` and
//! covers the discriminants; this file covers the roles.

use super::*;
use crate::opspec::{render_operands, spec, spec_at_byte};
use crate::{Instr, Opcode, WideOp, all_opcodes};

/// Number of assigned `Opcode` variants. A new variant must bump this, which
/// makes adding one a deliberate act rather than an accident — and, since
/// `opcode.rs` also pins every discriminant against a frozen transcript, a
/// variant cannot land here without someone updating two counts on purpose.
const OPCODE_COUNT: usize = 73;

/// Distinct byte values stamped into `a`, `b`, `c`.
///
/// Every number the disassembler can print is derived from these three bytes,
/// so each printed decimal identifies exactly one slot:
///
/// | printed number | source                                  |
/// |----------------|-----------------------------------------|
/// | `17`           | `a` (`0x11`)                             |
/// | `34`           | `b` (`0x22`)                             |
/// | `51`           | `c` (`0x33`)                             |
/// | `8755`         | `imm16()` = `b << 8 \| c` = `0x2233`     |
/// | `1122867`      | `imm24()` = `a<<16 \| b<<8 \| c`         |
const A: u8 = 0x11;
const B: u8 = 0x22;
const C: u8 = 0x33;

/// The table entry for `op`. Every assigned opcode has one; the exhaustive
/// coverage tests assert that separately.
fn spec_of(op: Opcode) -> OpSpec {
    spec(op).unwrap_or_else(|| panic!("{op:?} has no OpSpec entry"))
}

/// Which operand slot(s) a number printed by the disassembler came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Printed {
    /// Exactly one slot, by index.
    One(usize),
    /// A reassembled `imm16`: `b` high byte and `c` low byte.
    BC,
    /// A reassembled `imm24`: `a`, `b` and `c`.
    ABC,
}

impl Printed {
    /// This reading's slots, as indices.
    fn slots(self) -> [usize; 3] {
        match self {
            Self::One(i) => [i, usize::MAX, usize::MAX],
            Self::BC => [usize::MAX, 1, 2],
            Self::ABC => [0, 1, 2],
        }
    }
}

/// The slot a rendered operand number came from, if it names one.
///
/// The three operand bytes are stamped with distinct values, and every number
/// the disassembler prints is either one of them or a reassembly of a subset —
/// so each printed number identifies its slots unambiguously.
fn printed_from_number(text: &str) -> Option<Printed> {
    match text {
        // A: 0x11, B: 0x22, C: 0x33
        "17" => Some(Printed::One(0)),
        "34" => Some(Printed::One(1)),
        "51" => Some(Printed::One(2)),
        // imm16 = b << 8 | c = 0x2233; imm24 = a << 16 | b << 8 | c
        "8755" => Some(Printed::BC),
        "1122867" => Some(Printed::ABC),
        _ => None,
    }
}

/// Every role that denotes a register index, i.e. a slot `WideOp::RegExt`
/// could widen. A bare `r17` in a listing says *that the slot is a register*
/// but not *what it is for* — see `role_kinds_are_citation_backed` for what
/// that leaves unchecked.
const REGISTER_ROLES: &[Role] = &[
    Role::Dst,
    Role::Src,
    Role::InOut,
    Role::Cond,
    Role::Obj,
    Role::Key,
    Role::Val,
    Role::Callee,
    Role::ElemBase,
    Role::ArgsArr,
    Role::Excl,
    Role::Pair,
];

/// Reads one rendered operand fragment as `(slots it occupies, roles the
/// disassembler's own label permits there)`.
///
/// The labels are the disassembler's *order* statement: `GetEnvSlot` prints
/// `r{a}, depth={b}, slot={c}` while `SetEnvSlot` prints
/// `depth={a}, slot={b}, r{c}`, so the labels — not merely the slot count —
/// are what make a transposed entry detectable. `None` means this parser does
/// not understand the fragment, which the tests treat as a failure.
fn label_expectation(token: &str) -> Option<(Vec<usize>, &'static [Role])> {
    let one = |prefixes: &[&str], allowed: &'static [Role]| {
        prefixes
            .iter()
            .find_map(|p| token.strip_prefix(p))
            .and_then(|rest| match printed_from_number(rest) {
                Some(Printed::One(i)) => Some((vec![i], allowed)),
                _ => None,
            })
    };

    one(&["r"], REGISTER_ROLES)
        .or_else(|| one(&["depth="], &[Role::Depth][..]))
        // `NewEnvironment` prints the plural `slots=`; the env-slot opcodes
        // print `slot=`. Both name the environment slot vector.
        .or_else(|| one(&["slots=", "slot="], &[Role::Slot][..]))
        .or_else(|| one(&["argc=", "len="], &[Role::Count][..]))
        .or_else(|| one(&["start="], &[Role::Start][..]))
        .or_else(|| one(&["fn#"], &[Role::FnIndex][..]))
        .or_else(|| one(&["#"], &[Role::Imm8][..]))
        .or_else(|| {
            // `k<n>` is a constant-pool id for `LoadConst` and a string-table
            // id for the three global ops; the disassembler does not
            // distinguish them, so neither does this.
            token
                .strip_prefix('k')
                .and_then(|rest| match printed_from_number(rest) {
                    Some(Printed::BC) => Some((vec![1, 2], &[Role::ConstId, Role::NameId][..])),
                    _ => None,
                })
        })
        .or_else(|| {
            token
                .strip_prefix("-> ")
                .and_then(|rest| match printed_from_number(rest) {
                    Some(Printed::BC) => Some((vec![1, 2], &[Role::Imm16][..])),
                    Some(Printed::ABC) => Some((vec![0, 1, 2], &[Role::Imm24][..])),
                    _ => None,
                })
        })
}

/// The slots the disassembler names when rendering `op`'s operands.
fn printed_slots(op: Opcode) -> [bool; 3] {
    let mut slots = [false; 3];
    for token in operand_tokens(op) {
        for number in token
            .split(|ch: char| !ch.is_ascii_digit())
            .filter(|n| !n.is_empty())
        {
            for i in printed_from_number(number).map_or([usize::MAX; 3], Printed::slots) {
                if i != usize::MAX {
                    slots[i] = true;
                }
            }
        }
    }
    slots
}

/// Sorts a documented divergence list by discriminant so it compares as a set
/// of `(opcode, mask)` pairs rather than as a sequence.
fn sorted_documented(list: &[(Opcode, u8, &str)]) -> Vec<(Opcode, u8)> {
    let mut entries: Vec<(Opcode, u8)> = list.iter().map(|(op, mask, _)| (*op, *mask)).collect();
    entries.sort_by_key(|(op, _)| *op as u8);
    entries
}

/// Whether the disassembler's *labels* disagree with the table's roles for
/// `op`: a fragment that places a named operand (`depth=`, `slot=`, `argc=`,
/// `len=`, `start=`, `fn#`, `#`, `k`, `->`) in a slot whose role that name
/// excludes.
fn label_mismatches(op: Opcode) -> bool {
    let s = spec_of(op);
    operand_tokens(op).iter().any(|token| {
        let Some((slots, allowed)) = label_expectation(token) else {
            return true; // an unparsed fragment is a disagreement, not a pass
        };
        slots
            .iter()
            .any(|&slot| !allowed.contains(&s.role_at_index(slot)))
    })
}

/// Pack a `[bool; 3]` slot set into a bitmask, bit `n` = slot `n`.
fn slot_mask(slots: [bool; 3]) -> u8 {
    u8::from(slots[0]) | (u8::from(slots[1]) << 1) | (u8::from(slots[2]) << 2)
}

/// The slots where the table and the disassembler disagree about being live.
fn slot_divergent_mask(op: Opcode) -> u8 {
    let table = spec_of(op).used_slots();
    let printed = printed_slots(op);
    slot_mask([
        table[0] != printed[0],
        table[1] != printed[1],
        table[2] != printed[2],
    ])
}

/// The slots where a disassembler label names a kind the table's role there
/// excludes.
fn label_divergent_mask(op: Opcode) -> u8 {
    let s = spec_of(op);
    let mut mask = 0u8;
    for token in operand_tokens(op) {
        match label_expectation(&token) {
            // An unparsed fragment is a disagreement in its own right; give it
            // every slot so it cannot be silently exempted.
            None => mask |= 0b111,
            Some((slots, allowed)) => {
                for slot in slots {
                    if !allowed.contains(&s.role_at_index(slot)) {
                        mask |= 1 << slot;
                    }
                }
            }
        }
    }
    mask
}

/// `(opcode, divergent-slot mask)` for both disagreement classes, recomputed
/// from scratch and sorted by discriminant so two opcodes compare as sets.
fn disagreements() -> (Vec<(Opcode, u8)>, Vec<(Opcode, u8)>) {
    let mut slot = Vec::new();
    let mut label = Vec::new();
    for &op in all_opcodes().as_slice() {
        let s = slot_divergent_mask(op);
        if s != 0 {
            slot.push((op, s));
        }
        let l = label_divergent_mask(op);
        if l != 0 {
            label.push((op, l));
        }
    }
    slot.sort_by_key(|(op, _)| *op as u8);
    label.sort_by_key(|(op, _)| *op as u8);
    (slot, label)
}

/// A single `r17` / `k8755` / `argc=51` style operand fragment.
fn operand_tokens(op: Opcode) -> Vec<String> {
    render_operands(op, Instr::new(op, A, B, C))
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Coverage
// ---------------------------------------------------------------------------

/// The sweep list is complete: 73 assigned variants, strictly ascending, no
/// duplicates, and it spans every discriminant `TryFrom<u8>` accepts.
#[test]
fn all_opcodes_sweep_covers_every_assigned_variant() {
    assert_eq!(all_opcodes().len(), OPCODE_COUNT, "opcode count changed");

    let mut previous: Option<u8> = None;
    for &op in all_opcodes().as_slice() {
        let byte = op as u8;
        if let Some(prev) = previous {
            assert!(prev < byte, "{prev} precedes {byte}: sweep not ascending");
        }
        previous = Some(byte);
    }

    // Independent recount: how many bytes in 0..=255 decode at all?
    let decoded = (0u16..=255)
        .filter(|b| Opcode::try_from(*b as u8).is_ok())
        .count();
    assert_eq!(
        decoded, OPCODE_COUNT,
        "TryFrom<u8> and all_opcodes() disagree"
    );
}

/// Every assigned opcode has a spec, and no unassigned discriminant has one.
#[test]
fn every_assigned_opcode_has_a_spec() {
    for &op in all_opcodes().as_slice() {
        assert!(
            spec(op).is_some(),
            "{op:?} (= {}) has no OpSpec entry",
            op as u8
        );
    }
    for byte in 0u16..=255 {
        let populated = spec_at_byte(byte as u8).is_some();
        match Opcode::try_from(byte as u8) {
            Ok(op) => assert!(populated, "{op:?} has no entry"),
            Err(_) => assert!(!populated, "unassigned byte {byte} has an entry"),
        }
    }
}

// ---------------------------------------------------------------------------
// The cross-check proper
// ---------------------------------------------------------------------------

/// The table and the disassembler must agree, for every opcode and every slot,
/// except where a documented divergence says otherwise.
///
/// This is the cross-check the task asks for: it walks all 73 opcodes and
/// compares, slot by slot, against a statement of the contract derived
/// independently from the one the table was built from. The failure mode it
/// exists to catch is a wrong *width* — `Add` is ternary and renders three
/// registers, so a two-operand entry leaves slot `c` unaccounted for.
///
/// The comparison is per *slot*, not per opcode: the documented entries carry
/// a divergent-slot mask, so a second unrelated mistake in an already-exempt
/// opcode still fails.
#[test]
fn operand_roles_match_the_disassembler_for_every_opcode() {
    let (slot_divergent, _) = disagreements();
    let documented = sorted_documented(KNOWN_DISASSEMBLER_SLOT_DIVERGENCE);

    let mut checked = 0usize;
    for &op in all_opcodes().as_slice() {
        let expected = spec_of(op);
        let actual = slot_divergent_mask(op);
        let want = documented
            .iter()
            .find(|(d, _)| *d == op)
            .map_or((op, 0u8), |&(_, mask)| (op, mask));
        assert_eq!(
            actual,
            want.1,
            "{op:?} ({}) — table {:?}/{:?}/{:?}, disassembler {:?} \
             (rendered {:?}); divergent slots {actual:#05b}, documented \
             {:#05b}. Add or adjust a KNOWN_DISASSEMBLER_*_DIVERGENCE entry.",
            op as u8,
            expected.a,
            expected.b,
            expected.c,
            printed_slots(op),
            operand_tokens(op),
            want.1,
        );
        checked += 1;
    }
    assert_eq!(checked, OPCODE_COUNT, "cross-check skipped an opcode");
    assert_eq!(
        slot_divergent, documented,
        "the set of slot disagreements moved; update \
         KNOWN_DISASSEMBLER_SLOT_DIVERGENCE in the same commit"
    );
}

/// The disassembler's labels pin operand *order*, not just the slot count.
///
/// `GetEnvSlot` renders `r{a}, depth={b}, slot={c}` while `SetEnvSlot` renders
/// `depth={a}, slot={b}, r{c}`: the labels say which slot holds the depth and
/// which holds the destination. An entry that transposes those — the shape of
/// the `KNOWN_INCONSISTENT_OPERAND_ORDER` entries, applied to the wrong one of
/// the pair — fails here even though both slots stay live.
#[test]
fn operand_order_matches_the_disassembler_labels() {
    let (_, label_divergent) = disagreements();
    let documented = sorted_documented(KNOWN_DISASSEMBLER_LABEL_DIVERGENCE);
    assert_eq!(
        label_divergent, documented,
        "the set of label disagreements moved; update \
         KNOWN_DISASSEMBLER_LABEL_DIVERGENCE in the same commit"
    );
}

/// Every operand fragment the disassembler emits across the whole opcode set
/// is understood by [`label_expectation`].
///
/// Separate from the order test so that adding an opcode whose listing uses a
/// new fragment shape fails with a precise message rather than being counted
/// as a disagreement.
#[test]
fn every_disassembler_operand_fragment_is_understood() {
    let mut seen: Vec<String> = Vec::new();
    for &op in all_opcodes().as_slice() {
        for token in operand_tokens(op) {
            assert!(
                label_expectation(&token).is_some(),
                "{op:?} ({}) renders unparsed fragment {token:?}",
                op as u8
            );
            if !seen.contains(&token) {
                seen.push(token);
            }
        }
    }
    // `Wide`, `LoopHeader` and `PopEnvironment` print nothing; every other
    // opcode prints at least one fragment, so the corpus covers all fragment
    // shapes the disassembler can currently produce.
    assert!(
        !seen.is_empty(),
        "no operand fragments were rendered at all"
    );
    println!("operand fragment shapes understood: {}", seen.join(" | "));
}

// ---------------------------------------------------------------------------
// Widths — the failure mode that sank the previous attempt at this table
// ---------------------------------------------------------------------------

/// Every opcode is one word except `Wide`, whose width is only knowable by
/// decoding its header.
#[test]
fn width_is_one_word_except_for_wide() {
    for &op in all_opcodes().as_slice() {
        let s = spec_of(op);
        match op {
            Opcode::Wide => assert_eq!(s.width, Width::WidePayload, "{op:?}"),
            _ => assert_eq!(s.width, Width::One, "{op:?}"),
        }
    }
}

/// `logical_width` agrees with the encoder: one word for every narrow opcode,
/// and the header-plus-payload count for a real `Wide` sequence.
#[test]
fn logical_width_matches_the_wide_encoder() {
    for &op in all_opcodes().as_slice() {
        if op == Opcode::Wide {
            continue;
        }
        let words = [Instr::new(op, A, B, C)];
        assert_eq!(spec_of(op).logical_width(&words), 1, "{op:?}");
    }

    let reg_ext = WideOp::RegExt {
        mask: 0b0011,
        a_hi: 0,
        b_hi: 0,
        c_hi: 0,
    }
    .encode();
    assert_eq!(reg_ext.len(), 2, "RegExt is a header plus one payload word");
    assert_eq!(spec_of(Opcode::Wide).logical_width(&reg_ext), 2);

    let load_const = WideOp::LoadConstW {
        dst: 1,
        const_id: 2,
    }
    .encode();
    assert_eq!(load_const.len(), 3);
    assert_eq!(spec_of(Opcode::Wide).logical_width(&load_const), 3);

    let load_int = WideOp::LoadIntW { dst: 1, value: 2 }.encode();
    assert_eq!(load_int.len(), 4);
    assert_eq!(spec_of(Opcode::Wide).logical_width(&load_int), 4);

    // A header the decoder rejects must not over-report: `map_or(1, …)`.
    let unknown_disc = [Instr::new(Opcode::Wide, 0, 0, 0xEE)];
    assert_eq!(spec_of(Opcode::Wide).logical_width(&unknown_disc), 1);
}

/// `role_at` / `used_slots` agree with the three named fields, and the
/// byte-index-shaped accessors a consumer walking raw words uses line up with
/// `a`/`b`/`c`.
#[test]
fn slot_accessors_agree_with_the_struct_fields() {
    for &op in all_opcodes().as_slice() {
        let s = spec_of(op);
        assert_eq!(s.role_at(Slot::A), s.a, "{op:?}");
        assert_eq!(s.role_at(Slot::B), s.b, "{op:?}");
        assert_eq!(s.role_at(Slot::C), s.c, "{op:?}");
        assert_eq!(s.role_at_index(0), s.a, "{op:?}");
        assert_eq!(s.role_at_index(1), s.b, "{op:?}");
        assert_eq!(s.role_at_index(2), s.c, "{op:?}");
        assert_eq!(OpSpec::index_of(Slot::A), 0);
        assert_eq!(OpSpec::index_of(Slot::B), 1);
        assert_eq!(OpSpec::index_of(Slot::C), 2);
        assert_eq!(Slot::ALL.len(), 3);
    }
}

/// `slot_is_register` agrees with `used_slots`: a register is always a live
/// slot, and no immediate or unused slot is a register.
///
/// This does NOT validate the register/immediate split against the emitter —
/// `v12-bccompiler`'s `reg_mask` is invisible from here. It pins the
/// internal consistency of the derivation, which is what phase B will consume.
#[test]
fn register_slots_are_a_subset_of_used_slots() {
    for &op in all_opcodes().as_slice() {
        let s = spec_of(op);
        let used = s.used_slots();
        for i in 0..3 {
            assert!(
                !s.index_is_register(i) || used[i],
                "{op:?} slot {i} is a register but not marked used"
            );
        }
        // Spot-check the three cases the derivation gets wrong if `Role` grows
        // a register variant without updating `slot_is_register`.
        assert!(
            s.slot_is_register(Slot::A) == s.index_is_register(0),
            "{op:?}"
        );
        assert!(
            s.slot_is_register(Slot::B) == s.index_is_register(1),
            "{op:?}"
        );
        assert!(
            s.slot_is_register(Slot::C) == s.index_is_register(2),
            "{op:?}"
        );
    }
    // Known immediate slots: nothing here may be reported as a register.
    for &(op, slot) in &[
        (Opcode::Call, Slot::C),
        (Opcode::NewArray, Slot::C),
        (Opcode::GetEnvSlot, Slot::B),
        (Opcode::SetEnvSlot, Slot::A),
        (Opcode::Closure, Slot::B),
        (Opcode::CopyArrayRest, Slot::C),
    ] {
        assert!(
            !spec_of(op).slot_is_register(slot),
            "{op:?} slot {slot:?} is an immediate, not a register"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-opcode coverage report
// ---------------------------------------------------------------------------

/// Prints the full 73-row cross-check table and asserts its shape. Run with
/// `cargo test -p v12-bytecode -- --nocapture` (nextest captures stdout) to
/// read the report.
///
/// This exists so "the test covers all 73" is a claim with output behind it
/// rather than an assertion about a count.
#[test]
fn per_opcode_coverage_report() {
    let (slot_divergent, label_divergent) = disagreements();
    let slot_divergent: Vec<Opcode> = slot_divergent.iter().map(|(op, _)| *op).collect();
    let label_divergent: Vec<Opcode> = label_divergent.iter().map(|(op, _)| *op).collect();
    let mut rows: Vec<String> = Vec::with_capacity(OPCODE_COUNT);
    for &op in all_opcodes().as_slice() {
        let s = spec_of(op);
        let printed = render_operands(op, Instr::new(op, A, B, C));
        let verdict = match (slot_divergent.contains(&op), label_divergent.contains(&op)) {
            (false, false) => "ok".to_string(),
            (true, false) => "slot divergence (documented)".to_string(),
            (false, true) => "label divergence (documented)".to_string(),
            (true, true) => "slot+label divergence (documented)".to_string(),
        };
        rows.push(format!(
            "{:>3}  {:<20} a={:<9} b={:<9} c={:<9}  disasm{:<28} {}",
            op as u8,
            format!("{op:?}"),
            format!("{:?}", s.a),
            format!("{:?}", s.b),
            format!("{:?}", s.c),
            printed,
            verdict,
        ));
    }
    assert_eq!(rows.len(), OPCODE_COUNT);
    for (i, row) in rows.iter().enumerate() {
        assert!(row.contains(&format!("{:>3}", all_opcodes()[i] as u8)));
    }
    println!(
        "\noperand-contract cross-check — {} opcodes, {} slot divergences, {} label divergences\n{}",
        rows.len(),
        slot_divergent.len(),
        label_divergent.len(),
        rows.join("\n")
    );
    // Distinct opcodes: an entry may be in both lists (NewEnvironment is),
    // and its single row says "BOTH".
    let mut distinct: Vec<Opcode> = slot_divergent.clone();
    distinct.extend(label_divergent.iter().copied());
    distinct.sort_by_key(|op| *op as u8);
    distinct.dedup();
    assert_eq!(
        rows.iter().filter(|r| r.contains("divergence")).count(),
        distinct.len()
    );
}

// ---------------------------------------------------------------------------
// The limit of this cross-check, asserted so it cannot be forgotten
// ---------------------------------------------------------------------------

/// Names every opcode whose operands are all *unlabelled* registers — the
/// class where the disassembler can prove the slot count but says nothing
/// about what each slot is FOR.
///
/// Swapping `Key` and `Obj` in `In`'s entry, or calling `GetProperty`'s `b`
/// an `Src` instead of an `Obj`, leaves the disassembler's output byte-identical.
/// No test in this crate can catch that, because the only other statements of
/// the contract live in other crates (`v12-bccompiler`'s `reg_mask`, the
/// dispatch arms in `v12-interp`), and depending on them from here would invert
/// the dependency direction this module exists to establish.
///
/// Those entries rest on the emit/read citations in `opspec.rs` and on review,
/// and this test keeps the size of that unverified surface visible and
/// non-zero. It is also the honest scope statement for phase B: `Width` — what
/// W2.2 derives `word_width` from — is fully checked, and so is the slot set;
/// only the role *kind* is not.
#[test]
fn role_kinds_are_citation_backed_for_unlabelled_register_operands() {
    let mut unverified: Vec<(u8, String)> = Vec::new();
    for &op in all_opcodes().as_slice() {
        let tokens = operand_tokens(op);
        if tokens.is_empty() {
            continue; // renders nothing: nothing to verify either way
        }
        // Every fragment must be a bare `r<n>`, i.e. no label constrains the
        // role kind in any slot.
        let all_bare = tokens.iter().all(|t| {
            t.strip_prefix('r')
                .is_some_and(|rest| printed_from_number(rest).is_some())
        });
        if all_bare {
            let s = spec_of(op);
            unverified.push((op as u8, format!("{op:?}({:?}/{:?}/{:?})", s.a, s.b, s.c)));
        }
    }
    assert!(
        !unverified.is_empty(),
        "no unlabelled operand found — if the disassembler started labelling \\
         every operand, delete this test and say so in the module docs"
    );
    let names: Vec<String> = unverified.iter().map(|(_, n)| n.clone()).collect();
    println!(
        "role kinds resting on emit/read citations only ({} opcodes): {}",
        names.len(),
        names.join(", ")
    );
}

// ---------------------------------------------------------------------------
// W2.3 — `validate()` against the table
// ---------------------------------------------------------------------------

/// A one-entry constant pool, so a `LoadConst` fixture is real bytecode.
///
/// `validate` rejects a constant id the function does not have, because
/// `Interp::const_value` panics on one. A fixture with an empty pool would
/// therefore be rejected for a reason that has nothing to do with the operand
/// under test.
fn fn_with_consts(instrs: Vec<Instr>, max_regs: u16) -> FunctionBytecode {
    let mut fb = FunctionBytecode::with_instructions(instrs, max_regs);
    let _ = fb.consts.insert(Const::F64(1.0));
    fb
}

/// `validate`'s verdict on a synthetic function, as a string for assertion
/// messages that name the reason.
fn verdict(fb: &FunctionBytecode) -> String {
    match fb.validate() {
        Ok(()) => "ok".to_string(),
        Err(err) => err.to_string(),
    }
}

// --- MUTATION 1: an environment slot past the environment it names ----------

/// MUTATION-1. `SetEnvSlot` naming slot 5 of a 2-slot environment is rejected.
///
/// This is the check the plan asks for by name ("a `SetEnvSlot` slot fits the
/// environment the function allocated"), and the *slot* is the decidable half
/// of it — the depth is not, for the reason `depth_beyond_this_function_is_not_checked`
/// documents. Two allocations are on the stack, so the depth here is not a
/// guess: `envs = [2, 2]`, depth 0 is the inner one.
#[test]
fn validate_rejects_an_env_slot_past_the_allocated_environment() {
    let mut instrs = vec![Instr::new(Opcode::NewEnvironment, 0, 2, 0)];
    instrs.push(Instr::new(Opcode::SetEnvSlot, 0, 5, 1));
    instrs.push(Instr::new(Opcode::Return, 1, 0, 0));
    let fb = fn_with_consts(instrs, 4);
    assert!(
        matches!(
            fb.validate(),
            Err(BytecodeError::EnvSlotOutOfRange {
                depth: 0,
                slot: 5,
                env_slots: 2,
                ..
            })
        ),
        "slot 5 of a 2-slot environment must be rejected, got: {}",
        verdict(&fb)
    );

    // The control: the same function with slot 1, the last valid index.
    let mut instrs = vec![Instr::new(Opcode::NewEnvironment, 0, 2, 0)];
    instrs.push(Instr::new(Opcode::SetEnvSlot, 0, 1, 1));
    instrs.push(Instr::new(Opcode::Return, 1, 0, 0));
    assert_eq!(verdict(&fn_with_consts(instrs, 4)), "ok");
}

/// The depth operand addresses the allocation *this function* made, so a slot
/// is checked against the right environment rather than the innermost one.
///
/// `NewEnvironment` twice — 2 slots then 5 — so depth 0 is the 5-slot
/// environment and depth 1 the 2-slot one. A check that always compared
/// against the innermost would let `slot=4` at depth 1 through.
#[test]
fn env_slot_check_follows_the_depth_it_is_given() {
    let base = |depth: u8, slot: u8| {
        let mut instrs = vec![
            Instr::new(Opcode::NewEnvironment, 0, 2, 0),
            Instr::new(Opcode::NewEnvironment, 0, 5, 0),
        ];
        instrs.push(Instr::new(Opcode::GetEnvSlot, 1, depth, slot));
        instrs.push(Instr::new(Opcode::Return, 1, 0, 0));
        fn_with_consts(instrs, 4)
    };

    assert_eq!(verdict(&base(0, 4)), "ok", "slot 4 fits the 5-slot env");
    assert_eq!(verdict(&base(1, 1)), "ok", "slot 1 fits the 2-slot env");
    let wrong_depth = base(1, 4);
    assert!(
        matches!(
            wrong_depth.validate(),
            Err(BytecodeError::EnvSlotOutOfRange {
                depth: 1,
                slot: 4,
                env_slots: 2,
                ..
            })
        ),
        "slot 4 does not fit the 2-slot env at depth 1, got: {}",
        verdict(&wrong_depth)
    );
    // Depth 2 is past both allocations: the environment it names is the one
    // the frame inherited, whose slot count is not in this function at all.
    assert_eq!(
        verdict(&base(2, 200)),
        "ok",
        "an inherited env is not checked"
    );
}

/// The counterexample that makes an environment-depth bound impossible, pinned
/// so the unsound check cannot come back.
///
/// This is the `<arrow>4` shape from `validate.rs`'s module docs, measured with
/// `v12 --disasm` on `3df63e3d`:
///
/// ```text
/// function o(){ let a=1; function m(){ let b=2; function inner(){ let c=3;
///                return ()=>c+b+a; } return inner(); } return m(); }
/// ```
///
/// The arrow pushes **nothing** and addresses depths 0, 1 and 2 — all three
/// environments allocated by functions whose bytecode it does not contain.
/// `depth <= own pushes` rejects this, so no depth bound is enforced; the slot
/// check is the decidable half, and it *skips* an inherited environment rather
/// than assuming one.
#[test]
fn depth_beyond_this_function_is_not_checked() {
    let mut instrs = Vec::new();
    for depth in 0..=2u8 {
        instrs.push(Instr::new(Opcode::GetEnvSlot, 1, depth, 7));
    }
    instrs.push(Instr::new(Opcode::Return, 1, 0, 0));
    let fb = fn_with_consts(instrs, 4);
    assert_eq!(fb.instrs.len(), 4);
    assert_eq!(
        verdict(&fb),
        "ok",
        "three environments up with zero allocations is legal bytecode"
    );
}

// --- MUTATION 2: a register operand past the frame --------------------------

/// MUTATION-2. A register operand at or past `max_regs` is rejected.
///
/// `Move r9, r1` in a 4-register frame: the interpreter writes
/// `self.stack[base + 9]`, three slots past the end of *this* frame's window —
/// another frame's registers, or past the slice.
#[test]
fn validate_rejects_a_register_operand_past_the_frame() {
    let fb = fn_with_consts(
        vec![
            Instr::new(Opcode::Move, 9, 1, 0),
            Instr::new(Opcode::Return, 1, 0, 0),
        ],
        4,
    );
    assert!(
        matches!(
            fb.validate(),
            Err(BytecodeError::RegisterOutOfRange {
                op: Opcode::Move,
                reg: 9,
                max_regs: 4,
                ..
            })
        ),
        "r9 in a 4-register frame must be rejected, got: {}",
        verdict(&fb)
    );
    // `max_regs` is a count, so the last valid register is `max_regs - 1`.
    let fb = fn_with_consts(
        vec![
            Instr::new(Opcode::Move, 3, 1, 0),
            Instr::new(Opcode::Return, 1, 0, 0),
        ],
        4,
    );
    assert_eq!(verdict(&fb), "ok");
}

/// The same bound applies to a `RegExt`-widened operand, which is the only way
/// a narrow instruction names a register above 255.
#[test]
fn register_range_sees_through_a_regext_prefix() {
    // mask 0b011 → `a` and `b` carry high bytes: r300 and r272.
    let regext = WideOp::RegExt {
        mask: 0b011,
        a_hi: 1,
        b_hi: 1,
        c_hi: 0,
    }
    .encode();
    let build = |max_regs: u16| {
        let mut instrs = regext.clone();
        instrs.push(Instr::new(Opcode::Move, 0x2C, 0x10, 0));
        instrs.push(Instr::new(Opcode::Return, 0, 0, 0));
        fn_with_consts(instrs, max_regs)
    };
    assert_eq!(verdict(&build(301)), "ok", "r300 fits a 301-register frame");
    let too_small = build(300);
    assert!(
        matches!(
            too_small.validate(),
            Err(BytecodeError::RegisterOutOfRange {
                reg: 300,
                max_regs: 300,
                ..
            })
        ),
        "the merged register must be range-checked, got: {}",
        verdict(&too_small)
    );
}

/// A wide op's register fields are bounded the same way, from the decoded
/// payload rather than a byte slot.
#[test]
fn wide_op_registers_are_range_checked() {
    let mut instrs = WideOp::LoadIntW { dst: 40, value: 7 }.encode();
    instrs.push(Instr::new(Opcode::Return, 0, 0, 0));
    assert_eq!(verdict(&fn_with_consts(instrs.clone(), 41)), "ok");

    let too_small = fn_with_consts(instrs, 8);
    assert!(
        matches!(
            too_small.validate(),
            Err(BytecodeError::RegisterOutOfRange {
                reg: 40,
                max_regs: 8,
                ..
            })
        ),
        "a wide dst past the frame must be rejected, got: {}",
        verdict(&too_small)
    );
}

/// An operand whose *role* is a register window is checked as one: the element
/// window `NewArray` reads, and the setter of the packed accessor pair.
#[test]
fn positional_windows_are_checked_past_their_base() {
    // `NewArray dst, base, len` reads stack[base .. base + len].
    let array = |base: u8, len: u8, max_regs: u16| {
        fn_with_consts(
            vec![
                Instr::new(Opcode::NewArray, 0, base, len),
                Instr::new(Opcode::Return, 0, 0, 0),
            ],
            max_regs,
        )
    };
    assert_eq!(
        verdict(&array(3, 4, 8)),
        "ok",
        "3 + 4 == 7 fits 8 registers"
    );
    let overflow = array(3, 4, 7);
    assert!(
        matches!(
            overflow.validate(),
            Err(BytecodeError::RegisterOutOfRange {
                reg: 7,
                max_regs: 7,
                ..
            })
        ),
        "the element window must fit the frame, got: {}",
        verdict(&overflow)
    );

    // `DefineAccessor obj, key, pair` reads stack[pair] and stack[pair + 1].
    let accessor = |pair: u8, max_regs: u16| {
        fn_with_consts(
            vec![
                Instr::new(Opcode::DefineAccessor, 0, 1, pair),
                Instr::new(Opcode::Return, 0, 0, 0),
            ],
            max_regs,
        )
    };
    assert_eq!(
        verdict(&accessor(2, 4)),
        "ok",
        "pair 2 + setter r3 fits r0..r3"
    );
    let overflow = accessor(3, 4);
    assert!(
        matches!(
            overflow.validate(),
            Err(BytecodeError::RegisterOutOfRange {
                reg: 4,
                max_regs: 4,
                ..
            })
        ),
        "the setter must fit the frame, got: {}",
        verdict(&overflow)
    );
}

/// The `Call` header window is the one positional operand deliberately *not*
/// checked: its size is `CALL_HEADER_REGS + argc`, and that constant belongs to
/// `v12-bccompiler`. This pins the gap so it stays visible rather than
/// silently absent — W2.4 routes `call_setup.rs` through the one
/// `CALL_HEADER_REGS`, which is what makes the check writable.
#[test]
fn the_call_header_window_is_a_known_gap_not_a_silent_one() {
    // callee r3 with argc 9 in a 4-register frame: the window reaches r14.
    let fb = fn_with_consts(
        vec![
            Instr::new(Opcode::Call, 0, 3, 9),
            Instr::new(Opcode::Return, 0, 0, 0),
        ],
        4,
    );
    assert_eq!(
        verdict(&fb),
        "ok",
        "only the callee register itself is checked today"
    );
    // The callee register *is* checked, so the check is not absent by accident.
    let bad_callee = fn_with_consts(
        vec![
            Instr::new(Opcode::Call, 0, 9, 0),
            Instr::new(Opcode::Return, 0, 0, 0),
        ],
        4,
    );
    assert!(bad_callee.validate().is_err());
}

/// A constant id the function cannot resolve is a rejection, not the panic
/// `Interp::const_value` raises.
#[test]
fn validate_rejects_a_const_id_past_the_pool() {
    let fb = fn_with_consts(
        vec![
            Instr::new_imm16(Opcode::LoadConst, 1, 5),
            Instr::new(Opcode::Return, 1, 0, 0),
        ],
        4,
    );
    assert!(
        matches!(
            fb.validate(),
            Err(BytecodeError::ConstIdOutOfRange { id: 5, pool: 1, .. })
        ),
        "k5 in a 1-entry pool must be rejected, got: {}",
        verdict(&fb)
    );
    let ok = fn_with_consts(
        vec![
            Instr::new_imm16(Opcode::LoadConst, 1, 0),
            Instr::new(Opcode::Return, 1, 0, 0),
        ],
        4,
    );
    assert_eq!(verdict(&ok), "ok");
}

/// A branch past the end of the stream is rejected; a branch *to* the end is
/// not, because that is the implicit `return undefined` completion point.
#[test]
fn branch_targets_may_reach_the_end_but_not_past_it() {
    let body = |target: u32| {
        let mut instrs = vec![Instr::new_imm24(Opcode::Jump, target)];
        instrs.push(Instr::new(Opcode::Return, 0, 0, 0));
        fn_with_consts(instrs, 2)
    };
    assert_eq!(verdict(&body(0)), "ok", "backward branch into the body");
    assert_eq!(verdict(&body(2)), "ok", "forward branch to the end");
    let past = body(3);
    assert!(
        matches!(
            past.validate(),
            Err(BytecodeError::BranchTargetOutOfRange { target: 3, .. })
        ),
        "a target past the stream must be rejected, got: {}",
        verdict(&past)
    );

    // Same for the 16-bit conditional form.
    let cond = |target: u16| {
        fn_with_consts(
            vec![
                Instr::new_imm16(Opcode::JumpIfFalse, 0, target),
                Instr::new(Opcode::Return, 0, 0, 0),
            ],
            2,
        )
    };
    assert_eq!(verdict(&cond(2)), "ok");
    assert!(cond(3).validate().is_err());
}

/// A `Flag` operand outside `{0, 1}` is rejected: it is read as `== 0` /
/// `== 1`, so any other value silently picks one of the two paths.
#[test]
fn validate_rejects_a_flag_that_is_neither_zero_nor_one() {
    for (op, flag_slot) in [(Opcode::Await, 2usize), (Opcode::IteratorClose, 1usize)] {
        let mut operands = [0u8; 3];
        operands[flag_slot] = 2;
        let fb = fn_with_consts(
            vec![
                Instr::new(op, operands[0], operands[1], operands[2]),
                Instr::new(Opcode::Return, 0, 0, 0),
            ],
            4,
        );
        assert!(
            matches!(
                fb.validate(),
                Err(BytecodeError::FlagNotBoolean {
                    op: bad,
                    value: 2,
                    ..
                }) if bad == op
            ),
            "{op:?} flag 2 must be rejected, got: {}",
            verdict(&fb)
        );
        operands[flag_slot] = 1;
        let ok = fn_with_consts(
            vec![
                Instr::new(op, operands[0], operands[1], operands[2]),
                Instr::new(Opcode::Return, 0, 0, 0),
            ],
            4,
        );
        assert_eq!(verdict(&ok), "ok", "{op:?} flag 1 is in the domain");
    }
}

/// Instructions that cannot be decoded at all are rejected here rather than
/// panicking in whichever dispatcher meets them.
#[test]
fn validate_rejects_an_undecodable_instruction_stream() {
    // 0xEE is an unassigned discriminant; `Instr::op` returns `None` and
    // `Interp::decode_instr` panics on it.
    let fb = fn_with_consts(vec![Instr(0xEE00_0000)], 2);
    assert!(
        matches!(
            fb.validate(),
            Err(BytecodeError::UnassignedOpcode { pc: 0, byte: 0xEE })
        ),
        "an unassigned opcode byte must be rejected, got: {}",
        verdict(&fb)
    );

    // A `RegExt` prefix with no narrow instruction to extend: the interpreter
    // panics on exactly this shape.
    let dangling = fn_with_consts(
        WideOp::RegExt {
            mask: 0b001,
            a_hi: 0,
            b_hi: 0,
            c_hi: 0,
        }
        .encode(),
        4,
    );
    assert!(
        matches!(
            dangling.validate(),
            Err(BytecodeError::TruncatedWide { .. })
        ),
        "a dangling RegExt must be rejected, got: {}",
        verdict(&dangling)
    );

    // Two prefixes in a row, and a prefix followed by a wide op: `emit_regs`
    // writes the pair atomically, so both are malformed.
    let mut doubled = WideOp::RegExt {
        mask: 0b001,
        a_hi: 0,
        b_hi: 0,
        c_hi: 0,
    }
    .encode();
    doubled.extend(
        WideOp::RegExt {
            mask: 0b001,
            a_hi: 0,
            b_hi: 0,
            c_hi: 0,
        }
        .encode(),
    );
    assert!(
        fn_with_consts(doubled, 4).validate().is_err(),
        "a doubled RegExt must be rejected"
    );

    let mut prefixed_wide = WideOp::RegExt {
        mask: 0b001,
        a_hi: 0,
        b_hi: 0,
        c_hi: 0,
    }
    .encode();
    prefixed_wide.extend(WideOp::LoadIntW { dst: 1, value: 0 }.encode());
    assert!(
        fn_with_consts(prefixed_wide, 4).validate().is_err(),
        "a RegExt in front of a wide op must be rejected"
    );
}

// --- MUTATION 3: the eight divergences, accepted as they are emitted ---------

/// MUTATION-3, the negative that matters: every opcode in
/// [`KNOWN_INCONSISTENT_OPERAND_ORDER`] **validates** in the divergent order it
/// is emitted in today.
///
/// A gate that rejected these would reject every function the compiler
/// currently produces, so this is the test that keeps the checks honest: they
/// read roles per slot out of the table, so a documented divergence is checked
/// in its current shape rather than exempted. Normalising an operand order
/// later must move the table and the emitters in the same commit; this test is
/// what fails if only one of them moves.
#[test]
fn every_documented_operand_order_divergence_validates_as_emitted() {
    // Each instruction below is written exactly as its emit site writes it,
    // including the eight entries' operand order. max_regs 4 keeps every
    // register inside the frame so the *only* thing under test is the order.
    let emitted = [
        // One real allocation, so the environment-slot check is *engaged* for
        // the two env opcodes below. That is what makes this test able to fail:
        // normalising `SetEnvSlot` in the table alone (to `(dst, depth, slot)`,
        // W2.4's shape) would read slot `5` out of the source register and
        // reject, which is precisely the half-move the gate should catch.
        Instr::new(Opcode::NewEnvironment, 0, 2, 0),
        // GetEnvSlot / SetEnvSlot: (dst, depth, slot) vs (depth, slot, src).
        Instr::new(Opcode::GetEnvSlot, 1, 0, 0),
        Instr::new(Opcode::SetEnvSlot, 0, 0, 5),
        // GetProperty / SetProperty: (dst, obj, key) vs (obj, key, val).
        Instr::new(Opcode::GetProperty, 1, 0, 2),
        Instr::new(Opcode::SetProperty, 0, 1, 2),
        Instr::new(Opcode::DeleteProperty, 1, 0, 2),
        Instr::new(Opcode::DefineMethod, 0, 1, 2),
        // binary operators / In: (dst, lhs, rhs) vs (dst, key, obj).
        Instr::new(Opcode::Add, 1, 0, 2),
        Instr::new(Opcode::In, 1, 0, 2),
        Instr::new(Opcode::InstanceOf, 1, 0, 2),
        // MergeObject: (unused, dst, src) — the destination is not in `a`.
        Instr::new(Opcode::MergeObject, 0, 1, 2),
        // SetPrototype: (unused, obj, proto).
        Instr::new(Opcode::SetPrototype, 0, 1, 2),
        // GetGlobal / SetGlobal: (dst, name) vs (src, name).
        Instr::new(Opcode::GetGlobal, 1, 0, 0),
        Instr::new(Opcode::SetGlobal, 1, 0, 0),
        // Call / CallApply: identical (dst, callee) prefix, `c` differs.
        Instr::new(Opcode::Call, 3, 0, 0),
        Instr::new(Opcode::CallApply, 3, 0, 1),
        // SetProperty / DefineMethod / DefineAccessor: `c` is a value or a pair.
        Instr::new(Opcode::DefineAccessor, 0, 1, 2),
    ];
    let mut instrs = emitted.to_vec();
    instrs.push(Instr::new(Opcode::Return, 0, 0, 0));
    let fb = fn_with_consts(instrs, 8);
    assert_eq!(
        verdict(&fb),
        "ok",
        "today's divergent operand order must keep validating"
    );

    // Pin the list's size: a ninth divergence has to arrive with its own line
    // here, because this test is the negative control for the whole gate.
    assert_eq!(
        KNOWN_INCONSISTENT_OPERAND_ORDER.len(),
        8,
        "KNOWN_INCONSISTENT_OPERAND_ORDER changed; extend this test in the same commit"
    );
}

/// An operand the table marks [`Role::Unused`] is *not* range-checked — that is
/// the point of naming it. `MergeObject`'s `a` is a literal `0` the emitter
/// writes and nothing reads; a listing or a peephole pass that leaves 200
/// there has not broken anything.
#[test]
fn unused_operands_are_not_required_to_be_in_range() {
    // Every opcode, with its `Unused` slots filled with a value far past the
    // frame and every other operand left at zero.
    let mut unchecked = 0usize;
    for &op in all_opcodes().as_slice() {
        if op == Opcode::Wide {
            // A `Wide` header is a payload-prefixed sequence, not a
            // single-instruction operand layout; its live slots are the mask
            // and the discriminant, neither of which is a register.
            continue;
        }
        let spec = spec_of(op);
        let unused: Vec<usize> = (0..3)
            .filter(|&i| {
                !spec.index_is_register(i) && matches!(spec.role_at_index(i), Role::Unused)
            })
            .collect();
        if unused.is_empty() {
            continue;
        }
        unchecked += 1;
        let mut bytes = [0u8; 3];
        for &slot in &unused {
            bytes[slot] = 200;
        }
        let mut instrs = vec![Instr::new(op, bytes[0], bytes[1], bytes[2])];
        instrs.push(Instr::new(Opcode::Return, 0, 0, 0));
        let fb = fn_with_consts(instrs, 8);
        assert_eq!(
            verdict(&fb),
            "ok",
            "{op:?} slots {unused:?} are dead operands, not registers"
        );
    }
    assert!(
        unchecked >= 20,
        "only {unchecked} opcodes have a dead operand — if the table changed, \
         say so here rather than leaving the count to drift"
    );
}

/// And the complement: for **every** opcode, the slots the table calls
/// registers *are* range-checked, so the table — not a hand-written list —
/// drives the check.
#[test]
fn every_register_role_is_range_checked() {
    let mut checked = 0usize;
    let mut no_registers = Vec::new();
    for &op in all_opcodes().as_slice() {
        if op == Opcode::Wide {
            // The header's own operands are a mask and a discriminant; its
            // payload's registers are covered by `wide_op_registers_are_range_checked`.
            no_registers.push(op);
            continue;
        }
        let spec = spec_of(op);
        let registers: Vec<usize> = (0..3).filter(|&i| spec.index_is_register(i)).collect();
        if registers.is_empty() {
            no_registers.push(op);
            continue;
        }
        checked += 1;
        let mut bytes = [0u8; 3];
        for &slot in &registers {
            bytes[slot] = 200;
        }
        let mut instrs = vec![Instr::new(op, bytes[0], bytes[1], bytes[2])];
        instrs.push(Instr::new(Opcode::Return, 0, 0, 0));
        let fb = fn_with_consts(instrs, 8);
        assert!(
            matches!(fb.validate(), Err(BytecodeError::RegisterOutOfRange { .. })),
            "{op:?} slots {registers:?} are registers and must be range-checked, \
             got: {}",
            verdict(&fb)
        );
    }
    assert!(
        checked >= 60,
        "only {checked} opcodes had a register role; the table changed shape"
    );
    // The five opcodes with no register operand at all: three carry no
    // operand, and `Jump`/`NewEnvironment` are all-immediate (`Jump`'s three
    // slots are one 24-bit target; `NewEnvironment`'s live slot is a slot
    // *count*, its `depth` being discarded by the interpreter).
    assert_eq!(
        no_registers,
        vec![
            Opcode::Wide,
            Opcode::Jump,
            Opcode::LoopHeader,
            Opcode::NewEnvironment,
            Opcode::PopEnvironment,
        ],
        "an opcode with no register role appeared; the sweep is not covering it"
    );
}

/// Every immediate role stays unchecked for range — `Count`, `Start`, `Imm8`,
/// `FnIndex` and `NameId` are all wide-reaching by design, and only `Flag`
/// states a domain of its own.
#[test]
fn immediate_roles_are_not_range_checked() {
    for (op, slot, value) in [
        // `Call`'s argc can exceed any frame size.
        (Opcode::Call, 2usize, 250u8),
        // `NewArray`'s element count likewise.
        (Opcode::NewArray, 2, 250),
        // `CopyArrayRest`'s start index.
        (Opcode::CopyArrayRest, 2, 250),
        // `LoadInt`'s immediate is a signed byte, not a register.
        (Opcode::LoadInt, 2, 250),
        // `Closure`'s function index.
        (Opcode::Closure, 1, 250),
        // `GetGlobal`'s name id.
        (Opcode::GetGlobal, 1, 250),
        // The env depth and slot, reached without any allocation.
        (Opcode::GetEnvSlot, 1, 250),
        (Opcode::SetEnvSlot, 0, 250),
    ] {
        let mut bytes = [0u8; 3];
        bytes[slot] = value;
        let mut instrs = vec![Instr::new(op, bytes[0], bytes[1], bytes[2])];
        instrs.push(Instr::new(Opcode::Return, 0, 0, 0));
        // The frame is deliberately wider than any immediate here, so the only
        // thing that could reject these is a check on the *immediate* — which
        // is what the test denies. (`NewArray`'s element window is a separate,
        // real bound, hence the generous frame.)
        let fb = fn_with_consts(instrs, 260);
        assert_eq!(
            verdict(&fb),
            "ok",
            "{op:?} slot {slot} is an immediate, not a register"
        );
    }
}

/// `Role::name` is what a rejection message quotes, so every role needs one and
/// no two roles may share a name — otherwise the diagnostic cannot identify
/// the operand it is complaining about.
#[test]
fn every_role_has_a_distinct_name() {
    let mut names: Vec<&'static str> = Vec::new();
    for op in all_opcodes() {
        for i in 0..3 {
            names.push(spec_of(op).role_at_index(i).name());
        }
    }
    names.sort_unstable();
    names.dedup();
    // 26 roles, and every one of them is reachable from some opcode.
    assert_eq!(
        names.len(),
        26,
        "role names must be distinct and non-empty: {names:?}"
    );
    assert!(!names.iter().any(|n| n.is_empty()));
}

/// W2.4: the call ABI layout has exactly one statement, in this crate.
///
/// Before this, `CALL_HEADER_REGS` lived in `v12-bccompiler::model` and the
/// interpreter open-coded `callee_slot + 2` at 16 sites. That was not a
/// cosmetic duplication: `v12-bccompiler` is an OPTIONAL dependency of
/// `v12-interp`, so the interpreter could not name the compiler's copy without
/// reintroducing the front-end as a runtime edge. Moving the constant here,
/// beside the `OpSpec` table that describes the same calling convention, is what
/// makes one statement possible at all.
///
/// Pin the value and pin why the header is NOT part of the operand count:
/// `Call`'s third operand is a `Count`, not a register, so the header window
/// `CALL_HEADER_REGS + argc` is not derivable from the opcode's operand roles —
/// which is exactly why `validate()` cannot check it from the table alone.
#[test]
fn call_header_regs_is_the_single_statement_of_the_call_window_layout() {
    assert_eq!(CALL_HEADER_REGS, 2, "call layout is [callee][this][args...]");

    let call = spec(Opcode::Call).expect("Call has a table row");
    assert_eq!(
        call.c,
        Role::Count,
        "Call's c operand is the argument count, not a register — so the \\
         CALL_HEADER_REGS + argc window cannot be checked from operand roles alone"
    );

    // The header is a property of the CONVENTION, not of any operand: the
    // callee's r0 is `this` and its r{i} is argument i-1, so arguments begin at
    // callee_reg + CALL_HEADER_REGS on the caller side with no copying.
    assert!(CALL_HEADER_REGS >= 2, "callee and this both precede the arguments");
}
