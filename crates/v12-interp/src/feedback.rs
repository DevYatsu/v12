#![forbid(unsafe_code)]

//! Per-function execution feedback: saturating heat counters, the
//! monomorphic property-access inline cache, and per-opcode type feedback
//! used by the tier-2 optimizer for speculative specialization.
//!
//! The interpreter records observations, it does not act on them beyond
//! raising a flag: the execution driver owns tier transitions and learns
//! about them through [`TierHooks::on_tier_up`], invoked between frame
//! completions. Nothing here ever references the JIT crates.
//!
//! Type feedback is a per-pc lattice that classifies observed values at
//! selected bytecodes (arithmetic results, property loads). The lattice
//! drives guard selection in `v12-jit-opt`.

use std::collections::HashMap;

use v12_heap::{JsValue, ShapeHandle};

/// Loop-iteration / function-entry count at which a function is reported as
/// hot. 1024 iterations is the classic "this loop is worth compiling" signal:
/// high enough that cold one-shot loops never trigger bookkeeping work in the
/// driver, low enough to fire within milliseconds of sustained heat.
pub(crate) const FEEDBACK_TIER_UP_THRESHOLD: u16 = 1024;

/// One inline-cache site: the last shape seen at the access and the slot its
/// descriptor names. Monomorphic by design — a new shape simply replaces the
/// old entry, and every hit re-validates before trusting `slot`.
#[derive(Clone, Copy, Debug)]
pub struct MonoIc {
    pub shape: ShapeHandle,
    pub slot: u32,
}

/// Type lattice for speculative specialization.
///
/// Ordering (least to greatest):
/// `Unknown` is bottom (no observation yet), concrete types (`Smi`, `Double`,
/// `String`, `Object`) and the intermediate `Number` sit in the middle,
/// `Any` is top (conflicting or polymorphic observations).
///
/// Join is least upper bound, meet is greatest lower bound. The lattice is
/// flat except for the `Smi`/`Double` → `Number` diamond.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lattice {
    /// No observation yet.
    Unknown,
    /// 31-bit tagged integer.
    Smi,
    /// Heap-number double.
    Double,
    /// Join of `Smi` and `Double`.
    Number,
    /// String value.
    String,
    /// Object with a known shape.
    Object(ShapeHandle),
    /// Conflicting or unclassifiable (top).
    Any,
}

impl Lattice {
    /// Least upper bound.
    #[must_use]
    pub fn join(self, other: Self) -> Self {
        use Lattice::{Any, Double, Number, Object, Smi, String, Unknown};
        if self == other {
            return self;
        }
        match (self, other) {
            (Unknown, x) | (x, Unknown) => x,
            (Any, _) | (_, Any) => Any,
            // Smi-Double diamond.
            (Smi, Double) | (Double, Smi) => Number,
            (Smi, Number) | (Number, Smi) | (Double, Number) | (Number, Double) => Number,
            (Number, Number) => Number, // handled by equality above, kept for exhaustiveness.
            // Strings only join with themselves (equality handled).
            (String, _) | (_, String) => Any,
            // Objects: same shape stays, different shapes diverge to Any.
            (Object(a), Object(b)) => {
                if a == b {
                    Object(a)
                } else {
                    Any
                }
            }
            // Any cross-kind not covered above (e.g., Smi vs String, Number vs Object).
            _ => Any,
        }
    }

    /// Greatest lower bound.
    #[must_use]
    pub fn meet(self, other: Self) -> Self {
        use Lattice::{Any, Double, Number, Object, Smi, String, Unknown};
        if self == other {
            return self;
        }
        match (self, other) {
            (Unknown, _) | (_, Unknown) => Unknown,
            (Any, x) | (x, Any) => x,
            // Smi and Double are incomparable except via Number.
            (Smi, Double) | (Double, Smi) => Unknown,
            (Smi, Number) | (Number, Smi) => Smi,
            (Double, Number) | (Number, Double) => Double,
            (String, _) | (_, String) => Unknown,
            (Object(a), Object(b)) => {
                if a == b {
                    Object(a)
                } else {
                    Unknown
                }
            }
            _ => Unknown,
        }
    }

