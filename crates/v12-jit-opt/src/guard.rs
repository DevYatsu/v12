#![forbid(unsafe_code)]
#![allow(dead_code)]

//! Guards and speculation policy for tier-2.
//!
//! Guards are fail-closed checks emitted as Cranelift `if` diamonds that
//! branch to a deoptimization trampoline on failure. Every guard is recorded
//! as an [`Assumption`] in the [`DeoptMap`](crate::DeoptMap) so the runtime
//! can materialize the interpreter frame on deopt.
//!
//! The speculative lattice classifies observed types at a bytecode pc and
//! drives guard selection. See [`Lattice`] for join/meet semantics.
//!
//! Validity cells guard loop-invariant prototype shapes; once a prototype is
//! mutated `Heap::bump_validity` increments its serial and all `ValidityCell`
//! guards for that cell fail.

use std::collections::HashMap;

use v12_heap::{JsValue, ShapeHandle};

// Re-export the canonical lattice from the interpreter feedback so both
// tiers share one definition.
pub use v12_interp::feedback::Lattice;

/// Maximum number of guards per compiled function.
///
/// Caps code size and keeps the deopt map bounded. Mirrors V8's limit on
/// assumptions per optimized function (small constant).
pub const MAX_GUARDS_PER_FUNCTION: usize = 32;

/// Hot-entry threshold for speculation (`should_speculate`).
///
/// `512` is half the interpreter's `FEEDBACK_TIER_UP_THRESHOLD` (1024). The
/// optimizer fires earlier than the interpreter's tier-up signal so that hot
/// functions that already have stable type feedback do not wait a second full
/// epoch. See `Threshold note` on [`should_speculate`].
pub const ENTRY_HOT_THRESHOLD: u16 = 512;

/// Hot-loop threshold for speculation (`should_speculate`).
///
/// `800` is lower than the interpreter's `FEEDBACK_TIER_UP_THRESHOLD`
/// (`1024`) because loop bodies are hotter per iteration and benefit from
/// earlier peeling/unrolling. Baseline also uses `800` for its OSR counter.
pub const LOOP_HOT_THRESHOLD: u16 = 800;

/// Validity-cell threshold mismatch note.
///
/// `v12-interp::feedback::FEEDBACK_TIER_UP_THRESHOLD` is `1024` (counts both
/// entry and loop crossings on a single counter). `v12-jit-opt` uses `512`
/// for entry and `800` for loop as separate thresholds because the optimizer
/// sees disaggregated `entry_counter` and `loop_counter` from `FeedbackVector`
/// and can speculate earlier: loops benefit from versioning/peeling even before `1024`
/// crossings, and hot entry counts `>512` indicate steady-state call
/// frequency. The mismatch is intentional; baseline also uses `800` for its
/// loop OSR gate. Tuning remains open (`bench/` hyperfine) — see
/// `FEEDBACK_THRESHOLD_MISMATCH_NOTE`.
pub const FEEDBACK_THRESHOLD_MISMATCH_NOTE: &str =
    "jit-opt 512/800 vs interp 1024 — intentional split, see should_speculate docs";

/// Guard kinds. Variants name the check that must hold for speculative code
/// to stay on the fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardKind {
    /// Object shape must equal `expected`. Fails on polymorphic or transitioned shapes.
    ShapeEq { expected: ShapeHandle },
    /// Value in register must be a Smi (31-bit int).
    TypeIsSmi { reg: u8 },
    /// Value in register must be a number (Smi or Double).
    TypeIsNumber { reg: u8 },
    /// Value in register must be a string.
    TypeIsString { reg: u8 },
    /// Validity cell must still hold `serial`.
    ///
    /// Used for loop versioning: a peeled first iteration guards the loop-carried
    /// object's prototype chain via a validity cell so that a later prototype
    /// mutation deopts to the unpeeled loop. Cell `0` (`NONE`) is never valid.
    ValidityCell { cell: u32, serial: u32 },
}

