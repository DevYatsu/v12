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