    /// Whether this lattice is the bottom element.
    #[inline]
    #[must_use]
    pub fn is_unknown(self) -> bool {
        matches!(self, Self::Unknown)
    }

    /// Whether this lattice is the top element.
    #[inline]
    #[must_use]
    pub fn is_any(self) -> bool {
        matches!(self, Self::Any)
    }

    /// Whether this lattice denotes a concrete, speculatable type.
    ///
    /// Concrete means the feedback has observed a single kind that the
    /// optimizer can emit a guard for: `Smi`, `Double`, `Number`, `String`,
    /// or `Object(shape)`. Both `Unknown` (bottom) and `Any` (top) are not
    /// concrete because there is nothing stable to specialize on.
    #[inline]
    #[must_use]
    pub fn is_concrete(self) -> bool {
        matches!(
            self,
            Self::Smi | Self::Double | Self::Number | Self::String | Self::Object(_)
        )
    }

    /// Classifies a runtime value into a lattice element.
    ///
    /// Objects are mapped to `Object(shape)` when a shape is known;
    /// otherwise they become `Any`. This avoids polluting the lattice with
    /// unknown objects that the optimizer cannot specialize.
    #[must_use]
    pub fn from_value(value: JsValue, shape: Option<ShapeHandle>) -> Self {
        if value.is_smi() {
            Self::Smi
        } else if value.is_f64() {
            Self::Double
        } else if value.is_string() {
            Self::String
        } else if value.is_object() {
            if let Some(h) = shape {
                Self::Object(h)
            } else {
                Self::Any
            }
        } else {
            // undefined, null, boolean, symbol, bigint, holes → Any for now.
            // The optimizer never speculates on these at arithmetic sites.
            Self::Any
        }
    }
}

/// Feedback state for one program function. Allocated lazily on first
/// execution; counters saturate so long-running loops stop asking.
#[derive(Default)]
pub struct FeedbackVector {
    /// Inline caches keyed by the pc of their `GetProperty` instruction.
    pub ics: HashMap<u32, MonoIc>,
    /// Per-opcode type feedback keyed by bytecode pc.
    pub type_feedback: HashMap<u32, Lattice>,
    /// Saturating count of loop-header crossings.
    pub loop_counter: u16,
    /// Saturating count of activations.
    pub entry_counter: u16,
}

impl FeedbackVector {
    fn bump(counter: &mut u16) -> bool {
        let old = *counter;
        *counter = old.saturating_add(1);
        // Fire exactly once: at the crossing, not on every saturated tick.
        old < FEEDBACK_TIER_UP_THRESHOLD && *counter >= FEEDBACK_TIER_UP_THRESHOLD
    }

    /// Counts one activation; `true` exactly at the threshold crossing.
    pub fn activated(&mut self) -> bool {
        Self::bump(&mut self.entry_counter)
    }

    /// Counts one loop-header crossing; `true` exactly at the crossing.
    pub fn crossing_loop(&mut self) -> bool {
        Self::bump(&mut self.loop_counter)
    }

    /// Records a type observation at `pc`, joining with any prior value.
    pub fn record_type(&mut self, pc: u32, lattice: Lattice) {
        self.type_feedback
            .entry(pc)
            .and_modify(|cur| *cur = cur.join(lattice))
            .or_insert(lattice);
    }

    /// Returns the lattice at `pc`, or `Unknown` if none recorded.
    #[must_use]
    pub fn type_at(&self, pc: u32) -> Lattice {
        self.type_feedback
            .get(&pc)
            .copied()
            .unwrap_or(Lattice::Unknown)
    }

