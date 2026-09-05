//! Generator and async-function runtime: generator object creation, resume
//! protocol (`next`/`return`/`throw`), promise unwrapping for `await`, and the
//! parked-frame resume paths.

use v12_heap::{Handle, JsObject, JsValue, Kind, PropKey};

use super::{Frame, Interp, JSException, MAX_RESUME_DEPTH};
use crate::ops;

impl Interp<'_> {
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

    pub(crate) fn is_async_fn(&self, fn_idx: u32) -> bool {
        self.is_async_fn_for(fn_idx, self.program_id)
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
        captured_env: Option<Handle<JsObject>>,
        this_v: JsValue,
        callee_slot: usize,
        argc: u16,
    ) -> Result<Handle<JsObject>, JSException> {
        let (max_regs, has_rest, fixed, rest_reg) = {
            let f = &self.functions[fn_idx as usize];
            (f.max_regs, f.has_rest, f.fixed_params, f.rest_reg)
        };
        // Build initial register window snapshot via shared helper (DRY #4).
        let mut window = vec![JsValue::undefined(); usize::from(max_regs)];
        window[0] = this_v;
        let arg_src = callee_slot + 2;
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
        let r#gen = self.heap.alloc(JsObject::generator_with(
            fn_idx,
            0,
            0.0,
            0,
            window,
            captured_env,
            None,
        ));
        self.heap.add_root(JsValue::object(r#gen));
        Ok(r#gen)
    }

    pub(crate) fn generator_next(&mut self, this_v: JsValue, arg: JsValue) -> Result<JsValue, JSException> {
        let Some(r#gen) = this_v.as_object() else {
            return Err(JSException(
                self.error_value("TypeError: generator next called on non-object"),
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
            .and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64()))
            .unwrap_or(0.0)
            == 1.0;
        if done {
            return Ok(self.make_iterator_result(JsValue::undefined(), true));
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

    pub(crate) fn generator_return(&mut self, this_v: JsValue, arg: JsValue) -> Result<JsValue, JSException> {
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
            return Ok(self.make_iterator_result(arg, true));
        }
        // Mark done and unwind any suspended frame.
        if self.heap.get(r#gen).properties.len() >= 3 {
            self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
        }
        self.heap.get_mut(r#gen).elements.clear();
        self.gc_protect();
        Ok(self.make_iterator_result(arg, true))
    }

    pub(crate) fn generator_throw(&mut self, this_v: JsValue, arg: JsValue) -> Result<JsValue, JSException> {
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
            return Err(JSException(arg));
        }
        if self.heap.get(r#gen).properties.len() >= 3 {
            self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
        }
        self.heap.get_mut(r#gen).elements.clear();
        Err(JSException(arg))
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
                // Pending promise: for task 6, treat as fulfilled with undefined payload (no thenable unwrapping)
                return (v, false, JsValue::undefined());
            }
        }
        // Create fulfilled promise for non-promise arg
        self.gc_protect();
        let reactions = self.heap.alloc(JsObject::array(Vec::new()));
        self.heap.add_root(JsValue::object(reactions));
        let promise = self.heap.alloc(JsObject::fulfilled_promise(v, reactions));
        self.heap.add_root(JsValue::object(promise));
        (JsValue::object(promise), false, v)
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

    pub(crate) fn resume_async(&mut self, r#gen: Handle<JsObject>, value: JsValue) -> Result<(), JSException> {
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
        if (yield_dst as usize) < usize::from(f_max_regs) {
            self.stack[new_base + usize::from(yield_dst)] = value;
        }
        self.frames.push(Frame {
            fn_idx,
            program: gen_program,
            pc: resume_pc,
            base: new_base,
            max_regs: f_max_regs,
            env,
            generator: Some(r#gen),
            yield_dst: None,
            new_target: None,
        });
        self.top_result = None;
        let frames_before = self.frames.len();
        let exec_res = if is_throw {
            // Inject the exception through the normal unwind path first.
            self.unwind(value)?;
            // The nested run must stop at this frame's boundary: a throwing
            // generator body unwinds only the generator frame, leaving the
            // caller's frames intact for `generator_next`'s caller (the
            // for-of/await dispatch arm, which resumes its own dispatch).
            self.stop_at_frames = Some(self.frames.len() - 1);
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
                    Ok(None)
                } else {
                    let ret = self.top_result.take().unwrap_or(JsValue::undefined());
                    if done_val != 1.0 && self.heap.get(r#gen).properties.len() >= 3 {
                        self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
                    }
                    Ok(Some(ret))
                }
            }
            Err(e) => {
                // Pop the generator frame if still there, mark done.
                if self.frames.len() >= frames_before {
                    self.frames.pop();
                    self.stack.truncate(new_base);
                }
                if self.heap.get(r#gen).properties.len() >= 3 {
                    self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
                }
                Err(e)
            }
        }
    }
}
