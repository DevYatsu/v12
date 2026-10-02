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
//! against `Opcode`'s hand-written `TryFrom<u8>`, so a new variant cannot be
//! absent from the sweep. The hand-maintained `ALL_OPS` list in `opcode.rs`
//! *is* stale (it stops at `ToPropertyKey` = 75), which is precisely why this
//! file does not reuse it.

use super::*;
use crate::opspec::{render_operands, spec, spec_at_byte};
use crate::{Instr, Opcode, WideOp, all_opcodes};

/// Number of assigned `Opcode` variants. A new variant must bump this, which
/// makes adding one a deliberate act rather than an accident.
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
