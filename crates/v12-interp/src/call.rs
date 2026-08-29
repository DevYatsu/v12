use v12_heap::{JsObject, JsValue};

use crate::Interp;
use crate::ops;

/// Build a callee register window from a caller arg slice, handling rest.
pub(crate) fn fill_call_window(
    interp: &mut Interp,
    window: &mut [JsValue],
    args_src: &[JsValue],
    has_rest: bool,
    fixed: u16,
    rest_reg: u16,
) {
    if has_rest {
        let fixed_usize = fixed as usize;
        let to_copy = fixed_usize.min(args_src.len()).min(window.len().saturating_sub(1));
        for i in 0..to_copy {
            window[1 + i] = args_src[i];
        }
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
        let copied = args_src.len().min(window.len().saturating_sub(1));
        for i in 0..copied {
            window[1 + i] = args_src[i];
        }
    }
}

/// Fill the callee stack window at `new_base` from caller args at `arg_src`.
/// DRY helper for the sync `prepare_call` path — both async and sync paths
/// use `fill_call_window` via this wrapper.
pub(crate) fn fill_stack_call_window(
    interp: &mut Interp,
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
        let to_copy = fixed_usize
            .min(argc)
            .min(window_len.saturating_sub(1));
        for i in 0..to_copy {
            interp.stack[new_base + 1 + i] = interp.stack[arg_src + i];
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
        let copied = argc.min(window_len.saturating_sub(1));
        for i in 0..copied {
            interp.stack[new_base + 1 + i] = interp.stack[arg_src + i];
        }
    }
}

pub(crate) fn alloc_rest_array(interp: &mut Interp, elements: Vec<JsValue>) -> JsValue {
    let len = elements.len() as u32;
    let shape = interp.array_shape();
    let h = interp.heap_mut().alloc(JsObject {
        kind: crate::KIND_ARRAY,
        properties: vec![ops::box_number(f64::from(len))],
        elements,
        ..JsObject::default()
    });
    interp.bind_shape(h, shape);
    JsValue::object(h)
}
