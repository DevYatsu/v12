//! Per-function execution feedback: saturating heat counters and the
//! monomorphic property-access inline cache.
//!
//! The interpreter records observations, it does not act on them beyond
//! raising a flag: the execution driver owns tier transitions and learns
//! about them through [`TierHooks::on_tier_up`], invoked between frame
//! completions. Nothing here ever references the JIT crates.

use std::collections::HashMap;

use v12_heap::ShapeHandle;

/// Loop-iteration / function-entry count at which a function is reported as
/// hot. 1024 iterations is the classic "this loop is worth compiling" signal:
/// high enough that cold one-shot loops never trigger bookkeeping work in the
/// driver, low enough to fire within milliseconds of sustained heat.
pub(crate) const FEEDBACK_TIER_UP_THRESHOLD: u16 = 1024;

/// One inline-cache site: the last shape seen at the access and the slot its
/// descriptor names. Monomorphic by design — a new shape simply replaces the
/// old entry, and every hit re-validates before trusting `slot`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MonoIc {
    pub(crate) shape: ShapeHandle,
    pub(crate) slot: u32,
}

/// Feedback state for one program function. Allocated lazily on first
/// execution; counters saturate so long-running loops stop asking.
#[derive(Default)]
pub(crate) struct FeedbackVector {
    /// Inline caches keyed by the pc of their `GetProperty` instruction.
    pub(crate) ics: HashMap<u32, MonoIc>,
    /// Saturating count of loop-header crossings.
    pub(crate) loop_counter: u16,
    /// Saturating count of activations.
    pub(crate) entry_counter: u16,
}

impl FeedbackVector {
    fn bump(counter: &mut u16) -> bool {
        let old = *counter;
        *counter = old.saturating_add(1);
        // Fire exactly once: at the crossing, not on every saturated tick.
        old < FEEDBACK_TIER_UP_THRESHOLD && *counter >= FEEDBACK_TIER_UP_THRESHOLD
    }

    /// Counts one activation; `true` exactly at the threshold crossing.
    pub(crate) fn activated(&mut self) -> bool {
        Self::bump(&mut self.entry_counter)
    }

    /// Counts one loop-header crossing; `true` exactly at the crossing.
    pub(crate) fn crossing_loop(&mut self) -> bool {
        Self::bump(&mut self.loop_counter)
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