    /// Whether the inline caches are monomorphic (at most one shape per site).
    ///
    /// The current representation is already monomorphic per site, so this
    /// returns `true` when no site has conflicting shapes. It is retained as
    /// an explicit predicate for the optimizer gate.
    #[must_use]
    pub fn is_mono(&self) -> bool {
        // With MonoIc we store only the last shape, so mono is trivially true.
        // A future polymorphic IC would change this predicate.
        true
    }

    /// Accessor for tests: number of type-feedback entries.
    #[cfg(test)]
    pub fn type_feedback_len(&self) -> usize {
        self.type_feedback.len()
    }
}

/// Driver-facing seam: called between frame completions when a function
/// crossed [`FEEDBACK_TIER_UP_THRESHOLD`]. The default implementation is a
/// no-op; the engine overrides it when tiers exist to hand work to.
pub trait TierHooks {
    /// A function became hot and should be scheduled for compilation.
    fn on_tier_up(&mut self, function_index: u32) {
        let _ = function_index;
    }
}

impl TierHooks for () {}

/// `typeof` result strings indexed by the interpreter's internal type tags
/// (see `type_tag`): undefined, boolean, number, string, bigint, symbol,
/// object, function.
pub(crate) const TYPE_NAMES: [&str; 8] = [
    "undefined",
    "boolean",
    "number",
    "string",
    "bigint",
    "symbol",
    "object",
    "function",
];

/// Number of distinct `typeof` classifications (callable objects split out).
pub(crate) const TYPE_NAME_COUNT: usize = TYPE_NAMES.len();

#[cfg(test)]
mod tests {
    use super::*;
    use v12_heap::{Heap, JsValue, V12Str};

    #[test]
    fn lattice_join_unknown_is_identity() {
        for lat in [
            Lattice::Unknown,
            Lattice::Smi,
            Lattice::Double,
            Lattice::Number,
            Lattice::String,
            Lattice::Any,
        ] {
            assert_eq!(Lattice::Unknown.join(lat), lat);
            assert_eq!(lat.join(Lattice::Unknown), lat);
        }
    }

    #[test]
    fn lattice_join_any_is_absorbing() {
        for lat in [
            Lattice::Unknown,
            Lattice::Smi,
            Lattice::Double,
            Lattice::Number,
            Lattice::String,
            Lattice::Any,
        ] {
            assert_eq!(Lattice::Any.join(lat), Lattice::Any);
            assert_eq!(lat.join(Lattice::Any), Lattice::Any);
        }
    }

    #[test]
    fn lattice_smi_double_gives_number() {
        assert_eq!(Lattice::Smi.join(Lattice::Double), Lattice::Number);
        assert_eq!(Lattice::Double.join(Lattice::Smi), Lattice::Number);
        assert_eq!(Lattice::Smi.join(Lattice::Number), Lattice::Number);
        assert_eq!(Lattice::Number.join(Lattice::Double), Lattice::Number);
        assert_eq!(Lattice::Number.join(Lattice::Number), Lattice::Number);
    }

    #[test]
    fn lattice_string_only_joins_with_itself() {
        assert_eq!(Lattice::String.join(Lattice::String), Lattice::String);
        assert_eq!(Lattice::String.join(Lattice::Smi), Lattice::Any);
        assert_eq!(Lattice::Smi.join(Lattice::String), Lattice::Any);
        assert_eq!(Lattice::String.join(Lattice::Number), Lattice::Any);
    }

    #[test]
    fn lattice_object_same_shape_preserved() {
        let mut heap = Heap::new(v12_heap::GcPolicy::NoGC);
        let s0 = heap.root_shape();
        let s1 = {
            let h = heap.intern_string(V12Str::latin1(b"x".to_vec()));
            heap.add_root(JsValue::string(h));
            let k = v12_heap::PropKey::from_string(h);
            heap.add_property(s0, k, v12_heap::Attrs::DEFAULT)
        };
        assert_eq!(
            Lattice::Object(s0).join(Lattice::Object(s0)),
            Lattice::Object(s0)
        );
        assert_eq!(Lattice::Object(s0).join(Lattice::Object(s1)), Lattice::Any);
        assert_eq!(Lattice::Object(s0).join(Lattice::Smi), Lattice::Any);
    }

