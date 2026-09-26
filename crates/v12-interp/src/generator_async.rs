//! Generator and async-function runtime: generator object creation, resume
//! protocol (`next`/`return`/`throw`), promise unwrapping for `await`, and the
//! parked-frame resume paths.

use v12_heap::{Handle, HeapExt, JsObject, JsValue, Kind, PropKey};

use super::{Frame, Interp, JSException, MAX_RESUME_DEPTH};
use crate::ops;

/// The outcome of the ordinary-path `then` read that decides thenable
/// adoption (spec 27.2.1.3.1 `PromiseResolveFunctions` step 3).
pub(crate) enum ThenRead {
    /// No `then` property, or a present one that is not callable: not a
    /// thenable, so the promise fulfills with the value itself (step 3-b-i).
    NotThenable,
    /// `then` is callable: adopt through a thenable job.
    Then(JsValue),
    /// The `Get` was abrupt (a throwing getter, a proxy `get` trap): the
    /// thrown value rides the promise (step 3-a).
    Abrupt(JsValue),
}

impl Interp<'_> {
    /// Slot index of the async-generator request queue on an async generator
    /// object: a rooted array of the request promises returned by
    /// `.next()`/`.return()`/`.throw()` that have not settled yet, oldest first.
    /// Async *function* generators reuse slots 0..4 (`properties[4]` is the
    /// completion promise); async generators append here so the resume path can
    /// settle requests FIFO (spec 27.6.3: requests queue while the generator is
    /// executing or awaiting, and settle in arrival order).
    pub(crate) const ASYNC_GEN_QUEUE_SLOT: usize = 11;
    /// Slot marking that the generator suspended on a real `yield` (not an
    /// internal `await`) during the current resume. `SuspendYield` sets it for
    /// async generators; the resume path reads it to decide whether the
    /// pending request promise settles now or stays parked on the await.
    pub(crate) const ASYNC_GEN_YIELD_SLOT: usize = 7;
    /// `true` when `GetAsyncIterator` used the AsyncFromSyncIterator fallback.
    pub(crate) const ASYNC_GEN_FROM_SYNC_SLOT: usize = 10;
    pub(crate) const GENERATOR_RETURN_VALUE_SLOT: usize = 9;
    /// Retained materialized arguments value for every generator/async header.
    pub(crate) const GENERATOR_ARGUMENTS_SLOT: usize = 8;

    pub(crate) fn generator_header_value(
        &self,
        r#gen: Handle<JsObject>,
        slot: usize,
    ) -> Option<JsValue> {
        self.heap
            .get(r#gen)
            .properties
            .get(slot)
            .copied()
            .filter(|value| !value.is_undefined())
    }

    pub(crate) fn generator_completion_promise(
        &self,
        r#gen: Handle<JsObject>,
    ) -> Option<Handle<JsObject>> {
        self.generator_header_value(r#gen, 4)
            .and_then(|value| value.as_object())
    }

    pub(crate) fn set_generator_header_value(
        &mut self,
        r#gen: Handle<JsObject>,
        slot: usize,
        value: JsValue,
    ) {
        let object = self.heap.get_mut(r#gen);
        if object.properties.len() <= slot {
            object.properties.resize(slot + 1, JsValue::undefined());
            object.property_keys.resize(slot + 1, None);
        }
        object.properties[slot] = value;
    }

    pub(crate) fn is_generator_fn_for(&self, fn_idx: u32, program: u32) -> bool {
        let funcs = self.functions_for_program(program);
        if fn_idx as usize >= funcs.len() {
            return false; // native/host function index
        }
        let f = &funcs[fn_idx as usize];
        if f.is_generator {
            return true;
        }
        // Fallback for old bytecode / hand-built tests without flag.
        for instr in &f.instrs {
            if instr.op() == Some(v12_bytecode::Opcode::SuspendYield) {
                return true;
            }
            if instr.op() == Some(v12_bytecode::Opcode::Wide) {
                // Wide instructions cannot be SuspendYield, so ignore.
            }
        }
        false
    }

    /// Program-aware async check.
    pub(crate) fn is_async_fn_for(&self, fn_idx: u32, program: u32) -> bool {
        let funcs = self.functions_for_program(program);
        if fn_idx as usize >= funcs.len() {
            return false; // native/host function index
        }
        funcs[fn_idx as usize].is_async
    }

    pub(crate) fn create_generator_object(
        &mut self,
        fn_idx: u32,
        callee_program: u32,
        captured_env: Option<Handle<JsObject>>,
        this_v: JsValue,
        callee_slot: usize,
        argc: u16,
    ) -> Result<Handle<JsObject>, JSException> {
        // Program-aware: the generator's body resolves against its own
        // program's function table (an eval/module/realm closure must not be
        // indexed through this interpreter's main table — that reads the
        // wrong `max_regs`, and resuming then executes foreign bytecode in a
        // too-small register window, the register-window OOB class).
        let (max_regs, has_rest, fixed, rest_reg) = {
            let funcs = self.functions_for_program(callee_program);
            let f = &funcs[fn_idx as usize];
            (f.max_regs, f.has_rest, f.fixed_params, f.rest_reg)
        };
        // Build initial register window snapshot via shared helper (DRY #4).
        let mut window = vec![JsValue::undefined(); usize::from(max_regs)];
        window[0] = this_v;
        let arg_src = callee_slot + 2;
        let actuals = self.stack[arg_src..arg_src + usize::from(argc)].to_vec();
        let arguments =
            self.frame_arguments_for(fn_idx, callee_program, &actuals, self.stack[callee_slot]);
        self.fill_call_window(
            &mut window,
            arg_src,
            argc as usize,
            has_rest,
            fixed,
            rest_reg,
        );
        // Real suspension: store initial register window snapshot, not eager yields.
        self.gc_protect();
        let mut g_obj = JsObject::generator_with(fn_idx, 0, 0.0, 0, window, captured_env, None);
        // The resume path (`resume_generator_nested`) reads the program id
        // from the generator object; default 0 would resolve `fn_idx`
        // against the main program's table (see above).
        g_obj.program_id = callee_program;
        let r#gen = self.heap.alloc(g_obj);
        let retained_arguments = arguments.unwrap_or(JsValue::undefined());
        self.set_generator_header_value(r#gen, Self::GENERATOR_ARGUMENTS_SLOT, retained_arguments);
        if let Some(arguments) = self.generator_header_value(r#gen, Self::GENERATOR_ARGUMENTS_SLOT)
        {
            self.heap.remove_root(arguments);
        }
        self.heap.add_root(JsValue::object(r#gen));
        Ok(r#gen)
    }

    /// True when `r#gen` is an async-generator object (its function carries
    /// both `is_async` and `is_generator`). Async generators resolve against
    /// their own program table, so the lookup is program-aware.
    pub(crate) fn is_async_generator_for(&self, r#gen: Handle<JsObject>) -> bool {
        let (fn_idx, program) = self.generator_fn_and_program(r#gen);
        self.is_async_fn_for(fn_idx, program) && self.is_generator_fn_for(fn_idx, program)
    }

    /// Reads `(fn_idx, program_id)` off a generator object's header slots.
    fn generator_fn_and_program(&self, r#gen: Handle<JsObject>) -> (u32, u32) {
        let o = self.heap.get(r#gen);
        let fn_idx = o
            .properties
            .first()
            .and_then(|v| v.as_smi().map(|n| n as u32 as f64).or(v.as_f64()))
            .unwrap_or(0.0) as u32;
        (fn_idx, o.program_id)
    }

    /// Pads an async generator's property slots through the request queue so
    /// request, yield, and AsyncFromSyncIterator state are addressable
    /// (generators are created with only the 4 header slots).
    fn ensure_async_gen_slots(&mut self, r#gen: Handle<JsObject>) {
        let need = Self::ASYNC_GEN_QUEUE_SLOT.max(Self::ASYNC_GEN_FROM_SYNC_SLOT) + 1;
        let o = self.heap.get_mut(r#gen);
        if o.properties.len() < need {
            o.properties.resize(need, JsValue::undefined());
            o.property_keys.resize(need, None);
        }
    }

    /// Allocates a pending promise for an async-generator request
    /// (`next`/`return`/`throw`), linked to `%Promise.prototype%` so `.then`
    /// resolves through the promise surface.
    ///
    /// The promise is rooted before returning: the caller enqueues it, and any
    /// safepoint between allocation and enqueueing (a nested resume, the
    /// iterator-result allocation) must not collect a request the caller still
    /// has to hand back to JavaScript.
    fn alloc_async_gen_request(&mut self) -> Handle<JsObject> {
        self.gc_protect();
        let promise = self.heap.alloc_pending_promise();
        self.heap.add_root(JsValue::object(promise));
        if let Some(g) = self.global
            && let Some(pp) = self
                .heap
                .get(g)
                .properties
                .get(super::PROMISE_IDX.expect("intrinsic 'Promise' present"))
                .and_then(|v| v.as_object())
                .and_then(|ctor| self.heap.get(ctor).prototype)
        {
            self.heap.get_mut(promise).prototype = Some(pp);
        }
        promise
    }

    /// The request-queue array of `r#gen`, if it has one.
    fn async_gen_request_queue(&self, r#gen: Handle<JsObject>) -> Option<Handle<JsObject>> {
        self.heap
            .get(r#gen)
            .properties
            .get(Self::ASYNC_GEN_QUEUE_SLOT)
            .and_then(|v| v.as_object())
    }

    /// Appends `promise` to `r#gen`'s request queue (spec 27.6.3.4
    /// AsyncGeneratorEnqueue), creating the queue on first use. Every request
    /// gets its own slot: a request that arrives while the generator is
    /// suspended on an `await` waits behind the in-flight one instead of
    /// overwriting it.
    pub(crate) fn enqueue_async_gen_request(
        &mut self,
        r#gen: Handle<JsObject>,
        promise: Handle<JsObject>,
    ) {
        self.ensure_async_gen_slots(r#gen);
        let queue = match self.async_gen_request_queue(r#gen) {
            Some(queue) => queue,
            None => {
                self.gc_protect();
                let queue = self.heap.alloc(JsObject::array(Vec::new()));
                self.heap.add_root(JsValue::object(queue));
                self.heap.get_mut(r#gen).properties[Self::ASYNC_GEN_QUEUE_SLOT] =
                    JsValue::object(queue);
                queue
            }
        };
        self.heap
            .get_mut(queue)
            .elements_array
            .push(JsValue::object(promise));
    }

    /// Removes and returns the oldest unsettled request promise of `r#gen`.
    ///
    /// The queue array is a canonical dense array, so the head is dropped by
    /// shifting the tail down and popping the duplicated last slot.
    fn pop_oldest_async_gen_request(
        &mut self,
        r#gen: Handle<JsObject>,
    ) -> Option<Handle<JsObject>> {
        let queue = self.async_gen_request_queue(r#gen)?;
        let (head, len) = {
            let array = self.heap.get(queue);
            (
                array.elements_array.get(0).and_then(|v| v.as_object()),
                array.elements_array.len(),
            )
        };
        if head.is_none() {
            return None;
        }
        let array = self.heap.get_mut(queue);
        for i in 1..len {
            let Some(value) = array.elements_array.get(i as u32) else {
                break;
            };
            array.elements_array.set(i as u32 - 1, value);
        }
        // The shift duplicated the tail entry; the pop restores the length.
        array.elements_array.pop();
        head
    }

    /// Settles and clears the oldest pending request promise
    /// (`next`/`return`/`throw`) queued on `r#gen`, if any.
    ///
    /// `suspended` reports whether the resume stopped at a yield; `suspended`
    /// with a value is `{value, done: false}`, a completion is
    /// `{value, done: true}`, and `reject` rejects with `value`. A suspension
    /// on an internal `await` (not a real `yield`) leaves the request queued
    /// for the eventual yield/completion resume. Completing or rejecting the
    /// body also drains the requests still queued behind it: a request on a
    /// completed async generator resolves `{value: undefined, done: true}`.
    pub(crate) fn settle_async_gen_request_slot(
        &mut self,
        r#gen: Handle<JsObject>,
        suspended: bool,
        value: JsValue,
        reject: bool,
    ) {
        if !self.is_async_generator_for(r#gen) {
            return;
        }
        if suspended && !reject && !self.async_gen_yielded(r#gen) {
            return;
        }
        let Some(promise) = self.pop_oldest_async_gen_request(r#gen) else {
            return;
        };
        if reject {
            self.pending_settlements.push((promise, value, true));
            self.drain_completed_async_gen_requests(r#gen);
            return;
        }
        // Build the iterator result before touching the queue: the generator
        // (and hence the queue) is reachable from `frames` during the
        // allocation safepoint.
        let result = self.make_iterator_result(value, !suspended);
        self.pending_settlements.push((promise, result, false));
        if !suspended {
            self.drain_completed_async_gen_requests(r#gen);
        }
    }

    /// Resolves every request still queued behind a completed/rejected body.
    ///
    /// A completed async generator answers any further `next`/`return`/`throw`
    /// with `{value: undefined, done: true}`; the queued requests would each
    /// resume that completed generator, so they settle the same way instead of
    /// staying pending forever.
    fn drain_completed_async_gen_requests(&mut self, r#gen: Handle<JsObject>) {
        while let Some(promise) = self.pop_oldest_async_gen_request(r#gen) {
            let result = self.make_iterator_result(JsValue::undefined(), true);
            self.pending_settlements.push((promise, result, false));
        }
    }

    /// Reads the yield-vs-await flag left by the most recent resume
    /// (`SuspendYield` sets it, `Await` clears it).
    fn async_gen_yielded(&self, r#gen: Handle<JsObject>) -> bool {
        self.heap
            .get(r#gen)
            .properties
            .get(Self::ASYNC_GEN_YIELD_SLOT)
            .is_some_and(|v| v.is_true())
    }

    pub(crate) fn async_gen_from_sync(&self, r#gen: Handle<JsObject>) -> bool {
        self.heap
            .get(r#gen)
            .properties
            .get(Self::ASYNC_GEN_FROM_SYNC_SLOT)
            .is_some_and(|v| v.is_true())
    }

    pub(crate) fn set_async_gen_from_sync(&mut self, r#gen: Handle<JsObject>, from_sync: bool) {
        self.ensure_async_gen_slots(r#gen);
        self.heap.get_mut(r#gen).properties[Self::ASYNC_GEN_FROM_SYNC_SLOT] =
            JsValue::from_bool(from_sync);
    }

    /// Marks whether the current async-generator suspension is a real `yield`
    /// (`true`, settles the request promise) or an internal `await` (`false`,
    /// request promise stays parked).
    pub(crate) fn set_async_gen_suspended_on_yield(
        &mut self,
        r#gen: Handle<JsObject>,
        on_yield: bool,
    ) {
        self.ensure_async_gen_slots(r#gen);
        self.heap.get_mut(r#gen).properties[Self::ASYNC_GEN_YIELD_SLOT] =
            JsValue::from_bool(on_yield);
    }

    pub(crate) fn generator_next(
        &mut self,
        this_v: JsValue,
        arg: JsValue,
    ) -> Result<JsValue, JSException> {
        let Some(r#gen) = this_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: generator next called on non-object"),
            ));
        };
        if self.heap.get(r#gen).kind != Kind::Generator {
            return Err(JSException(self.error_value("TypeError: not a generator")));
        }
        let async_gen = self.is_async_generator_for(r#gen);
        let request = if async_gen {
            self.ensure_async_gen_slots(r#gen);
            Some(self.alloc_async_gen_request())
        } else {
            None
        };
        let done = self
            .heap
            .get(r#gen)
            .properties
            .get(2)
            .and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64()))
            .unwrap_or(0.0)
            == 1.0;
        if done {
            if let Some(promise) = request {
                let result = self.make_iterator_result(JsValue::undefined(), true);
                self.pending_settlements.push((promise, result, false));
                return Ok(JsValue::object(promise));
            }
            return Ok(self.make_iterator_result(JsValue::undefined(), true));
        }
        if let Some(promise) = request {
            // The request promise queues on the generator; the resume path
            // (`resume_generator_nested`) settles the oldest one, whether the
            // body yields synchronously here or suspends on an `await` and
            // resumes later. Async generators never throw synchronously from
            // `next()`: an abrupt body completion rejects the request promise.
            self.enqueue_async_gen_request(r#gen, promise);
            let _ = self.resume_generator(r#gen, arg, false);
            return Ok(JsValue::object(promise));
        }
        match self.resume_generator(r#gen, arg, false)? {
            Some(value) => Ok(self.make_iterator_result(value, true)),
            None => {
                let yielded = self.top_result.take().unwrap_or(JsValue::undefined());
                Ok(self.make_iterator_result(yielded, false))
            }
        }
    }

    pub(crate) fn make_iterator_result(&mut self, value: JsValue, done: bool) -> JsValue {
        self.gc_protect();
        let h = self.heap.alloc(JsObject::default());
        // Iterator-result objects are ordinary: inherit `%Object.prototype%`.
        self.link_object_proto(h);
        self.heap.add_root(JsValue::object(h));
        // Avoid set_property recursion issues for now: store directly via properties vec and shape binding via heap
        // Use minimal shape: add properties via heap without interpreter's set_property
        let value_key = self
            .heap
            .intern_string(v12_heap::V12Str::latin1(b"value".to_vec()));
        let done_key = self
            .heap
            .intern_string(v12_heap::V12Str::latin1(b"done".to_vec()));
        let pk_value = PropKey::from_string(value_key);
        let pk_done = PropKey::from_string(done_key);
        let shape0 = self.heap.root_shape();
        let shape1 = self
            .heap
            .add_property(shape0, pk_value, v12_heap::Attrs::DEFAULT);
        let shape2 = self
            .heap
            .add_property(shape1, pk_done, v12_heap::Attrs::DEFAULT);
        // Bind shape to object via interp's shape_of tracking
        self.bind_shape(h, shape2);
        let done_val = JsValue::from_bool(done);
        self.heap.get_mut(h).properties = smallvec::smallvec![value, done_val];
        self.heap.get_mut(h).property_keys = smallvec::smallvec![Some(pk_value), Some(pk_done)];
        JsValue::object(h)
    }

    pub(crate) fn generator_return(
        &mut self,
        this_v: JsValue,
        arg: JsValue,
    ) -> Result<JsValue, JSException> {
        let Some(r#gen) = this_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: generator return called on non-object"),
            ));
        };
        if self.heap.get(r#gen).kind != Kind::Generator {
            return Err(JSException(self.error_value("TypeError: not a generator")));
        }
        let done = self
            .heap
            .get(r#gen)
            .properties
            .get(2)
            .and_then(|v| v.as_f64().or(v.as_smi().map(|n| n as f64)))
            .unwrap_or(0.0)
            == 1.0;
        if done {
            if self.is_async_generator_for(r#gen) {
                let request = self.alloc_async_gen_request();
                let result = self.make_iterator_result(arg, true);
                self.pending_settlements.push((request, result, false));
                return Ok(JsValue::object(request));
            }
            return Ok(self.make_iterator_result(arg, true));
        }
        // Spec 27.5.3.4: resume the suspended body with a *return*
        // completion — active `finally` blocks must run (compiled per-yield
        // return paths keyed off the generator's mode slot), while `catch`
        // blocks must not. The compiled `GenResumeMode` check after each
        // yield branches to its finalizer-copy + `Return` trampoline.
        // Slot contract: properties[5] is the pending-completion mode. Sync
        // generators allocate only 4 slots (async_promise absent), so resize
        // before writing — the slot index is fixed, never appended.
        {
            let o = self.heap.get_mut(r#gen);
            if o.properties.len() < 6 {
                o.properties.resize(6, JsValue::undefined());
                o.property_keys.resize(6, None);
            }
            o.properties[5] = ops::box_number(1.0);
        }
        self.set_generator_header_value(r#gen, Self::GENERATOR_RETURN_VALUE_SLOT, arg);
        if self.is_async_generator_for(r#gen) {
            let request = self.alloc_async_gen_request();
            self.enqueue_async_gen_request(r#gen, request);
            // `return()` is a real suspension point for an async generator's
            // request queue: the resume path settles the oldest request when
            // the body finally yields again or completes.
            let _ = self.resume_generator(r#gen, arg, false);
            return Ok(JsValue::object(request));
        }
        match self.resume_generator(r#gen, arg, false)? {
            // The return trampoline completed the body.
            Some(ret) => Ok(self.make_iterator_result(ret, true)),
            // The body suspended again (finalizer yields); report the new
            // yield as a normal, still-open iteration result.
            None => {
                let yielded = self.top_result.take().unwrap_or(JsValue::undefined());
                Ok(self.make_iterator_result(yielded, false))
            }
        }
    }

    pub(crate) fn generator_throw(
        &mut self,
        this_v: JsValue,
        arg: JsValue,
    ) -> Result<JsValue, JSException> {
        let Some(r#gen) = this_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: generator throw called on non-object"),
            ));
        };
        if self.heap.get(r#gen).kind != Kind::Generator {
            return Err(JSException(self.error_value("TypeError: not a generator")));
        }
        let done = self
            .heap
            .get(r#gen)
            .properties
            .get(2)
            .and_then(|v| v.as_f64().or(v.as_smi().map(|n| n as f64)))
            .unwrap_or(0.0)
            == 1.0;
        if done {
            if self.is_async_generator_for(r#gen) {
                let request = self.alloc_async_gen_request();
                self.pending_settlements.push((request, arg, true));
                return Ok(JsValue::object(request));
            }
            return Err(JSException(arg));
        }
        // Spec 27.5.3.5: resume the suspended body with a throw completion —
        // the exception surfaces at the suspended yield, so `catch` can
        // intercept it and `finally` runs on the unwind path.
        if self.is_async_generator_for(r#gen) {
            let request = self.alloc_async_gen_request();
            self.enqueue_async_gen_request(r#gen, request);
            let _ = self.resume_generator(r#gen, arg, true);
            return Ok(JsValue::object(request));
        }
        match self.resume_generator(r#gen, arg, true) {
            Ok(Some(ret)) => Ok(self.make_iterator_result(ret, true)),
            Ok(None) => {
                let yielded = self.top_result.take().unwrap_or(JsValue::undefined());
                Ok(self.make_iterator_result(yielded, false))
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) fn is_promise(&self, v: JsValue) -> bool {
        let Some(obj) = v.as_object() else {
            return false;
        };
        let o = self.heap.get(obj);
        o.kind == Kind::Promise
            && o.properties[0]
                .as_smi()
                .is_some_and(|s| (0..=2).contains(&s))
    }

    /// The engine's `is_promise` predicate, mirrored: a promise carries
    /// *exactly* the three `[[State]]`/`[[Result]]`/`[[Reactions]]` slots, so
    /// a user-added own property takes it out. The `Promise.resolve` seam needs
    /// the same answer the engine's handler would give — it decides "identity
    /// or adopt" once, and the handler is the fallback for the branch the seam
    /// does not take.
    pub(crate) fn is_engine_promise(&self, v: JsValue) -> bool {
        let Some(obj) = v.as_object() else {
            return false;
        };
        let o = self.heap.get(obj);
        o.kind == Kind::Promise
            && o.properties.len() == 3
            && o.properties[0]
                .as_smi()
                .is_some_and(|s| (0..=2).contains(&s))
    }

    /// `Get(value, "then")` through the ordinary property path — prototype
    /// chain, accessor getter, proxy `get` trap — with the callability gate a
    /// thenable needs. Non-objects are never thenables.
    pub(crate) fn read_then(&mut self, v: JsValue) -> ThenRead {
        if v.as_object().is_none() {
            return ThenRead::NotThenable;
        }
        let then_key = self.new_temp_key("then");
        match self.get_property(0, 0, v, then_key) {
            Err(JSException(thrown)) => ThenRead::Abrupt(thrown),
            Ok(then_v) => {
                let callable = then_v
                    .as_object()
                    .is_some_and(|f| matches!(self.heap.get(f).kind, Kind::Function | Kind::Proxy));
                if callable {
                    ThenRead::Then(then_v)
                } else {
                    ThenRead::NotThenable
                }
            }
        }
    }

    /// A pending promise with an empty reactions array, rooted. The shape is
    /// the engine's `[[State]]`/`[[Result]]`/`[[Reactions]]` triple, so both
    /// sides' `is_promise` checks accept it.
    pub(crate) fn alloc_pending_promise(
        &mut self,
        prototype: Option<Handle<JsObject>>,
    ) -> Handle<JsObject> {
        let reactions = self.heap.alloc(JsObject::array(Vec::new()));
        self.heap.add_root(JsValue::object(reactions));
        let mut shell = JsObject {
            kind: Kind::Promise,
            prototype,
            ..JsObject::default()
        };
        shell
            .properties
            .push(JsValue::from_i32_smi(0).expect("0 fits Smi"));
        shell.properties.push(JsValue::undefined());
        shell.properties.push(JsValue::object(reactions));
        shell.property_keys.resize(3, None);
        let handle = self.heap.alloc(shell);
        self.heap.add_root(JsValue::object(handle));
        handle
    }

    /// Queues a thenable adoption for the engine to turn into a job. The
    /// interpreter cannot build a `Job` (they live in `v12-engine`), and the
    /// job sink is invisible to the collector, so the request rides this queue
    /// — rooted by `gc_protect` like `pending_settlements` — and the engine's
    /// drain walks it. The entry outlives its own job on purpose: the promise,
    /// the thenable and `then` must stay rooted until the job has run.
    pub(crate) fn push_pending_adoption(
        &mut self,
        promise: Handle<JsObject>,
        thenable: JsValue,
        then_v: JsValue,
        abrupt: bool,
    ) {
        self.pending_adoptions
            .push((promise, thenable, then_v, abrupt));
    }

    /// Number of queued adoption requests; the engine's drain walks it by
    /// index (a running job can append a nested adoption).
    pub fn pending_adoptions_len(&self) -> usize {
        self.pending_adoptions.len()
    }

    /// The adoption request at `index`, cloned out so the engine can enqueue
    /// its job without borrowing the queue.
    pub fn pending_adoption_at(
        &self,
        index: usize,
    ) -> Option<(Handle<JsObject>, JsValue, JsValue, bool)> {
        self.pending_adoptions.get(index).copied()
    }

    /// Drops the drained adoption requests. Called once the engine's drain has
    /// run the job for each of them.
    pub fn clear_pending_adoptions(&mut self) {
        self.pending_adoptions.clear();
    }

    /// The value an `await` parks on, plus how to resume: `(promise,
    /// is_reject, payload)` (spec 27.7.5.1 `AsyncFunctionAwait`).
    ///
    /// An object argument goes through the engine's capability path rather
    /// than a locally fulfilled promise: a thenable's `then` must be called
    /// from a job, and only the engine can queue one. So the `then` read
    /// happens here (the ordinary property path) and the frame parks on a
    /// pending promise the engine settles — from the thenable job, or right
    /// away for a plain object. Neither branch spins: a thenable that never
    /// calls back leaves the promise pending.
    pub(crate) fn promise_resolve_for_await(&mut self, v: JsValue) -> (JsValue, bool, JsValue) {
        if self.is_promise(v) {
            let obj = v.as_object().unwrap();
            let state = self.heap.get(obj).properties[0].as_smi().unwrap_or(0);
            let payload = self.heap.get(obj).properties[1];
            if state == 1 {
                return (v, false, payload);
            } else if state == 2 {
                return (v, true, payload);
            } else {
                // Pending promise: the await parks on the promise itself; the
                // resume paths poll its state (`await_resume_value`) and only
                // wake the frame once it settles, with its payload.
                return (v, false, v);
            }
        }
        self.gc_protect();
        let read = self.read_then(v);
        match read {
            ThenRead::Abrupt(thrown) => {
                // `? PromiseResolve(...)` inside the async body: the abrupt
                // rejects the async function (spec 27.7.5.1), which is what
                // resuming with `is_reject` does. No promise slot to root.
                self.heap.add_root(thrown);
                (JsValue::undefined(), true, thrown)
            }
            ThenRead::Then(then_v) => {
                let promise = self.alloc_pending_promise(None);
                self.push_pending_adoption(promise, v, then_v, false);
                let promise_v = JsValue::object(promise);
                (promise_v, false, promise_v)
            }
            ThenRead::NotThenable => {
                // A plain object (or a primitive) resolves to itself; the
                // fulfilled promise lets the existing resume path carry it.
                let reactions = self.heap.alloc(JsObject::array(Vec::new()));
                self.heap.add_root(JsValue::object(reactions));
                let promise = self.heap.alloc(JsObject::fulfilled_promise(v, reactions));
                self.heap.add_root(JsValue::object(promise));
                (JsValue::object(promise), false, v)
            }
        }
    }

    #[allow(dead_code)]
    pub(crate) fn try_unwrap_promise(&self, v: JsValue) -> Option<JsValue> {
        let obj = v.as_object()?;
        let p = self.heap.get(obj);
        if p.properties.len() >= 3
            && p.properties[0]
                .as_smi()
                .is_some_and(|s| (0..=2).contains(&s))
        {
            let state = p.properties[0].as_smi().unwrap();
            if state == 1 {
                return Some(p.properties[1]);
            }
        }
        None
    }

    pub(crate) fn resume_async(
        &mut self,
        r#gen: Handle<JsObject>,
        value: JsValue,
    ) -> Result<(), JSException> {
        self.resume_generator(r#gen, value, false)?;
        Ok(())
    }

    pub(crate) fn resume_async_throw(
        &mut self,
        r#gen: Handle<JsObject>,
        exc: JsValue,
    ) -> Result<(), JSException> {
        self.resume_generator(r#gen, exc, true)?;
        Ok(())
    }

    /// The single generator/async resume primitive.
    ///
    /// Restores the generator's saved register window, pushes a frame with
    /// `generator: Some(gen)`, optionally injects a throw through the unwind
    /// path, and runs the dispatch loop. On suspension (`SuspendYield`) the
    /// frame pops and `Some(yielded)` is returned; on completion
    /// `Some(returned)` is returned and the generator is marked done. Used by
    /// `generator_next` (which wraps the result in `{value, done}`) and by
    /// the async resume paths (which settle the async promise instead).
    pub(crate) fn resume_generator(
        &mut self,
        r#gen: Handle<JsObject>,
        value: JsValue,
        is_throw: bool,
    ) -> Result<Option<JsValue>, JSException> {
        if self.resume_depth >= MAX_RESUME_DEPTH {
            return Err(JSException(
                self.error_value("RangeError: maximum call stack size exceeded"),
            ));
        }
        self.resume_depth += 1;
        let result = self.resume_generator_nested(r#gen, value, is_throw);
        self.resume_depth -= 1;
        result
    }

    pub(crate) fn resume_generator_nested(
        &mut self,
        r#gen: Handle<JsObject>,
        value: JsValue,
        is_throw: bool,
    ) -> Result<Option<JsValue>, JSException> {
        let (fn_idx, resume_pc) = {
            let o = self.heap.get(r#gen);
            let fn_idx = o
                .properties
                .first()
                .and_then(|v| v.as_smi().map(|n| n as u32 as f64).or(v.as_f64()))
                .unwrap_or(0.0) as u32;
            let resume_pc = o
                .properties
                .get(1)
                .and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64()))
                .unwrap_or(0.0) as usize;
            (fn_idx, resume_pc)
        };
        // Resume against the generator's own program (async generators
        // created in eval carry a nonzero program id).
        let gen_program = self.heap.get(r#gen).program_id;
        let funcs = self.functions_for_program(gen_program);
        let snapshot = self.heap.get(r#gen).elements.clone();
        let env = self.heap.get(r#gen).prototype;
        let f_max_regs = funcs[fn_idx as usize].max_regs;
        let new_base = self.stack.len();
        self.stack
            .resize(new_base + usize::from(f_max_regs), JsValue::undefined());
        let copy_len = snapshot.len().min(usize::from(f_max_regs));
        self.stack[new_base..new_base + copy_len].copy_from_slice(&snapshot[..copy_len]);
        // On resume, feed the value into the yield-destination register.
        let yield_dst = self
            .heap
            .get(r#gen)
            .properties
            .get(3)
            .and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64()))
            .unwrap_or(0.0) as u16;
        if resume_pc != 0 && (yield_dst as usize) < usize::from(f_max_regs) {
            // First entry (`resume_pc == 0`) must not stash the `next()` payload
            // into the generator's stale `yield_dst` (initialized to 0, which
            // aliases register 0 = bound `this`): the body runs its prologue
            // from pc 0, so there is no pending yield destination yet.
            self.stack[new_base + usize::from(yield_dst)] = value;
        }
        // Display snapshot from the generator's live env chain (rebuilt
        // fresh at each resume; the chain may have grown before the yield).
        let env_display = self.env_display_for(env);
        let arguments = self.generator_header_value(r#gen, Self::GENERATOR_ARGUMENTS_SLOT);
        if let Some(arguments) = arguments {
            self.heap.add_root(arguments);
        }
        self.frames.push(Frame {
            fn_idx,
            program: gen_program,
            pc: resume_pc,
            base: new_base,
            max_regs: f_max_regs,
            env,
            env_display,
            generator: Some(r#gen),
            yield_dst: None,
            new_target: None,
            arguments,
        });
        self.top_result = None;
        let frames_before = self.frames.len();
        let exec_res = if is_throw {
            // Arm the boundary BEFORE unwinding: `unwind` consults
            // `stop_at_frames` to decide where to stop. A stale boundary
            // (e.g. an outer nested-execute boundary from a callback call)
            // makes it pop the generator frame -- and then keep draining
            // past it to that stale value -- leaving `execute` below driving
            // an unrelated frame whose register window no longer exists (the
            // `index out of bounds` panic in the `Call` arm).
            //
            // The generator frame is the last one pushed (line above), so
            // the boundary that preserves its caller is `frames_before - 1`:
            // `unwind` may pop the generator frame itself (when the body does
            // not catch the injected throw) but must stop there, leaving
            // `generator_next`'s caller frames intact for the for-of/await
            // dispatch arm, which resumes its own dispatch.
            let saved_stop = self.stop_at_frames;
            self.stop_at_frames = Some(frames_before - 1);
            // Inject the exception through the normal unwind path first.
            if let Err(e) = self.unwind(value) {
                self.stop_at_frames = saved_stop;
                // Nothing in the body caught the injected throw, so the
                // generator completed abruptly. An async generator's request
                // promise rides the completion: it rejects with the error and
                // the requests queued behind it drain (spec 27.6.3.7), rather
                // than staying pending for a body that no longer exists.
                if self.is_async_generator_for(r#gen) && self.heap.get(r#gen).properties.len() >= 3
                {
                    self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
                }
                self.settle_async_gen_request_slot(r#gen, false, e.0, true);
                return Err(e);
            }
            let r = self.execute();
            self.stop_at_frames = None;
            r
        } else {
            self.stop_at_frames = Some(self.frames.len() - 1);
            let r = self.execute();
            self.stop_at_frames = None;
            r
        };
        match exec_res {
            Ok(()) => {
                // Discriminate suspend (done==2.0, frames popped) vs
                // completion (done==1.0).
                let done_val = self
                    .heap
                    .get(r#gen)
                    .properties
                    .get(2)
                    .and_then(|v| v.as_f64().or(v.as_smi().map(|n| n as f64)))
                    .unwrap_or(0.0);
                if done_val == 2.0 && self.frames.len() < frames_before {
                    // Suspended: the yielded value stays in `top_result` for
                    // the caller (`generator_next` wraps it, async resumes
                    // settle the promise with it). `None` marks suspension.
                    // An async generator's pending request promise settles
                    // here — for a real `yield` now, or, for an internal
                    // `await`, not at all (the flag keeps it parked). A plain
                    // sync generator leaves `top_result` for `generator_next`.
                    if self.is_async_generator_for(r#gen) {
                        let yielded = self.top_result.take().unwrap_or(JsValue::undefined());
                        self.settle_async_gen_request_slot(r#gen, true, yielded, false);
                    }
                    Ok(None)
                } else {
                    let ret = self.top_result.take().unwrap_or(JsValue::undefined());
                    // The pending return-completion mode is consumed only by
                    // completion. An internal await may suspend between the
                    // user yield and the delegated return trampoline, so keep
                    // the mode while done == 2 (suspended).
                    {
                        let o = self.heap.get_mut(r#gen);
                        if o.properties.len() >= 6 {
                            o.properties[5] = ops::box_number(0.0);
                        }
                        if o.properties.len() > Self::GENERATOR_RETURN_VALUE_SLOT {
                            o.properties[Self::GENERATOR_RETURN_VALUE_SLOT] = JsValue::undefined();
                        }
                    }
                    if done_val != 1.0 && self.heap.get(r#gen).properties.len() >= 3 {
                        self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
                    }
                    // Async function *and* async generator completion: queue the
                    // completion promise for settlement (the engine drain runs
                    // its reactions — see `pending_settlements`). An async
                    // generator's completion also settles the pending request
                    // promise with `{value: ret, done: true}`.
                    self.settle_async_gen_request_slot(r#gen, false, ret, false);
                    if self.is_async_fn_for(fn_idx, gen_program)
                        && let Some(ph) = self
                            .heap
                            .get(r#gen)
                            .properties
                            .get(4)
                            .and_then(|v| v.as_object())
                    {
                        self.pending_settlements.push((ph, ret, false));
                    }
                    Ok(Some(ret))
                }
            }
            Err(e) => {
                // Pop the generator frame if still there, mark done.
                if self.frames.len() >= frames_before {
                    if let Some(frame) = self.frames.pop() {
                        self.drop_frame_arguments(&frame);
                        self.call_args.remove(&frame.base);
                        self.drop_frame_context(frame.base);
                    }
                    self.stack.truncate(new_base);
                }
                if self.heap.get(r#gen).properties.len() >= 3 {
                    self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
                }
                // An async generator's request promise rejects with the body's
                // abrupt completion rather than surfacing the throw.
                self.settle_async_gen_request_slot(r#gen, false, e.0, true);
                Err(e)
            }
        }
    }
}