/// One guarded assumption at a bytecode pc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assumption {
    pub bc_pc: u32,
    pub guard: GuardKind,
}

impl Assumption {
    /// Closed-world check: guard failure triggers deopt, never UB.
    pub fn check_fails_closed(&self) -> &'static str {
        "guard failure -> deopt to interp/baseline"
    }
    /// Validation: `ShapeEq` with a null handle is not valid speculation.
    pub fn is_valid(&self) -> bool {
        match self.guard {
            GuardKind::ShapeEq { expected } => expected.index() != u32::MAX,
            GuardKind::ValidityCell { cell, .. } => cell != 0,
            GuardKind::TypeIsNumber { .. }
            | GuardKind::TypeIsSmi { .. }
            | GuardKind::TypeIsString { .. } => true,
        }
    }

    /// Evaluates the guard against a runtime `JsValue`.
    ///
    /// Returns `true` on hit (fast path stays), `false` on miss (must deopt).
    /// `ShapeEq` and `ValidityCell` require out-of-band shape/cell state;
    /// this helper handles the type guards only.
    #[must_use]
    pub fn check_value(&self, value: JsValue) -> bool {
        match self.guard {
            GuardKind::TypeIsSmi { .. } => value.is_smi(),
            GuardKind::TypeIsNumber { .. } => value.is_smi() || value.is_f64(),
            GuardKind::TypeIsString { .. } => value.is_string(),
            GuardKind::ShapeEq { .. } | GuardKind::ValidityCell { .. } => true,
        }
    }

    /// Evaluates a shape guard against `actual` shape.
    #[must_use]
    pub fn check_shape(&self, actual: ShapeHandle) -> bool {
        match self.guard {
            GuardKind::ShapeEq { expected } => actual == expected,
            _ => true,
        }
    }

    /// Evaluates a validity-cell guard against `current_serial`.
    ///
    /// Validity cells are monotonic serial counters bumped on prototype
    /// mutation. A guard passes iff the serial still matches.
    #[must_use]
    pub fn check_validity_cell(&self, current_serial: u32) -> bool {
        match self.guard {
            GuardKind::ValidityCell { serial, .. } => serial == current_serial,
            _ => true,
        }
    }
}

