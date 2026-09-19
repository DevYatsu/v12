use v12_heap::{JsObject, JsValue};

use crate::Interp;

/// Build a callee register window from a caller arg slice, handling rest.
pub(crate) fn fill_call_window(
    interp: &mut Interp<'_>,
    window: &mut [JsValue],
    args_src: &[JsValue],
    has_rest: bool,
    fixed: u16,
    rest_reg: u16,
) {
    if has_rest {
        let fixed_usize = fixed as usize;
        let to_copy = fixed_usize
            .min(args_src.len())
            .min(window.len().saturating_sub(1));
        window[1..1 + to_copy].copy_from_slice(&args_src[..to_copy]);
        let rest_len = args_src.len().saturating_sub(fixed_usize);
        let slice = if rest_len > 0 {
            args_src[fixed_usize..].to_vec()
        } else {
            Vec::new()
        };
        interp.gc_protect();
        let arr = alloc_rest_array(interp, slice);
        if (rest_reg as usize) < window.len() {
            window[rest_reg as usize] = arr;
        }
    } else {
        // Copy only declared formals. Extra actuals must NOT land in the
        // window: the register just past the locals is the compiler's
        // reserved never-written `undefined` source (and the frame abi
        // pre-fills it), so letting actuals spill there corrupts `undefined`
        // and uninitialized locals. The `has_rest` branch above is the
        // reference bound; `arguments` is snapshotted by the caller.
        let copied = (fixed as usize)
            .min(args_src.len())
            .min(window.len().saturating_sub(1));
        window[1..1 + copied].copy_from_slice(&args_src[..copied]);
    }
}

/// Fill the callee stack window at `new_base` from caller args at `arg_src`.
/// DRY helper for the sync `prepare_call` path — both async and sync paths
/// use `fill_call_window` via this wrapper.
#[allow(clippy::too_many_arguments)]
pub(crate) fn fill_stack_call_window(
    interp: &mut Interp<'_>,
    new_base: usize,
    arg_src: usize,
    argc: usize,
    callee_max_regs: u16,
    has_rest: bool,
    fixed: u16,
    rest_reg: u16,
) {
    let window_len = callee_max_regs as usize;
    if has_rest {
        let fixed_usize = fixed as usize;
        let to_copy = fixed_usize.min(argc).min(window_len.saturating_sub(1));
        let dst_start = new_base + 1;
        // Source and destination may overlap when caller/callee windows
        // share the same stack. Use `split_at_mut` for non-overlapping
        // mutable slices.
        if arg_src < dst_start {
            let (left, right) = interp.stack.split_at_mut(dst_start);
            right[..to_copy].copy_from_slice(&left[arg_src..arg_src + to_copy]);
        } else {
            let (left, right) = interp.stack.split_at_mut(arg_src);
            left[dst_start..dst_start + to_copy].copy_from_slice(&right[..to_copy]);
        }
        let rest_len = argc.saturating_sub(fixed_usize);
        let slice = if rest_len > 0 {
            interp.stack[arg_src + fixed_usize..arg_src + fixed_usize + rest_len].to_vec()
        } else {
            Vec::new()
        };
        interp.gc_protect();
        let arr = alloc_rest_array(interp, slice);
        if (rest_reg as usize) < window_len {
            interp.stack[new_base + rest_reg as usize] = arr;
        }
    } else {
        // See `fill_call_window`: bound the copy by declared formals so extra
        // actuals cannot overwrite the reserved `undefined`/uninitialized
        // register past the locals.
        let copied = (fixed as usize).min(argc).min(window_len.saturating_sub(1));
        let dst_start = new_base + 1;
        if arg_src < dst_start {
            let (left, right) = interp.stack.split_at_mut(dst_start);
            right[..copied].copy_from_slice(&left[arg_src..arg_src + copied]);
        } else {
            let (left, right) = interp.stack.split_at_mut(arg_src);
            left[dst_start..dst_start + copied].copy_from_slice(&right[..copied]);
        }
    }
}

/// Allocates a JavaScript-visible array from `elements`, stamping the
/// canonical length shape and linking `%Array.prototype%`.
///
/// This is the one place the three-step array-allocation idiom lives (mirrors
/// `Opcode::NewArray` in `execute.rs` and `object_ops.rs`). Every engine
/// allocation that produces an array observable from JavaScript must go
/// through here, or `result.constructor === Array` and inherited methods
/// (`join`, `includes`, …) resolve against no prototype.
pub(crate) fn alloc_array(interp: &mut Interp<'_>, elements: Vec<JsValue>) -> JsValue {
    let shape = interp.array_shape();
    let h = interp.heap_mut().alloc(JsObject::array(elements));
    interp.bind_shape(h, shape);
    interp.link_array_proto(h);
    JsValue::object(h)
}

/// Rest-parameter array (`FunctionDeclarationInstantiation`). Delegates to
/// [`alloc_array`]; kept as a named entry point for its call sites.
pub(crate) fn alloc_rest_array(interp: &mut Interp<'_>, elements: Vec<JsValue>) -> JsValue {
    alloc_array(interp, elements)
}

/// Fill the callee stack window at `new_base` from a caller-side arg slice
/// (for entries whose args are not already on the stack: inline accessor
/// and iterator calls). Mirrors [`fill_stack_call_window`] including the
/// rest-parameter tail array.
pub(crate) fn fill_stack_window_from_slice(
    interp: &mut Interp<'_>,
    new_base: usize,
    args: &[JsValue],
    callee_max_regs: u16,
    has_rest: bool,
    fixed: u16,
    rest_reg: u16,
) {
    let window_len = callee_max_regs as usize;
    if has_rest {
        let fixed_usize = fixed as usize;
        let to_copy = fixed_usize
            .min(args.len())
            .min(window_len.saturating_sub(1));
        interp.stack[new_base + 1..new_base + 1 + to_copy].copy_from_slice(&args[..to_copy]);
        let slice = if args.len() > fixed_usize {
            args[fixed_usize..].to_vec()
        } else {
            Vec::new()
        };
        interp.gc_protect();
        let arr = alloc_rest_array(interp, slice);
        if (rest_reg as usize) < window_len {
            interp.stack[new_base + rest_reg as usize] = arr;
        }
    } else {
        // Same bound as the two helpers above: only declared formals.
        let copied = (fixed as usize)
            .min(args.len())
            .min(window_len.saturating_sub(1));
        interp.stack[new_base + 1..new_base + 1 + copied].copy_from_slice(&args[..copied]);
    }
}
