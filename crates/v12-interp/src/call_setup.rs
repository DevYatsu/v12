//! Call setup and teardown: argument-window preparation for plain calls,
//! `call`/`apply`/`new`, native inline calls, frame completion, and
//! exception unwinding.

use v12_heap::{Attrs, Descriptor, Handle, HeapExt, JsObject, JsValue, Kind, PropKey};

use super::{
    CallOutcome, Frame, Interp, JSException, MAX_CALL_DEPTH, MAX_RESUME_DEPTH, intrinsic_slot,
};
use crate::execute::decode_parked_call;
use crate::ops;
use crate::property::OwnDesc;
use v12_native::NativeId;
use v12_native::promise_combinator::{KIND_ALL_SETTLED, SLOT_KIND, SLOT_REJECT};

/// A `PromiseCapability Record` as the combinators hold it (spec 27.5.4.1 step
/// 2): the promise to return, and the pair every settlement goes through. The
/// pair is `C`'s own — 27.5.1.5 records whatever the constructor passed its
/// executor — so all three are `Call`s the interpreter makes, not writes it
/// performs.
///
/// Parked on the value stack for the duration of a combinator call, because
/// `gc_protect` republishes that stack as the root set and each of the three is
/// held in a Rust local meanwhile.
struct Capability {
    /// `[[Promise]]` — what `Promise.all`/`Promise.race` hand back.
    promise: JsValue,
    /// `[[Resolve]]`.
    resolve: JsValue,
    /// `[[Reject]]`.
    reject: JsValue,
}

/// Which half of a combinator's terminal step the value the engine seam handed
/// back belongs to. Every kind is [`Payoff::Fulfils`] except `KIND_ANY`, whose
/// payoff is the `errors` array an `AggregateError` is built from — so the
/// polarity is carried alongside the value rather than re-derived from the
/// value at each of the two call sites.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Payoff {
    /// `Call(capability.[[Resolve]], undefined, « value »)`.
    Fulfils,
    /// `Call(capability.[[Reject]], undefined, « AggregateError(value) »)`.
    Rejects,
}

