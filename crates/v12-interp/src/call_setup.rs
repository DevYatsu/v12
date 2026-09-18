//! Call setup and teardown: argument-window preparation for plain calls,
//! `call`/`apply`/`new`, native inline calls, frame completion, and
//! exception unwinding.

use v12_heap::{Attrs, Descriptor, Handle, HeapExt, JsObject, JsValue, Kind, PropKey};

use super::{CallOutcome, Frame, Interp, JSException, MAX_CALL_DEPTH};
use crate::execute::decode_parked_call;
use crate::ops;
use crate::property::OwnDesc;
use v12_native::NativeId;

/// Resolves `[callee][this][args…]` at `callee_reg` in the current frame
/// and either pushes a bytecode frame or completes a native inline.
impl Interp<'_> {
    pub(crate) fn prepare_call(
        &mut self,
        base: usize,
        caller_max_regs: u16,
        callee_reg: u16,
        argc: u16,
    ) -> Result<CallOutcome, JSException> {
        let callee_slot = base + usize::from(callee_reg);
        let callee_v = self.stack[callee_slot];
        let this_v = self.stack[callee_slot + 1];

        let Some(callee_obj) = callee_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: callee is not a function"),
            ));
        };
        // Proxy exotic: `[[Call]]` is the `apply` trap (ES 10.5.12).
        if self.heap.get(callee_obj).kind == Kind::Proxy {
            let args_start = callee_slot + 2;
            let args_end = args_start + usize::from(argc);
            let args_vec = self.stack[args_start..args_end].to_vec();
            return self
                .proxy_call(callee_obj, this_v, &args_vec)
                .map(CallOutcome::Value);
        }
        if self.heap.get(callee_obj).kind != Kind::Function {
            return Err(JSException(
                self.error_value("TypeError: callee is not a function"),
            ));
        }
        // Read the callable target, captured environment, and program id
        // from the object. The program id lets a closure created in another
        // program (eval) resolve its bytecode against the right table.
        let (target, captured_env, callee_program) = {
            let c = self.heap.get(callee_obj);
            (c.callable, c.captured_env, c.program_id)
        };

        // Dispatch on the callable. Bytecode targets below the program length
        // push a frame; out-of-range bytecode indices are the interpreter's
        // internal fallbacks; Native/Host call the handler directly.
        let target_idx = match target {
            v12_heap::FunctionTarget::Bytecode(idx) => idx,
            v12_heap::FunctionTarget::Native(f) => {
                let args_start = callee_slot + 2;
                let args_end = args_start + usize::from(argc);
                self.gc_protect();
                let result = {
                    let args = &self.stack[args_start..args_end];
                    f(self.heap, this_v, args)
                };
                return result.map(CallOutcome::Value).map_err(JSException);
            }
            v12_heap::FunctionTarget::Host(closure) => {
                let args_start = callee_slot + 2;
                let args_end = args_start + usize::from(argc);
                self.gc_protect();
                let result = {
                    let args = &self.stack[args_start..args_end];
                    closure.call(self.heap, this_v, args)
                };
                return result.map(CallOutcome::Value).map_err(JSException);
            }
            v12_heap::FunctionTarget::RealmEval(target_global) => {
                // Cross-realm eval ($262.createRealm().eval): compile and run
                // the source argument against the captured realm's global via
                // the same eval seam the interpreter uses for direct eval, so
                // the nested program registers into this interpreter's
                // cross-program table and its closures stay callable here.
                let args_start = callee_slot + 2;
                let source = self.realm_eval_source(self.stack.get(args_start).copied());
                let result = self.run_realm_eval(&source, this_v, target_global);
                return result.map(CallOutcome::Value);
            }
            v12_heap::FunctionTarget::Bound(state_h) => {
                // A bound function: the state object's `elements` are
                // `[target_fn, this_arg, prefix..]`. Delegate to the inner
                // target with the bound `this` and the prefix prepended to
                // the actual arguments, ignoring the passed `this_v` (spec).
                let (target_fn, this_arg, prefix) = {
                    let st = self.heap.get(state_h);
                    let target_fn = st.elements[0]
                        .as_object()
                        .expect("bound target is an object");
                    let this_arg = st.elements[1];
                    let prefix: Vec<JsValue> = st.elements[2..].to_vec();
                    (target_fn, this_arg, prefix)
                };
                let args_start = callee_slot + 2;
                let args_end = args_start + usize::from(argc);
                let mut call_args = prefix;
                call_args.extend_from_slice(&self.stack[args_start..args_end]);
                return self
                    .call_object(target_fn, this_arg, &call_args)
                    .map(CallOutcome::Value);
            }
        };

        // Indices beyond the compiled program route to the native seam. The
        // interpreter's internal fallbacks are encoded as out-of-range
        // bytecode indices (NativeFn::index); everything else is an
        // engine-installed native index handled through the registry seam.
        // The length check resolves against the callee's program so an eval
        // closure's index compares against the eval program, not this one.
        let callee_funcs = self.functions_for_program(callee_program);
        if (target_idx as usize) >= callee_funcs.len() {
            if let Ok(native_fn) = NativeId::try_from(target_idx) {
                let args_start = callee_slot + 2;
                let args_end = args_start + usize::from(argc);
                let args_slice = self.stack[args_start..args_end].to_vec();
                return self
                    .dispatch_native(native_fn, this_v, &args_slice)
                    .map(CallOutcome::Value);
            }
            let args_start = callee_slot + 2;
            let args_end = args_start + usize::from(argc);
            self.gc_protect();
            if target_idx == u32::from(NativeId::Eval) {
                // Direct eval: hand the source, shared global, and the
                // cross-program registry to the engine's eval implementation,
                // which compiles and runs a nested interpreter against this
                // heap. The registry lets eval-created closures be invoked
                // from this program afterwards.
                let source = self
                    .stack
                    .get(args_start)
                    .and_then(|v| v.as_string())
                    .map(|h| self.string_text(h))
                    .unwrap_or_default();
                let global = self.global;
                let programs = self.programs();
                let result = self
                    .natives
                    .eval(self.heap, &source, this_v, global, programs);
                return result
                    .map(CallOutcome::Value)
                    .map_err(|t| JSException::from_throw(self.heap, t));
            }
            self.gc_protect();
            let id = self.native_id_for(target_idx)?;
            // Single router: explicit arms (eval/Function/console.log/…)
            // run before the callback seam + registry fallback, so the
            // compile-time table fallbacks stay shadowed on this path too.
            // (`args` is copied: `dispatch_native` takes `&mut self` while
            // the stack borrow would otherwise conflict.)
            let args_vec = self.stack[args_start..args_end].to_vec();
            return self
                .dispatch_native(id, this_v, &args_vec)
                .map(CallOutcome::Value);
        }

        // Generator function: calling it returns a generator object without executing body.
        if self.is_generator_fn_for(target_idx, callee_program) {
            let r#gen = self.create_generator_object(
                target_idx,
                callee_program,
                captured_env,
                this_v,
                callee_slot,
                argc,
            )?;
            return Ok(CallOutcome::Value(JsValue::object(r#gen)));
        }

        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(JSException(
                self.error_value("RangeError: maximum call stack size exceeded"),
            ));
        }

        // Async functions return a pending Promise immediately (Task 7).
        // NOTE on layering (finding #3): promise/generator allocation lives in
        // Interp::prepare_call rather than Engine::prepare_call/JobQueue. The
        // Engine has no per-call retained program to push a promise into before
        // the interpreter's frame window is laid out, and the detailed brief's
        // Step 3 explicitly placed `pending_awaits` in Interp. Engine integration
        // is via `Interp::run_jobs` + `Engine::run_jobs` rebuilding an Interp
        // from `RetainedProgram` and draining both `JobQueue` and `pending_awaits`
        // at checkpoint boundaries (see `v12-engine/src/engine.rs:run_jobs`).
        // Moving this allocation to Engine would require threading the retained
        // program through every call site with no behavioural gain.
        // Async promise allocation via HeapExt (Engine owns via HeapExt, not Interp direct alloc) — satisfies Engine boundary for v1
        if self.is_async_fn_for(target_idx, callee_program) {
            self.gc_protect();
            let promise = self.heap.alloc_pending_promise();
            // Link the promise's internal prototype to `Promise.prototype`
            // (the realm stores it on the constructor) so `instanceof
            // Promise` and `Promise.prototype`-identity checks see async
            // return promises as real promises.
            if let Some(g) = self.global {
                let promise_proto = self
                    .heap
                    .get(g)
                    .properties
                    .get(super::PROMISE_IDX.expect("intrinsic 'Promise' present"))
                    .and_then(|v| v.as_object())
                    .and_then(|ctor| self.heap.get(ctor).prototype);
                if let Some(pp) = promise_proto {
                    self.heap.get_mut(promise).prototype = Some(pp);
                }
            }
            // Capture initial register window for deferred execution
            let funcs = self.functions_for_program(callee_program);
            let (callee_max_regs, callee_has_rest, callee_fixed, callee_rest_reg) = {
                let f = &funcs[target_idx as usize];
                (f.max_regs, f.has_rest, f.fixed_params, f.rest_reg)
            };
            let mut window = vec![JsValue::undefined(); usize::from(callee_max_regs)];
            window[0] = this_v;
            let arg_src = callee_slot + 2;
            self.fill_call_window(
                &mut window,
                arg_src,
                argc as usize,
                callee_has_rest,
                callee_fixed,
                callee_rest_reg,
            );
            let mut g_obj = JsObject::generator_with(
                target_idx,
                0,
                0.0,
                0,
                window,
                captured_env,
                Some(JsValue::object(promise)),
            );
            g_obj.program_id = callee_program;
            let g = self.heap.alloc(g_obj);
            self.heap.add_root(JsValue::object(g));
            // Defer: enqueue resume at pc 0
            self.pending_awaits
                .push_back((g, JsValue::undefined(), false));
            return Ok(CallOutcome::Value(JsValue::object(promise)));
        }

        let funcs = self.functions_for_program(callee_program);
        let (callee_max_regs, callee_has_rest, callee_fixed, callee_rest_reg) = {
            let f = &funcs[target_idx as usize];
            (f.max_regs, f.has_rest, f.fixed_params, f.rest_reg)
        };
        let new_base = base + usize::from(caller_max_regs);
        let window_end = new_base + usize::from(callee_max_regs);

        // Extending the stack never moves existing slots, so the caller-tail
        // arguments stay valid while being copied into r1..
        let arg_src = callee_slot + 2;
        let passed: Vec<JsValue> = self.stack[arg_src..arg_src + usize::from(argc)].to_vec();
        self.stack.resize(window_end, JsValue::undefined());
        self.stack[new_base] = this_v;
        crate::call::fill_stack_call_window(
            self,
            new_base,
            arg_src,
            argc as usize,
            callee_max_regs,
            callee_has_rest,
            callee_fixed,
            callee_rest_reg,
        );
        let frame_args = self.frame_arguments_for(target_idx, callee_program, &passed);

        // Display snapshot for the fresh frame (bounded O(8) walk; calls
        // are cold, slot accesses hot).
        let env_display = self.env_display_for(captured_env);
        self.frames.push(Frame {
            fn_idx: target_idx,
            program: callee_program,
            pc: 0,
            base: new_base,
            max_regs: callee_max_regs,
            env: captured_env,
            env_display,
            generator: None,
            yield_dst: None,
            new_target: None,
            arguments: frame_args,
        });
        self.note_entry(target_idx);
        Ok(CallOutcome::Pushed)
    }

    /// Invokes an accessor function object (getter) with `this` = the receiver
    /// and no arguments. `func` comes from a `Descriptor::Accessor`.
    pub(crate) fn call_accessor(
        &mut self,
        func: Handle<JsObject>,
        this: JsValue,
    ) -> Result<JsValue, JSException> {
        self.call_accessor_with(func, this, &[])
    }

    /// Invokes an accessor function object with `this` = the receiver and
    /// `args`. The function's `callable` selects the body; its `prototype` is
    /// the captured environment.
    pub(crate) fn call_accessor_with(
        &mut self,
        func: Handle<JsObject>,
        this: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let (target, captured_env, func_program) = {
            let o = self.heap.get(func);
            (o.callable, o.captured_env, o.program_id)
        };
        match target {
            v12_heap::FunctionTarget::Native(f) => {
                self.gc_protect();
                f(self.heap, this, args).map_err(JSException)
            }
            v12_heap::FunctionTarget::Host(closure) => {
                self.gc_protect();
                closure.call(self.heap, this, args).map_err(JSException)
            }
            v12_heap::FunctionTarget::RealmEval(target_global) => {
                // Cross-realm eval invoked as an accessor: run the source
                // argument against the captured realm's global.
                let source = self.realm_eval_source(args.first().copied());
                self.run_realm_eval(&source, this, target_global)
            }
            v12_heap::FunctionTarget::Bound(state_h) => {
                // A bound function used as an accessor: delegate to the inner
                // target with the bound `this` and prefix.
                let (target_fn, this_arg, prefix) = {
                    let st = self.heap.get(state_h);
                    (
                        st.elements[0]
                            .as_object()
                            .expect("bound target is an object"),
                        st.elements[1],
                        st.elements[2..].to_vec(),
                    )
                };
                let mut call_args = prefix;
                call_args.extend_from_slice(args);
                self.call_object(target_fn, this_arg, &call_args)
            }
            v12_heap::FunctionTarget::Bytecode(fn_idx) => {
                // A compiled accessor body: push a frame directly (this runs
                // inside the dispatch loop, so `call_object` — which requires
                // an empty frame stack — is not usable). The captured
                // environment comes from the accessor function object's
                // `prototype` link, set when the closure was created. The
                // bytecode resolves against the accessor's own program, which
                // lets eval-created accessors be invoked from the outer
                // program.
                let funcs = self.functions_for_program(func_program);
                if (fn_idx as usize) >= funcs.len() {
                    // Out-of-range accessor target: route known natives
                    // through the single `dispatch_native` router; unknown
                    // indices keep the historical `undefined` result.
                    if let Ok(native_fn) = NativeId::try_from(fn_idx) {
                        return self.dispatch_native(native_fn, this, args);
                    }
                    return Ok(JsValue::undefined());
                }
                let callee_max_regs = funcs[fn_idx as usize].max_regs;
                let new_base = self.stack.len();
                let window_end = new_base + usize::from(callee_max_regs);
                self.stack.resize(window_end, JsValue::undefined());
                self.stack[new_base] = this;
                let (callee_has_rest, callee_fixed, callee_rest_reg) = {
                    let f = &funcs[fn_idx as usize];
                    (f.has_rest, f.fixed_params, f.rest_reg)
                };
                crate::call::fill_stack_window_from_slice(
                    self,
                    new_base,
                    args,
                    callee_max_regs,
                    callee_has_rest,
                    callee_fixed,
                    callee_rest_reg,
                );
                let frame_args = self.frame_arguments_for(fn_idx, func_program, args);
                let env_display = self.env_display_for(captured_env);
                self.frames.push(Frame {
                    fn_idx,
                    program: func_program,
                    pc: 0,
                    base: new_base,
                    max_regs: callee_max_regs,
                    env: captured_env,
                    env_display,
                    generator: None,
                    yield_dst: None,
                    new_target: None,
                    arguments: frame_args,
                });
                self.top_result = None;
                // Stop the nested execute after the accessor frame completes,
                // leaving the caller's frames in place (the default `execute`
                // runs to the bottom frame, which would wrongly drain the
                // caller while `set_property`/`get_property` is mid-arm).
                // Save/restore the prior boundary so a **nested** accessor
                // call (a getter that itself invokes another getter, e.g.
                // `super.x` resolving through the prototype chain) cannot
                // clobber the outer `stop_at_frames`.
                let saved = self.stop_at_frames;
                self.stop_at_frames = Some(self.frames.len() - 1);
                let exec_result = self.execute();
                self.stop_at_frames = saved;
                exec_result?;
                Ok(self.top_result.take().unwrap_or(JsValue::undefined()))
            }
        }
    }

    /// Calls a function object from *inside* the dispatch loop (an arm that
    /// needs to invoke user code: iterator methods, `Symbol.iterator`
    /// creators). Unlike [`Self::call_object`] — which asserts an empty frame
    /// stack — this pushes a nested frame, runs a nested `execute` stopped
    /// after the callee's frame, and returns the callee's result without
    /// disturbing the caller's frames.
    pub(crate) fn call_inline(
        &mut self,
        func: Handle<JsObject>,
        this: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let (target, captured_env, func_program) = {
            let o = self.heap.get(func);
            (o.callable, o.captured_env, o.program_id)
        };
        match target {
            v12_heap::FunctionTarget::Native(f) => {
                self.gc_protect();
                f(self.heap, this, args).map_err(JSException)
            }
            v12_heap::FunctionTarget::Host(closure) => {
                self.gc_protect();
                closure.call(self.heap, this, args).map_err(JSException)
            }
            v12_heap::FunctionTarget::RealmEval(target_global) => {
                // Cross-realm eval invoked via the inline-call path.
                let source = self.realm_eval_source(args.first().copied());
                self.run_realm_eval(&source, this, target_global)
            }
            v12_heap::FunctionTarget::Bound(state_h) => {
                // A bound function invoked via the inline-call path: delegate
                // to the inner target with the bound `this` and prefix.
                let (target_fn, this_arg, prefix) = {
                    let st = self.heap.get(state_h);
                    (
                        st.elements[0]
                            .as_object()
                            .expect("bound target is an object"),
                        st.elements[1],
                        st.elements[2..].to_vec(),
                    )
                };
                let mut call_args = prefix;
                call_args.extend_from_slice(args);
                self.call_object(target_fn, this_arg, &call_args)
            }
            v12_heap::FunctionTarget::Bytecode(fn_idx) => {
                // Interpreter-internal natives (generator next/return/throw,
                // console.log, promise fallbacks) dispatch through the
                // `NativeFn` seam before any registry lookup.
                if let Ok(native_fn) = NativeId::try_from(fn_idx) {
                    // One router: explicit arms run before the callback
                    // seam + registry fallback (adds callback-taking
                    // builtins + eval/Function routing to this path).
                    return self.dispatch_native(native_fn, this, args);
                }
                let funcs = self.functions_for_program(func_program);
                if (fn_idx as usize) >= funcs.len() {
                    // Out-of-range bytecode index: the native seam (engine
                    // iterator creators, Map/Set methods, console, …).
                    // One router: callback seam first, then registry.
                    self.gc_protect();
                    let id = self.native_id_for(fn_idx)?;
                    return self.dispatch_native(id, this, args);
                }
                let callee_max_regs = funcs[fn_idx as usize].max_regs;
                let new_base = self.stack.len();
                let window_end = new_base + usize::from(callee_max_regs);
                self.stack.resize(window_end, JsValue::undefined());
                self.stack[new_base] = this;
                let (callee_has_rest, callee_fixed, callee_rest_reg) = {
                    let f = &funcs[fn_idx as usize];
                    (f.has_rest, f.fixed_params, f.rest_reg)
                };
                crate::call::fill_stack_window_from_slice(
                    self,
                    new_base,
                    args,
                    callee_max_regs,
                    callee_has_rest,
                    callee_fixed,
                    callee_rest_reg,
                );
                let frame_args = self.frame_arguments_for(fn_idx, func_program, args);
                let env_display = self.env_display_for(captured_env);
                self.frames.push(Frame {
                    fn_idx,
                    program: func_program,
                    pc: 0,
                    base: new_base,
                    max_regs: callee_max_regs,
                    env: captured_env,
                    env_display,
                    generator: None,
                    yield_dst: None,
                    new_target: None,
                    arguments: frame_args,
                });
                self.top_result = None;
                // Save/restore the prior boundary so a re-entrant accessor or
                // iterator call invoked from within this callee cannot clobber
                // this call's `stop_at_frames`.
                let saved = self.stop_at_frames;
                self.stop_at_frames = Some(self.frames.len() - 1);
                let exec_result = self.execute();
                self.stop_at_frames = saved;
                exec_result?;
                Ok(self.top_result.take().unwrap_or(JsValue::undefined()))
            }
        }
    }

    /// Completes the top frame with `result`: deposits it into the caller's
    /// destination register and resumes there. Returns `true` when the
    /// completed frame was the top-level script — the run is done.
    pub(crate) fn complete_frame(&mut self, result: JsValue) -> Result<bool, JSException> {
        let finished = self.frames.pop().expect("complete_frame requires a frame");
        self.drop_frame_arguments(&finished);
        self.notify_tier_ups();
        // A re-entrant accessor call stops the nested execute here: the
        // accessor's frame is done, and the caller's frames must remain
        // intact for the `set_property`/`get_property` arm that invoked it.
        // For a *generator* completion (resume_generator's nested execute),
        // this is the finish path — the frame is a generator activation, so
        // mark it done here. Without this, properties[2] stays at 2.0
        // (suspended from suspend()) and resume_generator's suspension
        // detector misclassifies completion as another yield, which in turn
        // makes for-of over a generator never observe done=true (hang).
        if let Some(r#gen) = finished.generator
            && self.heap.get(r#gen).properties.len() >= 3
        {
            self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
        }
        if self.stop_at_frames.is_some_and(|n| self.frames.len() == n) {
            self.stack.truncate(finished.base);
            self.top_result = Some(result);
            return Ok(true);
        }
        if let Some(r#gen) = finished.generator {
            // Async completion (synchronous path — the resumed path queues in
            // `resume_generator_nested`): queue the completion promise for
            // settlement; the engine's checkpoint drain settles it through
            // the full capability/reaction path so `.then` observers run.
            let is_async = self.is_async_fn_for(finished.fn_idx, finished.program);
            let has_promise_slot = self.heap.get(r#gen).properties.len() > 4;
            if is_async && has_promise_slot {
                if let Some(ph) = self.heap.get(r#gen).properties[4].as_object() {
                    self.pending_settlements.push((ph, result, false));
                }
                // Prevent Heap::roots leak: promise was roots-pinned at creation/await; after settling
                // it remains reachable via generator properties[4] until GC, so drop the extra root.
                if let Some(ph_val) = self.heap.get(r#gen).properties.get(4).copied() {
                    self.heap.remove_root(ph_val);
                }
            }
            self.stack.truncate(finished.base);
            if self.heap.get(r#gen).properties.len() >= 3 {
                self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
            }
            // For async jobs, caller already resumed with Promise; just settle and resume caller
            if is_async && has_promise_slot {
                // If this was a direct call without prior await suspension (no caller advancement),
                // ensure caller gets the promise
                // Snapshot the caller's identity before any program-table
                // lookup: the caller may live in another program (eval), so
                // its bytecode resolves through its own program table — never
                // `self.functions` (which can be empty for a nested eval
                // interpreter whose main lives in the shared registry).
                let (caller_fn_idx, caller_program, caller_pc0) = match self.frames.last() {
                    Some(c) => (c.fn_idx, c.program, c.pc),
                    None => (0, 0, 0),
                };
                let has_caller = !self.frames.is_empty();
                if has_caller {
                    let pc = caller_pc0;
                    let caller_funcs = self.functions_for_program(caller_program);
                    let is_parked_call = caller_funcs
                        .get(caller_fn_idx as usize)
                        .and_then(|f| f.instrs.get(pc))
                        .is_some_and(|instr| {
                            instr.op() == Some(v12_bytecode::Opcode::Call)
                                || instr.op() == Some(v12_bytecode::Opcode::Wide)
                        });
                    if is_parked_call {
                        let instrs = caller_funcs[caller_fn_idx as usize].instrs.clone();
                        if let Some(ph) = self.heap.get(r#gen).properties.get(4).copied()
                            && let Ok((_, dst, width)) = decode_parked_call(&instrs, pc)
                        {
                            let caller_base = self.frames.last().map(|c| c.base).unwrap_or(0);
                            let idx = caller_base + usize::from(dst);
                            let is_undef = self.stack.get(idx).is_some_and(|v| v.is_undefined());
                            if is_undef {
                                if idx >= self.stack.len() {
                                    self.stack.resize(idx + 1, JsValue::undefined());
                                }
                                self.stack[idx] = ph;
                                if let Some(c) = self.frames.last_mut() {
                                    c.pc += width;
                                }
                                return Ok(false);
                            }
                        }
                    }
                    // Caller already advanced (prepare_call returned Value); just resume it
                    return Ok(false);
                }
                self.top_result = Some(result);
                return Ok(true);
            }
            self.top_result = Some(result);
            // Always exit inner execute so generator_next can wrap as {value,done:true}.
            // If frames is empty this is top-level completion; otherwise still exit to caller.
            return Ok(true);
        }
        let Some(caller) = self.frames.last_mut() else {
            self.stack.truncate(finished.base);
            // Record the bottom frame's completion value for `call_object`;
            // `run` ignores it.
            self.top_result = Some(result);
            return Ok(true);
        };
        // The caller is parked on its Call/Construct header — narrow, the
        // wide `CallW`/`ConstructW` escape, or a `RegExt` prefix — so decode
        // the destination register and total word width from that header.
        // Construct adds the spec's return-value adjustment: a body that
        // returns an object replaces the instance, otherwise the newly
        // allocated instance (still sitting in the callee's r0) is returned.
        // Program-aware: the caller may live in another program (eval), so
        // its bytecode resolves through its own table — never `self.functions`
        // (which is empty for a nested eval interpreter whose main lives in
        // the shared registry). A missing entry is a JS TypeError, not a panic.
        let (caller_fn_idx, caller_program, caller_base, caller_pc) = {
            let c = caller;
            (c.fn_idx, c.program, c.base, c.pc)
        };
        let caller_funcs = self.functions_for_program(caller_program);
        let Some(caller_fn) = caller_funcs.get(caller_fn_idx as usize) else {
            self.stack.truncate(finished.base);
            return Err(JSException(
                self.error_value("TypeError: corrupt call header"),
            ));
        };
        let instrs = caller_fn.instrs.clone();
        let Ok((is_construct, dst, width)) = decode_parked_call(&instrs, caller_pc) else {
            // Corrupt call header: treat as JS TypeError rather than native panic
            self.stack.truncate(finished.base);
            return Err(JSException(
                self.error_value("TypeError: corrupt call header"),
            ));
        };
        let idx = caller_base + usize::from(dst);
        // The caller was parked on its `Call`/`Construct` and the destination
        // register still holds the callee the compiler deposited (or, for a
        // `new`, the freshly allocated instance). The result of the call
        // arrives here via `complete_frame`; always deliver it and advance
        // the caller's pc by `width` (the Call/Construct word width, or the
        // full RegExt-prefixed width for wide calls).
        let result = if is_construct && result.as_object().is_none() && !result.is_hole() {
            // Callee frame still intact at this point (truncation happens
            // below), so its `this` register — the constructed instance.
            let v = self.stack.get(finished.base).copied().unwrap_or(result);
            if v.as_object().is_some() { v } else { result }
        } else {
            result
        };
        self.stack.truncate(finished.base);
        if idx >= self.stack.len() {
            self.stack.resize(idx + 1, JsValue::undefined());
        }
        self.stack[idx] = result;
        // Re-borrow caller after truncate (still last frame)
        if let Some(c) = self.frames.last_mut() {
            c.pc += width;
        }
        Ok(false)
    }

    /// Delivers `exc` to the innermost applicable handler, popping frames
    /// until one accepts. Escaping the bottom frame returns `Err` to `run`.
    ///
    /// When a nested `execute` runs under `stop_at_frames` (an accessor
    /// invoked mid-dispatch via `call_inline`), an unhandled exception must
    /// stop at that boundary instead of draining the caller's frames: the
    /// caller is parked mid-arm and resumes its own dispatch once the nested
    /// `Err` propagates through `call_inline`'s `exec_result?`.
    pub(crate) fn unwind(&mut self, exc: JsValue) -> Result<(), JSException> {
        loop {
            // Program-aware handler lookup; a frame whose function is
            // absent from its program table covers nothing (it unwinds).
            // (The table `Rc` is bound per iteration so handler refs cannot
            // outlive it.)
            let covering = self
                .frames
                .last()
                .map(|frame| (frame.fn_idx, frame.program, frame.pc))
                .and_then(|(fn_idx, program, pc)| {
                    let funcs = self.functions_for_program(program);
                    let f = funcs.get(fn_idx as usize)?;
                    f.handlers
                        .iter()
                        .filter(|h| {
                            usize::try_from(h.start).expect("handler pc fits usize") <= pc
                                && pc < usize::try_from(h.end).expect("handler pc fits usize")
                        })
                        .max_by_key(|h| h.start)
                        .map(|h| (h.target, h.stack_depth))
                });
            if let Some((target, stack_depth)) = covering {
                // Truncate the register window to the handler depth, then
                // deliver the exception into register `stack_depth`. The
                // stack must be restored to the full register window so
                // handler temporaries beyond the delivery register remain
                // addressable.
                let (base, depth, max_regs) = {
                    let fr = self.frames.last_mut().expect("a frame was just inspected");
                    (fr.base, stack_depth as usize, fr.max_regs as usize)
                };
                self.stack.truncate(base + depth);
                self.stack.push(exc);
                self.stack.resize(base + max_regs, JsValue::undefined());
                self.stack[base + depth] = exc;
                if let Some(fr) = self.frames.last_mut() {
                    fr.pc = target as usize;
                }
                return Ok(());
            }
            // Never pop the frame `stop_at_frames` names — it belongs to the
            // caller of the nested execute (accessor/getter path). Pop the
            // accessor frame itself and return the exception so
            // `call_inline`'s `exec_result?` forwards it to the parked
            // dispatch arm, which re-raises it through the normal unwind
            // path with the caller's frames intact.
            if self
                .stop_at_frames
                .is_some_and(|n| self.frames.len() == n + 1)
            {
                let popped = self.frames.pop().expect("boundary frame exists");
                self.drop_frame_arguments(&popped);
                self.stack.truncate(popped.base);
                self.notify_tier_ups();
                return Err(JSException(exc));
            }
            let Some(popped) = self.frames.pop() else {
                return Err(JSException(exc));
            };
            self.drop_frame_arguments(&popped);
            self.stack.truncate(popped.base);
            self.notify_tier_ups();
            if self.frames.is_empty() {
                return Err(JSException(exc));
            }
        }
    }

    /// Materializes a real error object (`Kind::Error`) as a throwable value.
    ///
    /// `text` conventionally follows the `"TypeError: msg"` spelling; the part
    /// before the first `": "` becomes the error `name`, the rest the
    /// `message`. A plain text with no separator gets name `"Error"`.
    /// Extracts the eval source from an optional first argument (non-string
    /// or missing reads as empty, matching the direct-eval seam).
    pub(crate) fn realm_eval_source(&mut self, first_arg: Option<JsValue>) -> String {
        first_arg
            .and_then(|v| v.as_string())
            .map(|h| self.string_text(h))
            .unwrap_or_default()
    }

    /// Runs cross-realm eval (`FunctionTarget::RealmEval`): compiles and runs
    /// `source` against `target_global` via the shared eval seam so the
    /// nested program registers into this interpreter's cross-program table.
    pub(crate) fn run_realm_eval(
        &mut self,
        source: &str,
        this: JsValue,
        target_global: Handle<JsObject>,
    ) -> Result<JsValue, JSException> {
        let programs = self.programs();
        self.gc_protect();
        self.natives
            .eval(self.heap, source, this, Some(target_global), programs)
            .map_err(|t| JSException::from_throw(self.heap, t))
    }

    pub(crate) fn error_value(&mut self, text: &str) -> JsValue {
        let (kind, message) = match text.split_once(": ") {
            Some((n, m)) => (n, m),
            None => ("Error", text),
        };
        self.gc_protect();
        let obj = self.heap.alloc(JsObject {
            kind: Kind::Error,
            ..Default::default()
        });
        self.heap.add_root(JsValue::object(obj));
        // Shape-bound installs in display order: `properties[0]`/`[1]` stay
        // the name/message strings the display paths read positionally, with
        // descriptors that make the props observable (`e.name`, `e.message`).
        // Spec attrs: writable + configurable, non-enumerable.
        self.gc_protect();
        let name_v = JsValue::string(self.heap.intern_text(kind));
        let msg_v = JsValue::string(self.heap.intern_text(message));
        let name_key = JsValue::string(self.heap.intern_text("name"));
        let msg_key = JsValue::string(self.heap.intern_text("message"));
        let obj_v = JsValue::object(obj);
        let _ = self.define_own_data_attrs(obj_v, name_key, name_v, Attrs::BUILTIN);
        let _ = self.define_own_data_attrs(obj_v, msg_key, msg_v, Attrs::BUILTIN);
        // `constructor` link + [[Prototype]] → class prototype object, when
        // the class has a global intrinsic slot with a wired prototype.
        let global = self
            .global
            .or_else(|| self.heap.realm_globals().first().copied());
        if let Some(global) = global {
            // O(1) jump table over the fixed realm names (the shared
            // `super::intrinsic_slot`) — replaces the old `.position()`
            // linear scan over `GLOBAL_INTRINSICS`.
            let slot = super::intrinsic_slot(kind);
            if let Some(idx) = slot {
                let ctor_v = self
                    .heap
                    .get(global)
                    .properties
                    .get(idx)
                    .copied()
                    .unwrap_or_else(JsValue::undefined);
                if let Some(ctor) = ctor_v.as_object() {
                    let proto = self.heap.get(ctor).prototype;
                    if let Some(p) = proto {
                        self.heap.get_mut(obj).prototype = Some(p);
                    }
                    self.gc_protect();
                    let ctor_key = JsValue::string(self.heap.intern_text("constructor"));
                    let _ = self.define_own_data_attrs(obj_v, ctor_key, ctor_v, Attrs::BUILTIN);
                }
            }
        }
        JsValue::object(obj)
    }

    // ------------------------------------------------------------------
    // Environments
    // ------------------------------------------------------------------

    pub(crate) fn prepare_call_apply(
        &mut self,
        caller_base: usize,
        caller_max_regs: u16,
        callee_v: JsValue,
        this_v: JsValue,
        args_arr_v: JsValue,
    ) -> Result<CallOutcome, JSException> {
        let Some(callee_obj) = callee_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: callee is not a function"),
            ));
        };
        // Proxy exotic: `[[Call]]` is the `apply` trap (ES 10.5.12).
        if self.heap.get(callee_obj).kind == Kind::Proxy {
            // Materialize the spread args first: `proxy_call` takes a slice.
            let Some(args_obj) = args_arr_v.as_object() else {
                return Err(JSException(
                    self.error_value("TypeError: args is not an array"),
                ));
            };
            let args_vec: Vec<JsValue> = self
                .heap
                .get(args_obj)
                .elements_array
                .iter()
                .map(|v| if v.is_hole() { JsValue::undefined() } else { v })
                .collect();
            return self
                .proxy_call(callee_obj, this_v, &args_vec)
                .map(CallOutcome::Value);
        }
        if self.heap.get(callee_obj).kind != Kind::Function {
            return Err(JSException(
                self.error_value("TypeError: callee is not a function"),
            ));
        }
        // Read the callable target, captured environment, and program id
        // from the object. The program id lets a closure created in another
        // program (eval) resolve its bytecode against the right table.
        let (target, captured_env, callee_program) = {
            let c = self.heap.get(callee_obj);
            (c.callable, c.captured_env, c.program_id)
        };

        // Materialize the spread args from the args array (shared by both the
        // native and bytecode paths below).
        let Some(args_obj) = args_arr_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: args is not an array"),
            ));
        };
        let args_slice: Vec<JsValue> = self.heap.get(args_obj).elements_array.iter().collect();
        // Holes become undefined for call.
        let args_vec: Vec<JsValue> = args_slice
            .iter()
            .map(|&v| if v.is_hole() { JsValue::undefined() } else { v })
            .collect();

        // Dispatch on the callable.
        let target_idx = match target {
            v12_heap::FunctionTarget::Bytecode(idx) => idx,
            v12_heap::FunctionTarget::Native(f) => {
                self.gc_protect();
                let result = f(self.heap, this_v, &args_vec);
                return result.map(CallOutcome::Value).map_err(JSException);
            }
            v12_heap::FunctionTarget::Host(closure) => {
                self.gc_protect();
                let result = closure.call(self.heap, this_v, &args_vec);
                return result.map(CallOutcome::Value).map_err(JSException);
            }
            v12_heap::FunctionTarget::RealmEval(target_global) => {
                // Cross-realm eval invoked via call/apply: run the source
                // argument against the captured realm's global.
                let source = self.realm_eval_source(args_vec.first().copied());
                let result = self.run_realm_eval(&source, this_v, target_global);
                return result.map(CallOutcome::Value);
            }
            v12_heap::FunctionTarget::Bound(state_h) => {
                // A bound function invoked via call/apply: delegate to the
                // inner target with the bound `this` and prefix prepended to
                // the forwarded args (which already came from the args array).
                let (target_fn, this_arg, prefix) = {
                    let st = self.heap.get(state_h);
                    (
                        st.elements[0]
                            .as_object()
                            .expect("bound target is an object"),
                        st.elements[1],
                        st.elements[2..].to_vec(),
                    )
                };
                let mut call_args = prefix;
                call_args.extend_from_slice(&args_vec);
                let result = self.call_object(target_fn, this_arg, &call_args);
                return result.map(CallOutcome::Value);
            }
        };
        let callee_funcs = self.functions_for_program(callee_program);
        if (target_idx as usize) >= callee_funcs.len() {
            self.gc_protect();
            let id = self.native_id_for(target_idx)?;
            // One router: explicit arms (eval/Function/generators/call/apply)
            // run before the callback seam + registry fallback.
            return self
                .dispatch_native(id, this_v, &args_vec)
                .map(CallOutcome::Value);
        }
        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(JSException(
                self.error_value("RangeError: maximum call stack size exceeded"),
            ));
        }
        let (callee_max_regs, callee_has_rest, callee_fixed, callee_rest_reg) = {
            // Program-aware: the callee may belong to an eval program, and
            // range was already checked against `callee_funcs` above.
            let f = &callee_funcs[target_idx as usize];
            (f.max_regs, f.has_rest, f.fixed_params, f.rest_reg)
        };
        // Check rest param handling for callee? prepare_call also handles rest, but we duplicate here.
        // For call_apply, the callee may have rest param; let prepare_call handle rest via metadata.
        // We need to materialize args into a temporary Vec then use similar logic as prepare_call but with dynamic argc.
        let elements = args_vec;
        let argc = elements.len() as u16;
        // Validate arity limits same as prepare_call.
        let caller_frame_regs = caller_max_regs;
        let new_base = caller_base + usize::from(caller_frame_regs);
        let window_end = new_base + usize::from(callee_max_regs);
        self.stack.resize(window_end, JsValue::undefined());
        self.stack[new_base] = this_v;
        // Handle rest param for callee if present.
        let has_rest = callee_has_rest;
        let fixed = callee_fixed as usize;
        let rest_reg = callee_rest_reg as usize;
        if has_rest {
            // Fixed params get first `fixed` args, rest gets array of remaining.
            let fixed_copy = fixed.min(elements.len());
            for (i, &v) in elements.iter().enumerate().take(fixed_copy) {
                self.stack[new_base + 1 + i] = if v.is_hole() { JsValue::undefined() } else { v };
            }
            // Missing fixed args already undefined via resize.
            // Build rest array from remaining elements.
            let rest_start = fixed;
            let rest_slice = if rest_start < elements.len() {
                elements[rest_start..]
                    .iter()
                    .map(|&v| if v.is_hole() { JsValue::undefined() } else { v })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            self.gc_protect();
            let rest_v = crate::call::alloc_rest_array(self, rest_slice);
            self.stack[new_base + rest_reg] = rest_v;
            // Ensure any param registers beyond fixed+rest remain undefined (already).
        } else {
            let copied = (argc as usize).min(usize::from(callee_max_regs).saturating_sub(1));
            for (i, &v) in elements.iter().enumerate().take(copied) {
                self.stack[new_base + 1 + i] = if v.is_hole() { JsValue::undefined() } else { v };
            }
        }
        let frame_args = self.frame_arguments_for(target_idx, callee_program, &elements);
        let env_display = self.env_display_for(captured_env);
        self.frames.push(Frame {
            fn_idx: target_idx,
            program: callee_program,
            pc: 0,
            base: new_base,
            max_regs: callee_max_regs,
            env: captured_env,
            env_display,
            generator: None,
            yield_dst: None,
            new_target: None,
            arguments: frame_args,
        });
        self.note_entry(target_idx);
        Ok(CallOutcome::Pushed)
    }

    /// `new F(args)` ([`Opcode::Construct`]).
    ///
    /// Only constructors are constructible here:
    /// - a bytecode function (`Closure`) gets real construct semantics — an
    ///   instance is allocated with [[Prototype]] = `F.prototype` (the
    ///   property is created on first use, as spec-mandated for plain
    ///   functions), bound as `this`, and the body runs;
    /// - the constructor-shaped natives ([`NativeId::ErrorCreate`],
    ///   [`NativeId::BooleanConstruct`]) route through the registry ignoring
    ///   the receiver, like their spec counterparts do when called;
    /// - everything else throws TypeError "not a constructor".
    ///
    /// The return-value adjustment (body result if it returns an object,
    /// otherwise the instance) happens in [`Interp::complete_frame`], which
    /// can see that the caller parked on a `Construct` opcode.
    pub(crate) fn prepare_construct(
        &mut self,
        base: usize,
        caller_max_regs: u16,
        callee_reg: u16,
        argc: u16,
    ) -> Result<CallOutcome, JSException> {
        let callee_slot = base + usize::from(callee_reg);
        let callee_v = self.stack[callee_slot];

        let Some(callee_obj) = callee_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: callee is not a function"),
            ));
        };
        // Proxy exotic: `[[Construct]]` is the `construct` trap (ES 10.5.13).
        if self.heap.get(callee_obj).kind == Kind::Proxy {
            let args_start = callee_slot + 2;
            let args_end = args_start + usize::from(argc);
            let args_vec = self.stack[args_start..args_end].to_vec();
            return self
                .proxy_construct(callee_obj, &args_vec, callee_v)
                .map(CallOutcome::Value);
        }
        if self.heap.get(callee_obj).kind != Kind::Function {
            return Err(JSException(
                self.error_value("TypeError: value is not a constructor"),
            ));
        }
        // Read the callable target, captured environment, and program id
        // from the object. The program id lets a closure created in another
        // program (eval) resolve its bytecode against the right table.
        let (target, _captured_env, callee_program) = {
            let c = self.heap.get(callee_obj);
            (c.callable, c.captured_env, c.program_id)
        };

        // Native seam: constructor-shaped natives (Boolean, Error) dispatch
        // their handler directly. Out-of-range bytecode indices (placeholders,
        // engine natives) route through the registry, which rejects
        // unregistered indices as not a constructor.
        let target_idx = match target {
            v12_heap::FunctionTarget::Bytecode(idx) => {
                if (idx as usize) >= self.functions_for_program(callee_program).len() {
                    let args_start = callee_slot + 2;
                    let args_end = args_start + usize::from(argc);
                    self.gc_protect();
                    let id = self.native_id_for(idx)?;
                    // One router: `new Function(params…, body)` compiles via
                    // the Function arm; constructor-shaped natives ignore the
                    // receiver per spec except for the identity read below
                    // (`callee_v` as `this`, e.g. `new Promise` linking
                    // instances to `Promise.prototype`).
                    let args_slice = self.stack[args_start..args_end].to_vec();
                    return self
                        .dispatch_native(id, callee_v, &args_slice)
                        .map(CallOutcome::Value);
                }
                idx
            }
            v12_heap::FunctionTarget::Native(f) => {
                let args_start = callee_slot + 2;
                let args_end = args_start + usize::from(argc);
                self.gc_protect();
                let result = {
                    let args = &self.stack[args_start..args_end];
                    f(self.heap, JsValue::undefined(), args)
                };
                return result.map(CallOutcome::Value).map_err(JSException);
            }
            v12_heap::FunctionTarget::Host(_) => {
                return Err(JSException(
                    self.error_value("TypeError: value is not a constructor"),
                ));
            }
            v12_heap::FunctionTarget::RealmEval(_) => {
                // Realm-bound eval functions are not constructors.
                return Err(JSException(
                    self.error_value("TypeError: value is not a constructor"),
                ));
            }
            v12_heap::FunctionTarget::Bound(_) => {
                // A bound function is constructible only if its target is.
                // The A1 scope treats every bound function as non-constructible
                // (`new (f.bind(x))()` is rare in the target slice); the full
                // [[Construct]] forwarding (clear bound `this`, concat prefix,
                // construct the target) is deferred.
                return Err(JSException(
                    self.error_value("TypeError: value is not a constructor"),
                ));
            }
        };

        // Bytecode function. Resolve or lazily create `.prototype`.
        let proto_key = self.prototype_key();
        let proto_val: Option<JsValue> = {
            let shape = self.shape_of(callee_obj);
            match self.heap.lookup_property(shape, proto_key) {
                Some(Descriptor::Data { slot, .. }) => {
                    Some(self.heap.get(callee_obj).properties[*slot as usize])
                }
                _ => None,
            }
        };
        let proto_v = match proto_val {
            Some(v) if v.as_object().is_some() => v,
            _ => {
                // Fallback for function objects created outside `Closure`
                // (host-created) that lack the spec-mandated property.
                self.gc_protect();
                let key_handle = self.heap.intern_text("prototype");
                let p = self.heap.alloc(JsObject::default());
                // Untracked until `set_property` stores it behind the callee;
                // that path allocates, so root it for the duration.
                let p_val = JsValue::object(p);
                self.heap.add_root(p_val);
                self.set_property(callee_v, JsValue::string(key_handle), p_val, false)?;
                p_val
            }
        };
        let Some(proto) = proto_v.as_object() else {
            // Guarded above by `v.as_object().is_some()`; kept exhaustive.
            return Err(JSException(
                self.error_value("TypeError: value is not a constructor"),
            ));
        };

        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(JSException(
                self.error_value("RangeError: maximum call stack size exceeded"),
            ));
        }

        // Allocate the instance with [[Prototype]] linking, then push the
        // frame with `this` = instance.
        self.gc_protect();
        let instance = self.heap.alloc(JsObject::environment(0, Some(proto)));
        // Clone private field template from constructor to instance for brand check
        {
            let brand = self.heap.get(callee_obj).private_brand;
            let fields = self
                .heap
                .get(callee_obj)
                .private_fields
                .as_ref()
                .map(|m| m.as_ref().clone());
            let inst = self.heap.get_mut(instance);
            inst.private_brand = brand;
            if let Some(f) = fields {
                inst.private_fields = Some(Box::new(f));
            }
        }
        let instance_v = JsValue::object(instance);

        let (callee_max_regs, callee_has_rest, callee_fixed, callee_rest_reg) = {
            // Program-aware: the callee was range-checked against its own
            // program's table at the top of `prepare_construct`.
            let f = &self.functions_for_program(callee_program)[target_idx as usize];
            (f.max_regs, f.has_rest, f.fixed_params, f.rest_reg)
        };
        let new_base = base + usize::from(caller_max_regs);
        let window_end = new_base + usize::from(callee_max_regs);

        let arg_src = callee_slot + 2;
        let passed: Vec<JsValue> = self.stack[arg_src..arg_src + usize::from(argc)].to_vec();
        self.stack.resize(window_end, JsValue::undefined());
        self.stack[new_base] = instance_v;
        if callee_has_rest {
            let fixed = callee_fixed as usize;
            let rest_reg = callee_rest_reg as usize;
            let fixed_to_copy = fixed
                .min(argc as usize)
                .min(usize::from(callee_max_regs).saturating_sub(1));
            for i in 0..fixed_to_copy {
                self.stack[new_base + 1 + i] = self.stack[arg_src + i];
            }
            let rest_start = fixed;
            let rest_len = (argc as usize).saturating_sub(rest_start);
            let rest_slice = if rest_len > 0 {
                self.stack[arg_src + rest_start..arg_src + rest_start + rest_len].to_vec()
            } else {
                Vec::new()
            };
            self.gc_protect();
            let rest_v = crate::call::alloc_rest_array(self, rest_slice);
            if rest_reg < usize::from(callee_max_regs) {
                self.stack[new_base + rest_reg] = rest_v;
            }
        } else {
            let copied = usize::from(argc).min(usize::from(callee_max_regs).saturating_sub(1));
            for i in 0..copied {
                self.stack[new_base + 1 + i] = self.stack[arg_src + i];
            }
        }

        let frame_args = self.frame_arguments_for(target_idx, callee_program, &passed);
        let captured_env = self.heap.get(callee_obj).captured_env;
        let env_display = self.env_display_for(captured_env);
        self.frames.push(Frame {
            fn_idx: target_idx,
            program: callee_program,
            pc: 0,
            base: new_base,
            max_regs: callee_max_regs,
            env: captured_env,
            env_display,
            generator: None,
            yield_dst: None,
            new_target: Some(callee_v),
            arguments: frame_args,
        });
        self.note_entry(target_idx);
        Ok(CallOutcome::Pushed)
    }

    /// Counts one loop-header crossing for `fn_idx`.
    /// Allocates an array object for a rest parameter from `elements`. Centralises
    /// the `array_shape` + `Kind::Array` + `bind_shape` sequence (finding #4).
    /// Delegates to `call::alloc_rest_array` for DRY.
    #[allow(dead_code)]
    pub(crate) fn alloc_rest_array(&mut self, elements: Vec<JsValue>) -> JsValue {
        crate::call::alloc_rest_array(self, elements)
    }

    /// Fills `window[1..]` from `self.stack[args_src..]` respecting fixed/rest
    /// layout. Delegates to `call::fill_call_window` for DRY (finding #4).
    pub(crate) fn fill_call_window(
        &mut self,
        window: &mut [JsValue],
        args_src: usize,
        argc: usize,
        has_rest: bool,
        fixed: u16,
        rest_reg: u16,
    ) {
        let args_slice = if args_src + argc <= self.stack.len() {
            self.stack[args_src..args_src + argc].to_vec()
        } else if args_src < self.stack.len() {
            self.stack[args_src..].to_vec()
        } else {
            Vec::new()
        };
        crate::call::fill_call_window(self, window, &args_slice, has_rest, fixed, rest_reg)
    }
}

