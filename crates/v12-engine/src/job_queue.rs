//! Microtask job queue.
//!
//! Promise reactions and queued microtasks run at checkpoints.
//! The queue is a ring buffer; the engine drains it explicitly.

use std::collections::VecDeque;

/// Maximum number of queued microtasks before backpressure would be applied.
/// The limit prevents unbounded growth when a microtask enqueues another.
const MAX_QUEUE_LEN: usize = 10_000;

/// A microtask job.
pub type Job = Box<dyn FnOnce(&mut v12_heap::Heap)>;

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

    /// Drains the queue, executing each job against `heap`.
    ///
    /// New jobs enqueued while draining run in the same checkpoint
    /// until the queue empties, matching the microtask checkpoint
    /// semantics. Returns the number of jobs executed.
    pub fn drain(&mut self, heap: &mut v12_heap::Heap) -> usize {
        let mut count = 0usize;
        while let Some(job) = self.jobs.pop_front() {
            // Each job runs to completion; panics from engine bugs propagate,
            // while jobs themselves should be panic-free.
            job(heap);
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