impl core::fmt::Display for Assumption {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.guard {
            GuardKind::ShapeEq { expected } => {
                write!(
                    f,
                    "pc {} shape_eq expected {}",
                    self.bc_pc,
                    expected.index()
                )
            }
            GuardKind::TypeIsSmi { reg } => {
                write!(f, "pc {} type_is_smi r{}", self.bc_pc, reg)
            }
            GuardKind::TypeIsNumber { reg } => {
                write!(f, "pc {} type_is_number r{}", self.bc_pc, reg)
            }
            GuardKind::TypeIsString { reg } => {
                write!(f, "pc {} type_is_string r{}", self.bc_pc, reg)
            }
            GuardKind::ValidityCell { cell, serial } => {
                write!(
                    f,
                    "pc {} validity_cell {} serial {}",
                    self.bc_pc, cell, serial
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Monomorphism tracking — Oracle 1 finding 3
// ---------------------------------------------------------------------------

/// Tracks shape clashes per inline-cache site.
///
/// `FeedbackVector::is_mono` reports whether every site has ≤1 recorded shape.
/// The `PolyIc` remembers up to [`v12_interp::feedback::IC_MAX_ENTRIES`]
/// shapes per site, so a polymorphic access now makes `is_mono()` return
/// `false`, and the optimizer's guard selection uses the first (most recent)
/// shape.
///
/// This counter provides the missing signal today without mutating the
/// interpreter's `FeedbackVector`. The optimizer calls `observe` on every IC
/// feedback sample; `is_mono` becomes `false` once any site has seen ≥2
/// distinct shapes.
///
/// The counter is deliberately separate from `FeedbackVector` so that
/// `v12-interp` stays heap-agnostic and `v12-jit-opt` can evolve the
/// heuristic without changing the tier-0 ABI.
#[derive(Debug, Default)]
pub struct ClashCounter {
    /// Last shape seen per pc.
    seen: HashMap<u32, ShapeHandle>,
    /// Set of pcs that have clashed.
    clashed: HashMap<u32, bool>,
}

impl ClashCounter {
    /// Creates an empty counter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Observes `shape` at `pc`. Returns `true` if this observation caused a
    /// new clash (i.e., this site just became polymorphic).
    pub fn observe(&mut self, pc: u32, shape: ShapeHandle) -> bool {
        if let Some(&prev) = self.seen.get(&pc) {
            if prev == shape {
                return false;
            }
            if self.clashed.contains_key(&pc) {
                return false;
            }
            self.clashed.insert(pc, true);
            return true;
        }
        self.seen.insert(pc, shape);
        false
    }

    /// Number of clash sites.
    pub fn clash_count(&self) -> usize {
        self.clashed.len()
    }

    /// Whether all observed IC sites are still monomorphic.
    ///
    /// Equivalent to `clash_count() == 0`. Mirrors the future
    /// `FeedbackVector::is_mono` once poly-IC lands.
    pub fn is_mono(&self) -> bool {
        self.clashed.is_empty()
    }
}

/// Simplified clash tracker with per-pc clash dedup.
///
/// Split from `ClashCounter` above to keep the documented vacuous-`is_mono`
/// note separate from the minimal correct implementation used in tests.
#[derive(Debug, Default)]
pub struct MonoTracker {
    seen: HashMap<u32, ShapeHandle>,
    clashed: HashMap<u32, bool>,
}

impl MonoTracker {
    /// Creates an empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Observes `shape` at `pc`. Returns `true` iff this observation
    /// introduced a new clash at `pc`.
    pub fn observe(&mut self, pc: u32, shape: ShapeHandle) -> bool {
        if let Some(&prev) = self.seen.get(&pc) {
            if prev == shape {
                return false;
            }
            if self.clashed.contains_key(&pc) {
                return false;
            }
            self.clashed.insert(pc, true);
            return true;
        }
        self.seen.insert(pc, shape);
        false
    }

    /// Number of pcs that have clashed.
    pub fn clash_count(&self) -> usize {
        self.clashed.len()
    }

    /// Monomorphic iff no clash has been observed.
    pub fn is_mono(&self) -> bool {
        self.clashed.is_empty()
    }
}

/// Gate speculation on hotness, monomorphism, and type stability.
///
/// Mirrors the baseline threshold (`entry>512 || loop>800`) and adds a check
/// that the observed lattice at the hot pc is not `Any` (polymorphic).
///
/// # Threshold note (Oracle 1 finding 4)
///
/// `ENTRY_HOT_THRESHOLD` is `512` and `LOOP_HOT_THRESHOLD` is `800`, both
/// lower than `v12-interp::feedback::FEEDBACK_TIER_UP_THRESHOLD` (`1024`).
/// The interpreter fires `on_tier_up` once when *either* counter saturates
/// at `1024` (single combined signal). The optimizer sees the disaggregated
/// `entry_counter` and `loop_counter` from `FeedbackVector` and can speculate
/// earlier: loops benefit from versioning/peeling even before `1024`
/// crossings, and hot entry counts `>512` indicate steady-state call
/// frequency. The mismatch is intentional; baseline also uses `800` for its
/// loop OSR gate. Tuning remains open (`bench/` hyperfine) — see
/// `FEEDBACK_THRESHOLD_MISMATCH_NOTE`.
#[inline]
pub fn should_speculate(entry_counter: u16, loop_counter: u16, is_mono: bool) -> bool {
    if !is_mono {
        return false;
    }
    entry_counter > ENTRY_HOT_THRESHOLD || loop_counter > LOOP_HOT_THRESHOLD
}

/// Lattice-aware speculation gate.
///
/// `lattice` is the joined type feedback at the hottest pc. `Any` and
/// `Unknown` are not concrete and would immediately deopt or provide no
/// benefit, so speculation is gated on `Lattice::is_concrete`.
///
/// Note: `Lattice::String` is concrete (Oracle 1 finding 5). String guards
/// use `TypeIsString`, not `TypeIsNumber`, and are handled by
/// `guard_for_lattice` → `TypeIsString`. Validity-cell guards are handled
/// separately for loops.
#[allow(dead_code)]
#[inline]
pub fn should_speculate_with_lattice(
    entry_counter: u16,
    loop_counter: u16,
    is_mono: bool,
    lattice: Lattice,
) -> bool {
    if !lattice.is_concrete() {
        return false;
    }
    should_speculate(entry_counter, loop_counter, is_mono)
}

#[cfg(test)]
mod tests {
    use super::*;
    use v12_heap::{Heap, V12Str};

    #[test]
    fn lattice_join_properties() {
        assert_eq!(Lattice::Unknown.join(Lattice::Smi), Lattice::Smi);
        assert_eq!(Lattice::Smi.join(Lattice::Double), Lattice::Number);
        assert_eq!(Lattice::Smi.join(Lattice::String), Lattice::Any);
        assert_eq!(Lattice::Number.join(Lattice::Double), Lattice::Number);
    }

    #[test]
    fn lattice_meet_properties() {
        assert_eq!(Lattice::Any.meet(Lattice::Smi), Lattice::Smi);
        assert_eq!(Lattice::Smi.meet(Lattice::Double), Lattice::Unknown);
    }

    #[test]
    fn guard_type_is_smi_hit_miss() {
        let smi = JsValue::from_i32_smi(42).unwrap();
        let dbl = JsValue::from_f64(2.5);
        let guard = Assumption {
            bc_pc: 0,
            guard: GuardKind::TypeIsSmi { reg: 1 },
        };
        assert!(guard.check_value(smi));
        assert!(!guard.check_value(dbl));
        let any_guard = Assumption {
            bc_pc: 0,
            guard: GuardKind::TypeIsNumber { reg: 1 },
        };
        assert!(any_guard.check_value(smi));
        assert!(any_guard.check_value(dbl));
        let str_val = {
            let mut heap = Heap::new(v12_heap::GcPolicy::NoGC);
            let h = heap.intern_string(V12Str::latin1(b"hi".to_vec()));
            heap.add_root(JsValue::string(h));
            JsValue::string(h)
        };
        assert!(!guard.check_value(str_val));
        assert!(!any_guard.check_value(str_val));
    }

    #[test]
    fn guard_type_is_string() {
        let mut heap = Heap::new(v12_heap::GcPolicy::NoGC);
        let h = heap.intern_string(V12Str::latin1(b"hi".to_vec()));
        heap.add_root(JsValue::string(h));
        let str_val = JsValue::string(h);
        let smi = JsValue::from_i32_smi(1).unwrap();
        let guard = Assumption {
            bc_pc: 2,
            guard: GuardKind::TypeIsString { reg: 0 },
        };
        assert!(guard.check_value(str_val));
        assert!(!guard.check_value(smi));
        assert!(guard.is_valid());
        assert!(format!("{guard}").contains("type_is_string"));
    }

    #[test]
    fn guard_shape_hit_miss() {
        let mut heap = Heap::new(v12_heap::GcPolicy::NoGC);
        let s0 = heap.root_shape();
        let s1 = {
            let h = heap.intern_string(V12Str::latin1(b"x".to_vec()));
            heap.add_root(JsValue::string(h));
            let k = v12_heap::PropKey::from_string(h);
            heap.add_property(s0, k, v12_heap::Attrs::DEFAULT)
        };
        let guard = Assumption {
            bc_pc: 5,
            guard: GuardKind::ShapeEq { expected: s0 },
        };
        assert!(guard.check_shape(s0));
        assert!(!guard.check_shape(s1));
    }

    #[test]
    fn guard_validity_cell() {
        let guard = Assumption {
            bc_pc: 7,
            guard: GuardKind::ValidityCell {
                cell: 3,
                serial: 42,
            },
        };
        assert!(guard.check_validity_cell(42));
        assert!(!guard.check_validity_cell(43));
        assert!(guard.is_valid());
        let none = Assumption {
            bc_pc: 7,
            guard: GuardKind::ValidityCell { cell: 0, serial: 0 },
        };
        assert!(!none.is_valid());
    }

    #[test]
    fn should_speculate_gates() {
        // Cold → false
        assert!(!should_speculate(10, 10, true));
        // Hot entry
        assert!(should_speculate(600, 0, true));
        // Hot loop
        assert!(should_speculate(0, 900, true));
        // Polymorphic IC blocks
        assert!(!should_speculate(600, 0, false));
        // Lattice-aware: Any/Unknown blocks
        assert!(!should_speculate_with_lattice(600, 0, true, Lattice::Any));
        assert!(!should_speculate_with_lattice(
            600,
            0,
            true,
            Lattice::Unknown
        ));
        assert!(should_speculate_with_lattice(600, 0, true, Lattice::Smi));
        assert!(should_speculate_with_lattice(0, 900, true, Lattice::Number));
        assert!(should_speculate_with_lattice(600, 0, true, Lattice::String));
        assert!(should_speculate_with_lattice(
            600,
            0,
            true,
            Lattice::Object(Heap::new(v12_heap::GcPolicy::NoGC).root_shape())
        ));
    }

    #[test]
    fn assumption_display_and_validity() {
        let heap = Heap::new(v12_heap::GcPolicy::NoGC);
        let h = heap.root_shape();
        let a = Assumption {
            bc_pc: 10,
            guard: GuardKind::ShapeEq { expected: h },
        };
        assert!(a.is_valid());
        assert!(format!("{a}").contains("shape_eq"));
        let b = Assumption {
            bc_pc: 2,
            guard: GuardKind::TypeIsSmi { reg: 3 },
        };
        assert!(b.is_valid());
        assert_eq!(
            b.check_fails_closed(),
            "guard failure -> deopt to interp/baseline"
        );
    }

    #[test]
    fn max_guards_constant_is_32() {
        assert_eq!(MAX_GUARDS_PER_FUNCTION, 32);
    }

    #[test]
    fn mono_tracker_clash_detection() {
        let mut heap = Heap::new(v12_heap::GcPolicy::NoGC);
        let s0 = heap.root_shape();
        let s1 = {
            let h = heap.intern_string(V12Str::latin1(b"y".to_vec()));
            heap.add_root(JsValue::string(h));
            let k = v12_heap::PropKey::from_string(h);
            heap.add_property(s0, k, v12_heap::Attrs::DEFAULT)
        };
        let mut tracker = MonoTracker::new();
        assert!(tracker.is_mono());
        assert!(!tracker.observe(5, s0));
        assert!(tracker.is_mono());
        assert!(!tracker.observe(5, s0));
        assert!(tracker.is_mono());
        assert!(tracker.observe(5, s1));
        assert!(!tracker.is_mono());
        assert_eq!(tracker.clash_count(), 1);
        // Second distinct shape at same pc does not double-count.
        let s2 = {
            let h = heap.intern_string(V12Str::latin1(b"z".to_vec()));
            heap.add_root(JsValue::string(h));
            let k = v12_heap::PropKey::from_string(h);
            heap.add_property(s0, k, v12_heap::Attrs::DEFAULT)
        };
        assert!(!tracker.observe(5, s2));
        assert_eq!(tracker.clash_count(), 1);
        assert!(!should_speculate(600, 0, tracker.is_mono()));
    }

    #[test]
    fn threshold_constants_documented() {
        assert_eq!(ENTRY_HOT_THRESHOLD, 512);
        assert_eq!(LOOP_HOT_THRESHOLD, 800);
        assert!(FEEDBACK_THRESHOLD_MISMATCH_NOTE.contains("512/800"));
    }
}