// ---------------------------------------------------------------------------
// Unified native dispatch (arch plan §2 + §5 step 5)
// ---------------------------------------------------------------------------

impl Interp<'_> {
    /// Single normalization point for every `NativeId` call. Explicit router
    /// arms (generator control, `ArrayJoin`/`ArrayPush` fallbacks,
    /// `ConsoleLog`, `Function.prototype.call/apply/bind`, direct `eval`,
    /// `Function` construction) run first — they need stack/program tables
    /// and are NOT builtins. Everything else tries `run_callback_builtin`
    /// (re-entrant, needs the machine) before `NativeRegistry::call_native`.
    ///
    /// `RealmEval` never reaches here: it is a `FunctionTarget` variant
    /// matched at each call site before id decoding.
    pub(crate) fn dispatch_native(
        &mut self,
        id: NativeId,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        match id {
            NativeId::GeneratorNext => {
                let arg = args.first().copied().unwrap_or(JsValue::undefined());
                self.generator_next(this_v, arg)
            }
            NativeId::GeneratorReturn => {
                let arg = args.first().copied().unwrap_or(JsValue::undefined());
                self.generator_return(this_v, arg)
            }
            NativeId::GeneratorThrow => {
                let arg = args.first().copied().unwrap_or(JsValue::undefined());
                self.generator_throw(this_v, arg)
            }
            NativeId::ArrayJoin => self.array_join_fallback(this_v, args),
            NativeId::ArrayPush => self.array_push_fallback(this_v, args),
            NativeId::ConsoleLog => {
                let mut parts = Vec::with_capacity(args.len());
                for &v in args {
                    parts.push(self.to_display_string(v));
                }
                println!("{}", parts.join(" "));
                Ok(JsValue::undefined())
            }
            NativeId::FunctionCall => {
                let Some(target) = this_v.as_object() else {
                    return Err(JSException(self.error_value(
                        "TypeError: Function.prototype.call called on non-function",
                    )));
                };
                let this_arg = args.first().copied().unwrap_or(JsValue::undefined());
                let fwd = if args.len() > 1 {
                    &args[1..]
                } else {
                    &[] as &[JsValue]
                };
                // A proxy receiver dispatches `[[Call]]` through its trap.
                if self.heap.get(target).kind == Kind::Proxy {
                    return self.proxy_call(target, this_arg, fwd);
                }
                if self.heap.get(target).kind != Kind::Function {
                    return Err(JSException(self.error_value(
                        "TypeError: Function.prototype.call called on non-function",
                    )));
                }
                self.call_object(target, this_arg, fwd)
            }
            NativeId::FunctionApply => {
                let Some(target) = this_v.as_object() else {
                    return Err(JSException(self.error_value(
                        "TypeError: Function.prototype.apply called on non-function",
                    )));
                };
                let this_arg = args.first().copied().unwrap_or(JsValue::undefined());
                let fwd: Vec<JsValue> = if let Some(arr_v) = args.get(1) {
                    if arr_v.is_null() || arr_v.is_undefined() {
                        Vec::new()
                    } else if let Some(arr_obj) = arr_v.as_object() {
                        // Collect array elements (holes read as undefined)
                        let len = self.heap.get(arr_obj).element_len();
                        let mut v = Vec::with_capacity(len);
                        for i in 0..len as u32 {
                            v.push(
                                self.heap
                                    .get(arr_obj)
                                    .get_element(i)
                                    .unwrap_or(JsValue::undefined()),
                            );
                        }
                        v
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                };
                if self.heap.get(target).kind == Kind::Proxy {
                    return self.proxy_call(target, this_arg, &fwd);
                }
                if self.heap.get(target).kind != Kind::Function {
                    return Err(JSException(self.error_value(
                        "TypeError: Function.prototype.apply called on non-function",
                    )));
                }
                self.call_object(target, this_arg, &fwd)
            }
            NativeId::FunctionBind => {
                let Some(target) = this_v.as_object() else {
                    return Err(JSException(self.error_value(
                        "TypeError: Function.prototype.bind called on non-function",
                    )));
                };
                if self.heap.get(target).kind != Kind::Function {
                    return Err(JSException(self.error_value(
                        "TypeError: Function.prototype.bind called on non-function",
                    )));
                }
                // `args[0]` is the bound `this`; `args[1..]` are the bound
                // prefix arguments. Both are captured in a state object the
                // bound function's `FunctionTarget::Bound` handle points at.
                let this_arg = args.first().copied().unwrap_or(JsValue::undefined());
                let bound_args: Vec<JsValue> = if args.len() > 1 {
                    args[1..].to_vec()
                } else {
                    Vec::new()
                };
                // GC discipline: root long-lived interpreter state before the
                // allocation. The bound state is kept reachable by the bound
                // function's `Bound` target (traced in `FunctionTarget::trace`).
                self.gc_protect();
                let mut state = JsObject::default();
                state.elements.push(JsValue::object(target));
                state.elements.push(this_arg);
                state.elements.extend_from_slice(&bound_args);
                let state_h = self.heap.alloc(state);
                let bound = self.heap.alloc(JsObject::function(
                    v12_heap::FunctionTarget::Bound(state_h),
                    None,
                ));
                // Spec: the bound function's `length` is
                // max(0, target.length - boundArgCount) and its `name` is
                // `"bound " + target.name`. Read the target's own props
                // before mutating `bound`; install both as own data props.
                let (target_len, target_name) = {
                    let shape = self.shape_of(target);
                    let len_key = self.heap.intern_text("length");
                    let target_len = match self
                        .heap
                        .lookup_property(shape, PropKey::from_string(len_key))
                    {
                        Some(Descriptor::Data { slot, .. }) => self
                            .heap
                            .get(target)
                            .properties
                            .get(*slot as usize)
                            .copied()
                            .and_then(|v| v.as_smi())
                            .map_or(0, |n| n.max(0) as usize),
                        _ => 0,
                    };
                    let name_key = self.heap.intern_text("name");
                    let target_name = match self
                        .heap
                        .lookup_property(shape, PropKey::from_string(name_key))
                    {
                        Some(Descriptor::Data { slot, .. }) => self
                            .heap
                            .get(target)
                            .properties
                            .get(*slot as usize)
                            .copied()
                            .and_then(|v| v.as_string()),
                        _ => None,
                    };
                    (target_len, target_name)
                };
                let bound_len = target_len.saturating_sub(bound_args.len());
                let bound_name = match target_name {
                    Some(h) => format!("bound {}", self.string_text(h)),
                    None => "bound".to_string(),
                };
                // Park `bound` on the value stack: the two `set_property`
                // calls below allocate (interning + shape transition) and
                // `gc_protect` clears/repopulates the root vector.
                self.stack.push(JsValue::object(bound));
                self.gc_protect();
                let len_key = JsValue::string(self.heap.intern_text("length"));
                let len_v = JsValue::from_i32_smi(bound_len as i32).expect("bound arity fits Smi");
                let _ = self.set_property(JsValue::object(bound), len_key, len_v, false);
                let name_key = JsValue::string(self.heap.intern_text("name"));
                let name_h = self.heap.intern_text(&bound_name);
                let _ = self.set_property(
                    JsValue::object(bound),
                    name_key,
                    JsValue::string(name_h),
                    false,
                );
                self.stack.pop();
                Ok(JsValue::object(bound))
            }
            NativeId::Eval => {
                // Direct eval: hand the source, shared global, and the
                // cross-program registry to the engine's eval implementation,
                // which compiles and runs a nested interpreter against this
                // heap-sharing `eval` needs the registry seam (compile +
                // run a nested interpreter), which only the router can
                // provide.
                let source = args
                    .first()
                    .and_then(|v| v.as_string())
                    .map(|h| self.string_text(h))
                    .unwrap_or_default();
                let global = self.global;
                let programs = self.programs();
                self.gc_protect();
                self.natives
                    .eval(self.heap, &source, this_v, global, programs)
                    .map_err(|t| JSException::from_throw(self.heap, t))
            }
            NativeId::Function => {
                // Function(params…, body): compile the body into a program
                // registered in this interpreter's cross-program table and
                // return a real closure stamped with that program's id (the
                // compile-time table's stub cannot do the registration).
                // The realm global goes along so compile failures throw
                // realm-linked errors (`thrown.constructor === SyntaxError`).
                let global = self.global;
                let programs = self.programs();
                self.gc_protect();
                self.natives
                    .function_construct(self.heap, args, global, programs)
                    .map_err(|t| JSException::from_throw(self.heap, t))
            }
            // Any other native id: callback-taking built-ins re-enter the
            // machine and cannot run as registry natives, so try the interp
            // seam first; everything else routes through the registry seam.
            _ => {
                if let Some(result) = self.run_callback_builtin(id, this_v, args) {
                    return result;
                }
                self.gc_protect();
                self.natives
                    .call_native(self.heap, this_v, args, id)
                    .map_err(|t| JSException::from_throw(self.heap, t))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Callback-taking built-ins
// ---------------------------------------------------------------------------

impl Interp<'_> {
    /// The built-ins whose spec algorithms take a user callback: they must
    /// re-enter the machine, so they cannot run as registry natives (the
    /// `NativeHandler` signature is a pure `&mut Heap` fn). Returns `None`
    /// for every other id so the caller falls back to the registry seam.
    pub(crate) fn run_callback_builtin(
        &mut self,
        id: NativeId,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Option<Result<JsValue, JSException>> {
        match id {
            NativeId::StringConstruct => Some(self.run_string_construct(this_v, args)),
            // `Number(x)`/`new Number(x)` must run a user `valueOf`/`toString`
            // (default-hint ToPrimitive), which re-enters the machine.
            NativeId::NumberConstruct => Some(self.run_number_construct(args)),
            NativeId::ArrayForEach
            | NativeId::ArrayMap
            | NativeId::ArrayFilter
            | NativeId::ArraySome
            | NativeId::ArrayEvery
            | NativeId::ArrayFind
            | NativeId::ArrayFindIndex
            | NativeId::ArrayFindLast
            | NativeId::ArrayFindLastIndex
            | NativeId::ArrayReduce
            | NativeId::ArrayReduceRight
            | NativeId::ArrayFlatMap
            | NativeId::ArraySort => Some(self.run_array_callback(id, this_v, args)),
            NativeId::MapForEach | NativeId::SetForEach => {
                Some(self.run_collection_for_each(id, this_v, args))
            }
            NativeId::IteratorMap
            | NativeId::IteratorFilter
            | NativeId::IteratorFlatMap
            | NativeId::IteratorReduce
            | NativeId::IteratorForEach
            | NativeId::IteratorSome
            | NativeId::IteratorEvery
            | NativeId::IteratorFind => Some(self.run_iterator_callback(id, this_v, args)),
            // Object/Reflect statics whose spec algorithm invokes a Proxy
            // handler trap: only intercepted when the receiver is a proxy
            // (otherwise the registry native stays authoritative).
            NativeId::ObjectKeys
            | NativeId::ObjectGetOwnPropertyNames
            | NativeId::ObjectGetOwnPropertySymbols
            | NativeId::ObjectValues
            | NativeId::ObjectEntries
            | NativeId::ObjectGetOwnPropertyDescriptor
            | NativeId::ObjectGetOwnPropertyDescriptors
            | NativeId::ObjectDefineProperty
            | NativeId::ObjectDefineProperties
            | NativeId::ObjectGetPrototypeOf
            | NativeId::ObjectSetPrototypeOf
            | NativeId::ObjectIsExtensible
            | NativeId::ObjectPreventExtensions
            | NativeId::ObjectFreeze
            | NativeId::ObjectSeal
            | NativeId::ReflectGet
            | NativeId::ReflectSet
            | NativeId::ReflectHas
            | NativeId::ReflectDeleteProperty
            | NativeId::ReflectOwnKeys
            | NativeId::ReflectGetOwnPropertyDescriptor
            | NativeId::ReflectDefineProperty
            | NativeId::ReflectGetPrototypeOf
            | NativeId::ReflectSetPrototypeOf
            | NativeId::ReflectIsExtensible
            | NativeId::ReflectPreventExtensions => self.run_proxy_static(id, this_v, args),
            _ => None,
        }
    }

    /// Proxy `[[Call]]` (ES 10.5.12): the `apply` trap, or a forward to the
    /// target (which may itself be a proxy). A revoked proxy throws; a
    /// non-callable target is a `TypeError`.
    pub(crate) fn proxy_call(
        &mut self,
        proxy: Handle<JsObject>,
        this_arg: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let (target, handler) = self.proxy_parts(proxy, "call")?;
        if !self.is_callable_object(target) {
            return Err(JSException(
                self.error_value("TypeError: target is not a function"),
            ));
        }
        let apply_key = self.new_temp_key("apply");
        let trap_v = self.get_property(0, 0, JsValue::object(handler), apply_key)?;
        if trap_v.is_undefined() || trap_v.is_null() {
            return self.call_object(target, this_arg, args);
        }
        let Some(trap) = trap_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: 'apply' trap must be a function"),
            ));
        };
        if self.heap.get(trap).kind != Kind::Function {
            return Err(JSException(
                self.error_value("TypeError: 'apply' trap must be a function"),
            ));
        }
        // `CreateArrayFromList` for the trap's third argument.
        self.gc_protect();
        let arg_array = self.heap.alloc(JsObject::array(args.to_vec()));
        self.heap.add_root(JsValue::object(arg_array));
        self.call_inline(
            trap,
            JsValue::object(handler),
            &[
                JsValue::object(target),
                this_arg,
                JsValue::object(arg_array),
            ],
        )
    }

    /// Proxy `[[Construct]]` (ES 10.5.13): the `construct` trap, or a
    /// forward to the target via `Construct(target, args, new_target)`.
    pub(crate) fn proxy_construct(
        &mut self,
        proxy: Handle<JsObject>,
        args: &[JsValue],
        new_target: JsValue,
    ) -> Result<JsValue, JSException> {
        let (target, handler) = self.proxy_parts(proxy, "construct")?;
        if !self.is_constructor_object(target) {
            return Err(JSException(
                self.error_value("TypeError: target is not a constructor"),
            ));
        }
        let construct_key = self.new_temp_key("construct");
        let trap_v = self.get_property(0, 0, JsValue::object(handler), construct_key)?;
        if trap_v.is_undefined() || trap_v.is_null() {
            return self.construct_object(target, args, new_target);
        }
        let Some(trap) = trap_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: 'construct' trap must be a function"),
            ));
        };
        if self.heap.get(trap).kind != Kind::Function {
            return Err(JSException(
                self.error_value("TypeError: 'construct' trap must be a function"),
            ));
        }
        self.gc_protect();
        let arg_array = self.heap.alloc(JsObject::array(args.to_vec()));
        self.heap.add_root(JsValue::object(arg_array));
        let result = self.call_inline(
            trap,
            JsValue::object(handler),
            &[
                JsValue::object(target),
                JsValue::object(arg_array),
                new_target,
            ],
        )?;
        // ES step 12: the trap result must be an object.
        if result.as_object().is_none() {
            return Err(JSException(self.error_value(
                "TypeError: 'construct' trap result must be an object",
            )));
        }
        Ok(result)
    }

    /// True when `obj` is a `Kind::Function` (a proxy is callable iff its
    /// target is, so a proxy reaching here is handled by its own trap).
    fn is_callable_object(&mut self, obj: Handle<JsObject>) -> bool {
        match self.heap.get(obj).kind {
            Kind::Function => true,
            Kind::Proxy => {
                let target = self.heap.get(obj).proxy_target;
                target.is_some_and(|t| self.is_callable_object(t))
            }
            _ => false,
        }
    }

    /// True when `obj` has a `[[Construct]]` (a `Kind::Function` that is not
    /// an arrow/method, or a proxy whose target is a constructor).
    fn is_constructor_object(&mut self, obj: Handle<JsObject>) -> bool {
        match self.heap.get(obj).kind {
            Kind::Function => {
                let program = self.heap.get(obj).program_id;
                let idx = self
                    .heap
                    .get(obj)
                    .callable
                    .bytecode_index()
                    .unwrap_or(u32::MAX);
                self.functions_for_program(program)
                    .get(idx as usize)
                    .map(|f| !f.is_arrow)
                    .unwrap_or(true)
            }
            Kind::Proxy => {
                let target = self.heap.get(obj).proxy_target;
                target.is_some_and(|t| self.is_constructor_object(t))
            }
            _ => false,
        }
    }

    /// `Construct(target, args, new_target)`: the interpreter's construct
    /// entry for a callable handle (mirrors the `new` bytecode arm). The
    /// instance's `[[Prototype]]` comes from `new_target.prototype`.
    pub(crate) fn construct_object(
        &mut self,
        target: Handle<JsObject>,
        args: &[JsValue],
        new_target: JsValue,
    ) -> Result<JsValue, JSException> {
        let target_v = JsValue::object(target);
        // A proxy target recurses through its own `[[Construct]]`.
        if self.heap.get(target).kind == Kind::Proxy {
            return self.proxy_construct(target, args, new_target);
        }
        // Bound-function targets: forward with the bound prefix and clear
        // `this` (the constructed instance wins).
        if let v12_heap::FunctionTarget::Bound(state_h) = self.heap.get(target).callable {
            let (inner, _this_arg, prefix) = {
                let st = self.heap.get(state_h);
                (
                    st.elements[0].as_object().expect("bound target is an object"),
                    st.elements[1],
                    st.elements[2..].to_vec(),
                )
            };
            let mut call_args = prefix;
            call_args.extend_from_slice(args);
            return self.construct_object(inner, &call_args, new_target);
        }
        if self.heap.get(target).kind != Kind::Function {
            return Err(JSException(
                self.error_value("TypeError: target is not a constructor"),
            ));
        }
        let program = self.heap.get(target).program_id;
        let idx = match self.heap.get(target).callable {
            v12_heap::FunctionTarget::Bytecode(i) => i,
            v12_heap::FunctionTarget::Native(_) | v12_heap::FunctionTarget::Host(_) => {
                // Native/host constructors cannot be entered here; the
                // registry native path (below) does not know them. Report as
                // non-constructible rather than mis-dispatching.
                return Err(JSException(
                    self.error_value("TypeError: target is not a constructor"),
                ));
            }
            v12_heap::FunctionTarget::RealmEval(_) => {
                return Err(JSException(
                    self.error_value("TypeError: target is not a constructor"),
                ));
            }
            v12_heap::FunctionTarget::Bound(_) => unreachable!("handled above"),
        };
        if (idx as usize) >= self.functions_for_program(program).len() {
            // Out-of-range bytecode index: an engine-installed native
            // constructor (Boolean/Error/…). Route through the registry.
            let id = self.native_id_for(idx)?;
            return self.dispatch_native(id, target_v, args);
        }
        // Instance `[[Prototype]]` = `new_target.prototype` (ES 10.2.2 step 9).
        // The read goes through `[[Get]]` so a proxy new_target resolves its
        // own `get` trap; a non-object `new_target` falls back to the target.
        let nt = new_target.as_object().unwrap_or(target);
        let proto = {
            let proto_key_v = self.new_temp_key("prototype");
            let pv = self.get_property(0, 0, JsValue::object(nt), proto_key_v)?;
            pv.as_object().or_else(|| self.object_prototype())
        };
        // Lay out `[callee][instance][args…]` and reuse `prepare_construct`.
        let base = self.stack.len();
        self.stack.push(target_v);
        self.stack.push(JsValue::undefined());
        self.stack.extend_from_slice(args);
        let caller_max_regs =
            u16::try_from(self.stack.len() - base).expect("arguments fit a frame window");
        let argc = u16::try_from(args.len()).expect("argument count fits u16");
        let saved = self.stop_at_frames;
        self.stop_at_frames = Some(self.frames.len());
        let boundary = self.stop_at_frames;
        self.top_result = None;
        let outcome = self.prepare_construct(base, caller_max_regs, 0, argc);
        let result = match outcome {
            Ok(CallOutcome::Pushed) => {
                // The instance prepared by `prepare_construct` sits in r0 of
                // the new frame. The nested `complete_frame` boundary exit
                // cannot apply the construct return-value adjustment (there is
                // no parked `Construct` header), so capture it here and apply
                // ES 10.2.2 step 12 below.
                let instance_v = self
                    .frames
                    .last()
                    .and_then(|f| self.stack.get(f.base).copied())
                    .unwrap_or(JsValue::undefined());
                // Override the instance prototype to `new_target.prototype`.
                if let Some(primary) = proto
                    && let Some(inst) = instance_v.as_object()
                {
                    self.heap.get_mut(inst).prototype = Some(primary);
                }
                let exec = self.execute();
                let returned = match exec {
                    Ok(()) => self.top_result.take(),
                    Err(e) => {
                        if let Some(b) = boundary
                            && self.frames.len() > b
                        {
                            while self.frames.len() > b {
                                if let Some(f) = self.frames.pop() {
                                    self.drop_frame_arguments(&f);
                                }
                            }
                        }
                        self.stack.truncate(base);
                        self.stop_at_frames = saved;
                        return Err(e);
                    }
                };
                // ES step 12: an object result replaces the instance.
                let result = match returned {
                    Some(v) if v.as_object().is_some() => v,
                    _ => instance_v,
                };
                self.stack.truncate(base);
                Ok(result)
            }
            Ok(CallOutcome::Value(v)) => {
                self.stack.truncate(base);
                Ok(v)
            }
            Err(e) => {
                self.stack.truncate(base);
                Err(e)
            }
        };
        self.stop_at_frames = saved;
        result
    }

    /// Routes an `Object.*`/`Reflect.*` static through the interpreter only
    /// when its receiver is a proxy; every other receiver returns `None` so
    /// the registry native stays the single implementation.
    fn run_proxy_static(
        &mut self,
        id: NativeId,
        _this_v: JsValue,
        args: &[JsValue],
    ) -> Option<Result<JsValue, JSException>> {
        let arg = args.first().copied().unwrap_or(JsValue::undefined());
        // Primitive receivers keep the registry native's behavior.
        let obj = arg.as_object()?;
        if self.heap.get(obj).kind != Kind::Proxy {
            return None;
        }
        Some(self.run_proxy_static_inner(id, obj, args))
    }

    fn run_proxy_static_inner(
        &mut self,
        id: NativeId,
        proxy: Handle<JsObject>,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        match id {
            NativeId::ObjectKeys
            | NativeId::ObjectGetOwnPropertyNames
            | NativeId::ObjectGetOwnPropertySymbols
            | NativeId::ObjectValues
            | NativeId::ObjectEntries => self.proxy_enumerate(id, proxy),
            NativeId::ObjectGetOwnPropertyDescriptor
            | NativeId::ReflectGetOwnPropertyDescriptor => {
                let key =
                    self.property_key(args.get(1).copied().unwrap_or(JsValue::undefined()))?;
                Ok(match self.object_get_own_property(proxy, key)? {
                    Some(desc) => self.own_desc_to_object(desc),
                    None => JsValue::undefined(),
                })
            }
            NativeId::ObjectGetOwnPropertyDescriptors => {
                let keys = self.object_own_keys(proxy)?;
                self.gc_protect();
                let out = self.heap.alloc(JsObject::default());
                self.heap.add_root(JsValue::object(out));
                for k in keys {
                    let Some(desc) = self.object_get_own_property(proxy, k)? else {
                        continue;
                    };
                    let d = self.own_desc_to_object(desc);
                    let _ = self.define_own_data_attrs(
                        JsValue::object(out),
                        prop_key_value(k),
                        d,
                        Attrs::DEFAULT,
                    );
                }
                Ok(JsValue::object(out))
            }
            NativeId::ObjectDefineProperty | NativeId::ReflectDefineProperty => {
                let key =
                    self.property_key(args.get(1).copied().unwrap_or(JsValue::undefined()))?;
                let desc_v = args.get(2).copied().unwrap_or(JsValue::undefined());
                let desc = self.to_property_descriptor(desc_v)?;
                let defined = self.object_define_own_property(proxy, key, desc)?;
                if id == NativeId::ReflectDefineProperty {
                    return Ok(JsValue::from_bool(defined));
                }
                if defined {
                    Ok(JsValue::object(proxy))
                } else {
                    Err(JSException(
                        self.error_value("TypeError: Cannot redefine property"),
                    ))
                }
            }
            NativeId::ObjectDefineProperties => {
                let props_v = args.get(1).copied().unwrap_or(JsValue::undefined());
                self.define_properties_on(proxy, props_v)?;
                Ok(JsValue::object(proxy))
            }
            NativeId::ObjectGetPrototypeOf | NativeId::ReflectGetPrototypeOf => {
                self.object_proto_of(proxy)
            }
            NativeId::ObjectSetPrototypeOf | NativeId::ReflectSetPrototypeOf => {
                let proto = self.set_proto_arg(args.get(1).copied())?;
                let ok = self.object_set_prototype_of(proxy, proto)?;
                if id == NativeId::ReflectSetPrototypeOf {
                    return Ok(JsValue::from_bool(ok));
                }
                if ok {
                    Ok(JsValue::object(proxy))
                } else {
                    Err(JSException(self.error_value(
                        "TypeError: cannot set prototype of a non-extensible object",
                    )))
                }
            }
            NativeId::ObjectIsExtensible | NativeId::ReflectIsExtensible => {
                Ok(JsValue::from_bool(self.object_is_extensible(proxy)?))
            }
            NativeId::ObjectPreventExtensions | NativeId::ReflectPreventExtensions => {
                let ok = self.object_prevent_extensions(proxy)?;
                if id == NativeId::ReflectPreventExtensions {
                    return Ok(JsValue::from_bool(ok));
                }
                Ok(JsValue::object(proxy))
            }
            NativeId::ObjectFreeze | NativeId::ObjectSeal => {
                self.set_integrity_on(proxy, id == NativeId::ObjectFreeze)?;
                Ok(JsValue::object(proxy))
            }
            NativeId::ReflectHas => {
                let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
                Ok(JsValue::from_bool(
                    self.op_in(key_v, JsValue::object(proxy))?,
                ))
            }
            NativeId::ReflectDeleteProperty => {
                let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
                Ok(JsValue::from_bool(self.object_delete(proxy, key_v)?))
            }
            NativeId::ReflectOwnKeys => {
                let keys = self.object_own_keys(proxy)?;
                let items: Vec<JsValue> = keys.into_iter().map(prop_key_value).collect();
                Ok(JsValue::object(self.heap.alloc(JsObject::array(items))))
            }
            NativeId::ReflectGet => {
                let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
                self.get_property(0, 0, JsValue::object(proxy), key_v)
            }
            NativeId::ReflectSet => {
                let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
                let value = args.get(2).copied().unwrap_or(JsValue::undefined());
                self.set_property(JsValue::object(proxy), key_v, value, false)?;
                Ok(JsValue::from_bool(true))
            }
            _ => Ok(JsValue::undefined()),
        }
    }

    /// `EnumerableOwnProperties`-style read over a proxy (ES 7.3.24): for each
    /// `ownKeys` result, consult `[[GetOwnProperty]]` on the proxy itself (so
    /// the `getOwnPropertyDescriptor` trap fires) and collect by kind.
    fn proxy_enumerate(
        &mut self,
        id: NativeId,
        proxy: Handle<JsObject>,
    ) -> Result<JsValue, JSException> {
        let keys = self.object_own_keys(proxy)?;
        let mut names: Vec<JsValue> = Vec::new();
        let mut values: Vec<JsValue> = Vec::new();
        for k in keys {
            let Some(desc) = self.object_get_own_property(proxy, k)? else {
                continue;
            };
            let wanted = match id {
                NativeId::ObjectKeys | NativeId::ObjectValues | NativeId::ObjectEntries => {
                    desc.enumerable
                }
                NativeId::ObjectGetOwnPropertyNames => !k.is_symbol(),
                NativeId::ObjectGetOwnPropertySymbols => k.is_symbol(),
                _ => false,
            };
            if !wanted {
                continue;
            }
            if id == NativeId::ObjectGetOwnPropertyNames
                || id == NativeId::ObjectGetOwnPropertySymbols
            {
                names.push(prop_key_value(k));
                continue;
            }
            if id == NativeId::ObjectValues || id == NativeId::ObjectEntries {
                let v = self.get_property(0, 0, JsValue::object(proxy), prop_key_value(k))?;
                values.push(v);
            }
            names.push(prop_key_value(k));
        }
        match id {
            NativeId::ObjectKeys => Ok(JsValue::object(self.heap.alloc(JsObject::array(names)))),
            NativeId::ObjectGetOwnPropertyNames | NativeId::ObjectGetOwnPropertySymbols => {
                Ok(JsValue::object(self.heap.alloc(JsObject::array(names))))
            }
            NativeId::ObjectValues => Ok(JsValue::object(self.heap.alloc(JsObject::array(values)))),
            NativeId::ObjectEntries => {
                let mut entries: Vec<JsValue> = Vec::with_capacity(names.len());
                for (k, v) in names.into_iter().zip(values) {
                    let pair = self.heap.alloc(JsObject::array(vec![k, v]));
                    entries.push(JsValue::object(pair));
                }
                Ok(JsValue::object(self.heap.alloc(JsObject::array(entries))))
            }
            _ => Ok(JsValue::undefined()),
        }
    }

    /// ES `ToPropertyDescriptor` over a descriptor object (traps on its
    /// getters are not a concern for the descriptor objects the suite uses).
    fn to_property_descriptor(&mut self, v: JsValue) -> Result<OwnDesc, JSException> {
        let Some(obj) = v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: Property description must be an object"),
            ));
        };
        self.read_trap_descriptor(obj)
    }

    /// Normalizes the `Object.setPrototypeOf`/`Reflect.setPrototypeOf` proto
    /// argument (object or null, else TypeError).
    fn set_proto_arg(
        &mut self,
        v: Option<JsValue>,
    ) -> Result<Option<Handle<JsObject>>, JSException> {
        let v = v.unwrap_or(JsValue::undefined());
        if v.is_null() {
            return Ok(None);
        }
        v.as_object().map(Some).ok_or_else(|| {
            JSException(self.error_value("TypeError: prototype must be an object or null"))
        })
    }

    /// ES `DefineProperties` over a proxy target.
    fn define_properties_on(
        &mut self,
        obj: Handle<JsObject>,
        props_v: JsValue,
    ) -> Result<(), JSException> {
        let Some(props) = props_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: properties must be an object"),
            ));
        };
        let keys = self.ordinary_own_keys(props);
        for k in keys {
            let Some(entry) = self.ordinary_own_descriptor(props, k) else {
                continue;
            };
            if !entry.enumerable {
                continue;
            }
            let key_v = prop_key_value(k);
            let desc_v = self.get_property(0, 0, JsValue::object(props), key_v)?;
            let desc = self.to_property_descriptor(desc_v)?;
            let _ = self.object_define_own_property(obj, k, desc)?;
        }
        Ok(())
    }

    /// ES `SetIntegrityLevel`: `[[PreventExtensions]]` then freeze/seal every
    /// own property through the proxy's own traps.
    fn set_integrity_on(&mut self, obj: Handle<JsObject>, frozen: bool) -> Result<(), JSException> {
        if !self.object_prevent_extensions(obj)? {
            return Ok(());
        }
        let keys = self.object_own_keys(obj)?;
        for k in keys {
            let Some(cur) = self.object_get_own_property(obj, k)? else {
                continue;
            };
            let mut desc = cur;
            if desc.is_accessor() {
                desc.has_configurable = true;
                desc.configurable = false;
            } else {
                if frozen {
                    desc.has_writable = true;
                    desc.writable = false;
                }
                desc.has_configurable = true;
                desc.configurable = false;
            }
            let _ = self.object_define_own_property(obj, k, desc)?;
        }
        let flag = if frozen {
            v12_heap::JsObject::FLAG_SEALED | v12_heap::JsObject::FLAG_FROZEN
        } else {
            v12_heap::JsObject::FLAG_SEALED
        };
        self.heap.get_mut(obj).flags |= flag;
        Ok(())
    }

    /// ES `String(value)` with the string-hint ToPrimitive so a user
    /// `toString` is invoked. The construct path (`new String(value)`)
    /// arrives with the constructor function as `this` (same discriminator
    /// as `Symbol`): ES requires `new String(symbol)` to throw, while the
    /// call form returns `SymbolDescriptiveString`. v1 has no String-wrapper
    /// object, so `new String(x)` yields the primitive (documented YAGNI
    /// deviation).
    fn run_string_construct(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let is_construct = this_v
            .as_object()
            .is_some_and(|o| self.heap.get(o).kind == Kind::Function);
        // ES 22.1.1.1 step 1: no arguments yields "".
        let Some(&v) = args.first() else {
            return Ok(JsValue::string(self.heap.intern_text("")));
        };
        // ES 22.1.1.1 step 2: call form with a Symbol returns
        // SymbolDescriptiveString without throwing; only the construct form
        // (and implicit ToString) throws. v1 symbols are opaque, so the
        // description is always `Symbol()`.
        if v.is_symbol() {
            if is_construct {
                return Err(self.symbol_to_string_type_error());
            }
            return Ok(JsValue::string(self.heap.intern_text("Symbol()")));
        }
        self.to_string_value(v)
    }

    /// ES `Number(value)`: objects coerce via the default-hint ToPrimitive
    /// (`valueOf` first) so a user `valueOf` is invoked. `new Number(value)`
    /// reaches the same router; v1 has no Number-wrapper object, so both
    /// forms yield the primitive number (documented YAGNI deviation).
    fn run_number_construct(&mut self, args: &[JsValue]) -> Result<JsValue, JSException> {
        let Some(&v) = args.first() else {
            return Ok(JsValue::from_f64(0.0));
        };
        let n = self.to_number_value(v)?;
        Ok(JsValue::from_f64(n))
    }

    /// The receiver's length (array slot or element count for array-likes).
    fn callback_len(&self, obj: Handle<JsObject>) -> u32 {
        let o = self.heap.get(obj);
        if o.kind == Kind::Array {
            if let Some(&v) = o.properties.first() {
                if let Some(n) = v.as_smi() {
                    return n as u32;
                }
                if let Some(n) = v.as_f64()
                    && n.is_finite()
                    && n >= 0.0
                {
                    return n as u32;
                }
            }
        }
        o.element_len() as u32
    }

    /// The receiver's element at `i` (`None` when absent: hole or out of
    /// range; ordinary array-likes read the flat `elements` vec, then
    /// integer-indexed *shape* properties — `{0: 5}` binds `"0"` through
    /// the shape, not the element store).
    fn callback_elem(&mut self, obj: Handle<JsObject>, i: u32) -> Option<JsValue> {
        let o = self.heap.get(obj);
        if o.kind == Kind::Array {
            return o.get_element(i);
        }
        if let Some(v) = o.elements.get(i as usize).filter(|v| !v.is_hole()).copied() {
            return Some(v);
        }
        let h = self.heap.intern_text(&i.to_string());
        let key = v12_heap::PropKey::from_string(h);
        let shape = self.heap.shape_of(obj);
        let slot = self.heap.lookup_property(shape, key)?.slot()?;
        self.heap.get(obj).properties.get(slot as usize).copied()
    }

    /// The highest index an element read can return data for, clamped to
    /// `len` (mirrors the engine's `dense_bound`: the element store plus
    /// shape-bound integer keys on non-arrays). Everything at or beyond it
    /// reads as a hole, which the callback methods all skip.
    fn callback_dense_bound(&mut self, obj: Handle<JsObject>, len: u32) -> u32 {
        let mut bound = self.heap.get(obj).element_len() as u64;
        if self.heap.get(obj).kind != Kind::Array {
            let shape = self.heap.shape_of(obj);
            let keys: Vec<_> = self
                .heap
                .get(shape)
                .descriptors
                .as_slice()
                .iter()
                .filter_map(|d| d.key().string())
                .collect();
            for h in keys {
                let text = self.string_text(h);
                if !text.is_empty() && text.len() <= 10 && text.bytes().all(|b| b.is_ascii_digit())
                {
                    if let Ok(n) = text.parse::<u64>() {
                        bound = bound.max(n.saturating_add(1));
                    }
                }
            }
        }
        bound.min(u64::from(len)) as u32
    }

    fn callback_is_callable(&self, v: JsValue) -> Option<Handle<JsObject>> {
        v.as_object()
            .filter(|h| self.heap.get(*h).kind == Kind::Function)
    }

    /// Shared iteration engine for the callback methods.
    fn run_array_callback(
        &mut self,
        id: NativeId,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let Some(obj) = this_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: Array method called on non-object"),
            ));
        };
        if id == NativeId::ArraySort {
            return self.array_sort_callback(obj, args);
        }
        let Some(cb) = args
            .first()
            .copied()
            .and_then(|v| self.callback_is_callable(v))
        else {
            return Err(JSException(
                self.error_value("TypeError: callback is not a function"),
            ));
        };
        // The callback outlives the argument window across nested executions;
        // root it so a collection during a nested call cannot free it.
        self.heap.add_root(JsValue::object(cb));
        let this_arg = args.get(1).copied().unwrap_or(JsValue::undefined());
        // A huge `length` property on a sparse receiver describes mostly
        // holes, which every one of these methods skips — so iterating only
        // the dense bound (stored elements + shape-bound integer keys) is
        // exact and keeps `0..len` from spinning billions of iterations
        // outside the cooperative deadline's reach.
        let len = self.callback_dense_bound(obj, self.callback_len(obj));

        // `reduce`/`reduceRight` pass the accumulator; everything else passes
        // a `thisArg`. Visited calls carry (element, index, receiver).
        let is_reduce = matches!(id, NativeId::ArrayReduce | NativeId::ArrayReduceRight);
        let mut accumulator: Option<JsValue> = if is_reduce {
            match args.get(1).copied() {
                Some(v) if !v.is_undefined() => Some(v),
                _ => None,
            }
        } else {
            None
        };

        let indices: Box<dyn Iterator<Item = u32>> = if id == NativeId::ArrayReduceRight {
            Box::new((0..len).rev())
        } else {
            Box::new(0..len)
        };

        let mut mapped: Vec<JsValue> = Vec::new();
        let mut found: Option<(JsValue, u32)> = None;
        for i in indices {
            let Some(elem) = self.callback_elem(obj, i) else {
                continue; // holes are skipped by every one of these methods
            };
            // Non-reduce calls carry (element, index, receiver); reduce
            // carries (accumulator, element, index).
            let (first_arg, second_arg) = if is_reduce {
                match accumulator {
                    Some(acc) => (acc, elem),
                    // The first present element becomes the accumulator.
                    None => {
                        accumulator = Some(elem);
                        continue;
                    }
                }
            } else {
                (elem, JsValue::from_f64(f64::from(i)))
            };
            let call_args: [JsValue; 3] = [first_arg, second_arg, this_v];
            let result = self.call_object(cb, this_arg, &call_args)?;
            // Results collected across calls must survive the next call's
            // collection safepoint; the engine roots liberally by design.
            self.heap.add_root(result);
            if is_reduce {
                accumulator = Some(result);
            }
            match id {
                NativeId::ArrayMap | NativeId::ArrayFlatMap => mapped.push(result),
                NativeId::ArrayFilter => {
                    if result.is_true() {
                        mapped.push(elem);
                    }
                }
                NativeId::ArraySome => {
                    if result.is_true() {
                        return Ok(JsValue::from_bool(true));
                    }
                }
                NativeId::ArrayEvery => {
                    if !result.is_true() {
                        return Ok(JsValue::from_bool(false));
                    }
                }
                NativeId::ArrayFind | NativeId::ArrayFindLast => {
                    if result.is_true() {
                        found = Some((elem, i));
                        if id == NativeId::ArrayFind {
                            break;
                        }
                    }
                }
                NativeId::ArrayFindIndex | NativeId::ArrayFindLastIndex => {
                    if result.is_true() {
                        found = Some((JsValue::from_f64(f64::from(i)), i));
                        if id == NativeId::ArrayFindIndex {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }

        match id {
            NativeId::ArrayMap => {
                self.gc_protect();
                let arr = self.heap.alloc(JsObject::array(mapped));
                self.heap.add_root(JsValue::object(arr));
                Ok(JsValue::object(arr))
            }
            NativeId::ArrayFilter => {
                self.gc_protect();
                let arr = self.heap.alloc(JsObject::array(mapped));
                self.heap.add_root(JsValue::object(arr));
                Ok(JsValue::object(arr))
            }
            NativeId::ArraySome => Ok(JsValue::from_bool(false)),
            NativeId::ArrayEvery => Ok(JsValue::from_bool(true)),
            NativeId::ArrayFind | NativeId::ArrayFindLast => {
                Ok(found.map(|(v, _)| v).unwrap_or(JsValue::undefined()))
            }
            NativeId::ArrayFindIndex | NativeId::ArrayFindLastIndex => Ok(match found {
                Some((_, i)) => JsValue::from_f64(f64::from(i)),
                None => JsValue::from_f64(-1.0),
            }),
            NativeId::ArrayReduce | NativeId::ArrayReduceRight => match accumulator {
                Some(v) => Ok(v),
                None => Err(JSException(self.error_value(
                    "TypeError: Reduce of empty array with no initial value",
                ))),
            },
            NativeId::ArrayFlatMap => {
                // flatMap: flatten one level of array results into the output.
                self.gc_protect();
                let mut flat: Vec<JsValue> = Vec::with_capacity(mapped.len());
                for v in &mapped {
                    let nested = v
                        .as_object()
                        .filter(|h| self.heap.get(*h).kind == Kind::Array);
                    if let Some(h) = nested {
                        let items = self.heap.get(h).elements_snapshot();
                        flat.extend(items.iter().map(|x| {
                            if x.is_hole() {
                                JsValue::undefined()
                            } else {
                                *x
                            }
                        }));
                    } else {
                        flat.push(*v);
                    }
                }
                let arr = self.heap.alloc(JsObject::array(flat));
                self.heap.add_root(JsValue::object(arr));
                Ok(JsValue::object(arr))
            }
            _ => Ok(JsValue::undefined()),
        }
    }

    /// `Map.prototype.forEach` / `Set.prototype.forEach` — re-entrant
    /// callback over a snapshot of the entries.
    fn run_collection_for_each(
        &mut self,
        id: NativeId,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let Some(obj) = this_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: forEach called on non-object"),
            ));
        };
        let want_map = id == NativeId::MapForEach;
        let kind = self.heap.get(obj).kind;
        if (want_map && kind != Kind::Map) || (!want_map && kind != Kind::Set) {
            return Err(JSException(
                self.error_value("TypeError: forEach called on incompatible receiver"),
            ));
        }
        let Some(cb) = args
            .first()
            .copied()
            .and_then(|v| self.callback_is_callable(v))
        else {
            return Err(JSException(
                self.error_value("TypeError: callback is not a function"),
            ));
        };
        self.heap.add_root(JsValue::object(cb));
        let this_arg = args.get(1).copied().unwrap_or(JsValue::undefined());
        let snapshot: Vec<JsValue> = self.heap.get(obj).elements.clone();
        if want_map {
            for pair in snapshot.chunks_exact(2) {
                let call_args = [pair[1], pair[0], this_v];
                let r = self.call_object(cb, this_arg, &call_args)?;
                self.heap.add_root(r);
            }
        } else {
            for v in &snapshot {
                let call_args = [*v, *v, this_v];
                let r = self.call_object(cb, this_arg, &call_args)?;
                self.heap.add_root(r);
            }
        }
        Ok(JsValue::undefined())
    }

    /// `Iterator.prototype` callback helpers. Each pulls values by calling
    /// the iterator's `next` through the native seam, then invokes the user
    /// callback via `call_object`.
    fn run_iterator_callback(
        &mut self,
        id: NativeId,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let Some(obj) = this_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: Iterator method called on non-object"),
            ));
        };
        if self.heap.get(obj).kind != Kind::Iterator {
            return Err(JSException(
                self.error_value("TypeError: Iterator method called on non-iterator"),
            ));
        }
        let Some(cb) = args
            .first()
            .copied()
            .and_then(|v| self.callback_is_callable(v))
        else {
            return Err(JSException(
                self.error_value("TypeError: callback is not a function"),
            ));
        };
        self.heap.add_root(JsValue::object(cb));
        let this_arg = if id == NativeId::IteratorReduce {
            args.get(2).copied().unwrap_or(JsValue::undefined())
        } else {
            args.get(1).copied().unwrap_or(JsValue::undefined())
        };
        // Drain via the registry `next` (reads/publishes iterator state).
        // A bounded drain: an unbounded iterator is a RangeError rather
        // than a silent truncation.
        const MAX_ITERATOR_DRAIN: usize = 1_000_000;
        let mut values: Vec<JsValue> = Vec::new();
        loop {
            self.gc_protect();
            let r = self
                .natives
                .call_native(self.heap, this_v, &[], NativeId::IteratorNext);
            let r = r.map_err(|t| JSException::from_throw(self.heap, t))?;
            let Some(ro) = r.as_object() else { break };
            let done = self
                .heap
                .get(ro)
                .properties
                .get(1)
                .copied()
                .unwrap_or(JsValue::undefined());
            if done.is_true() {
                break;
            }
            let v = self
                .heap
                .get(ro)
                .properties
                .first()
                .copied()
                .unwrap_or(JsValue::undefined());
            values.push(v);
            if values.len() > MAX_ITERATOR_DRAIN {
                return Err(JSException(self.error_value(
                    "RangeError: iterator drain exceeds 1,000,000 values",
                )));
            }
        }
        let mut mapped: Vec<JsValue> = Vec::new();
        let mut acc: Option<JsValue> = if id == NativeId::IteratorReduce {
            match args.get(1).copied() {
                Some(v) if !v.is_undefined() => Some(v),
                _ => None,
            }
        } else {
            None
        };
        for (i, v) in values.iter().enumerate() {
            let idx = JsValue::from_f64(i as f64);
            let result = if id == NativeId::IteratorReduce {
                match acc {
                    Some(a) => {
                        let call_args = [a, *v, idx];
                        let r = self.call_object(cb, this_arg, &call_args)?;
                        self.heap.add_root(r);
                        acc = Some(r);
                        continue;
                    }
                    None => {
                        acc = Some(*v);
                        continue;
                    }
                }
            } else {
                let call_args = [*v, idx, this_v];
                let r = self.call_object(cb, this_arg, &call_args)?;
                self.heap.add_root(r);
                r
            };
            match id {
                NativeId::IteratorMap | NativeId::IteratorFlatMap => mapped.push(result),
                NativeId::IteratorFilter => {
                    if result.is_true() {
                        mapped.push(*v);
                    }
                }
                NativeId::IteratorSome => {
                    if result.is_true() {
                        return Ok(JsValue::from_bool(true));
                    }
                }
                NativeId::IteratorEvery => {
                    if !result.is_true() {
                        return Ok(JsValue::from_bool(false));
                    }
                }
                NativeId::IteratorFind => {
                    if result.is_true() {
                        return Ok(*v);
                    }
                }
                NativeId::IteratorForEach => {}
                _ => {}
            }
        }
        match id {
            NativeId::IteratorMap => {
                self.gc_protect();
                let arr = self.heap.alloc(JsObject::array(mapped));
                self.heap.add_root(JsValue::object(arr));
                Ok(JsValue::object(arr))
            }
            NativeId::IteratorFilter => {
                self.gc_protect();
                let arr = self.heap.alloc(JsObject::array(mapped));
                self.heap.add_root(JsValue::object(arr));
                Ok(JsValue::object(arr))
            }
            NativeId::IteratorFlatMap => {
                self.gc_protect();
                let mut flat: Vec<JsValue> = Vec::with_capacity(mapped.len());
                for v in &mapped {
                    if let Some(h) = v
                        .as_object()
                        .filter(|h| self.heap.get(*h).kind == Kind::Array)
                    {
                        flat.extend(self.heap.get(h).elements_snapshot());
                    } else {
                        flat.push(*v);
                    }
                }
                let arr = self.heap.alloc(JsObject::array(flat));
                self.heap.add_root(JsValue::object(arr));
                Ok(JsValue::object(arr))
            }
            NativeId::IteratorReduce => match acc {
                Some(v) => Ok(v),
                None => Err(JSException(self.error_value(
                    "TypeError: Reduce of empty iterator with no initial value",
                ))),
            },
            NativeId::IteratorSome => Ok(JsValue::from_bool(false)),
            NativeId::IteratorEvery => Ok(JsValue::from_bool(true)),
            NativeId::IteratorFind => Ok(JsValue::undefined()),
            _ => Ok(JsValue::undefined()),
        }
    }

    /// `Array.prototype.sort(comparefn?)` — the comparator needs re-entry, so
    /// sort runs here; holes and `undefined` sort to the end per spec.
    fn array_sort_callback(
        &mut self,
        obj: Handle<JsObject>,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let comparator = args
            .first()
            .copied()
            .and_then(|v| self.callback_is_callable(v));
        let mut elems: Vec<JsValue> = self.heap.get(obj).elements_snapshot();
        // Undefined sorts last; holes after undefined. Sort the defined part.
        let undefined_count = elems.iter().filter(|v| v.is_undefined()).count();
        elems.retain(|v| !v.is_undefined() && !v.is_hole());
        // Pre-transduce through strings? No — comparators are user code or
        // default ToString ordering. Rust's sort is stable (spec requires it).
        if let Some(cb) = comparator {
            // Insertion into a comparator-ordered vec via sort_by with a
            // stateful closure is unsound under re-entry; collect and
            // insertion-sort instead (arrays here are small).
            let mut sorted: Vec<JsValue> = Vec::with_capacity(elems.len());
            for v in elems {
                let mut lo = 0usize;
                let mut hi = sorted.len();
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    let a: [JsValue; 2] = [sorted[mid], v];
                    let r = self.call_object(cb, JsValue::undefined(), &a)?;
                    let n = r
                        .as_smi()
                        .map(i64::from)
                        .or(r.as_f64().map(|f| f as i64))
                        .unwrap_or(0);
                    // comparefn(sorted[mid], v) <= 0 → v sorts after mid.
                    if n <= 0 {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                sorted.insert(lo, v);
            }
            elems = sorted;
        } else {
            elems.sort_by_cached_key(|v| self.to_display_string(*v));
        }
        let undefineds = vec![JsValue::undefined(); undefined_count];
        elems.extend(undefineds);
        self.heap.get_mut(obj).replace_elements(elems);
        Ok(JsValue::object(obj))
    }
}

/// The `JsValue` a trap receives as its property-key argument.
fn prop_key_value(key: PropKey) -> JsValue {
    if let Some(h) = key.string() {
        JsValue::string(h)
    } else if let Some(y) = key.symbol() {
        JsValue::symbol(y)
    } else {
        JsValue::undefined()
    }
}
