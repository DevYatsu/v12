//! A direct-call seam for builtin handlers (plan §4, task W3.3).
//!
//! Before this existed, every test of a builtin went through
//! `Engine::new()` + `eval`, because there was no way to hand a handler a
//! `&mut Ctx` — `Ctx`'s four fields are exactly the state a test cannot fake
//! its way past: `global` decides whether a thrown `TypeError` is linked to
//! `%TypeError.prototype%`, `pending` decides whether an enqueued job is
//! observable at all, `regex_cache` decides whether a regexp-backed method can
//! compile, and `heap` is the values themselves. So the entire `&mut Ctx`
//! signature migration (F4) was only ever verifiable through test262, which is
//! how it could stall for weeks with nothing red.
//!
//! [`BuiltinTestHarness`] closes that: it owns a heap with a **fully built
//! realm** on it and hands out a `Ctx` with every field populated.
//!
//! **Why a fixture that owns the heap rather than a `Ctx::for_test(heap)`.**
//! `Ctx` borrows its heap, and the realm has to be built *on* that heap before
//! the `Ctx` exists, so a bare constructor cannot both create the realm and
//! return a context borrowing it — it would have to be `unsafe` (and this
//! crate is `#![forbid(unsafe_code)]`) or leak. The fixture sidesteps the
//! self-reference by never *storing* a `Ctx`: [`BuiltinTestHarness::with_ctx`]
//! builds one inside a closure that borrows `self.heap` for the duration of
//! the call, which is the borrow shape the borrow checker can check.
//!
//! **Why it is not gated behind `#[cfg(test)]`.** An inline `#[cfg(test)]`
//! module is unreachable from `crates/v12-engine/tests/*.rs` — the 30
//! integration-test files this task exists to serve, which are external crates
//! to the lib. Gating would confine the seam to in-crate unit tests and leave
//! the boilerplate W3.4 wants to delete untouched. The module is small and
//! contains no policy: it constructs `Heap`/`Realm`/`Ctx` and nothing else.
//!
//! It lives here rather than in the `test-support` crate because
//! `test-support` does not depend on `v12-engine`; building the same fixture
//! there would require adding that dependency, which as a *dev*-dependency of
//! `v12-engine` is the cycle `v12-engine -> test-support -> v12-engine` and as a
//! normal one drags `mini.rs` in behind it. W3.5 decides `mini.rs`'s fate
//! separately; this seam must not pre-empt that by coupling the two.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use v12_heap::{GcPolicy, Handle, Heap, JsObject, JsValue};

use crate::builtins::ctx::Ctx;
use crate::builtins::regexp::RegexCache;
use crate::job_queue::Job;
use crate::realm::Realm;

/// A heap with one fully-built realm and every `Ctx` side channel wired, for
/// calling builtin handlers directly.
///
/// Construct with [`BuiltinTestHarness::new`]; no `Engine`, no `eval`, no
/// bytecode. `GcPolicy::NoGC` so a test never loses a value it did not root
/// itself.
///
/// ```ignore
/// let mut h = BuiltinTestHarness::new();
/// let is_nan = h.with_ctx(|ctx| {
///     number::number_is_nan(ctx, JsValue::undefined(), &[JsValue::from_f64(f64::NAN)])
/// });
/// ```
pub struct BuiltinTestHarness {
    heap: Heap,
    /// Retained so a test can read realm ground truth ([`realm_intrinsic`])
    /// without holding a second mutable borrow of the heap. The global object
    /// itself is rooted by `Realm::new`, so dropping this `Realm` would not
    /// free anything — it is kept only so its intrinsic table stays readable.
    realm: Realm,
    pending: Rc<RefCell<Vec<Job>>>,
    regex_cache: RegexCache,
}

impl BuiltinTestHarness {
    /// Builds a `NoGC` heap, installs a complete realm on it (real `global`
    /// with the intrinsic prefix populated), and allocates fresh `pending`
    /// and `regex_cache` sinks.
    #[must_use]
    pub fn new() -> Self {
        let mut heap = Heap::new(GcPolicy::NoGC);
        let realm = Realm::new(&mut heap);
        Self {
            heap,
            realm,
            pending: Rc::new(RefCell::new(Vec::new())),
            regex_cache: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// Shared read access to the heap (e.g. to inspect a result handle after
    /// the context is dropped).
    #[must_use]
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// This realm's global object — the handle `Ctx::new` is given as
    /// `global`, and the root `Ctx::intrinsic` reads through.
    #[must_use]
    pub fn global(&self) -> Handle<JsObject> {
        self.realm.global()
    }

    /// Realm intrinsic value by name, read through [`Realm::get_intrinsic`] —
    /// the **ground-truth** reader, deliberately distinct from `Ctx::intrinsic`
    /// (which has a `realm_globals()` fallback and is the thing under test).
    /// A test asserting realm linkage should read the expected value here,
    /// never via the seam.
    #[must_use]
    pub fn realm_intrinsic(&self, name: &str) -> Option<JsValue> {
        self.realm.get_intrinsic(name)
    }

    /// Realm intrinsic value by name, as an object handle.
    #[must_use]
    pub fn realm_intrinsic_object(&self, name: &str) -> Option<Handle<JsObject>> {
        self.realm.get_intrinsic(name).and_then(|v| v.as_object())
    }

    /// Number of jobs enqueued onto the `pending` sink so far.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.borrow().len()
    }

    /// Runs `body` with a `Ctx` wired to this heap's realm and to *live*
    /// `pending` / `regex_cache` sinks.
    ///
    /// The closure form is the whole point: a `Ctx` borrows the heap
    /// exclusively, so it cannot be stored on the fixture. Handlers that need
    /// to keep the context past the call are the case that genuinely does
    /// require an `Engine`.
    pub fn with_ctx<R>(&mut self, body: impl FnOnce(&mut Ctx) -> R) -> R {
        let mut ctx = Ctx::new(
            &mut self.heap,
            Some(self.realm.global()),
            Some(Rc::clone(&self.pending)),
        )
        .with_regex_cache(Rc::clone(&self.regex_cache));
        body(&mut ctx)
    }
}

impl Default for BuiltinTestHarness {
    fn default() -> Self {
        Self::new()
    }
}
