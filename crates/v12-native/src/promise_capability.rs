//! Layout of a `PromiseCapability Record` (spec 27.5.1.5 step 2).
//!
//! The record is a plain heap object rather than interpreter state, because
//! the executor that fills it in is a host closure: `NewPromiseCapability`
//! allocates it, hands `C` a `GetCapabilitiesExecutor` builtin that writes into
//! it, and reads the pair back after `Construct` returned. The two sides agree
//! on the slot names below, so neither hardcodes an index.
//!
//! One record per `NewPromiseCapability(C)` — which is one per `new Promise`,
//! one per `Promise.all`/`race`/`any`/`allSettled` call, and one per
//! `withResolvers`. It is rooted by the engine (the closure captures the
//! handle) and parked on the interpreter's value stack for the duration of the
//! `Construct`.

/// `[[Resolve]]`: whatever the constructor's own code passed to the executor,
/// or `undefined` if it has not run yet.
pub const SLOT_RESOLVE: usize = 0;
/// `[[Reject]]`, laid out as [`SLOT_RESOLVE`].
pub const SLOT_REJECT: usize = 1;
/// `[[AlreadyResolved]]`: an Smi that is [`CAPABILITY_FRESH`] until the
/// executor has been called once and [`CAPABILITY_USED`] afterwards. The
/// executor's guard reads this rather than testing both halves for
/// `undefined`, which would accept a second `executor()` call with no
/// arguments.
pub const SLOT_ALREADY_RESOLVED: usize = 2;
/// The number of populated slots.
pub const SLOTS: usize = 3;

/// `[[AlreadyResolved]]` before the executor has run.
pub const CAPABILITY_FRESH: i32 = 0;
/// `[[AlreadyResolved]]` after it has.
pub const CAPABILITY_USED: i32 = 1;
