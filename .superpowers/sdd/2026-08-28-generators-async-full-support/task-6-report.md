# Task 6 Report: Await via Promise reaction (fix)

Status: fixed
Commit: ec9862b
Previous: e28e105

Changes:
- Replaced LIFO Vec pending_awaits with VecDeque FIFO microtask queue
- Added is_promise / promise_resolve_for_await helper (Promise.resolve identity for promises, otherwise create fulfilled promise)
- Added NATIVE_PROMISE_RESOLVE/REJECT fallbacks in prepare_call for standalone interp tests
- Added minimal Promise wiring in ensure_default_global (slot 10) for interp-alone tests
- Added async promise allocation in prepare_call is_async path (pending promise stored in generator properties[4])
- Rewrote Await arm to suspend like SuspendYield but enqueue FIFO reaction via promise_resolve_for_await, handle rejection flag, deliver Promise to caller when available
- Added resume_async_throw for rejection path (unwind then execute)
- Fixed complete_frame to settle async promise on completion and handle caller advancement correctly
- Fixed run_jobs to drain FIFO via pop_front and dispatch rejection vs fulfillment
- Fixed gc_protect for new tuple shape
- Updated async_await.rs to two tests: await 42 and await Promise.resolve(42) both assert FIFO enqueue and drain

Tests:
- cargo nextest run -p v12-interp --test async_await -v => 2 passed
- cargo nextest run -p v12-interp -v => 66 passed

Concerns:
- is_promise structurally matches any 3-slot object with state 0..2; could collide with non-promise (engine uses same check)
- pending promise settlement does not yet propagate reactions (then handlers) – sufficient for Task 6 await resume, full chain needs JobQueue enqueue_reaction in engine
- global Promise wiring for new_with_heap path relies on realm; standalone path now creates minimal Promise ctor but without full prototype chain
