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
pub const NATIVE_EVAL: u32 = 1800;
pub const NATIVE_FUNCTION: u32 = 1801;
pub const NATIVE_CONSOLE_LOG: u32 = 1900;

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
    registry.register(NATIVE_EVAL, eval_stub);
    registry.register(NATIVE_FUNCTION, function_stub);
    registry.register(NATIVE_CONSOLE_LOG, console_log);
}

fn queue_microtask(heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, JsValue> {
    let _ = heap;
    Ok(JsValue::undefined())
}

fn eval_stub(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    // v1 stub: non-string args return as-is; string args are syntax-checked
    // via the compiler and return `undefined` on success. The full
    // heap-sharing `eval` path is exercised via `Engine::eval_direct`.
    if let Some(first) = args.first() {
        if let Some(h) = first.as_string() {
            heap.flatten(h);
            let text = match &heap.get(h).storage {
                v12_heap::StrStorage::Latin1(b) => String::from_utf8_lossy(b).into_owned(),
                v12_heap::StrStorage::Utf16(u) => String::from_utf16_lossy(u),
                _ => String::new(),
            };
            if let Err(err) = v12_bccompiler::compile_source_with_strings(&text) {
                let msg = err.message;
                let handle = if msg.is_ascii() {
                    heap.intern_string(v12_heap::V12Str::latin1(msg.into_bytes()))
                } else {
                    heap.intern_string(v12_heap::V12Str::utf16(msg.encode_utf16().collect()))
                };
                return Err(JsValue::string(handle));
            }
            Ok(JsValue::undefined())
        } else {
            Ok(*first)
        }
    } else {
        Ok(JsValue::undefined())
    }
}

fn function_stub(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    // v1 stub for `new Function`: validate syntax and return a placeholder
    // function object. Full compilation is via `Engine::create_function`.
    if args.is_empty() {
        let func = heap.alloc(v12_heap::JsObject {
            kind: v12_heap::KIND_FUNCTION,
            elements: vec![JsValue::from_i32_smi(0).unwrap()],
            ..Default::default()
        });
        heap.add_root(JsValue::object(func));
        return Ok(JsValue::object(func));
    }
    let mut param_parts = Vec::new();
    for &arg in &args[..args.len() - 1] {
        if let Some(h) = arg.as_string() {
            heap.flatten(h);
            let txt = match &heap.get(h).storage {
                v12_heap::StrStorage::Latin1(b) => String::from_utf8_lossy(b).into_owned(),
                v12_heap::StrStorage::Utf16(u) => String::from_utf16_lossy(u),
                _ => String::new(),
            };
            param_parts.push(txt);
        }
    }
    let param_str = param_parts.join(",");
    let body = if let Some(last) = args.last().and_then(|v| v.as_string()) {
        heap.flatten(last);
        match &heap.get(last).storage {
            v12_heap::StrStorage::Latin1(b) => String::from_utf8_lossy(b).into_owned(),
            v12_heap::StrStorage::Utf16(u) => String::from_utf16_lossy(u),
            _ => String::new(),
        }
    } else {
        String::new()
    };
    let src = format!("function __f({param_str}){{{body}}}");
    if let Err(err) = v12_bccompiler::compile_source_with_strings(&src) {
        let msg = err.message;
        let handle = if msg.is_ascii() {
            heap.intern_string(v12_heap::V12Str::latin1(msg.into_bytes()))
        } else {
            heap.intern_string(v12_heap::V12Str::utf16(msg.encode_utf16().collect()))
        };
        return Err(JsValue::string(handle));
    }
    let func = heap.alloc(v12_heap::JsObject {
        kind: v12_heap::KIND_FUNCTION,
        elements: vec![JsValue::from_i32_smi(1).unwrap()],
        ..Default::default()
    });
    heap.add_root(JsValue::object(func));
    Ok(JsValue::object(func))
}

fn console_log(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsValue> {
    // Mirrors `Engine::to_display_string` for the subset of values that
    // `console.log` observes in Tier-0. Strings are flattened first so
    // composite/sliced representations print correctly.
    let mut parts = Vec::with_capacity(args.len());
    for &v in args {
        let text = if let Some(handle) = v.as_string() {
            heap.flatten(handle);
            match &heap.get(handle).storage {
                v12_heap::StrStorage::Latin1(bytes) => {
                    String::from_utf8_lossy(bytes).into_owned()
                }
                v12_heap::StrStorage::Utf16(units) => String::from_utf16_lossy(units),
                _ => String::new(),
            }
        } else if let Some(number) = v.as_smi().map(f64::from).or(v.as_f64()) {
            if number.is_nan() {
                "NaN".to_string()
            } else if number == f64::INFINITY {
                "Infinity".to_string()
            } else if number == f64::NEG_INFINITY {
                "-Infinity".to_string()
            } else {
                format!("{number}")
            }
        } else if v.is_true() {
            "true".to_string()
        } else if v.is_false() {
            "false".to_string()
        } else if v.is_undefined() {
            "undefined".to_string()
        } else if v.is_null() {
            "null".to_string()
        } else if v.is_object() {
            "[object Object]".to_string()
        } else {
            "<unprintable>".to_string()
        };
        parts.push(text);
    }
    println!("{}", parts.join(" "));
    Ok(JsValue::undefined())
}

fn intern_type_error(heap: &mut Heap, msg: &str) -> JsValue {
    let h = heap.intern_string(v12_heap::V12Str::latin1(msg.as_bytes().to_vec()));
    JsValue::string(h)
}
