//! The fully executing value-call capability.
//!
//! `prepare_call` remains the parked-opcode dispatcher. This module only owns
//! the lifecycle around it: laying out a value call, bounding nested execution,
//! and restoring the caller's frame and stack state on every exit.

use v12_heap::JsValue;

use super::{CallOutcome, Interp, JSException, MAX_CALL_DEPTH, MAX_RESUME_DEPTH};

impl Interp<'_> {
    /// Invoke any callable value through the interpreter's normal call path.
    pub fn call_value(
        &mut self,
        callee: JsValue,
        this: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        self.call_value_nested(callee, this, args)
    }

    /// Private nested implementation used by `prepare_call` for bound targets.
    pub(crate) fn call_value_nested(
        &mut self,
        callee: JsValue,
        this: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(JSException(
                self.error_value("RangeError: maximum call stack size exceeded"),
            ));
        }
        // `prepare_call` + `execute` below nest a Rust call per value-call
        // (a callback-taking builtin whose callback calls it again), so this
        // seam spends *native* stack like the accessor and inline-call seams
        // and is bounded by the same budget. `MAX_CALL_DEPTH` is far too high
        // to fire first: a few hundred nested levels already exhaust an 8 MiB
        // thread stack in a debug build, and the process aborts with
        // `fatal runtime error: stack overflow` before the frame cap is
        // reached.
        if self.resume_depth >= MAX_RESUME_DEPTH {
            return Err(JSException(
                self.error_value("RangeError: maximum call stack size exceeded"),
            ));
        }

        // The parked Call ABI is [callee][this][args...]. Put all values on
        // the stack before gc_protect so the first allocation/safepoint sees
        // the complete callback accumulator.
        let base = self.stack.len();
        self.stack.push(callee);
        self.stack.push(this);
        self.stack.extend_from_slice(args);
        self.gc_protect();

        let Ok(caller_max_regs) = u16::try_from(self.stack.len() - base) else {
            self.stack.truncate(base);
            return Err(JSException(
                self.error_value("RangeError: too many call arguments"),
            ));
        };
        let Ok(argc) = u16::try_from(args.len()) else {
            self.stack.truncate(base);
            return Err(JSException(
                self.error_value("RangeError: too many call arguments"),
            ));
        };

        let saved_stop = self.stop_at_frames;
        let boundary = self.frames.len();
        self.stop_at_frames = Some(boundary);
        self.top_result = None;

        let result = match self.prepare_call(base, caller_max_regs, 0, argc, false, 0) {
            Ok(CallOutcome::Pushed) => {
                self.resume_depth += 1;
                let execution = self.execute();
                self.resume_depth -= 1;
                self.cleanup_nested_call_frames(boundary, base);
                execution.and_then(|()| {
                    self.top_result.take().ok_or_else(|| {
                        JSException(
                            self.error_value("InternalError: call completed without a result"),
                        )
                    })
                })
            }
            Ok(CallOutcome::Value(value)) => Ok(value),
            Err(error) => Err(error),
        };

        self.stop_at_frames = saved_stop;
        self.stack.truncate(base);
        result
    }

    fn cleanup_nested_call_frames(&mut self, boundary: usize, base: usize) {
        while self.frames.len() > boundary {
            let Some(frame) = self.frames.pop() else {
                break;
            };
            self.drop_frame_arguments(&frame);
            self.call_args.remove(&frame.base);
            self.drop_frame_context(frame.base);
        }
        self.stack.truncate(base);
    }
}
