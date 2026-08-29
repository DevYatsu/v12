//! Microtask job queue.
//!
//! Promise reactions and queued microtasks run at checkpoints.
//! The queue is a ring buffer; the engine drains it explicitly.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use v12_heap::{Handle, Heap, JsObject, JsValue};
use v12_interp::{Interp, JSException};

/// Maximum number of queued microtasks before backpressure would be applied.
/// The limit prevents unbounded growth when a microtask enqueues another.
const MAX_QUEUE_LEN: usize = 10_000;

/// Execution context handed to each job during a drain.
///
/// The interpreter that owns the heap is built by the engine for the
/// duration of the checkpoint (from the retained program of the last eval);
/// jobs may touch the heap directly or activate user function objects via
/// [`JobCtx::call_object`] — the seam Promise reaction handlers run through.
pub struct JobCtx<'a, 'b> {
    interp: &'a mut Interp<'b>,
    /// Follow-up jobs enqueued while this job ran (by the job itself or by
    /// natives it called); they join the same drain per microtask checkpoint
    /// semantics. Shared with the native registry through the engine.
    pending: Rc<RefCell<Vec<Job>>>,
}

impl<'a, 'b> JobCtx<'a, 'b> {
    /// Mutable heap access for the job.
    pub fn heap_mut(&mut self) -> &mut Heap {
        self.interp.heap_mut()
    }

    /// Activates a function object (see `Interp::call_object`); used to run
    /// reaction handlers and queued callbacks. The callee's environment
    /// capture and native routing are preserved by `prepare_call`.
    pub fn call_object(
        &mut self,
        callee: Handle<JsObject>,
        this: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        self.interp.call_object(callee, this, args)
    }

    /// Queues a follow-up microtask discovered while this job ran.
    /// Returns `false` when the queue is full.
    pub fn enqueue(&mut self, job: Job) -> bool {
        if self.pending.borrow().len() >= MAX_QUEUE_LEN {
            return false;
        }
        self.pending.borrow_mut().push(job);
        true
    }
}

/// A microtask job. Runs with a [`JobCtx`] so it can reach the heap and
/// activate user functions.
pub type Job = Box<dyn FnOnce(&mut JobCtx<'_, '_>)>;

/// Ordered queue of pending microtasks.
#[derive(Default)]
pub struct JobQueue {
    jobs: VecDeque<Job>,
}

impl std::fmt::Debug for JobQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobQueue")
            .field("len", &self.jobs.len())
            .finish()
    }
}

impl JobQueue {
    /// Creates an empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            jobs: VecDeque::new(),
        }
    }

    /// Number of pending jobs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    /// True when no jobs are pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// Enqueues a microtask. Returns `false` when the queue is full.
    pub fn enqueue(&mut self, job: Job) -> bool {
        if self.jobs.len() >= MAX_QUEUE_LEN {
            return false;
        }
        self.jobs.push_back(job);
        true
    }

    /// Drains the queue against `interp`'s heap.
    ///
    /// `pending` is the native-shared side channel: follow-up jobs enqueued
    /// during the checkpoint join the same drain (microtask checkpoint
    /// semantics). Returns the number of jobs executed.
    pub fn drain(&mut self, interp: &mut Interp<'_>, pending: Rc<RefCell<Vec<Job>>>) -> usize {
        let mut count = 0usize;
        loop {
            // Adopt follow-ups discovered by the previous job/natives before
            // picking the next one, keeping FIFO order.
            if !pending.borrow().is_empty() {
                let mut p = pending.borrow_mut();
                for job in p.drain(..) {
                    self.jobs.push_back(job);
                }
            }
            let Some(job) = self.jobs.pop_front() else {
                break;
            };
            // Each job runs to completion; panics from engine bugs propagate,
            // while jobs themselves should be panic-free. `&mut *interp` is a
            // fresh reborrow per iteration — the reference lifetime (`'a`)
            // and the interp's own heap-borrow lifetime are decoupled in
            // `JobCtx<'a, 'b>`.
            let mut ctx = JobCtx {
                interp: &mut *interp,
                pending: Rc::clone(&pending),
            };
            job(&mut ctx);
            drop(ctx);
            count += 1;
            if count > MAX_QUEUE_LEN * 2 {
                break;
            }
        }
        count
    }

    /// Clears all pending jobs without running them.
    pub fn clear(&mut self) {
        self.jobs.clear();
    }
}
