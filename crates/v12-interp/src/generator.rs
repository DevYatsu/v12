use v12_heap::{Handle, JsObject, JsValue};

use crate::{Interp, JSException};

const _GEN_FN_SLOT: usize = 0;
const GEN_PC_SLOT: usize = 1;
const GEN_DONE_SLOT: usize = 2;
const GEN_DST_SLOT: usize = 3;

pub trait Suspendable {
    fn suspend(
        &mut self,
        dst: u16,
        val: JsValue,
        resume_pc: usize,
    ) -> Result<Handle<JsObject>, JSException>;
    fn resume(&mut self, r#gen: Handle<JsObject>, arg: JsValue) -> Result<JsValue, JSException>;
}

impl Suspendable for Interp<'_> {
    fn suspend(
        &mut self,
        dst: u16,
        val: JsValue,
        resume_pc: usize,
    ) -> Result<Handle<JsObject>, JSException> {
        // Snapshot current register window, capture resume pc and env, store into generator.
        let frame = self.frames.last().expect("suspend requires a frame");
        let base = frame.base;
        let max_regs = frame.max_regs;
        let r#gen = frame.generator.expect("suspend requires generator");
        let env = frame.env;
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
                vec![JsValue::undefined(); usize::from(max_regs)]
            }
        };
        if self.heap.get(r#gen).properties.len() < 4 {
            self.heap
                .get_mut(r#gen)
                .properties
                .resize(4, JsValue::undefined());
        }
        self.heap.get_mut(r#gen).properties[GEN_PC_SLOT] = JsValue::from_f64(resume_pc as f64);
        self.heap.get_mut(r#gen).properties[GEN_DST_SLOT] = JsValue::from_f64(f64::from(dst));
        self.heap.get_mut(r#gen).properties[GEN_DONE_SLOT] = JsValue::from_f64(2.0);
        self.heap.get_mut(r#gen).elements = snapshot;
        // TODO: dedicated env slot, using prototype as env storage per Task 2 contract
        self.heap.get_mut(r#gen).prototype = env;
        let finished_base = self.frames.pop().expect("pop").base;
        self.stack.truncate(finished_base);
        // Encapsulate yielded value: caller no longer needs to set top_result separately
        // for SuspendYield. Await caller will overwrite with None after.
        self.top_result = Some(val);
        Ok(r#gen)
    }

    fn resume(&mut self, r#gen: Handle<JsObject>, arg: JsValue) -> Result<JsValue, JSException> {
        // Delegate to canonical generator_next to avoid duplicating restore/execute logic.
        self.generator_next(JsValue::object(r#gen), arg)
    }
}