    #[test]
    fn lattice_meet_properties() {
        assert_eq!(Lattice::Unknown.meet(Lattice::Smi), Lattice::Unknown);
        assert_eq!(Lattice::Any.meet(Lattice::Smi), Lattice::Smi);
        assert_eq!(Lattice::Smi.meet(Lattice::Double), Lattice::Unknown);
        assert_eq!(Lattice::Smi.meet(Lattice::Number), Lattice::Smi);
        assert_eq!(Lattice::String.meet(Lattice::String), Lattice::String);
    }

    #[test]
    fn lattice_from_value_classifies() {
        assert_eq!(
            Lattice::from_value(JsValue::from_i32_smi(42).unwrap(), None),
            Lattice::Smi
        );
        assert_eq!(
            Lattice::from_value(JsValue::from_f64(2.5), None),
            Lattice::Double
        );
        let mut heap = Heap::new(v12_heap::GcPolicy::NoGC);
        let s = heap.intern_string(V12Str::latin1(b"hi".to_vec()));
        heap.add_root(JsValue::string(s));
        assert_eq!(
            Lattice::from_value(JsValue::string(s), None),
            Lattice::String
        );
        let obj = heap.alloc(v12_heap::JsObject::default());
        heap.add_root(JsValue::object(obj));
        let shape = heap.root_shape();
        assert_eq!(
            Lattice::from_value(JsValue::object(obj), Some(shape)),
            Lattice::Object(shape)
        );
        assert_eq!(
            Lattice::from_value(JsValue::object(obj), None),
            Lattice::Any
        );
    }

    #[test]
    fn feedback_record_type_joins() {
        let mut fv = FeedbackVector::default();
        fv.record_type(10, Lattice::Smi);
        assert_eq!(fv.type_at(10), Lattice::Smi);
        fv.record_type(10, Lattice::Double);
        assert_eq!(fv.type_at(10), Lattice::Number);
        fv.record_type(10, Lattice::String);
        assert_eq!(fv.type_at(10), Lattice::Any);
        assert_eq!(fv.type_at(99), Lattice::Unknown);
    }

    #[test]
    fn lattice_join_is_commutative_and_associative() {
        let variants = [
            Lattice::Unknown,
            Lattice::Smi,
            Lattice::Double,
            Lattice::Number,
            Lattice::String,
            Lattice::Any,
        ];
        for &a in &variants {
            for &b in &variants {
                assert_eq!(a.join(b), b.join(a), "commutative {a:?} {b:?}");
                for &c in &variants {
                    let left = a.join(b).join(c);
                    let right = a.join(b.join(c));
                    assert_eq!(left, right, "associative {a:?} {b:?} {c:?}");
                }
            }
        }
    }

    #[test]
    fn lattice_meet_is_commutative() {
        let variants = [
            Lattice::Unknown,
            Lattice::Smi,
            Lattice::Double,
            Lattice::Number,
            Lattice::String,
            Lattice::Any,
        ];
        for &a in &variants {
            for &b in &variants {
                assert_eq!(a.meet(b), b.meet(a), "meet commutative {a:?} {b:?}");
                // Join and meet absorption: a.join(a.meet(b)) == a.join(b) style check
                assert_eq!(a.meet(Lattice::Unknown), Lattice::Unknown);
                assert_eq!(a.meet(Lattice::Any), a);
            }
        }
    }

