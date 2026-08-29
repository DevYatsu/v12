use v12_heap::{Handle, JsObject, JsValue};

use crate::{ops, Frame, Interp, JSException, KIND_GENERATOR};

pub trait Suspendable {
    fn suspend(&mut self, dst: u16, val: JsValue) -> Result<Handle<JsObject>, JSException>;
    fn resume(&mut self, r#gen: Handle<JsObject>, arg: JsValue) -> Result<JsValue, JSException>;
}

impl Suspendable for Interp {
    fn suspend(&mut self, dst: u16, val: JsValue) -> Result<Handle<JsObject>, JSException> {
        // Snapshot current register window, capture resume pc and env, store into generator.
        let frame = self.frames.last().expect("suspend requires a frame");
        let base = frame.base;
        let max_regs = frame.max_regs;
        let pc = frame.pc;
        let r#gen = frame.generator.expect("suspend requires generator");
        let env = frame.env;
        // Compute width = 1 for now (narrow); caller has already accounted? Use pc+1 as resume.
        // We capture pc+1 assuming op_width=1; await/yield are narrow. Wide suspend not emitted.
        let resume_pc = pc + 1;
        // Snapshot stack[base..base+max_regs]
        let snapshot = {
            let end = base + usize::from(max_regs);
            if end <= self.stack.len() {
                self.stack[base..end].to_vec()
            } else if base <= self.stack.len() {
                let mut v = self.stack[base..].to_vec();
                v.resize(usize::from(max_regs), JsValue::undefined());
                v
            } else {
                vec![JsValue::undefined(); usize::from(max_regs) as usize]
            }
        };
        self.heap.get_mut(r#gen).properties[1] = ops::box_number(resume_pc as f64);
        if self.heap.get(r#gen).properties.len() < 4 {
            self.heap.get_mut(r#gen).properties.resize(4, JsValue::undefined());
        }
        self.heap.get_mut(r#gen).properties[3] = ops::box_number(f64::from(dst));
        self.heap.get_mut(r#gen).properties[2] = ops::box_number(2.0);
        self.heap.get_mut(r#gen).elements = snapshot;
        self.heap.get_mut(r#gen).prototype = env;
        let finished_base = self.frames.pop().expect("pop").base;
        self.stack.truncate(finished_base);
        // Store yielded value leaves handling to caller via top_result; we just ensure handle returned.
        let _ = val;
        Ok(r#gen)
    }

    fn resume(&mut self, r#gen: Handle<JsObject>, arg: JsValue) -> Result<JsValue, JSException> {
        // Mirrors generator_next restore path to satisfy trait contract for T4 delegation.
        if self.heap.get(r#gen).kind != KIND_GENERATOR {
            return Err(JSException(self.error_value("TypeError: not a generator")));
        }
        let done = self.heap.get(r#gen).properties.get(2).and_then(|v| v.as_f64().or(v.as_smi().map(|n| n as f64))).unwrap_or(0.0) == 1.0;
        if done {
            return Ok(self.make_iterator_result(JsValue::undefined(), true));
        }
        let fn_idx = self.heap.get(r#gen).properties.first().and_then(|v| v.as_smi().map(|n| n as u32 as f64).or(v.as_f64())).unwrap_or(0.0) as u32;
        let resume_pc = self.heap.get(r#gen).properties.get(1).and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64())).unwrap_or(0.0) as usize;
        let snapshot = self.heap.get(r#gen).elements.clone();
        let env = self.heap.get(r#gen).prototype;
        let f_max_regs = self.functions[fn_idx as usize].max_regs;
        let new_base = self.stack.len();
        let window_end = new_base + usize::from(f_max_regs);
        self.stack.resize(window_end, JsValue::undefined());
        let copy_len = snapshot.len().min(usize::from(f_max_regs));
        for i in 0..copy_len {
            self.stack[new_base + i] = snapshot[i];
        }
        if resume_pc != 0 {
            let yield_dst = self.heap.get(r#gen).properties.get(3).and_then(|v| v.as_smi().map(|n| n as f64).or(v.as_f64())).unwrap_or(0.0) as u16;
            if (yield_dst as usize) < usize::from(f_max_regs) {
                self.stack[new_base + usize::from(yield_dst)] = arg;
            }
        }
        self.frames.push(Frame {
            fn_idx,
            pc: resume_pc,
            base: new_base,
            max_regs: f_max_regs,
            env,
            generator: Some(r#gen),
            yield_dst: None,
        });
        self.top_result = None;
        let frames_before = self.frames.len();
        let exec_res = self.execute();
        match exec_res {
            Ok(()) => {
                let done_val = self.heap.get(r#gen).properties.get(2).and_then(|v| v.as_f64().or(v.as_smi().map(|n| n as f64))).unwrap_or(0.0);
                if done_val == 2.0 && self.frames.len() < frames_before {
                    let yielded = self.top_result.take().unwrap_or(JsValue::undefined());
                    return Ok(self.make_iterator_result(yielded, false));
                } else {
                    let ret = self.top_result.take().unwrap_or(JsValue::undefined());
                    if done_val != 1.0 && self.heap.get(r#gen).properties.len() >= 3 {
                        self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
                    }
                    return Ok(self.make_iterator_result(ret, true));
                }
            }
            Err(e) => {
                if self.frames.len() >= frames_before {
                    self.frames.pop();
                    self.stack.truncate(new_base);
                }
                if self.heap.get(r#gen).properties.len() >= 3 {
                    self.heap.get_mut(r#gen).properties[2] = ops::box_number(1.0);
                }
                return Err(e);
            }
        }
    }
}
