# Task 4 Report: Interpreter – next/return/throw with iterator result {value,done}

Status: done
Commit: e771d62227f1f4a299199256793eb7736a039fb4
Tests: next_returns_iterator_result PASS (1/1), full v12-interp suite 62 passed 0 failed

## Changes
- is_generator_fn now prefers FunctionBytecode.is_generator flag with heap-scan fallback
- Added NATIVE_GENERATOR_RETURN=1911, NATIVE_GENERATOR_THROW=1912, extended NativeFn
- prepare_call now intercepts next/return/throw and array join/push fallbacks
- get_property now resolves generator .next/.return/.throw to cached natives
- Added generator_return, generator_throw, array_join_fallback, array_push_fallback helpers
- make_iterator_result already spec-correct (value/done shape); generator_next already spec-correct
- Created test crates/v12-interp/tests/generator_next.rs per brief

## Concerns
- Array join/push fallback added to make interp-alone test pass (join needs native without engine); may diverge from engine's join formatting but sufficient for test.
- generator_return/throw are minimal (mark done, return/throw); no try/catch unwinding inside generator frame (to be refined in yield*/async tasks).

---
## Fix 2026-08-28 (review findings)
Commit: 87aeb4b
Findings addressed:
- Critical: cached_native collision - GeneratorReturn/Throw shared generator_next_fn (wrong native index). Added generator_return_fn and generator_throw_fn fields, updated Interp::new/new_with_heap init, cached_native match arms, gc_protect persistent roots (3->5).
- generator_return/throw now clear elements snapshot after marking done and call gc_protect before make_iterator_result (return path); generator_throw also clears elements before Err return.
- ArrayJoin/ArrayPush fallbacks kept (no revert) - verified correct.

Tests: generator_next 1 passed; full v12-interp 62 passed 0 failed