    // Proptest over random lattices: join is commutative, associative, idempotent.
    #[test]
    fn proptest_lattice_join_properties() {
        use proptest::prelude::*;
        use proptest::strategy::ValueTree;
        fn arb_lattice() -> impl Strategy<Value = Lattice> {
            prop_oneof![
                Just(Lattice::Unknown),
                Just(Lattice::Smi),
                Just(Lattice::Double),
                Just(Lattice::Number),
                Just(Lattice::String),
                Just(Lattice::Any),
            ]
        }
        let mut runner = proptest::test_runner::TestRunner::deterministic();
        for _ in 0..256 {
            let a = arb_lattice().new_tree(&mut runner).unwrap().current();
            let b = arb_lattice().new_tree(&mut runner).unwrap().current();
            let c = arb_lattice().new_tree(&mut runner).unwrap().current();
            // Idempotent
            assert_eq!(a.join(a), a, "idempotent {a:?}");
            // Commutative
            assert_eq!(a.join(b), b.join(a), "commutative {a:?} {b:?}");
            // Associative
            assert_eq!(
                a.join(b).join(c),
                a.join(b.join(c)),
                "associative {a:?} {b:?} {c:?}"
            );
            // Unknown identity
            assert_eq!(Lattice::Unknown.join(a), a);
            // Any absorbing
            assert_eq!(Lattice::Any.join(a), Lattice::Any);
            // Smi join Double = Number (when neither is Unknown/Any)
            if matches!(a, Lattice::Smi) && matches!(b, Lattice::Double)
                || matches!(a, Lattice::Double) && matches!(b, Lattice::Smi)
            {
                assert_eq!(a.join(b), Lattice::Number);
            }
        }
    }

    #[test]
    fn type_feedback_collection_via_interp() {
        // Run `1+1` through the interpreter and verify the Add sampled Smi.
        use crate::Interp;
        use v12_bytecode::{ConstantPool, FunctionBytecode, Instr, Opcode};

        let instrs = vec![
            Instr::new(Opcode::LoadInt, 0, 0, 1),
            Instr::new(Opcode::LoadInt, 1, 0, 1),
            Instr::new(Opcode::Add, 2, 0, 1),   // pc 2
            Instr::new(Opcode::Throw, 2, 0, 0), // surface result via throw
        ];
        let fb = FunctionBytecode::with_instructions(instrs, 3);
        let mut interp = Interp::new(vec![fb], 0, Vec::new());
        let _ = interp.run(); // will throw 2
        let lat = interp.type_feedback_at(0, 2);
        assert_eq!(
            lat,
            Lattice::Smi,
            "Add 1+1 should be recorded as Smi, got {lat:?}"
        );
        // Build proper double test via f64 const pool.
        let mut pool = ConstantPool::new();
        let k = pool.insert(v12_bytecode::Const::F64(0.5)).unwrap();
        let instrs2 = vec![
            Instr::new(Opcode::LoadInt, 0, 0, 1),
            Instr::new_imm16(Opcode::LoadConst, 1, k),
            Instr::new(Opcode::Add, 2, 0, 1),
            Instr::new(Opcode::Return, 2, 0, 0),
        ];
        let mut fb2 = FunctionBytecode::with_instructions(instrs2, 3);
        fb2.consts = pool;
        let mut interp2 = Interp::new(vec![fb2], 0, Vec::new());
        let _ = interp2.run();
        let lat2 = interp2.type_feedback_at(0, 2);
        assert_eq!(
            lat2,
            Lattice::Double,
            "Add int+double should be Double, got {lat2:?}"
        );
    }

    #[test]
    fn type_feedback_collection_via_source() {
        // End-to-end via `Interp::from_source`: `let x=1; throw x+1` avoids
        // constant-folding so the `Add` bytecode actually executes and records
        // Smi feedback.
        use crate::Interp;
        let mut interp = Interp::from_source("let x = 1; throw x + 1;").expect("compile");
        let _ = interp.run();
        let fv = interp.feedback_vector(0).expect("feedback exists");
        // At least one pc should have Smi feedback (the Add).
        let has_smi = fv.type_feedback.values().any(|&l| l == Lattice::Smi);
        assert!(
            has_smi,
            "expected Smi in feedback, got {:?}",
            fv.type_feedback
        );
    }
}