/// Resolves `[callee][this][args…]` at `callee_reg` in the current frame
/// and either pushes a bytecode frame or completes a native inline.
impl Interp<'_> {
    pub(crate) fn prepare_call(
        &mut self,
        base: usize,
        caller_max_regs: u16,
        callee_reg: u16,
        argc: u16,
        direct_eval: bool,
        call_pc: u32,
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

        if direct_eval && matches!(target, v12_heap::FunctionTarget::RealmEval(_)) {
            let args_start = callee_slot + 2;
            let source = self
                .stack
                .get(args_start)
                .and_then(|v| v.as_string())
                .map(|h| self.string_text(h))
                .unwrap_or_default();
            let context = self.capture_eval_context(call_pc);
            let facts = context.compile_facts();
            let compiled = self
                .natives
                .compile_eval(self.heap, &source, self.global, &facts)
                .map_err(|t| JSException::from_throw(self.heap, t))?;
            let result = self.run_compiled_child(compiled, context)?;
            return Ok(CallOutcome::Value(result));
        }

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
                    .call_value_nested(JsValue::object(target_fn), this_arg, &call_args)
                    .map(CallOutcome::Value);
            }
        };

        if direct_eval && target_idx == u32::from(NativeId::Eval) {
            let args_start = callee_slot + 2;
            let source = self
                .stack
                .get(args_start)
                .and_then(|v| v.as_string())
                .map(|h| self.string_text(h))
                .unwrap_or_default();
            let context = self.capture_eval_context(call_pc);
            let facts = context.compile_facts();
            let compiled = self
                .natives
                .compile_eval(self.heap, &source, self.global, &facts)
                .map_err(|t| JSException::from_throw(self.heap, t))?;
            let result = self.run_compiled_child(compiled, context)?;
            return Ok(CallOutcome::Value(result));
        }

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
                    .dispatch_call_native(callee_obj, native_fn, this_v, &args_slice)
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
                .dispatch_call_native(callee_obj, id, this_v, &args_vec)
                .map(CallOutcome::Value);
        }

        // Generator function: calling it returns a generator object without executing body.
        if self.is_generator_fn_for(target_idx, callee_program) {
            // Bind `this` at call time (OrdinaryCallBindThis); the generator
            // body observes the bound receiver when resumed.
            let gen_this = self.bind_this(target_idx, callee_program, this_v);
            let r#gen = self.create_generator_object(
                target_idx,
                callee_program,
                captured_env,
                gen_this,
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
            window[0] = self.bind_this(target_idx, callee_program, this_v);
            let arg_src = callee_slot + 2;
            let actuals = self.stack[arg_src..arg_src + usize::from(argc)].to_vec();
            let arguments =
                self.frame_arguments_for(target_idx, callee_program, &actuals, callee_v);
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
            let retained_arguments = arguments.unwrap_or(JsValue::undefined());
            self.set_generator_header_value(g, Self::GENERATOR_ARGUMENTS_SLOT, retained_arguments);
            if let Some(arguments) = self.generator_header_value(g, Self::GENERATOR_ARGUMENTS_SLOT)
            {
                self.heap.remove_root(arguments);
            }
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
        // ES OrdinaryCallBindThis: resolve the frame's `this` binding for the
        // callee's strictness *before* the frame runs.
        let bound_this = self.bind_this(target_idx, callee_program, this_v);
        self.stack[new_base] = bound_this;
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
        let frame_args = self.frame_arguments_for(target_idx, callee_program, &passed, callee_v);

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
        self.call_args
            .insert(new_base, (arg_src, usize::from(argc)));
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
                //
                // Each level of accessor re-entry nests a Rust `execute` call
                // (below), so it spends *native* stack, not frame-stack
                // budget: a getter that resolves to itself — `super.x` landing
                // back on the receiver's own accessor, or two objects whose
                // same-key accessors alias — recursed until the process
                // aborted with `fatal runtime error: stack overflow`. The
                // ordinary call seams' `MAX_CALL_DEPTH` is far too high to
                // fire first here (a few thousand levels already exhaust an
                // 8 MiB thread stack in a debug build), so this seam is
                // governed by `MAX_RESUME_DEPTH`, the budget that exists
                // precisely for nested `execute()` re-entries.
                if self.resume_depth >= MAX_RESUME_DEPTH {
                    return Err(JSException(
                        self.error_value("RangeError: maximum call stack size exceeded"),
                    ));
                }
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
                let frame_args =
                    self.frame_arguments_for(fn_idx, func_program, args, JsValue::object(func));
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
                self.resume_depth += 1;
                let exec_result = self.execute();
                self.resume_depth -= 1;
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
        // Like [`Self::call_accessor_with`], the bytecode arm below nests a
        // Rust `execute()` call, so it spends native stack and needs the
        // `MAX_RESUME_DEPTH` budget rather than `MAX_CALL_DEPTH`. A callback
        // that re-enters its own callback-taking builtin (`arr.map(x =>
        // arr.map(f))`) otherwise recurses until the process aborts.
        if self.resume_depth >= MAX_RESUME_DEPTH {
            return Err(JSException(
                self.error_value("RangeError: maximum call stack size exceeded"),
            ));
        }
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
                    return self.dispatch_call_native(func, native_fn, this, args);
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
                self.stack[new_base] = self.bind_this(fn_idx, func_program, this);
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
                let frame_args =
                    self.frame_arguments_for(fn_idx, func_program, args, JsValue::object(func));
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
                self.resume_depth += 1;
                let exec_result = self.execute();
                self.resume_depth -= 1;
                self.stop_at_frames = saved;
                exec_result?;
                Ok(self.top_result.take().unwrap_or(JsValue::undefined()))
            }
        }
    }

    /// ES 10.2.1.2 `OrdinaryCallBindThis`, global-`this` mode: a *sloppy*
    /// function called with `undefined`/`null` binds the realm's global
    /// object. Strict functions keep the receiver verbatim. Arrow functions
    /// are skipped: they have no own `this` binding (their `ThisExpression`
    /// resolves to the lexically enclosing home unit's `this`, so this slot
    /// is never read for them).
    ///
    /// v1 has no primitive wrapper objects (`String`/`Number`/`Boolean`
    /// boxing is a documented engine-wide gap), so a primitive receiver is
    /// left as the primitive rather than boxed.
    fn bind_this(&self, fn_idx: u32, program: u32, this_v: JsValue) -> JsValue {
        let funcs = self.functions_for_program(program);
        let Some(f) = funcs.get(fn_idx as usize) else {
            return this_v;
        };
        if f.is_strict || f.is_arrow {
            return this_v;
        }
        if (this_v.is_null() || this_v.is_undefined())
            && let Some(g) = self.global
        {
            return JsValue::object(g);
        }
        this_v
    }

    /// Completes the top frame with `result`: deposits it into the caller's
    /// destination register and resumes there. Returns `true` when the
    /// completed frame was the top-level script — the run is done.
    pub(crate) fn complete_frame(&mut self, result: JsValue) -> Result<bool, JSException> {
        let finished = self.frames.pop().expect("complete_frame requires a frame");
        self.drop_frame_arguments(&finished);
        self.call_args.remove(&finished.base);
        self.drop_frame_context(finished.base);
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
            let completion_promise = self.generator_completion_promise(r#gen);
            let has_completion_promise = completion_promise.is_some();
            if is_async && has_completion_promise {
                if let Some(ph) = completion_promise {
                    self.pending_settlements.push((ph, result, false));
                }
                // Prevent Heap::roots leak: promise was roots-pinned at creation/await; after settling
                // it remains reachable via generator properties[4] until GC, so drop the extra root.
                if let Some(ph_val) = self.generator_header_value(r#gen, 4) {
                    self.heap.remove_root(ph_val);
                }
            }
            self.stack.truncate(finished.base);
            if self.heap.get(r#gen).properties.len() >= 3 {
                self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
            }
            // For async jobs, caller already resumed with Promise; just settle and resume caller
            if is_async && has_completion_promise {
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
                self.drop_frame_context(popped.base);
                self.stack.truncate(popped.base);
                self.notify_tier_ups();
                return Err(JSException(exc));
            }
            let Some(popped) = self.frames.pop() else {
                return Err(JSException(exc));
            };
            self.drop_frame_arguments(&popped);
            self.drop_frame_context(popped.base);
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

    /// Creates a realm-linked error value for embedder/module-loader failures.
    pub fn error_value(&mut self, text: &str) -> JsValue {
        let (kind, message) = match text.split_once(": ") {
            Some((n, m)) => (n, m),
            None => ("Error", text),
        };
        self.gc_protect();
        let obj = self.heap.alloc(JsObject {
            kind: Kind::Error,
            ..Default::default()
        });
        let obj_v = JsValue::object(obj);
        // `add_root` cannot cover the span below: every `gc_protect` (this
        // function's own, and the one `define_own_data_attrs` takes before it
        // touches the receiver) clears the transient root set, so the next
        // collection would free the slot this function keeps writing to. Rooted
        // through `scratch_roots`, which `gc_protect` republishes; popped
        // before the return, so a thrown error is not retained.
        self.scratch_roots.push(obj_v);
        // Shape-bound installs in display order: `properties[0]`/`[1]` stay
        // the name/message strings the display paths read positionally, with
        // descriptors that make the props observable (`e.name`, `e.message`).
        // Spec attrs: writable + configurable, non-enumerable.
        self.gc_protect();
        let name_v = JsValue::string(self.heap.intern_text(kind));
        let msg_v = JsValue::string(self.heap.intern_text(message));
        let name_key = JsValue::string(self.heap.intern_text("name"));
        let msg_key = JsValue::string(self.heap.intern_text("message"));
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
        self.scratch_roots.pop();
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
                .dispatch_call_native(callee_obj, id, this_v, &args_vec)
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
        let bound_this = self.bind_this(target_idx, callee_program, this_v);
        self.stack[new_base] = bound_this;
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
        let frame_args = self.frame_arguments_for(target_idx, callee_program, &elements, callee_v);
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
                self.error_value("TypeError: value is not a constructor"),
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
                    // `onFulfilledElement` is a `CreateBuiltinFunction` and has
                    // no `[[Construct]]` (spec 27.5.4.1.1 step 4), so `new` on
                    // it is a `TypeError` — and the *spec's* `TypeError`, not
                    // the registry's "not registered", which is neither a
                    // `TypeError` object nor the right message.
                    if idx == u32::from(NativeId::PromiseCombinatorElement) {
                        return Err(JSException(
                            self.error_value("TypeError: not a constructor"),
                        ));
                    }
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

        let frame_args = self.frame_arguments_for(target_idx, callee_program, &passed, callee_v);
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
        self.call_args
            .insert(new_base, (arg_src, usize::from(argc)));
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
    /// [`Self::dispatch_native`] for a *call*, with the callee object in hand.
    ///
    /// Identical but for one interception: `onFulfilledElement` reads its
    /// element record off the callee, which `dispatch_native` is not handed. It
    /// cannot live in `run_callback_builtin` either, because a reaction reaches
    /// the handler through whichever call site ran the user's `then` — a
    /// `call` opcode in `prepare_call`, an engine arm in `call_inline`, or
    /// `Interp::call_object` in `call_value` — so every call site funnels
    /// through here first.
    ///
    /// The construct paths deliberately do not: nothing constructs a reaction
    /// handler, it is only ever called.
    fn dispatch_call_native(
        &mut self,
        callee: Handle<JsObject>,
        id: NativeId,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        if id == NativeId::PromiseCombinatorElement {
            return self.run_combinator_element(callee, args);
        }
        self.dispatch_native(id, this_v, args)
    }

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
                if self.heap.get(target).kind != Kind::Function
                    && self.heap.get(target).kind != Kind::Proxy
                {
                    return Err(JSException(self.error_value(
                        "TypeError: Function.prototype.call called on non-function",
                    )));
                }
                self.call_value(JsValue::object(target), this_arg, fwd)
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
                if self.heap.get(target).kind != Kind::Function
                    && self.heap.get(target).kind != Kind::Proxy
                {
                    return Err(JSException(self.error_value(
                        "TypeError: Function.prototype.apply called on non-function",
                    )));
                }
                self.call_value(JsValue::object(target), this_arg, &fwd)
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

/// A helper's source iterator plus the `next` method resolved once when the
/// helper was called (spec 7.4.3: `GetIteratorDirect` fetches `next` a single
/// time, so a `next` getter that returns a different function per read is
/// observed once).
struct HelperSource {
    iter: JsValue,
    next: JsValue,
}

/// Mirror of the helper-wrapper element layout and state machine owned by
/// `v12_engine::builtins::iterator`.
///
/// The engine crate *depends* on this one, so the constants cannot be imported
/// from there — the engine is downstream. They are duplicated deliberately and
/// must stay in step; `crates/v12-engine/tests/iterator_helpers.rs` pins both
/// sides against real behaviour (a wrapper built here, stepped here, drained by
/// the engine, and vice versa), so a drift fails the build rather than a
/// conformance run.
mod helper_layout {
    /// `v12_engine::builtins::iterator::ITER_KIND_HELPER_TAKE`.
    pub const ITER_KIND_HELPER_TAKE: i32 = 8;
    /// `v12_engine::builtins::iterator::ITER_KIND_HELPER_DROP`.
    pub const ITER_KIND_HELPER_DROP: i32 = 9;
    /// `v12_engine::builtins::iterator::ITER_KIND_HELPER_MAP`.
    pub const ITER_KIND_HELPER_MAP: i32 = 10;
    /// `v12_engine::builtins::iterator::ITER_KIND_HELPER_FILTER`.
    pub const ITER_KIND_HELPER_FILTER: i32 = 11;
    /// `v12_engine::builtins::iterator::ITER_KIND_HELPER_FLATMAP`.
    pub const ITER_KIND_HELPER_FLATMAP: i32 = 12;

    pub const SLOT_KIND: usize = 0;
    pub const SLOT_SOURCE: usize = 1;
    pub const SLOT_INDEX: usize = 2;
    /// The source's `next` method, or `undefined` when the chain is
    /// machine-free.
    pub const SLOT_SOURCE_NEXT: usize = 3;
    /// The `limit` (`take`/`drop`) or the callback (`map`/`filter`/`flatMap`).
    pub const SLOT_ARG: usize = 4;
    /// `flatMap` only: the array whose elements are still to be yielded.
    pub const SLOT_PENDING: usize = 5;
    /// `flatMap` only: the cursor into [`SLOT_PENDING`].
    pub const SLOT_PENDING_INDEX: usize = 6;

    /// The element-store kinds (every kind at or below
    /// `ENGINE_KIND_MAX`): the iterators the engine's own `iterator_next` can
    /// step with no machine and no user code.
    pub const ENGINE_KIND_MAX: i32 = 7;
    pub const ENGINE_KINDS: [i32; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
    /// `take`/`drop`: the kinds whose `SLOT_ARG` is a limit and whose counter
    /// tracks emitted or discarded values.
    pub const LIMIT_KINDS: [i32; 2] = [ITER_KIND_HELPER_TAKE, ITER_KIND_HELPER_DROP];

    /// What a wrapper's `next` must do. Mirrors
    /// `v12_engine::builtins::iterator::WrapperStep`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WrapperStep {
        Done,
        Skip(usize),
        Emit,
    }

    /// Mirrors `v12_engine::builtins::iterator::wrapper_step`.
    pub const fn wrapper_step(kind: i32, count: usize, limit: usize) -> WrapperStep {
        if kind == ITER_KIND_HELPER_TAKE {
            if count >= limit {
                return WrapperStep::Done;
            }
        } else if kind == ITER_KIND_HELPER_DROP {
            if count < limit {
                return WrapperStep::Skip(limit - count);
            }
        }
        WrapperStep::Emit
    }
}

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
            NativeId::MapConstruct | NativeId::SetConstruct => {
                Some(self.construct_collection(id, this_v, args))
            }
            NativeId::MapForEach | NativeId::SetForEach => {
                Some(self.run_collection_for_each(id, this_v, args))
            }
            // `Promise.all`/`Promise.race`/`Promise.allSettled`/`Promise.any`
            // read `@@iterator` and drive `next`/`return` on it, so their
            // iterator half re-enters the machine; the engine handler stays the
            // settled-promise aggregation and receives the drained values.
            NativeId::PromiseAll
            | NativeId::PromiseRace
            | NativeId::PromiseAllSettled
            | NativeId::PromiseAny => Some(self.run_promise_combinator(id, this_v, args)),
            // `Promise.withResolvers()` runs the receiver's constructor
            // (`NewPromiseCapability(C)` step 6 is a `Construct`), and
            // `Promise.try` calls a user callback and then resolves through the
            // receiver's own resolving pair — both machine work, so neither can
            // run as a pure registry native.
            NativeId::PromiseWithResolvers => Some(self.run_promise_with_resolvers(this_v, args)),
            NativeId::PromiseTry => Some(self.run_promise_try(this_v, args)),
            // `Promise.resolve(x)` adopts a thenable through a job the
            // interpreter cannot build, so the `then` read happens here and
            // the adoption is queued for the engine's drain. Non-thenables
            // (and `then`-present-but-not-callable) fall through to the
            // registry native, which fulfills with the value as-is.
            NativeId::PromiseResolve => self.run_promise_resolve(this_v, args),
            // `new Promise(executor)` *calls* the executor, so it cannot run
            // as a pure registry native — and 27.5.1.1 step 5 wants that call
            // synchronous, before the constructor returns. `prepare_construct`
            // routes out-of-range native indices through `dispatch_native`,
            // so intercepting here is what a plain `new` reaches.
            NativeId::PromiseConstruct => Some(self.run_promise_construct(this_v, args)),
            // `then`/`catch`/`finally` select the derived promise's constructor
            // (27.5.5.4 step 3, 27.5.5.8 step 3) and then build a capability
            // from it, so all three are machine work — two `Get`s that can run
            // user code, then a `Construct`. The engine keeps its own
            // `promise_then`/`promise_catch`/`promise_finally` as the bare-heap
            // fallback (no interpreter, no realm, no species to select), which
            // is also what a non-interpreter embedder reaches.
            NativeId::PromiseThen => Some(self.run_promise_then(this_v, args)),
            NativeId::PromiseCatch => Some(self.run_promise_catch(this_v, args)),
            NativeId::PromiseFinally => Some(self.run_promise_finally(this_v, args)),
            // `new AggregateError(errors, message)` drains `errors` with
            // `IteratorToList` and `ToString`s the message — both run user
            // code, so neither can be a pure `&mut Heap` registry native. The
            // engine handler gets the drained array and builds the instance.
            NativeId::AggregateErrorCreate => Some(self.run_aggregate_error(this_v, args)),
            NativeId::IteratorMap
            | NativeId::IteratorFilter
            | NativeId::IteratorFlatMap
            | NativeId::IteratorReduce
            | NativeId::IteratorForEach
            | NativeId::IteratorSome
            | NativeId::IteratorEvery
            | NativeId::IteratorFind => Some(self.run_iterator_callback(id, this_v, args)),
            // `take`/`drop`/`toArray` are pure-heap in the engine, but the spec
            // brand check is a `GetMethod(O, "next")` and a wrapper over a
            // generator or a plain object has to be stepped by the machine, so
            // the seam decides whether to hand the call over.
            NativeId::IteratorTake | NativeId::IteratorDrop | NativeId::IteratorToArray => {
                Some(self.run_iterator_passthrough(id, this_v, args))
            }
            // `next` on one of our lazy wrappers. `None` for every other
            // receiver, so element-store iterators and the machine-free
            // `take`/`drop` wrappers keep going to the engine's `iterator_next`.
            NativeId::IteratorNext => self.run_iterator_next_native(this_v),
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
            | NativeId::ObjectHasOwn
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
            NativeId::ReflectApply => Some(self.run_reflect_apply(this_v, args)),
            NativeId::ReflectConstruct => Some(self.run_reflect_construct(this_v, args)),
            _ => None,
        }
    }

    /// `Reflect.apply`/`Reflect.construct` are not constructors; invoking
    /// either with `new` presents the method itself as `this`.
    fn reject_reflect_construct(&mut self, this_v: JsValue, name: &str) -> Result<(), JSException> {
        if this_v
            .as_object()
            .is_some_and(|o| self.heap.get(o).kind == Kind::Function)
        {
            return Err(JSException(self.error_value(&format!(
                "TypeError: Reflect.{name} is not a constructor"
            ))));
        }
        Ok(())
    }

    /// ES `CreateListFromArrayLike(argumentsList)`: an object with a `length`,
    /// then indices `0..len`. A missing/undefined list is an empty list only
    /// when the caller passes `undefined`; a non-object throws.
    fn create_list_from_array_like(&mut self, v: JsValue) -> Result<Vec<JsValue>, JSException> {
        let Some(obj) = v.as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: CreateListFromArrayLike called on non-object",
            )));
        };
        let len_key = self.new_temp_key("length");
        let len_v = self.get_property(0, 0, JsValue::object(obj), len_key)?;
        let len_f = ops::to_number(self.heap, len_v);
        let len = if len_f.is_finite() && len_f > 0.0 {
            len_f.trunc() as usize
        } else {
            0
        };
        let mut out = Vec::with_capacity(len.min(1024));
        for i in 0..len {
            let key = JsValue::string(self.heap.intern_text(&i.to_string()));
            out.push(self.get_property(0, 0, JsValue::object(obj), key)?);
        }
        Ok(out)
    }

    /// `Reflect.apply(target, thisArgument, argumentsList)` (ES 28.1.1):
    /// `Call(target, thisArgument, args)`. Runs here because a proxy or a
    /// bytecode target needs the interpreter; the registry native cannot
    /// re-enter the machine.
    fn run_reflect_apply(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        self.reject_reflect_construct(this_v, "apply")?;
        let target_v = args.first().copied().unwrap_or(JsValue::undefined());
        let Some(target) = target_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: Reflect.apply target is not a function"),
            ));
        };
        // `IsCallable` includes a proxy whose target is callable; a revoked
        // proxy is not callable (ES 10.5.12 step 1 uses the resolved target).
        if !self.is_callable_object(target) {
            return Err(JSException(
                self.error_value("TypeError: Reflect.apply target is not a function"),
            ));
        }
        let this_arg = args.get(1).copied().unwrap_or(JsValue::undefined());
        let list =
            self.create_list_from_array_like(args.get(2).copied().unwrap_or(JsValue::undefined()))?;
        // `call_object` routes a proxy through its `apply` trap.
        self.call_value(JsValue::object(target), this_arg, &list)
    }

    /// `Reflect.construct(target, argumentsList[, newTarget])` (ES 28.1.2):
    /// `Construct(target, args, newTarget)`. Same interpreter requirement as
    /// `Reflect.apply`.
    fn run_reflect_construct(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        self.reject_reflect_construct(this_v, "construct")?;
        let target_v = args.first().copied().unwrap_or(JsValue::undefined());
        let Some(target) = target_v.as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: Reflect.construct target is not a constructor",
            )));
        };
        if !self.is_constructor_object(target) {
            return Err(JSException(self.error_value(
                "TypeError: Reflect.construct target is not a constructor",
            )));
        }
        let list =
            self.create_list_from_array_like(args.get(1).copied().unwrap_or(JsValue::undefined()))?;
        let new_target = args.get(2).copied().unwrap_or(target_v);
        let Some(nt) = new_target.as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: Reflect.construct newTarget is not a constructor",
            )));
        };
        if !self.is_constructor_object(nt) {
            return Err(JSException(self.error_value(
                "TypeError: Reflect.construct newTarget is not a constructor",
            )));
        }
        self.construct_object(target, &list, new_target)
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
        let arg_array = crate::call::alloc_array(self, args.to_vec());
        self.heap.add_root(arg_array);
        self.call_inline(
            trap,
            JsValue::object(handler),
            &[JsValue::object(target), this_arg, arg_array],
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
        let arg_array = crate::call::alloc_array(self, args.to_vec());
        self.heap.add_root(arg_array);
        let result = self.call_inline(
            trap,
            JsValue::object(handler),
            &[JsValue::object(target), arg_array, new_target],
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
    ///
    /// A `Native`/`Host` callable is a built-in function object and has no
    /// `[[Construct]]` unless it is one of the engine's registered
    /// constructor-shaped natives (mirrors `builtins::reflect::is_constructor`
    /// and the `prepare_construct` native seam). An out-of-range bytecode
    /// index names an engine-installed native, so it is constructible only
    /// when it maps to one of those ids.
    pub(crate) fn is_constructor_object(&mut self, obj: Handle<JsObject>) -> bool {
        match self.heap.get(obj).kind {
            Kind::Function => {
                let program = self.heap.get(obj).program_id;
                let callable = self.heap.get(obj).callable;
                match callable {
                    v12_heap::FunctionTarget::Native(_) | v12_heap::FunctionTarget::Host(_) => {
                        false
                    }
                    v12_heap::FunctionTarget::RealmEval(_) => false,
                    v12_heap::FunctionTarget::Bound(state_h) => {
                        // A bound function is constructible iff its target is.
                        let inner = self.heap.get(state_h).elements[0].as_object();
                        inner.is_some_and(|t| self.is_constructor_object(t))
                    }
                    v12_heap::FunctionTarget::Bytecode(idx) => {
                        let funcs = self.functions_for_program(program);
                        if (idx as usize) < funcs.len() {
                            !funcs[idx as usize].is_arrow
                        } else {
                            // Out-of-range index: an engine native. Only the
                            // registered constructor-shaped ids qualify.
                            self.native_constructor_id(idx)
                        }
                    }
                }
            }
            Kind::Proxy => {
                let target = self.heap.get(obj).proxy_target;
                target.is_some_and(|t| self.is_constructor_object(t))
            }
            _ => false,
        }
    }

    /// `IsConstructor(C)` for `NewPromiseCapability` (spec 27.5.1.5 step 1,
    /// 27.2.4.1 step 6 / 27.2.4.2 step 3), which is what makes
    /// `Promise.all.call(eval, …)` throw a `TypeError` before the iterator is
    /// touched, and what keeps a derived `super(executor)`'s pre-made instance
    /// out of the gate in `run_promise_construct`.
    ///
    /// Now just [`Self::is_constructor_object`]: `%Promise%` is wired as an
    /// out-of-range bytecode index ([`NativeId::PromiseConstruct`]) and
    /// [`Self::native_constructor_id`] names it, so the shared oracle answers
    /// for it and `Reflect.construct(Promise, …)` reaches the construct path as
    /// well. A proxy in front of the constructor inherits the answer, per
    /// `IsConstructor` on a proxy exotic object (and a proxy with no target
    /// answers false there too, revoked or not).
    fn is_capability_constructor(&mut self, v: JsValue) -> bool {
        let Some(obj) = v.as_object() else {
            return false;
        };
        self.is_constructor_object(obj)
    }

    /// True when a bytecode-table index names one of the engine's
    /// constructor-shaped natives (`new Object/Array/Boolean/Error/Promise/…`).
    fn native_constructor_id(&self, idx: u32) -> bool {
        matches!(
            NativeId::try_from(idx),
            Ok(NativeId::ObjectConstruct
                | NativeId::ArrayConstruct
                | NativeId::BooleanConstruct
                | NativeId::PromiseConstruct
                | NativeId::ErrorCreate
                | NativeId::TypeErrorCreate
                | NativeId::RangeErrorCreate
                | NativeId::ReferenceErrorCreate
                | NativeId::SyntaxErrorCreate
                | NativeId::EvalErrorCreate
                | NativeId::UriErrorCreate
                | NativeId::AggregateErrorCreate)
        )
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
                    st.elements[0]
                        .as_object()
                        .expect("bound target is an object"),
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
        // A native/host constructor invoked without a parked `Construct`
        // opcode: call its handler directly and take its object result (the
        // engine's native constructors return the fresh instance). Mirrors
        // `prepare_construct`'s `Native`/`Host` arms.
        match self.heap.get(target).callable {
            v12_heap::FunctionTarget::Native(f) => {
                self.gc_protect();
                return f(self.heap, target_v, args).map_err(JSException);
            }
            v12_heap::FunctionTarget::Host(c) => {
                self.gc_protect();
                return c.call(self.heap, target_v, args).map_err(JSException);
            }
            _ => {}
        }
        let program = self.heap.get(target).program_id;
        let idx = match self.heap.get(target).callable {
            v12_heap::FunctionTarget::Bytecode(i) => i,
            v12_heap::FunctionTarget::RealmEval(_) => {
                return Err(JSException(
                    self.error_value("TypeError: target is not a constructor"),
                ));
            }
            v12_heap::FunctionTarget::Bound(_) => unreachable!("handled above"),
            v12_heap::FunctionTarget::Native(_) | v12_heap::FunctionTarget::Host(_) => {
                unreachable!("handled above")
            }
        };
        if (idx as usize) >= self.functions_for_program(program).len() {
            // Out-of-range bytecode index: an engine-installed native
            // constructor (Boolean/Error/…). Route through the registry.
            // `onFulfilledElement` is the one native here with no
            // `[[Construct]]` at all (see `prepare_construct`).
            if idx == u32::from(NativeId::PromiseCombinatorElement) {
                return Err(JSException(
                    self.error_value("TypeError: not a constructor"),
                ));
            }
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
                                    self.drop_frame_context(f.base);
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
        if !matches!(self.heap.get(obj).kind, Kind::Proxy | Kind::ModuleNamespace) {
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
            NativeId::ObjectHasOwn => {
                let key =
                    self.property_key(args.get(1).copied().unwrap_or(JsValue::undefined()))?;
                Ok(JsValue::from_bool(
                    self.object_get_own_property(proxy, key)?.is_some(),
                ))
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
                Ok(crate::call::alloc_array(self, items))
            }
            NativeId::ReflectGet => {
                let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
                self.get_property(0, 0, JsValue::object(proxy), key_v)
            }
            NativeId::ReflectSet => {
                let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
                let value = args.get(2).copied().unwrap_or(JsValue::undefined());
                let receiver = args.get(3).copied().unwrap_or(JsValue::object(proxy));
                // `Reflect.set` reports the `[[Set]]` boolean rather than
                // throwing; the receiver defaults to the proxy itself.
                let ok = self.proxy_op_set(proxy, receiver, key_v, value)?;
                Ok(JsValue::from_bool(ok))
            }
            _ => Ok(JsValue::undefined()),
        }
    }

    /// `EnumerableOwnProperties`-style read over a proxy (ES 7.3.24): for each
    /// `ownKeys` result, consult `[[GetOwnProperty]]` on the proxy itself (so
    /// the `getOwnPropertyDescriptor` trap fires) and collect by kind.
    ///
    /// `Object.getOwnPropertyNames`/`getOwnPropertySymbols` take the
    /// `GetOwnPropertyKeys` path instead (ES 20.1.2.8): they filter
    /// `[[OwnPropertyKeys]]` by key type and never consult
    /// `[[GetOwnProperty]]`, so a handler with only `ownKeys` installed must
    /// not trigger the descriptor trap.
    fn proxy_enumerate(
        &mut self,
        id: NativeId,
        proxy: Handle<JsObject>,
    ) -> Result<JsValue, JSException> {
        let keys = self.object_own_keys(proxy)?;
        if id == NativeId::ObjectGetOwnPropertyNames || id == NativeId::ObjectGetOwnPropertySymbols
        {
            let want_symbols = id == NativeId::ObjectGetOwnPropertySymbols;
            let names: Vec<JsValue> = keys
                .into_iter()
                .filter(|k| k.is_symbol() == want_symbols)
                .map(prop_key_value)
                .collect();
            return Ok(crate::call::alloc_array(self, names));
        }
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
                _ => false,
            };
            if !wanted {
                continue;
            }
            if id == NativeId::ObjectValues || id == NativeId::ObjectEntries {
                let v = self.get_property(0, 0, JsValue::object(proxy), prop_key_value(k))?;
                values.push(v);
            }
            names.push(prop_key_value(k));
        }
        match id {
            NativeId::ObjectKeys => Ok(crate::call::alloc_array(self, names)),
            NativeId::ObjectValues => Ok(crate::call::alloc_array(self, values)),
            NativeId::ObjectEntries => {
                let mut entries: Vec<JsValue> = Vec::with_capacity(names.len());
                for (k, v) in names.into_iter().zip(values) {
                    let pair = crate::call::alloc_array(self, vec![k, v]);
                    entries.push(pair);
                }
                Ok(crate::call::alloc_array(self, entries))
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
            let result = self.call_value(JsValue::object(cb), this_arg, &call_args)?;
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
                let arr = crate::call::alloc_array(self, mapped);
                self.heap.add_root(arr);
                Ok(arr)
            }
            NativeId::ArrayFilter => {
                self.gc_protect();
                let arr = crate::call::alloc_array(self, mapped);
                self.heap.add_root(arr);
                Ok(arr)
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
                let arr = crate::call::alloc_array(self, flat);
                self.heap.add_root(arr);
                Ok(arr)
            }
            _ => Ok(JsValue::undefined()),
        }
    }

    fn construct_collection(
        &mut self,
        id: NativeId,
        constructor: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let (kind, ctor_name, adder_name) = match id {
            NativeId::MapConstruct => (Kind::Map, "Map", "set"),
            NativeId::SetConstruct => (Kind::Set, "Set", "add"),
            _ => unreachable!("only Map and Set constructors use this seam"),
        };
        let ctor = constructor
            .as_object()
            .filter(|obj| self.heap.get(*obj).kind == Kind::Function)
            .or_else(|| {
                self.global
                    .and_then(|global| self.global_intrinsic_value(global, ctor_name))
                    .and_then(|value| value.as_object())
            });
        let collection = self.heap.alloc(JsObject {
            kind,
            ..JsObject::default()
        });
        let collection_v = JsValue::object(collection);
        self.heap.add_root(collection_v);
        if let Some(ctor) = ctor {
            let proto_key = self.new_temp_key("prototype");
            let proto = self.get_property(0, 0, JsValue::object(ctor), proto_key)?;
            if let Some(proto) = proto.as_object() {
                self.heap.get_mut(collection).prototype = Some(proto);
            }
        }

        let iterable = match args.first() {
            None => return Ok(collection_v),
            Some(&value) if value.is_null() || value.is_undefined() => return Ok(collection_v),
            Some(&value) => value,
        };
        // For a present iterable, resolve the ordinary adder before
        // GetIterator, matching the constructor algorithms and preserving
        // prototype overrides. The empty-construction path above must not
        // read the adder at all.
        let adder_key = self.new_temp_key(adder_name);
        let adder = self.get_property(0, 0, collection_v, adder_key)?;
        let Some(adder) = adder
            .as_object()
            .filter(|obj| matches!(self.heap.get(*obj).kind, Kind::Function | Kind::Proxy))
        else {
            return Err(JSException(
                self.error_value("TypeError: collection adder is not callable"),
            ));
        };
        let iterator = self.get_iterator(iterable, false)?;
        let result = self.consume_collection_iterable(iterator, collection_v, adder, kind);
        match result {
            Ok(()) => Ok(collection_v),
            Err(error) => {
                // The original abrupt completion wins over an error raised by
                // return(), as required by IteratorClose's throw path.
                self.op_iterator_close(iterator, true)?;
                Err(error)
            }
        }
    }

    fn consume_collection_iterable(
        &mut self,
        iterator: JsValue,
        collection: JsValue,
        adder: Handle<JsObject>,
        kind: Kind,
    ) -> Result<(), JSException> {
        loop {
            let result = self.op_iterator_next(iterator)?;
            let done_key = self.new_temp_key("done");
            let done = self.get_property(0, 0, result, done_key)?;
            if ops::to_boolean(self.heap, done) {
                return Ok(());
            }
            let value_key = self.new_temp_key("value");
            let value = self.get_property(0, 0, result, value_key)?;
            let call_args = if kind == Kind::Map {
                let Some(entry) = value.as_object() else {
                    return Err(JSException(
                        self.error_value("TypeError: Map iterator entry is not an object"),
                    ));
                };
                let key_key = self.new_temp_key("0");
                let value_key = self.new_temp_key("1");
                let key = self.get_property(0, 0, JsValue::object(entry), key_key)?;
                let entry_value = self.get_property(0, 0, JsValue::object(entry), value_key)?;
                vec![key, entry_value]
            } else {
                vec![value]
            };
            self.call_value(JsValue::object(adder), collection, &call_args)?;
        }
    }

    /// `Promise.all(iterable)` / `Promise.race(iterable)` /
    /// `Promise.allSettled(iterable)`, streamed (spec 27.5.4.1 / 27.5.4.2 /
    /// 27.5.4.5).
    ///
    /// The spec algorithm alternates between two halves, and so does this. The
    /// interpreter owns everything that is a machine operation or a call into
    /// user code: `NewPromiseCapability(C)` — which *runs* `C` — `Get(C,
    /// "resolve")`, `GetIterator`, the element loop, and every `Call` of the
    /// capability's own `[[Resolve]]`/`[[Reject]]` (user code whenever the
    /// receiver supplied its own pair). The engine owns the bookkeeping those
    /// calls feed: the results array, the unsettled count and the reaction
    /// records (jobs live in `v12-engine`). The two meet at a combinator state
    /// object on the heap ([`v12_native::promise_combinator`]).
    ///
    /// The order below is the spec's, and it is load-bearing. Steps 2-4 run
    /// before `GetIterator` at step 5, so `Promise.all.call(eval, …)` throws its
    /// `TypeError` before a single iterator step, and a poisoned `C.resolve`
    /// rejects without the iterator's `@@iterator` getter ever running.
    ///
    /// Steps 2 and 3-4 differ in how their abrupt completion surfaces, and that
    /// split is the spec's: step 2's `NewPromiseCapability(C)` has no
    /// `IfAbruptRejectPromise` after it, so a receiver that is not a constructor
    /// or that ignored its executor *throws*; step 4 applies
    /// `IfAbruptRejectPromise` to `GetPromiseResolve(C)`, so a throwing
    /// `resolve` getter or a non-callable `resolve` *rejects* the capability and
    /// the call still returns it.
    ///
    /// One element at a time is the point: an element whose `promiseResolve`,
    /// whose `GetV(nextPromise, "then")` or whose `Invoke` throws closes the
    /// iterator and settles the aggregate, so an unbounded iterator of those is
    /// not stalled by the loop. The element's *own* `then` is not on that list
    /// any more — 27.5.4.1 step 11-b hands the element to `promiseResolve`, so a
    /// thenable element's `then` is called by the adoption job, and an
    /// unbounded iterator of thenables runs the loop out the way the spec says
    /// it should. Per spec an abrupt `next`/`done`/`value` does *not* close —
    /// it rejects the capability where it happened (`PerformPromiseAll`'s
    /// `IfAbruptRejectPromise`), and so does a failed `GetIterator`.
    ///
    /// Everything the loop holds across a safepoint — the capability, the
    /// state, the iterator, the element — rides the value stack, because
    /// `gc_protect` republishes the stack as the root set and any of those
    /// calls can collect.
    fn run_promise_combinator(
        &mut self,
        id: NativeId,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let kind = match id {
            NativeId::PromiseAll => v12_native::promise_combinator::KIND_ALL,
            NativeId::PromiseAllSettled => v12_native::promise_combinator::KIND_ALL_SETTLED,
            NativeId::PromiseAny => v12_native::promise_combinator::KIND_ANY,
            _ => v12_native::promise_combinator::KIND_RACE,
        };
        let base = self.stack.len();
        // Step 2 (27.5.4.1) / step 3 (27.5.4.2):
        // `promiseCapability = NewPromiseCapability(C)`. Its own step 1 is
        // `If IsConstructor(C) is false, throw a TypeError` and its steps 7-8
        // are the two `IsCallable` checks on the pair the constructor recorded,
        // so this is where a non-constructor receiver, one that ignored its
        // executor and one that handed the executor an uncallable all throw —
        // before `Get(C, "resolve")` and before a single iterator step.
        let capability = match self.new_promise_capability(this_v) {
            Ok((promise, resolve, reject)) => Capability {
                promise,
                resolve,
                reject,
            },
            Err(error) => {
                self.stack.truncate(base);
                return Err(error);
            }
        };
        // Park the capability for the whole call: the three values live in Rust
        // locals from here to the return, and `gc_protect` cannot see them — it
        // republishes the value stack as the root set. The state object needs
        // no parking for the collector's sake (the engine pins it), but it does
        // for `gc_protect`'s.
        self.stack.push(capability.promise);
        self.stack.push(capability.resolve);
        self.stack.push(capability.reject);
        // Step 3: `promiseResolve = ? Get(C, "resolve")` (the `GetPromiseResolve`
        // abstract operation), read on the receiver `this` rather than
        // `%Promise%` so a subclassed receiver keeps its own `resolve`, and read
        // once for the whole call rather than once per element.
        let resolve_key = self.new_temp_key("resolve");
        let promise_resolve = self
            .get_property(0, 0, this_v, resolve_key)
            .and_then(|v| self.callable_or_err(v, "Promise combinator resolve is not callable"));
        let promise_resolve = match promise_resolve {
            Ok(promise_resolve) => promise_resolve,
            Err(JSException(thrown)) => {
                // Step 4: `IfAbruptRejectPromise(promiseResolve, promiseCapability)`
                // — the capability rejects with the thrown value (or with the
                // `TypeError` the `IsCallable` gate raised) and the call returns
                // the capability promise rather than propagating. The `Call` is
                // `?`, so a receiver whose own reject throws does propagate.
                self.stack.push(thrown);
                self.gc_protect();
                let rejected = self.call_value(capability.reject, JsValue::undefined(), &[thrown]);
                self.stack.truncate(base);
                rejected?;
                return Ok(capability.promise);
            }
        };
        self.stack.push(promise_resolve);
        let state = match self.combinator_begin(kind, capability.promise, capability.reject) {
            Ok(state) => state,
            Err(error) => {
                self.stack.truncate(base);
                return Err(error);
            }
        };
        self.stack.push(state);
        let result = self.stream_combinator(
            &capability,
            this_v,
            args.first().copied().unwrap_or_else(JsValue::undefined),
            promise_resolve,
            state,
        );
        self.stack.truncate(base);
        result
    }

    /// The element loop, from `GetIterator` to the terminal settlement.
    fn stream_combinator(
        &mut self,
        capability: &Capability,
        ctor: JsValue,
        iterable: JsValue,
        promise_resolve: JsValue,
        state: JsValue,
    ) -> Result<JsValue, JSException> {
        let iterator = match self.get_iterator(iterable, false) {
            Ok(iterator) => iterator,
            Err(JSException(thrown)) => {
                // No iterator record, so nothing to close: the capability
                // rejects where the abrupt happened.
                return self.settle_combinator(capability, state, false, thrown);
            }
        };
        // Park the iterator for the whole loop: `gc_protect` republishes the
        // stack as the root set, and every `next()` call can collect.
        let base = self.stack.len();
        self.stack.push(iterator);
        let mut index: u32 = 0;
        // `(exhausted, value)`: whether the iterator ran out rather than
        // throwing, and the rejection reason when it did not.
        let outcome = loop {
            // IteratorStepValue: an abrupt here rejects without closing.
            let result = match self.op_iterator_next(iterator) {
                Ok(result) => result,
                Err(JSException(thrown)) => break (false, thrown),
            };
            let done_key = self.new_temp_key("done");
            let done = match self.get_property(0, 0, result, done_key) {
                Ok(done) => done,
                Err(JSException(thrown)) => break (false, thrown),
            };
            if ops::to_boolean(self.heap, done) {
                break (true, JsValue::undefined());
            }
            let value_key = self.new_temp_key("value");
            let element = match self.get_property(0, 0, result, value_key) {
                Ok(value) => value,
                Err(JSException(thrown)) => break (false, thrown),
            };
            match self.combinator_element(capability, ctor, promise_resolve, state, index, element)
            {
                Ok(()) => {}
                // `promiseResolve`, the `then` read, the engine step and the
                // `then` invocation are element-processing aborts: the iterator
                // closes first, and the original error wins over anything
                // `return()` throws.
                Err(JSException(thrown)) => {
                    self.op_iterator_close(iterator, true)?;
                    break (false, thrown);
                }
            }
            index += 1;
        };
        self.stack.truncate(base);
        self.settle_combinator(capability, state, outcome.0, outcome.1)
    }

    /// One element of a combinator: `Call(promiseResolve, C, …)`, read `then`
    /// off the result, engine step, then either invoke that `then` on it with
    /// the pair the engine built or settle the element in place.
    ///
    /// The element is parked on the value stack for the calls that can collect;
    /// the truncate is unconditional so a throwing `promiseResolve`, `GetV` or
    /// `Invoke` leaves the same stack it found.
    fn combinator_element(
        &mut self,
        capability: &Capability,
        ctor: JsValue,
        promise_resolve: JsValue,
        state: JsValue,
        index: u32,
        element: JsValue,
    ) -> Result<(), JSException> {
        let base = self.stack.len();
        self.stack.push(element);
        let result =
            self.combinator_element_calls(capability, ctor, promise_resolve, state, index, element);
        self.stack.truncate(base);
        result
    }

    /// The body of [`Self::combinator_element`], with the element already
    /// parked on the stack.
    ///
    /// 27.5.4.1 steps 11 to 16 (27.5.4.2 has the same shape). The load-bearing
    /// line is that `then` is read and invoked on **`nextPromise`**, the value
    /// `promiseResolve` returned — never on the element. The element is only
    /// what step 11-b hands to `promiseResolve` and what step 15-c resolves the
    /// aggregate with.
    ///
    /// Reading the element's own `then` is what made a plain thenable's `then`
    /// run twice: once from this loop and once from the
    /// `PromiseResolveThenableJob` that `Promise.resolve(x)` queues for the
    /// same thenable. It is invisible when the element *is* a promise, because
    /// 27.5.4.7.1's identity check makes `nextPromise` the element — which is
    /// why `built-ins/Promise/race/resolve-ignores-late-rejection-deferred.js`
    /// was the only member of its family to fail. The element's reaction path
    /// is now reached only through `then`, with no engine watcher record.
    ///
    /// `nextPromise` and `then` ride the value stack from the `Call` to the
    /// `Invoke`: both can be a user getter's output, and `gc_protect`
    /// republishes the stack as the root set. [`Self::combinator_element`]
    /// truncates.
    fn combinator_element_calls(
        &mut self,
        capability: &Capability,
        ctor: JsValue,
        promise_resolve: JsValue,
        state: JsValue,
        index: u32,
        element: JsValue,
    ) -> Result<(), JSException> {
        // Step 11-b: `nextPromise = ? Call(promiseResolve, C, « nextValue »)`.
        // The `resolve` it names was read once, before `GetIterator`, so this
        // is a `Call` per element and no `Get`.
        //
        // The receiver is `C`, the constructor `Get(C, "resolve")` was read
        // from — the ES2015 text said `undefined` here, and the ratified one
        // says `C`. It is load-bearing, not cosmetic: 27.5.4.7.1 step 3 only
        // takes the identity resolve when
        // `SameValue(Get(x, "constructor"), C)`, so a promise element is the
        // identity only if `promiseResolve` is called *on* `C`.
        // `built-ins/Promise/{all,race}/invoke-resolve.js` asserts the receiver
        // directly.
        let next_promise = self.call_value(promise_resolve, ctor, &[element])?;
        self.stack.push(next_promise);
        // Step 12: `then = ? GetV(nextPromise, "then")`. The receiver is
        // `nextPromise`, and the nullish `TypeError` `GetV` specifies comes
        // from `get_property` — see [`Self::get_v`].
        let then_v = self.get_v(next_promise, "then")?;
        self.stack.push(then_v);
        // Steps 12-13: the engine counts the element unsettled and either
        // reports that there is no `then` to invoke (a one-element carrier) or
        // builds the `[onFulfilled, onRejected]` pair to hand it.
        let carrier = self.combinator_step(state, index, element, then_v, capability.resolve)?;
        // The carrier's length is the whole protocol: one element means there
        // was no callable `then` to invoke, two means the pair to hand it.
        if self.carrier_len(carrier) == v12_native::promise_combinator::carrier::SETTLE_NOW {
            let zero = self.new_temp_key("0");
            let value = self.get_property(0, 0, carrier, zero)?;
            return self.settle_element_now(capability, state, index, value);
        }
        let zero = self.new_temp_key("0");
        let one = self.new_temp_key("1");
        let on_fulfilled = self.get_property(0, 0, carrier, zero)?;
        let on_rejected = self.get_property(0, 0, carrier, one)?;
        self.stack.push(carrier);
        self.stack.push(on_fulfilled);
        self.stack.push(on_rejected);
        // Step 16: `Perform ? Invoke(nextPromise, "then", « pair »)`. The abrupt
        // is an element-processing one, so `stream_combinator` closes the
        // iterator and rejects the capability with it — which is what
        // `built-ins/Promise/all/invoke-then-get-error-close.js` and
        // `-reject.js` assert. `Invoke` itself only rejects when `GetV` throws
        // (a `nullish` `nextPromise`, a step earlier) or when the second read of
        // `then` is not callable.
        self.invoke_on(next_promise, "then", &[on_fulfilled, on_rejected])?;
        Ok(())
    }

    /// `GetV(V, P)` (spec 7.3.9). `get_property` already opens with the
    /// nullish `TypeError` that `GetV` specifies, so that half needs nothing
    /// here; the name records which abstract operation the combinator's two
    /// `then` reads are, because that is what fixes the receiver.
    ///
    /// The other half — `ToObject(V)` for a primitive receiver — is the
    /// engine's general gap, not this path's: `get_property` routes a
    /// primitive to its string-primitive surface and yields `undefined`
    /// otherwise, so a number wrapper's `then` is not reachable. It is
    /// unobservable here in practice, because `Promise.resolve(x)` returns a
    /// promise for every `x` — a primitive `nextPromise` needs a receiver whose
    /// own `resolve` returns one, and its wrapper has no `then` either way.
    fn get_v(&mut self, v: JsValue, name: &str) -> Result<JsValue, JSException> {
        let key = self.new_temp_key(name);
        self.get_property(0, 0, v, key)
    }

    /// `Invoke(V, P, arguments)` (spec 7.3.13): `GetV(V, P)`, an `IsCallable`
    /// gate, then `Call(f, V, arguments)`.
    ///
    /// `Invoke` has no "V must be an Object" step — its only guards are
    /// `GetV`'s nullish check and `IsCallable` — so a primitive receiver is
    /// legal and reaches `Call` with the primitive as `this`.
    ///
    /// The `GetV` is a *second* read of `P`: the abstract operation does not
    /// take the value 27.5.4.1 step 12 already read, so a `nextPromise` whose
    /// `then` becomes non-callable between the two steps throws instead of
    /// being called.
    fn invoke_on(
        &mut self,
        v: JsValue,
        name: &str,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let f = self.get_v(v, name)?;
        let f = self.callable_or_err(f, &format!("{name} is not a function"))?;
        self.call_value(f, v, args)
    }

    /// The non-callable-`then` paths (27.5.4.1 step 11-c / 27.5.4.2 step 8-e):
    /// the element settles without a `Call`, through the engine's bookkeeping
    /// and then — if it was the element that settles the aggregate — the
    /// capability's own `[[Resolve]]`.
    fn settle_element_now(
        &mut self,
        capability: &Capability,
        state: JsValue,
        index: u32,
        value: JsValue,
    ) -> Result<(), JSException> {
        let settled = self.combinator_element_settled(state, index, true, value)?;
        self.resolve_through(capability.resolve, capability.reject, settled)
    }

    /// `Call(promiseCapability.[[Resolve]], undefined, « value »)` for the
    /// value the engine says the aggregate settles with, with
    /// `IfAbruptRejectPromise` on a throw. That handler is the point of routing
    /// settlement through the capability at all: `[[Resolve]]` is the
    /// receiver's own code, so its throw becomes the rejection reason instead
    /// of disappearing.
    ///
    /// A `None` means the element did not settle the aggregate, so there is no
    /// call to make.
    fn resolve_through(
        &mut self,
        resolve_v: JsValue,
        reject_v: JsValue,
        value: Option<JsValue>,
    ) -> Result<(), JSException> {
        let Some(value) = value else {
            return Ok(());
        };
        match self.call_value(resolve_v, JsValue::undefined(), &[value]) {
            Ok(_) => Ok(()),
            // 27.5.1.1.1: reject with the thrown value instead. This `Call` is
            // itself `?`, so a throw from the receiver's reject propagates.
            Err(JSException(thrown)) => {
                self.call_value(reject_v, JsValue::undefined(), &[thrown])?;
                Ok(())
            }
        }
    }

    /// The terminal settlement: `KIND_ALL` exhaustion fulfills through the
    /// capability when nothing is still outstanding, every abrupt path rejects
    /// through it, and either way `Promise.all`/`Promise.race` return
    /// `promiseCapability.[[Promise]]` (27.5.4.1 step 13).
    fn settle_combinator(
        &mut self,
        capability: &Capability,
        state: JsValue,
        exhausted: bool,
        reason: JsValue,
    ) -> Result<JsValue, JSException> {
        self.stack.push(reason);
        self.gc_protect();
        let outcome = if exhausted {
            // Exhaustion is not a half of any element's pair, so the payoff is
            // the kind's own: `KIND_ANY` spends the exhaustion token on its
            // errors list (27.5.4.3.1 step 6-c-d, and the all-rejected case
            // that arrives here with the count already down to the token).
            let payoff =
                if self.combinator_kind(state) == Some(v12_native::promise_combinator::KIND_ANY) {
                    Payoff::Rejects
                } else {
                    Payoff::Fulfils
                };
            match self.combinator_settle(state) {
                Ok(settled) => self.settle_through(
                    state,
                    capability.resolve,
                    capability.reject,
                    payoff,
                    settled,
                ),
                Err(error) => Err(error),
            }
        } else {
            // Step 7's `IfAbruptRejectPromise`: the capability rejects with the
            // thrown value and the call still returns the capability promise,
            // so a bad iterator rejects rather than throwing at the call site.
            self.call_value(capability.reject, JsValue::undefined(), &[reason])
                .map(|_| ())
        };
        self.stack.truncate(self.stack.len() - 1);
        outcome?;
        Ok(capability.promise)
    }

    /// The single point where a settled element (or an exhausted loop) hands the
    /// value the engine says the aggregate settles with over to the capability.
    /// Two kinds of payoff come out of that seam and they are the two halves of
    /// the operation, not two code paths: a **values** array the interpreter
    /// resolves the aggregate with, and — for `KIND_ANY` — an **errors** array
    /// it wraps in an `AggregateError` and rejects with.
    ///
    /// `KIND_ALL`, `KIND_ALL_SETTLED` and `KIND_RACE` all take the fulfilment
    /// payoff, so `KIND_ANY` is the only kind that reaches the rejection one —
    /// a kind test here rather than a second reaction path, so both callers
    /// ([`Self::settle_combinator`] and [`Self::run_combinator_element`]) keep
    /// one line each however the combinator families grow.
    fn settle_through(
        &mut self,
        state: JsValue,
        resolve_v: JsValue,
        reject_v: JsValue,
        payoff: Payoff,
        value: Option<JsValue>,
    ) -> Result<(), JSException> {
        let Some(value) = value else {
            return Ok(());
        };
        if payoff == Payoff::Rejects
            && self.combinator_kind(state) == Some(v12_native::promise_combinator::KIND_ANY)
        {
            return self.reject_with_aggregate_error(reject_v, value);
        }
        self.resolve_through(resolve_v, reject_v, Some(value))
    }

    /// `KIND_ANY`'s terminal step, at both places the spec has it:
    /// `let error be AggregateError(errors)` then
    /// `Perform ! Call(promiseCapability.[[Reject]], undefined, « error »)`
    /// (27.5.4.3.1 step 6-c-d for the empty iterable, and
    /// 27.5.4.3.2 step 4-i-i-ii for the last element to reject).
    ///
    /// The construction lives here rather than in the engine seam for the two
    /// reasons the whole combinator is split across the seam: `Construct` is an
    /// interpreter operation, and `promiseCapability.[[Reject]]` is user code
    /// whenever the receiver is a subclass that supplied its own executor pair.
    ///
    /// `errors` is the array the engine snapshotted at the elements' own
    /// indices, so a later rejection cannot extend it. Both `AggregateError` and
    /// the call's argument are parked on the value stack across the `Construct`
    /// and the `Call`, since `gc_protect` republishes the stack as the root set
    /// and either step can collect.
    fn reject_with_aggregate_error(
        &mut self,
        reject_v: JsValue,
        errors: JsValue,
    ) -> Result<(), JSException> {
        let base = self.stack.len();
        let ctor = self
            .global
            .and_then(|global| self.global_intrinsic_value(global, "AggregateError"))
            .and_then(|value| value.as_object());
        let Some(ctor) = ctor else {
            self.stack.truncate(base);
            return Err(JSException(
                self.error_value("TypeError: AggregateError is not available"),
            ));
        };
        self.stack.push(JsValue::object(ctor));
        self.stack.push(errors);
        self.gc_protect();
        let ctor_v = self.stack[base];
        let errors_v = self.stack[base + 1];
        let error = self.construct_object(ctor, &[errors_v], ctor_v)?;
        if let Some(error) = error.as_object() {
            self.heap.add_pinned_root(error);
        }
        self.stack.push(error);
        self.gc_protect();
        // Both steps are asserted (`!`) in the spec, so a receiver whose own
        // `reject` throws propagates out of the reaction job.
        let outcome = self.call_value(reject_v, JsValue::undefined(), &[error]);
        self.stack.truncate(base);
        outcome.map(|_| ())
    }

    /// `PerformPromiseAll`'s `onFulfilledElement` (spec 27.5.4.1.1 step 4) —
    /// and, for `KIND_ALL_SETTLED`, both halves of the pair 27.5.4.2.1 steps
    /// 4 and 5 build. The one remaining path from an element to the aggregate.
    ///
    /// It is an interpreter-intercepted native rather than an engine host
    /// closure because its last step is
    /// `Perform ? Call(resultCapability.[[Resolve]], undefined, « valuesArray
    /// »)` (step 4-b-iii-2), and for a subclassed receiver `[[Resolve]]` is
    /// user code that a `&mut Heap` closure cannot reach. The element's own
    /// record — state, index, `[[Resolve]]`, the spec's `alreadyCalled` flag and
    /// which half of the pair this is — rides the function object, so this is
    /// reached from [`Self::call_value`] with the callee in hand.
    ///
    /// Rooting across the `Call` below needs nothing: the function object is
    /// pinned by the engine, `elements` is traced, and the reaction record pins
    /// the handler itself, so the state object, the results array it holds and
    /// the flag all stay reachable for the whole call.
    fn run_combinator_element(
        &mut self,
        callee: Handle<JsObject>,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        use v12_native::promise_combinator::element as record;
        let element_record = self.heap.get(callee).elements.clone();
        if element_record.len() < record::SLOTS {
            return Err(JSException(
                self.error_value("TypeError: combinator element has no record"),
            ));
        }
        let state = element_record[record::STATE];
        let index = element_record[record::INDEX].as_smi().ok_or_else(|| {
            JSException(self.error_value("TypeError: combinator index is not Smi"))
        })? as u32;
        let resolve_v = element_record[record::RESOLVE];
        let Some(already) = element_record[record::ALREADY].as_object() else {
            return Err(JSException(self.error_value(
                "TypeError: combinator element has no alreadyResolved",
            )));
        };
        let fulfilled = ops::to_boolean(self.heap, element_record[record::FULFILLED]);
        // Steps 4-a-1/2 (and 5-a-1/2): the per-element first-wins guard, so a
        // `then` free to call its resolve function twice records the first
        // outcome only. `KIND_ALL_SETTLED`'s two halves share one record, so
        // the second half to fire is the one that returns here.
        if ops::to_boolean(self.heap, self.heap.get(already).properties[0]) {
            return Ok(JsValue::undefined());
        }
        self.heap.get_mut(already).properties[0] = JsValue::from_bool(true);
        let value = args.first().copied().unwrap_or_else(JsValue::undefined);
        // `KIND_ALL` stores the value itself; `KIND_ALL_SETTLED` stores the
        // status object the half builds first (27.5.4.2.1 step 4-b-iii-iv /
        // 5-b-iii-iv), so the engine's `element_settled` writes the same
        // `values[thisIndex]` in both cases.
        let outcome = if self.combinator_kind(state) == Some(KIND_ALL_SETTLED) {
            self.all_settled_status(fulfilled, value)?
        } else {
            value
        };
        // Steps 4-a-iii to 4-b: the bookkeeping, and — for the last outstanding
        // element — the values array `[[Resolve]]` is called with, or, for
        // `KIND_ANY`, the errors array an `AggregateError` is built from and
        // `[[Reject]]` is called with.
        let settled = self.combinator_element_settled(state, index, fulfilled, outcome)?;
        self.heap.add_pinned_root(callee);
        let reject_v = self.combinator_state_slot(state, SLOT_REJECT);
        let payoff = if fulfilled {
            Payoff::Fulfils
        } else {
            Payoff::Rejects
        };
        self.settle_through(state, resolve_v, reject_v, payoff, settled)?;
        Ok(JsValue::undefined())
    }

    /// The combinator's own kind, read back off the state object
    /// ([`Self::combinator_state_slot`]) through
    /// [`v12_native::promise_combinator::combinator_kind`]. `None` for a state
    /// object whose kind slot is not a known kind, which the element handler
    /// treats as "not `allSettled`" — the same reading the engine's own
    /// `parts` would reject loudly, and unreachable for a state object the
    /// engine itself allocated.
    fn combinator_kind(&mut self, state: JsValue) -> Option<i32> {
        let slot = self.combinator_state_slot(state, SLOT_KIND);
        v12_native::promise_combinator::combinator_kind(slot)
    }

    /// The element's status object: `OrdinaryObjectCreate(%Object.prototype%)`
    /// plus `CreateDataPropertyOrThrow(status, "status", …)` and
    /// `CreateDataPropertyOrThrow(status, "value"|"reason", …)`, all with
    /// `DEFAULT` attributes (27.5.4.2.1 step 4-b-ii-iii / 5-b-ii-iii).
    ///
    /// The property order is the spec's: `status` first, which is what
    /// `Object.keys(status)` reports. `CreateDataPropertyOrThrow` cannot throw
    /// here — the object is fresh, ordinary and extensible by construction — so
    /// the `?` is discharged by the allocation itself.
    fn all_settled_status(
        &mut self,
        fulfilled: bool,
        payload: JsValue,
    ) -> Result<JsValue, JSException> {
        let base = self.stack.len();
        // Park the payload: the two `define_own_data_attrs` calls are pure heap
        // work, but `gc_protect` republishes the stack as the root set and the
        // status object itself is not on it yet.
        self.stack.push(payload);
        self.gc_protect();
        let h = self.heap.alloc(JsObject::default());
        self.link_object_proto(h);
        let status = JsValue::object(h);
        let (label, key) = if fulfilled {
            ("fulfilled", "value")
        } else {
            ("rejected", "reason")
        };
        let label_v = JsValue::string(self.heap.intern_text(label));
        let status_key = self.new_temp_key("status");
        let payload_key = self.new_temp_key(key);
        self.define_own_data_attrs(status, status_key, label_v, v12_heap::Attrs::DEFAULT)?;
        self.define_own_data_attrs(status, payload_key, payload, v12_heap::Attrs::DEFAULT)?;
        self.stack.truncate(base);
        Ok(status)
    }

    /// [`Self::resolve_through`] for the element handler, which reads the pair
    /// off its own record rather than off a live [`Capability`].
    ///
    /// Reads one named slot of a combinator state object (slot names in
    /// `v12_native::promise_combinator`).
    fn combinator_state_slot(&mut self, state: JsValue, slot: i32) -> JsValue {
        self.heap
            .get(state.as_object().expect("a state object"))
            .properties
            .get(slot as usize)
            .copied()
            .unwrap_or_else(JsValue::undefined)
    }

    /// The element count of a `combinator_step` carrier, which is what tells
    /// the "settle this now" carrier from the `[onFulfilled, onRejected]` pair.
    fn carrier_len(&self, carrier: JsValue) -> usize {
        carrier
            .as_object()
            .map_or(0, |obj| self.heap.get(obj).element_len())
    }

    /// `combinator_begin` over the registry seam. The registry builds its own
    /// `Ctx` from the shared pending sink, so the interpreter hands over only
    /// the heap.
    fn combinator_begin(
        &mut self,
        kind: i32,
        promise: JsValue,
        reject: JsValue,
    ) -> Result<JsValue, JSException> {
        let result = self
            .natives
            .combinator_begin(self.heap, kind, promise, reject);
        result.map_err(|t| JSException::from_throw(self.heap, t))
    }

    /// `combinator_step` over the registry seam.
    fn combinator_step(
        &mut self,
        state: JsValue,
        index: u32,
        value: JsValue,
        then_v: JsValue,
        resolve: JsValue,
    ) -> Result<JsValue, JSException> {
        let result = self
            .natives
            .combinator_step(self.heap, state, index, value, then_v, resolve);
        result.map_err(|t| JSException::from_throw(self.heap, t))
    }

    /// `combinator_element_settled` over the registry seam.
    fn combinator_element_settled(
        &mut self,
        state: JsValue,
        index: u32,
        fulfilled: bool,
        value: JsValue,
    ) -> Result<Option<JsValue>, JSException> {
        let result = self
            .natives
            .combinator_element_settled(self.heap, state, index, fulfilled, value);
        result.map_err(|t| JSException::from_throw(self.heap, t))
    }

    /// `combinator_settle` over the registry seam.
    fn combinator_settle(&mut self, state: JsValue) -> Result<Option<JsValue>, JSException> {
        let result = self.natives.combinator_settle(self.heap, state, true);
        result.map_err(|t| JSException::from_throw(self.heap, t))
    }

    /// The `IsCallable` gate for a value the spec `Call`s.
    fn callable_or_err(&mut self, v: JsValue, message: &str) -> Result<JsValue, JSException> {
        let callable = v
            .as_object()
            .is_some_and(|o| matches!(self.heap.get(o).kind, Kind::Function | Kind::Proxy));
        if !callable {
            return Err(JSException(
                self.error_value(&format!("TypeError: {message}")),
            ));
        }
        Ok(v)
    }

    /// `Promise.resolve(x)` when `x` is a thenable (spec 27.2.1.3.1).
    ///
    /// The `then` read runs here — the ordinary property path, so a getter or
    /// a proxy `get` trap is honored at the right moment — and the adoption
    /// itself is queued for the engine, which owns the job that calls `then`
    /// with the capability pair. Returns `None` when `x` is not a thenable,
    /// leaving the registry native to fulfill with the value unchanged.
    fn run_promise_resolve(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Option<Result<JsValue, JSException>> {
        let value = args.first().copied().unwrap_or_else(JsValue::undefined);
        // A throwing `constructor` getter is a `TypeError` out of
        // `Promise.resolve` itself, so the identity check's `Result` has to
        // become the `Option`'s `Some(Err(..))` rather than short-circuiting
        // the arm: `None` is reserved for "not a thenable, let the registry
        // native fulfill".
        match self.promise_resolve_identity(this_v, value) {
            Ok(Some(identity)) => return Some(Ok(identity)),
            Ok(None) => {
                // A different constructor: steps 2-4 still run, so `value` goes
                // through the ordinary resolving-function path below.
            }
            Err(error) => return Some(Err(error)),
        }
        // Park the argument on the value stack: the `then` read may run a
        // getter, and `gc_protect` republishes the stack as the root set.
        self.stack.push(value);
        let read = self.read_then(value);
        let (then_v, abrupt) = match read {
            crate::generator_async::ThenRead::NotThenable => {
                self.stack.pop();
                return None;
            }
            crate::generator_async::ThenRead::Abrupt(thrown) => (thrown, true),
            crate::generator_async::ThenRead::Then(then_v) => (then_v, false),
        };
        let result = self.adopt_thenable(this_v, value, then_v, abrupt);
        self.stack.pop();
        Some(result)
    }

    /// `PromiseResolve(C, x)` step 1-b (spec 27.5.4.7.1): the identity
    /// short-circuit, `If IsPromise(x) is true, then … If SameValue(xConstructor,
    /// C) is true, return x`.
    ///
    /// `Some(x)` is that return; `None` means the short-circuit does not apply
    /// and the caller must build a capability. Shared by the `Promise.resolve`
    /// static (which then reads `then` and adopts) and by `Promise.try` (which
    /// then calls `NewPromiseCapability` + `[[Resolve]]`) because the two
    /// abstract operations are the same function of `C` and the resolution.
    ///
    /// The identity is conditional — `IsPromise` alone is not enough, which is
    /// what `built-ins/Promise/resolve/arg-uniq-ctor.js` pins: a promise whose
    /// own `constructor` is `null` is resolved by *building* a new one. The
    /// read is a machine call, so it belongs here rather than in
    /// `is_engine_promise`; `ReturnIfAbrupt` propagates out of
    /// `Promise.resolve` as a `TypeError` from a throwing getter.
    fn promise_resolve_identity(
        &mut self,
        this_v: JsValue,
        value: JsValue,
    ) -> Result<Option<JsValue>, JSException> {
        if !self.is_engine_promise(value) {
            return Ok(None);
        }
        self.stack.push(value);
        let ctor_key = self.new_temp_key("constructor");
        let x_constructor = self.get_property(0, 0, value, ctor_key);
        self.stack.pop();
        let x_constructor = x_constructor?;
        if x_constructor.bits() == this_v.bits() {
            return Ok(Some(value));
        }
        Ok(None)
    }

    /// `PromiseResolve(C, x)` (spec 27.5.4.7.1 steps 1-4) as a value:
    /// `Promise.try`'s step 5 calls it and returns what it produces.
    ///
    /// The identity short-circuit is [`Self::promise_resolve_identity`]'s;
    /// steps 2-4 are `NewPromiseCapability(C)`, `Call([[Resolve]], undefined,
    /// « resolution »)` and `return [[Promise]]`. The `Call` is the
    /// interpreter's — for a subclassed receiver `[[Resolve]]` is user code — and
    /// the `?` on it propagates, as step 3's `Perform ?` does.
    fn promise_resolve(
        &mut self,
        ctor: JsValue,
        resolution: JsValue,
    ) -> Result<JsValue, JSException> {
        if let Some(identity) = self.promise_resolve_identity(ctor, resolution)? {
            return Ok(identity);
        }
        let (promise, resolve, _reject) = self.new_promise_capability(ctor)?;
        // Park the resolution across the call: `gc_protect` republishes the
        // value stack as the root set, and a subclass's `[[Resolve]]` can
        // collect.
        let base = self.stack.len();
        self.stack.push(resolution);
        self.gc_protect();
        let result = self.call_value(resolve, JsValue::undefined(), &[resolution]);
        self.stack.truncate(base);
        result?;
        Ok(promise)
    }

    /// `Promise.withResolvers()` (spec 27.5.4.9).
    ///
    /// Two `NewPromiseCapability(ctor)`-shaped steps and three
    /// `CreateDataPropertyOrThrow` calls, so the capability is the only
    /// machine work: the object is `OrdinaryObjectCreate(%Object.prototype%)`
    /// and each property is installed with `Attrs::DEFAULT` —
    /// `{ writable: true, enumerable: true, configurable: true }`, which is
    /// what `built-ins/Promise/withResolvers/result.js` verifies (a
    /// `BUILTIN`-shaped install would read `enumerable: false` and fail it).
    ///
    /// The property order is the spec's, and `Object.getOwnPropertyNames`
    /// reports it: `promise`, `resolve`, `reject`.
    fn run_promise_with_resolvers(
        &mut self,
        this_v: JsValue,
        _args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        // Step 2: `NewPromiseCapability(C)`, whose own step 1 is the
        // `IsConstructor` gate — so a non-constructor receiver throws here, and
        // a non-object one with it.
        let (promise, resolve, reject) = self.new_promise_capability(this_v)?;
        let base = self.stack.len();
        self.stack.push(promise);
        self.stack.push(resolve);
        self.stack.push(reject);
        self.gc_protect();
        let h = self.heap.alloc(JsObject::default());
        self.link_object_proto(h);
        let obj = JsValue::object(h);
        for (name, value) in [
            ("promise", promise),
            ("resolve", resolve),
            ("reject", reject),
        ] {
            let key = self.new_temp_key(name);
            self.define_own_data_attrs(obj, key, value, Attrs::DEFAULT)?;
        }
        self.stack.truncate(base);
        Ok(obj)
    }

    /// `Promise.try(callback, ...args)` (spec 27.5.4.8).
    ///
    /// The order is the spec's and it is load-bearing twice over: step 1's
    /// `If ctor is not an Object, throw a TypeError exception` runs *before*
    /// the callback, so a non-object receiver never runs user code; and the
    /// capability is built *inside* the abrupt branch (step 4-a), so a
    /// non-throwing call never constructs through the receiver at all — which is
    /// why `Promise.try.call(SubPromise, () => {})` reports `callCount === 1`
    /// (the single construction is the one step 5's `PromiseResolve` makes).
    ///
    /// Step 5 is [`Self::promise_resolve`], so a promise the callback returns is
    /// adopted by identity rather than wrapped.
    fn run_promise_try(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        // Step 1: `If ctor is not an Object, throw a TypeError exception`.
        if this_v.as_object().is_none() {
            return Err(JSException(
                self.error_value("TypeError: Promise.try requires an object receiver"),
            ));
        }
        let callback = args.first().copied().unwrap_or_else(JsValue::undefined);
        let forwarded = args.get(1..).unwrap_or(&[]);
        // Step 2: `Let status be Completion(Call(callback, undefined, args))`.
        // A missing or uncallable callback throws out of `Call` like any other,
        // and lands in the same abrupt branch — so the *promise* rejects rather
        // than the call throwing.
        let status = self.call_value(callback, JsValue::undefined(), forwarded);
        match status {
            Err(JSException(thrown)) => {
                // Step 4: the capability is built here, not before the call.
                let (promise, _resolve, reject) = self.new_promise_capability(this_v)?;
                let base = self.stack.len();
                self.stack.push(thrown);
                self.gc_protect();
                let result = self.call_value(reject, JsValue::undefined(), &[thrown]);
                self.stack.truncate(base);
                result?;
                Ok(promise)
            }
            // Step 5: `Return ? PromiseResolve(ctor, status)`.
            Ok(value) => self.promise_resolve(this_v, value),
        }
    }

    /// `new Promise(executor)` (spec 27.5.1.1 steps 2-7).
    ///
    /// Step 5 is the reason this is not a registry native:
    /// `Call(executor, undefined, « [[Resolve]], [[Reject]] »)` runs *before* the
    /// constructor returns, so script that captures a resolving function can
    /// call it in the same turn. Deferring the call to a checkpoint — which is
    /// how the thenable adoption below works, because a `then` the script
    /// supplies is only callable from a job — inverts that order and breaks
    /// every program that reaches for `resolve` after the `new`.
    ///
    /// The three capability values arrive from the engine as one array
    /// (`NativeRegistry::promise_capability`) and the executor is called here,
    /// so the machine re-entry this needs stays on this side of the seam.
    fn run_promise_construct(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        // Step 2: `If IsCallable(executor) is false, throw a TypeError`. Ahead
        // of the `IsConstructor` check, as the spec orders them.
        let executor = args.first().copied().unwrap_or_else(JsValue::undefined);
        self.callable_or_err(executor, "Promise executor is not a function")?;
        // Step 3: `NewPromiseCapability(C)`, whose first act is
        // `If IsConstructor(C) is false, throw a TypeError`. `Promise` without
        // `new` lands here with a receiver that is not the constructor, so this
        // is also step 1's `NewTarget is undefined` in practice: a plain call's
        // receiver never carries the construct target. A proxy in front of
        // `Promise` is honored, as `IsConstructor` on a proxy exotic object
        // requires.
        //
        // The gate is skipped when the receiver is already the instance — a
        // derived constructor's `super(executor)`, which is a `Call` with the
        // pre-made `this`, not a `Construct`. The engine's
        // `promise_capability` initializes that object in place rather than
        // allocating a second one, so this arm returns the receiver itself.
        if !self.promise_instance_receiver(this_v) && !self.is_capability_constructor(this_v) {
            return Err(JSException(
                self.error_value("TypeError: Promise constructor requires 'new'"),
            ));
        }
        let (promise, resolve, reject) = self.promise_capability(this_v)?;
        let Some(executor_obj) = executor.as_object() else {
            unreachable!("callable_or_err rejects a non-object executor")
        };
        // The capability pair and the executor live in Rust locals across the
        // call below, which `gc_protect` cannot see: it republishes the value
        // stack as the root set, so park all three for its duration (the
        // `stream_combinator` pattern). `promise` needs no parking — the
        // engine pins it, as it pins the pair's own closures.
        let base = self.stack.len();
        self.stack.push(executor);
        self.stack.push(resolve);
        self.stack.push(reject);
        self.gc_protect();
        let outcome = self.call_object(executor_obj, JsValue::undefined(), &[resolve, reject]);
        self.stack.truncate(base);
        match outcome {
            Ok(_) => Ok(promise),
            // Step 6: `IfAbruptRejectPromise` — the throw becomes the rejection
            // reason, and the constructor still returns the promise.
            Err(JSException(thrown)) => {
                self.capability_settle(promise, resolve, reject, thrown, true)?;
                Ok(promise)
            }
        }
    }

    /// `NewPromiseCapability(C)` over the registry seam: the engine builds the
    /// promise and the two host-closure resolving functions, and hands all three
    /// back as one array. Read through the ordinary property path, like
    /// `combinator_element_calls` reads its pair.
    fn promise_capability(
        &mut self,
        ctor: JsValue,
    ) -> Result<(JsValue, JsValue, JsValue), JSException> {
        let capability = self
            .natives
            .promise_capability(self.heap, ctor)
            .map_err(|t| JSException::from_throw(self.heap, t))?;
        let read = |interp: &mut Self, i: u32| -> Result<JsValue, JSException> {
            let key = interp.new_temp_key(&i.to_string());
            interp.get_property(0, 0, capability, key)
        };
        let promise = read(self, 0)?;
        let resolve = read(self, 1)?;
        let reject = read(self, 2)?;
        Ok((promise, resolve, reject))
    }

    /// `capability_settle` over the registry seam, for the step-6 abrupt path.
    fn capability_settle(
        &mut self,
        promise: JsValue,
        resolve: JsValue,
        reject: JsValue,
        value: JsValue,
        rejecting: bool,
    ) -> Result<JsValue, JSException> {
        self.natives
            .capability_settle(self.heap, promise, resolve, reject, value, rejecting)
            .map_err(|t| JSException::from_throw(self.heap, t))
    }

    /// True when `this_v` is the object a derived constructor's
    /// `super(executor)` passes — a `Call`, not a `Construct`, so the receiver
    /// is the pre-made instance rather than the constructor.
    ///
    /// A deliberately shallow mirror of the engine's `in_place_instance`
    /// (which is the authority on whether the receiver really is initialized in
    /// place): both are structural header reads, and the two agreeing is what
    /// keeps the `IsConstructor` gate from rejecting a legitimate subclass
    /// while still rejecting `Promise.call({})`-shaped nonsense from arm A's
    /// perspective. A disagreement would surface as a `TypeError`, never as a
    /// mis-shaped object.
    fn promise_instance_receiver(&self, this_v: JsValue) -> bool {
        this_v.as_object().is_some_and(|obj| {
            !matches!(
                self.heap.get(obj).kind,
                Kind::Function | Kind::Proxy | Kind::Promise
            )
        })
    }

    /// The `SpeciesConstructor(O, defaultConstructor)` sequence of spec
    /// 27.5.5.4 step 3, inlined.
    ///
    /// (It is spelled as its own abstract operation in the ES2015+ *text*, but
    /// the ratified standard writes the steps out at each use site — there is no
    /// `SpeciesConstructor` in the operation index — so this is a helper, not a
    /// named AO, and it is deliberately not exported under that name.)
    ///
    /// 1. `C = Get(O, "constructor")`
    /// 2. `undefined` `C` → `defaultConstructor`
    /// 3. a non-object `C` → `TypeError`
    /// 4. `S = Get(C, @@species)`
    /// 5. `null`/`undefined` `S` → `defaultConstructor`
    /// 6. a non-constructor `S` → `TypeError`
    ///
    /// Both `Get`s run user code (a getter, or a proxy `get` trap), and step 6's
    /// `IsConstructor` has to follow a proxy exotic to its target, so this is
    /// interpreter work with [`Self::is_capability_constructor`] as the oracle —
    /// the same one `NewPromiseCapability` step 1 uses, which is why a proxy in
    /// front of a constructor passes here and a revoked one does not.
    pub(crate) fn species_constructor(
        &mut self,
        obj: JsValue,
        default_ctor: JsValue,
    ) -> Result<JsValue, JSException> {
        // Step 1. The read can collect, so park the receiver for it.
        let ctor_key = self.new_temp_key("constructor");
        self.stack.push(obj);
        let read = self.get_property(0, 0, obj, ctor_key);
        self.stack.pop();
        let c = read?;
        // Step 2: only `undefined` falls back — a `null` constructor is the
        // step-3 `TypeError`, which is what makes `p.constructor = null`
        // throw rather than silently degrade.
        let c = if c.is_undefined() { default_ctor } else { c };
        // Step 3.
        if c.as_object().is_none() {
            return Err(JSException(
                self.error_value("TypeError: species constructor is not an object"),
            ));
        }
        // Step 4.
        let species_key = self
            .species_wk_key()
            .symbol()
            .map(JsValue::symbol)
            .unwrap_or_else(JsValue::undefined);
        self.stack.push(c);
        let read = self.get_property(0, 0, c, species_key);
        self.stack.pop();
        let s = read?;
        // Step 5: both the null and the undefined species fall back.
        let s = if s.is_null() || s.is_undefined() {
            default_ctor
        } else {
            s
        };
        // Step 6.
        if !self.is_capability_constructor(s) {
            return Err(JSException(
                self.error_value("TypeError: species is not a constructor"),
            ));
        }
        Ok(s)
    }

    /// This realm's `%Promise%`, the `defaultConstructor` 27.5.5.4 step 3 and
    /// 27.5.5.8 step 3 pass.
    ///
    /// Read through the `Promise` intrinsic slot rather than cached in a field,
    /// so an embedder with no realm answers `undefined` (and the `IsConstructor`
    /// gate downstream reports the missing constructor) rather than caching a
    /// stale handle across an install.
    fn promise_ctor_value(&mut self) -> JsValue {
        let Some(global) = self.global else {
            return JsValue::undefined();
        };
        let idx = intrinsic_slot("Promise").expect("Promise intrinsic");
        self.heap
            .get(global)
            .properties
            .get(idx)
            .copied()
            .unwrap_or_else(JsValue::undefined)
    }

    /// `Promise.prototype.then(onFulfilled, onRejected)` (spec 27.5.5.4),
    /// the interpreter seam.
    ///
    /// Steps 1-2 are the receiver check the engine handler also makes; steps
    /// 3-5 are the species selection and the capability (both need the
    /// machine — a getter, then a `Construct`); step 6 is the engine's
    /// reaction record. The derived promise is therefore
    /// `resultCapability.[[Promise]]`, the object the *selected* constructor
    /// built, which is the whole observable point of the species read.
    ///
    /// `resultCapability`'s pair rides along to the engine: 27.5.2.1 step 4
    /// settles the derived promise by `Call`ing `[[Resolve]]`/`[[Reject]]`` with
    /// the handler's result, and that is what adopts a returned thenable and
    /// what lets a species constructor supply resolving functions of its own.
    /// The engine calls them from the reaction job, where a `JobCtx` holds this
    /// interpreter — a `NativeHandler` cannot, because either half may be user
    /// code.
    fn run_promise_then(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        if !self.is_promise(this_v) {
            return Err(JSException(self.error_value(
                "TypeError: Promise.prototype.then requires a promise",
            )));
        }
        let on_fulfilled = args.first().copied().unwrap_or_else(JsValue::undefined);
        let on_rejected = args.get(1).copied().unwrap_or_else(JsValue::undefined);
        let default_ctor = self.promise_ctor_value();
        let ctor = self.species_constructor(this_v, default_ctor)?;
        let (derived, resolve, reject) = self.new_promise_capability(ctor)?;
        // Park the derived promise across the engine seam: the record install
        // allocates, and `gc_protect` (run by the allocation) republishes the
        // value stack as the root set, so a Rust local would not survive it.
        // The pair is parked for the same reason — it is live in Rust locals
        // across the same allocation, and the record that roots it is built on
        // the other side of it.
        let base = self.stack.len();
        self.stack.push(derived);
        self.stack.push(resolve);
        self.stack.push(reject);
        self.stack.push(on_fulfilled);
        self.stack.push(on_rejected);
        self.gc_protect();
        let installed = self.natives.promise_perform_then(
            self.heap,
            this_v,
            derived,
            resolve,
            reject,
            on_fulfilled,
            on_rejected,
        );
        self.stack.truncate(base);
        installed.map_err(|t| JSException::from_throw(self.heap, t))?;
        Ok(derived)
    }

    /// `Promise.prototype.catch(onRejected)` (spec 27.5.5.6): `then` with an
    /// absent fulfill handler, so it shares the species selection verbatim.
    fn run_promise_catch(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let on_rejected = args.first().copied().unwrap_or_else(JsValue::undefined);
        self.run_promise_then(this_v, &[JsValue::undefined(), on_rejected])
    }

    /// `Promise.prototype.finally(onFinally)` (spec 27.5.5.8), the
    /// interpreter seam.
    ///
    /// Steps 1-4 mirror [`Self::run_promise_then`]: the species selection and
    /// `NewPromiseCapability` happen here so the derived promise is the one the
    /// selected constructor built, and the pair is the derived promise's
    /// settlement route. Steps 5-13 are the engine's watcher.
    ///
    /// The callback's result is adopted by the engine's settlement job
    /// (27.5.5.8 steps 3-5), but the *watched* value is what the derived
    /// promise ends up with: a returned thenable is resolved first, so a
    /// rejecting one turns the outcome into a rejection, and a plain return
    /// value is discarded.
    fn run_promise_finally(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        if !self.is_promise(this_v) {
            return Err(JSException(self.error_value(
                "TypeError: Promise.prototype.finally requires a promise",
            )));
        }
        let on_finally = args.first().copied().unwrap_or_else(JsValue::undefined);
        let default_ctor = self.promise_ctor_value();
        let ctor = self.species_constructor(this_v, default_ctor)?;
        let (derived, resolve, reject) = self.new_promise_capability(ctor)?;
        let base = self.stack.len();
        self.stack.push(derived);
        self.stack.push(resolve);
        self.stack.push(reject);
        self.gc_protect();
        let installed = self
            .natives
            .promise_watch_finally(self.heap, this_v, derived, resolve, reject, on_finally);
        self.stack.truncate(base);
        installed.map_err(|t| JSException::from_throw(self.heap, t))?;
        Ok(derived)
    }

    /// `NewPromiseCapability(C)` (spec 27.5.1.5), as
    /// `(promise, resolve, reject)`.
    ///
    /// The spec's own constructor path, and the one that differs from 27.5.1.1
    /// in kind rather than in detail: the resolving pair is *not* built here.
    /// Step 3 makes a `PromiseCapability Record`, step 4 hands `C` a
    /// `GetCapabilitiesExecutor` builtin, step 6 is a `Construct` that runs `C`
    /// — so the pair ends up being whatever `C`'s own code passed in — and steps
    /// 7-8 read it back and check it.
    ///
    /// Step 6 is why this is an interpreter method and not a registry native:
    /// `Construct` on an arbitrary `C` is a machine operation, and it is
    /// synchronous user code (the spec has no job queue between steps 4 and 7).
    ///
    /// Steps 1, 6 and 7-8 are the whole reason this is not
    /// `Interp::promise_capability`: that one takes a ready-made pair and
    /// returns a freshly allocated promise, which is 27.5.1.1's shape. The
    /// combinators are the production callers — 27.5.4.1 step 6 and
    /// 27.5.4.2 step 3 are exactly this call.
    pub fn new_promise_capability(
        &mut self,
        ctor: JsValue,
    ) -> Result<(JsValue, JsValue, JsValue), JSException> {
        // Step 1: `If IsConstructor(C) is false, throw a TypeError exception`.
        // Ahead of everything else, so a non-constructor never reaches the
        // `Construct` below.
        if !self.is_capability_constructor(ctor) {
            return Err(JSException(self.error_value(
                "TypeError: promise constructor is not a constructor",
            )));
        }
        // Step 2: the `PromiseCapability Record` — a plain three-slot object
        // (`[[Resolve]]`, `[[Reject]]`, `[[AlreadyResolved]]`; slot names in
        // `v12_native::promise_capability`). The engine's `capability_executor`
        // records the pair into it and `capability_pair` reads it back, so it
        // is a real heap object rather than interpreter state.
        let fresh = JsValue::from_i32_smi(v12_native::promise_capability::CAPABILITY_FRESH)
            .expect("the fresh marker fits Smi");
        let state = self.heap.alloc(JsObject {
            properties: smallvec::smallvec![JsValue::undefined(), JsValue::undefined(), fresh],
            property_keys: smallvec::smallvec![None; 3],
            ..JsObject::default()
        });
        self.heap.add_pinned_root(state);
        // Step 4-5: the `GetCapabilitiesExecutor` builtin. It is a host closure,
        // so calling it needs no machine and needs no job.
        let executor = self
            .natives
            .capability_executor(self.heap, JsValue::object(state))
            .map_err(|t| JSException::from_throw(self.heap, t))?;
        // Step 6: `Construct(C, « executor »)` — the constructor's own code runs
        // here, synchronously.
        //
        // The record and the executor live in Rust locals across that call,
        // which `gc_protect` cannot see: it republishes the value stack as the
        // root set, so park both for its duration (the `run_promise_construct`
        // pattern). This is belt to the registry's braces rather than the load-
        // bearing root — `capability_executor`'s contract has it pinning the
        // record for the *rest of the run*, which is the window this park does
        // not cover — but the interpreter should not have to trust an
        // implementation to have read that contract for the window it can see
        // itself. `new_target` is `C` itself, per the spec's single argument.
        let base = self.stack.len();
        self.stack.push(JsValue::object(state));
        self.stack.push(executor);
        self.gc_protect();
        let ctor_obj = ctor.as_object().expect("IsConstructor implies an object");
        let constructed = self.construct_object(ctor_obj, &[executor], ctor);
        self.stack.truncate(base);
        let promise = constructed?;
        // Step 7: read the recorded pair back.
        let (resolve, reject) = self
            .natives
            .capability_pair(self.heap, JsValue::object(state))
            .map_err(|t| JSException::from_throw(self.heap, t))?;
        // Step 8: `If IsCallable(resolve) is false, throw a TypeError` (and the
        // same for `reject`). A constructor that ignored its executor, or passed
        // something uncallable, lands here — 27.5.1.1 has no equivalent check,
        // which is what makes `new Promise(function () {})` legal there.
        self.callable_or_err(resolve, "Promise resolve function is not callable")?;
        self.callable_or_err(reject, "Promise reject function is not callable")?;
        Ok((promise, resolve, reject))
    }

    /// Allocates the pending promise an adoption settles and queues the
    /// adoption. `prototype` comes from the receiver's `prototype` field, so
    /// the result is an ordinary `%Promise.prototype%` instance.
    ///
    /// A receiver that is not an object is the unbound case: the combinators
    /// `Call(promiseResolve, undefined, « nextPromise »)` (spec 27.2.4.1
    /// step 6-g), so there is no `prototype` to read and the result is a
    /// prototype-less promise — the same degradation the engine handler
    /// documents for a destructured `Promise.resolve`.
    fn adopt_thenable(
        &mut self,
        this_v: JsValue,
        thenable: JsValue,
        then_v: JsValue,
        abrupt: bool,
    ) -> Result<JsValue, JSException> {
        let prototype = if this_v.as_object().is_some() {
            let proto_key = self.new_temp_key("prototype");
            self.get_property(0, 0, this_v, proto_key)?.as_object()
        } else {
            None
        };
        self.gc_protect();
        let promise = self.alloc_pending_promise(prototype);
        self.push_pending_adoption(promise, thenable, then_v, abrupt);
        Ok(JsValue::object(promise))
    }

    /// `Map.prototype.forEach` / `Set.prototype.forEach` — re-entrant
    /// callback over live entries. Tombstones are skipped; an append is
    /// observed once the current index reaches it.
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
        let step = if want_map { 2 } else { 1 };
        let mut index = 0usize;
        loop {
            let call_args = {
                let elements = &self.heap.get(obj).elements;
                if index * step >= elements.len() {
                    break;
                }
                if elements[index * step].is_hole() {
                    index += 1;
                    continue;
                }
                if want_map {
                    [elements[index * step + 1], elements[index * step], this_v]
                } else {
                    [elements[index], elements[index], this_v]
                }
            };
            let r = self.call_value(JsValue::object(cb), this_arg, &call_args)?;
            self.heap.add_root(r);
            index += 1;
        }
        Ok(JsValue::undefined())
    }

    /// `Iterator.prototype` helper methods (ES2025, spec 27.1.4-27.1.5).
    ///
    /// `map`/`filter`/`flatMap` return a **lazy** iterator: the callback is
    /// stored, never called, and the wrapper's `next` (intercepted below as
    /// `NativeId::IteratorNext`) runs it one value at a time.
    /// `reduce`/`forEach`/`some`/`every`/`find` consume the source and return
    /// a value, so they drain eagerly with an early exit.
    ///
    /// The two halves delegate where they can: building a wrapper is pure
    /// allocation and goes to the engine's `iterator_map`/`filter`/`flatmap`
    /// through the registry seam, which is where the wrapper's element layout
    /// lives; `take`/`drop` do the same, handing over the resolved `next`
    /// method so the engine knows whether *it* can step the wrapper or the
    /// machine has to.
    fn run_iterator_callback(
        &mut self,
        id: NativeId,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        // Spec 27.1.4.5 steps 1-5, in this order — the order is observable:
        // `IsCallable(mapper)` is checked *before* `GetIteratorDirect`, so a
        // non-callable callback throws without ever reading `next`
        // (`argument-effect-order`), and it closes the receiver on the way out
        // (`argument-validation-failure-closes-underlying`).
        if this_v.as_object().is_none() {
            return Err(JSException(self.error_value(
                "TypeError: Iterator.prototype method called on a non-object",
            )));
        }
        let callback = match args.first().copied().filter(|v| {
            v.as_object()
                .is_some_and(|h| matches!(self.heap.get(h).kind, Kind::Function | Kind::Proxy))
        }) {
            Some(v) => v,
            None => {
                // Step 4.b: `IteratorClose(O, ThrowCompletion(TypeError))` —
                // the original abrupt wins, so close errors are swallowed.
                let _ = self.op_iterator_close(this_v, true);
                return Err(JSException(self.error_value(
                    "TypeError: Iterator.prototype callback is not a function",
                )));
            }
        };
        // Step 5: `GetIteratorDirect(O, sync)`.
        let source = self.helper_source_open(this_v)?;
        // Everything the helper carries across a call has to live in a stack
        // slot: `gc_protect` clears the root set and re-publishes the value
        // stack, so an `add_root`-ed value does not survive it (the same
        // discipline `op_iterator_next` uses). Park for the whole helper and
        // release on every path.
        let base = self.park(&[source.next, source.iter, callback]);
        let out = match id {
            NativeId::IteratorMap | NativeId::IteratorFilter | NativeId::IteratorFlatMap => {
                self.build_helper_wrapper(id, source, callback)
            }
            _ => self.drain_helper(id, source, callback, args),
        };
        self.unpark(base);
        out
    }

    /// Copies `values` into fresh value-stack slots and re-publishes the stack
    /// as the GC root set. The returned depth is what [`Self::unpark`] takes.
    fn park(&mut self, values: &[JsValue]) -> usize {
        let base = self.stack.len();
        self.stack.extend_from_slice(values);
        self.gc_protect();
        base
    }

    /// Releases slots opened by [`Self::park`]. Every call between the two must
    /// be balanced on its own `Ok` and `Err` path (as `op_iterator_next` is), so
    /// truncating back to `base` cannot cut into a live frame.
    fn unpark(&mut self, base: usize) {
        self.stack.truncate(base);
    }

    /// `take`/`drop`/`toArray` (spec 27.1.4.3-27.1.4.5, 27.1.4.9).
    ///
    /// All three need the spec brand check, which lives here because it is a
    /// `GetMethod(O, "next")` — user code on a user object. The construction
    /// and the machine-free drain are the engine's; the drain is taken over
    /// only when the source cannot be stepped without the machine.
    fn run_iterator_passthrough(
        &mut self,
        id: NativeId,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let source = self.helper_source_open(this_v)?;
        if id == NativeId::IteratorToArray && self.helper_engine_can_drive(&source) {
            // A plain element-store iterator: the engine's own `drain_iterator`
            // reads it positionally and needs no machine.
            return self
                .natives
                .call_native(self.heap, this_v, &[], NativeId::IteratorToArray)
                .map_err(|t| JSException::from_throw(self.heap, t));
        }
        if id == NativeId::IteratorToArray {
            let base = self.park(&[source.next, source.iter]);
            let out = self.drain_to_array(source);
            self.unpark(base);
            return out;
        }
        let source_next = self.helper_source_next_for_engine(&source);
        let limit = args.first().copied().unwrap_or(JsValue::undefined());
        // Park across the engine builder: it allocates, and `gc_protect` would
        // drop an `add_root`-ed `next`/`limit` on the floor.
        let base = self.park(&[this_v, source_next, limit]);
        let out = self
            .natives
            .call_native(self.heap, this_v, &[limit, source_next], id)
            .map_err(|t| JSException::from_throw(self.heap, t));
        self.unpark(base);
        out
    }

    /// `next` on one of our lazy helper wrappers, for the wrappers the engine
    /// cannot step: anything whose source is a generator, a plain object, or
    /// itself a wrapper. Returns `None` for every other receiver so the caller
    /// falls through to the registry seam and the engine's own `iterator_next`
    /// (element-store iterators and the machine-free `take`/`drop` wrappers)
    /// stays authoritative.
    fn run_iterator_next_native(
        &mut self,
        this_v: JsValue,
    ) -> Option<Result<JsValue, JSException>> {
        let obj = this_v.as_object()?;
        if self.heap.get(obj).kind != Kind::Iterator {
            return None;
        }
        let kind = self.wrapper_kind(obj)?;
        // An element-store iterator is the engine's own, always.
        if kind >= 0 && kind <= helper_layout::ENGINE_KIND_MAX {
            return None;
        }
        // `source_next` is `undefined` exactly when the whole chain is
        // machine-free, which is the case the engine handles.
        let source_next = self.wrapper_slot(obj, helper_layout::SLOT_SOURCE_NEXT);
        if source_next.as_object().is_none() {
            return None;
        }
        Some(self.step_helper_wrapper(obj, kind))
    }

    /// Builds the lazy `map`/`filter`/`flatMap` wrapper through the engine's
    /// builder, which owns the wrapper's element layout.
    fn build_helper_wrapper(
        &mut self,
        id: NativeId,
        source: HelperSource,
        callback: JsValue,
    ) -> Result<JsValue, JSException> {
        // A callback wrapper ALWAYS needs the machine, even over a
        // machine-free source: running the stored callback is the whole point
        // of its `next`. So the real `next` method always goes in, and the
        // interpreter's `IteratorNext` arm always drives the wrapper.
        self.natives
            .call_native(self.heap, source.iter, &[callback, source.next], id)
            .map_err(|t| JSException::from_throw(self.heap, t))
    }

    /// One `next` of a lazy wrapper, driven from the machine. The control flow
    /// comes from the engine's `wrapper_step` mirror below, which is the same
    /// state machine `v12_engine::builtins::iterator::helper_wrapper_next`
    /// runs for the machine-free case — one decision, two executors.
    fn step_helper_wrapper(
        &mut self,
        obj: Handle<JsObject>,
        kind: i32,
    ) -> Result<JsValue, JSException> {
        let limit = if helper_layout::LIMIT_KINDS.contains(&kind) {
            self.wrapper_limit(obj)
        } else {
            0
        };
        let mut source = HelperSource {
            iter: self.wrapper_slot(obj, helper_layout::SLOT_SOURCE),
            next: self.wrapper_slot(obj, helper_layout::SLOT_SOURCE_NEXT),
        };
        let mut count = self.wrapper_count(obj);
        // `filter` loops rather than recursing when the predicate rejects a
        // value: the spec's Iterator Helper Closure pulls again, and a
        // Rust frame per rejection would overflow the stack on a long run.
        'pull: loop {
            loop {
                match helper_layout::wrapper_step(kind, count, limit) {
                    helper_layout::WrapperStep::Done => return Ok(self.done_result()),
                    helper_layout::WrapperStep::Skip(0) => break,
                    helper_layout::WrapperStep::Skip(n) => {
                        for _ in 0..n {
                            if self.helper_source_step(&mut source)?.is_none() {
                                return Ok(self.done_result());
                            }
                            count += 1;
                        }
                        self.set_wrapper_count(obj, helper_layout::SLOT_INDEX, count);
                    }
                    helper_layout::WrapperStep::Emit => break,
                }
            }
            // `flatMap` first: while a stashed array has elements left, one `next`
            // yields one of them without touching the source at all.
            if kind == helper_layout::ITER_KIND_HELPER_FLATMAP {
                let pending = self.wrapper_slot(obj, helper_layout::SLOT_PENDING);
                if let Some(h) = pending.as_object() {
                    let elements = self.heap.get(h).elements_snapshot();
                    let cursor = self
                        .wrapper_slot(obj, helper_layout::SLOT_PENDING_INDEX)
                        .as_smi()
                        .and_then(|n| usize::try_from(n).ok())
                        .unwrap_or(0);
                    if let Some(&value) = elements.get(cursor) {
                        self.set_wrapper_count(obj, helper_layout::SLOT_PENDING_INDEX, cursor + 1);
                        return Ok(self.make_iterator_result(value, false));
                    }
                    // Drained: clear it and pull the next source value in this
                    // same frame rather than recursing.
                    self.set_wrapper_slot(obj, helper_layout::SLOT_PENDING, JsValue::undefined());
                    count = self.wrapper_count(obj);
                    continue 'pull;
                }
            }
            let Some(value) = self.helper_source_step(&mut source)? else {
                return Ok(self.done_result());
            };
            let emitted = if helper_layout::LIMIT_KINDS.contains(&kind) {
                if kind == helper_layout::ITER_KIND_HELPER_TAKE {
                    count += 1;
                }
                self.set_wrapper_count(obj, helper_layout::SLOT_INDEX, count);
                value
            } else {
                // `map`/`filter`/`flatMap` count *pulled* values, so the counter
                // advances once per source value whether or not it is kept.
                self.set_wrapper_count(obj, helper_layout::SLOT_INDEX, count + 1);
                let callback = self.wrapper_slot(obj, helper_layout::SLOT_ARG);
                self.heap.add_root(callback);
                let counter = JsValue::from_f64(count as f64);
                self.heap.add_root(counter);
                // Spec 27.1.4.5 step 5.b.iv: `Call(mapper, undefined, « value,
                // 𝔽(counter) »)` — two arguments and no thisArgument.
                let mapped = self
                    .call_value(callback, JsValue::undefined(), &[value, counter])
                    .map_err(|e| {
                        let _ = self.op_iterator_close(source.iter, true);
                        e
                    })?;
                self.heap.add_root(mapped);
                match kind {
                    helper_layout::ITER_KIND_HELPER_FILTER => {
                        if !ops::to_boolean(self.heap, mapped) {
                            // Rejected: pull the next candidate in this same frame.
                            count = self.wrapper_count(obj);
                            continue 'pull;
                        }
                        // `filter` yields the *source* value, not the predicate's.
                        value
                    }
                    helper_layout::ITER_KIND_HELPER_FLATMAP => {
                        // Spec 27.1.4.5 step 5.b.vii-ix: an array result is
                        // walked one element per `next`, so `flatMap` over a
                        // long array is as lazy as `map`. Stash it with a
                        // cursor and loop back; a non-array result is appended
                        // whole, which is the only other case
                        // `GetIteratorFlattenable` unwraps.
                        if self.value_is_array(mapped) {
                            self.set_wrapper_slot(obj, helper_layout::SLOT_PENDING, mapped);
                            self.set_wrapper_count(obj, helper_layout::SLOT_PENDING_INDEX, 0);
                            count = self.wrapper_count(obj);
                            continue 'pull;
                        }
                        mapped
                    }
                    // `map` passes the callback's result through unchanged.
                    helper_layout::ITER_KIND_HELPER_MAP => mapped,
                    _ => mapped,
                }
            };
            return Ok(self.make_iterator_result(emitted, false));
        }
    }

    /// Spec 27.1.4.5's flatten step: one level of an Array result, anything
    /// else appended whole (`GetIteratorFlattenable` on an array fast path).
    /// True when `value` is a JS Array — the one case spec 27.1.4.5's
    /// `GetIteratorFlattenable` unwraps.
    fn value_is_array(&self, value: JsValue) -> bool {
        value
            .as_object()
            .is_some_and(|h| self.heap.get(h).kind == Kind::Array)
    }

    /// `reduce`/`forEach`/`some`/`every`/`find`: consume the source and return
    /// a value, short-circuiting as the spec's algorithms do.
    fn drain_helper(
        &mut self,
        id: NativeId,
        mut source: HelperSource,
        callback: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let acc_slot = self.stack.len();
        self.stack.push(JsValue::undefined());
        self.gc_protect();
        // Spec 27.1.4.9 step 6 asks whether `initialValue` is *present*, not
        // whether it is `undefined`: `reduce(fn, undefined)` seeds with
        // `undefined`. `args` is the exact call window, so its length is the
        // arity.
        let mut acc_is_unset = true;
        let initial = if id == NativeId::IteratorReduce && args.len() >= 2 {
            acc_is_unset = false;
            Some(args[1])
        } else {
            None
        };
        if let Some(v) = initial {
            self.stack[acc_slot] = v;
            acc_is_unset = false;
            self.gc_protect();
        }
        let mut count = 0usize;
        while let Some(value) = self.helper_source_step(&mut source)? {
            let index = JsValue::from_f64(count as f64);
            let result = if id == NativeId::IteratorReduce {
                let memo = self.stack[acc_slot];
                if memo.is_undefined() && acc_is_unset {
                    // No initial value: the first value *is* the seed, and the
                    // callback is not called for it. `undefined` is a legal
                    // initial value, so the "unset" flag — not the slot — is
                    // what distinguishes the two.
                    self.stack[acc_slot] = value;
                    self.gc_protect();
                    acc_is_unset = false;
                    count += 1;
                    continue;
                }
                // Spec 27.1.4.9 step 4.d: `Call(reducer, undefined, « acc,
                // value, 𝔽(counter) »)`.
                let r = self.call_value(callback, JsValue::undefined(), &[memo, value, index]);
                let r = match r {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = self.op_iterator_close(source.iter, true);
                        return Err(e);
                    }
                };
                self.stack[acc_slot] = r;
                self.gc_protect();
                count += 1;
                continue;
            } else {
                // Spec 27.1.4.7/27.1.4.11-27.1.4.13: `Call(fn, undefined, «
                // value, 𝔽(counter) »)`.
                match self.call_value(callback, JsValue::undefined(), &[value, index]) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = self.op_iterator_close(source.iter, true);
                        return Err(e);
                    }
                }
            };
            match id {
                NativeId::IteratorForEach => {}
                NativeId::IteratorSome => {
                    if ops::to_boolean(self.heap, result) {
                        self.op_iterator_close(source.iter, false)?;
                        return Ok(JsValue::from_bool(true));
                    }
                }
                NativeId::IteratorEvery => {
                    if !ops::to_boolean(self.heap, result) {
                        self.op_iterator_close(source.iter, false)?;
                        return Ok(JsValue::from_bool(false));
                    }
                }
                NativeId::IteratorFind => {
                    if ops::to_boolean(self.heap, result) {
                        self.op_iterator_close(source.iter, false)?;
                        return Ok(value);
                    }
                }
                _ => {}
            }
            count += 1;
        }
        let out = match id {
            NativeId::IteratorForEach => JsValue::undefined(),
            NativeId::IteratorSome => JsValue::from_bool(false),
            NativeId::IteratorEvery => JsValue::from_bool(true),
            NativeId::IteratorFind => JsValue::undefined(),
            // Spec 27.1.4.9: an empty source with no initial value is a
            // TypeError. A supplied `undefined` initial value is NOT that case,
            // which is what `acc_is_unset` (not the slot) decides.
            _ if acc_is_unset => {
                return Err(JSException(self.error_value(
                    "TypeError: Reduce of empty iterator with no initial value",
                )));
            }
            _ => self.stack[acc_slot],
        };
        self.stack.truncate(acc_slot);
        Ok(out)
    }

    /// `Iterator.prototype.toArray()` (spec 27.1.4.9) over a source the engine
    /// cannot step: the drain runs here so each `next` is a real call. Bounded
    /// like the engine's, so a runaway iterator is a `RangeError` rather than
    /// an out-of-memory abort.
    fn drain_to_array(&mut self, mut source: HelperSource) -> Result<JsValue, JSException> {
        const MAX_DRAIN: usize = 1_000_000;
        let mut out: Vec<JsValue> = Vec::new();
        while let Some(value) = self.helper_source_step(&mut source)? {
            self.heap.add_root(value);
            out.push(value);
            if out.len() > MAX_DRAIN {
                return Err(JSException(self.error_value(
                    "RangeError: iterator drain exceeds 1,000,000 values",
                )));
            }
        }
        Ok(crate::call::alloc_array(self, out))
    }

    /// ES `GetIteratorDirect` for the helpers: spec 27.1.4 steps 1-3 require an
    /// Object receiver and a callable `next`. The method is resolved **once**
    /// and carried on the record, so a `next` getter runs a single time
    /// (spec 7.4.3 step 3, and what the `get-next-method-only-once` tests
    /// pin) and later mutations of the property are not observed.
    fn helper_source_open(&mut self, this_v: JsValue) -> Result<HelperSource, JSException> {
        if this_v.as_object().is_none() {
            return Err(JSException(self.error_value(
                "TypeError: Iterator.prototype method called on a non-object",
            )));
        }
        let next_key = self.new_temp_key("next");
        let next_v = self.get_property(0, 0, this_v, next_key)?;
        let next =
            next_v
                .as_object()
                .filter(|h| matches!(self.heap.get(*h).kind, Kind::Function | Kind::Proxy))
                .ok_or_else(|| {
                    JSException(self.error_value(
                        "TypeError: Iterator.prototype method requires a callable next",
                    ))
                })?;
        self.heap.add_root(JsValue::object(next));
        self.heap.add_root(this_v);
        Ok(HelperSource {
            iter: this_v,
            next: JsValue::object(next),
        })
    }

    /// One `IteratorStepValue` over a helper source: call the cached `next`,
    /// require an Object result (spec 7.4.2 step 3), then read `done` and only
    /// then `value` — so a throwing `value` getter on a `done` result is never
    /// observed.
    fn helper_source_step(
        &mut self,
        source: &mut HelperSource,
    ) -> Result<Option<JsValue>, JSException> {
        // Same discipline as `op_iterator_next`: park the callee and the
        // receiver on the value stack so the `gc_protect` below keeps them
        // alive, and pop on both the `Ok` and the `Err` path.
        let base = self.park(&[source.next, source.iter]);
        let stepped = (|| {
            let result = self.call_value(source.next, source.iter, &[])?;
            if result.as_object().is_none() {
                return Err(JSException(
                    self.error_value("TypeError: iterator next() returned a non-object"),
                ));
            }
            self.heap.add_root(result);
            let done_key = self.new_temp_key("done");
            let done = self.get_property(0, 0, result, done_key)?;
            if ops::to_boolean(self.heap, done) {
                return Ok(None);
            }
            let value_key = self.new_temp_key("value");
            let value = self.get_property(0, 0, result, value_key)?;
            Ok(Some(value))
        })();
        self.unpark(base);
        stepped
    }

    /// The `next` method to hand the engine builder: `undefined` when the
    /// engine can step the resulting wrapper on its own (a plain element-store
    /// iterator with a machine-free chain), the resolved method otherwise.
    fn helper_source_next_for_engine(&mut self, source: &HelperSource) -> JsValue {
        if self.helper_engine_can_drive(source) {
            JsValue::undefined()
        } else {
            source.next
        }
    }

    /// True when the engine's `iterator_next` can step this source without the
    /// machine: a `Kind::Iterator` whose kind is an element-store kind, or one
    /// of our wrappers that is itself already machine-free.
    fn helper_engine_can_drive(&mut self, source: &HelperSource) -> bool {
        let Some(obj) = source.iter.as_object() else {
            return false;
        };
        if self.heap.get(obj).kind != Kind::Iterator {
            return false;
        }
        let Some(kind) = self.wrapper_kind(obj) else {
            return false;
        };
        if helper_layout::ENGINE_KINDS.contains(&kind) {
            return true;
        }
        // A wrapper is machine-free exactly when it was handed `undefined` as
        // its source's `next` method.
        self.wrapper_slot(obj, helper_layout::SLOT_SOURCE_NEXT)
            .as_object()
            .is_none()
    }

    /// `{value: undefined, done: true}`.
    fn done_result(&mut self) -> JsValue {
        self.make_iterator_result(JsValue::undefined(), true)
    }

    // -- wrapper element access (layout mirror, see `helper_layout`) ---------

    fn wrapper_kind(&mut self, obj: Handle<JsObject>) -> Option<i32> {
        self.wrapper_slot(obj, helper_layout::SLOT_KIND)
            .as_smi()
            .and_then(|k| i32::try_from(k).ok())
    }

    fn wrapper_slot(&mut self, obj: Handle<JsObject>, slot: usize) -> JsValue {
        self.heap
            .get(obj)
            .elements
            .get(slot)
            .copied()
            .unwrap_or(JsValue::undefined())
    }

    /// The counter at [`helper_layout::SLOT_INDEX`]: values emitted, discarded
    /// or pulled, per wrapper kind.
    fn wrapper_count(&mut self, obj: Handle<JsObject>) -> usize {
        self.wrapper_slot(obj, helper_layout::SLOT_INDEX)
            .as_smi()
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0)
    }

    fn set_wrapper_slot(&mut self, obj: Handle<JsObject>, slot: usize, value: JsValue) {
        if let Some(cell) = self.heap.get_mut(obj).elements.get_mut(slot) {
            *cell = value;
        }
    }

    fn set_wrapper_count(&mut self, obj: Handle<JsObject>, slot: usize, count: usize) {
        self.set_wrapper_slot(
            obj,
            slot,
            JsValue::from_i32_smi(count as i32).unwrap_or(JsValue::undefined()),
        );
    }

    fn wrapper_limit(&mut self, obj: Handle<JsObject>) -> usize {
        let arg = self.wrapper_slot(obj, helper_layout::SLOT_ARG);
        let n = crate::ops::to_number(self.heap, arg);
        if !n.is_finite() || n <= 0.0 {
            return 0;
        }
        n.trunc().max(0.0) as usize
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
                    let r = self.call_value(JsValue::object(cb), JsValue::undefined(), &a)?;
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

// ---------------------------------------------------------------------------
// `AggregateError` (27.5.5.1)
// ---------------------------------------------------------------------------

impl Interp<'_> {
    /// `AggregateError ( errors [ , message ] [ , options ] )`.
    ///
    /// Split across the two seams, exactly like the `Promise` combinators:
    /// `ToString(message)` and `IteratorToList(errors)` both run user code, so
    /// they happen here; the instance itself is a pure `&mut Heap`
    /// allocation and stays in the registry native
    /// ([`NativeId::AggregateErrorCreate`]), which receives the drained array.
    ///
    /// The engine's `drain_iterator` was not an option: it drives
    /// `Kind::Iterator` objects (the array/map/set natives) through their
    /// `elements` state, so it cannot see a generator's iterator, a string, or
    /// a plain object with a `@@iterator` method — which is most of what
    /// `IteratorToList` has to accept.
    fn run_aggregate_error(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let base = self.stack.len();
        // Step 4: `message = ? ToString(message)` when it is not `undefined`,
        // and *before* the iterator is touched —
        // `built-ins/AggregateError/order-of-args-evaluation.js` pins that
        // superclass-then-subclass order.
        let message = match args.get(1).copied() {
            Some(v) if !v.is_undefined() => {
                let s = self.to_string_value(v)?;
                self.stack.push(s);
                s
            }
            _ => JsValue::undefined(),
        };
        // Step 3: `errors = ? IteratorToList(errors)`.
        let iterable = args.first().copied().unwrap_or(JsValue::undefined());
        self.stack.push(iterable);
        let list = self.iterator_to_list(iterable)?;
        // Step 4 proper: `CreateArrayFromList`.
        self.gc_protect();
        let errors = crate::call::alloc_array(self, list);
        self.heap.add_root(errors);
        let result = self
            .natives
            .call_native(
                self.heap,
                this_v,
                &[errors, message],
                NativeId::AggregateErrorCreate,
            )
            .map_err(|t| JSException::from_throw(self.heap, t));
        self.stack.truncate(base);
        result
    }

    /// `IteratorToList(iterable)` (spec 7.4.23) — the drain half of
    /// 27.5.5.1 step 3, over [`Self::get_iterator`] and
    /// [`Self::op_iterator_next`] so every `GetMethod`/`IsCallable` gate and
    /// every user `next` runs the ordinary machine path.
    ///
    /// No `IteratorClose` on an abrupt: `IteratorToList` has no close step
    /// (unlike the `Promise` combinators' element loop).
    fn iterator_to_list(&mut self, iterable: JsValue) -> Result<Vec<JsValue>, JSException> {
        let iterator = self.get_iterator(iterable, false)?;
        let base = self.stack.len();
        // Parked for the whole loop: `gc_protect` republishes the stack as the
        // root set, and every `next()` call can collect.
        self.stack.push(iterator);
        let mut out = Vec::new();
        let outcome = loop {
            let result = match self.op_iterator_next(iterator) {
                Ok(result) => result,
                Err(JSException(thrown)) => break Err(thrown),
            };
            let done_key = self.new_temp_key("done");
            let done = match self.get_property(0, 0, result, done_key) {
                Ok(done) => done,
                Err(JSException(thrown)) => break Err(thrown),
            };
            if ops::to_boolean(self.heap, done) {
                break Ok(out);
            }
            let value_key = self.new_temp_key("value");
            match self.get_property(0, 0, result, value_key) {
                Ok(value) => out.push(value),
                Err(JSException(thrown)) => break Err(thrown),
            }
        };
        self.stack.truncate(base);
        outcome.map_err(JSException)
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
