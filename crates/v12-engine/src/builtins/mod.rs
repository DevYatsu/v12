//! Built-in objects and functions.
//!
//! Each built-in is a native function registered with the interpreter's
//! `NativeRegistry`. The functions operate directly on the heap, using shape
//! transitions and string primitives.

pub mod array;
pub mod boolean;
pub mod error;
pub mod math;
pub mod number;
pub mod object;
pub mod string;

use v12_heap::{Heap, JsValue};

/// Registry of native function indices. Indices beyond the compiled program
/// length route to this table.
#[derive(Debug, Default, Clone)]
pub struct NativeRegistry {
    handlers: std::collections::HashMap<u32, NativeHandler>,
}

/// A native handler.
pub type NativeHandler = fn(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, JsValue>;

impl NativeRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a handler at `index`.
    pub fn register(&mut self, index: u32, handler: NativeHandler) {
        self.handlers.insert(index, handler);
    }

    /// Dispatches a native call.
    pub fn dispatch(
        &mut self,
        heap: &mut Heap,
        this: JsValue,
        args: &[JsValue],
        index: u32,
    ) -> Result<JsValue, JsValue> {
        if let Some(handler) = self.handlers.get(&index).copied() {
            handler(heap, this, args)
        } else {
            let msg = format!("TypeError: native function #{index} is not registered");
            let h = heap.intern_string(v12_heap::V12Str::latin1(msg.into_bytes()));
            Err(JsValue::string(h))
        }
    }
}

impl v12_interp::NativeRegistry for NativeRegistry {
    fn call_native(
        &mut self,
        heap: &mut Heap,
        this: JsValue,
        args: &[JsValue],
        index: u32,
    ) -> Result<JsValue, JsValue> {
        self.dispatch(heap, this, args, index)
    }
}

/// Indices for built-ins. Chosen beyond any plausible program function count.
pub const NATIVE_OBJECT_CREATE: u32 = 1000;
pub const NATIVE_OBJECT_GET_PROTOTYPE_OF: u32 = 1001;
pub const NATIVE_OBJECT_DEFINE_PROPERTY: u32 = 1002;
pub const NATIVE_ARRAY_PUSH: u32 = 1100;
pub const NATIVE_ARRAY_POP: u32 = 1101;
pub const NATIVE_STRING_CHAR_AT: u32 = 1200;
pub const NATIVE_STRING_SLICE: u32 = 1201;
pub const NATIVE_NUMBER_IS_NAN: u32 = 1300;
pub const NATIVE_MATH_ABS: u32 = 1400;
pub const NATIVE_BOOLEAN_CONSTRUCT: u32 = 1500;
pub const NATIVE_ERROR_CREATE: u32 = 1600;
pub const NATIVE_QUEUE_MICROTASK: u32 = 1700;

/// Installs the core built-ins into `registry`.
pub fn install_core(registry: &mut NativeRegistry) {
    registry.register(NATIVE_OBJECT_CREATE, object::object_create);
    registry.register(
        NATIVE_OBJECT_GET_PROTOTYPE_OF,
        object::object_get_prototype_of,
    );
    registry.register(
        NATIVE_OBJECT_DEFINE_PROPERTY,
        object::object_define_property,
    );
    registry.register(NATIVE_ARRAY_PUSH, array::array_push);
    registry.register(NATIVE_ARRAY_POP, array::array_pop);
    registry.register(NATIVE_STRING_CHAR_AT, string::string_char_at);
    registry.register(NATIVE_STRING_SLICE, string::string_slice);
    registry.register(NATIVE_NUMBER_IS_NAN, number::number_is_nan);
    registry.register(NATIVE_MATH_ABS, math::math_abs);
    registry.register(NATIVE_BOOLEAN_CONSTRUCT, boolean::boolean_construct);
    registry.register(NATIVE_ERROR_CREATE, error::error_create);
    registry.register(NATIVE_QUEUE_MICROTASK, queue_microtask);
}

fn queue_microtask(heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, JsValue> {
    let _ = heap;
    Ok(JsValue::undefined())
}

fn intern_type_error(heap: &mut Heap, msg: &str) -> JsValue {
    let h = heap.intern_string(v12_heap::V12Str::latin1(msg.as_bytes().to_vec()));
    JsValue::string(h)
}
