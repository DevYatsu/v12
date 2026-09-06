//! Built-in objects and functions.
//!
//! Each built-in is a native function registered with the interpreter's
//! `NativeRegistry`. The functions operate directly on the heap, using shape
//! transitions and string primitives.

pub mod array;
pub mod boolean;
pub mod ctx;
pub mod error;
pub mod global;
pub mod helpers;
pub mod iterator;
pub mod json;
pub mod map;
pub mod math;
pub mod number;
pub mod object;
pub mod promise;
pub mod registry;
pub mod regexp;
pub mod string;
pub mod symbol;

pub use ctx::{BuiltinFn, Ctx, call_ctx, call_legacy};
pub use registry::{HostClosure, HostFn, NativeHandler, NativeRegistry};

use v12_heap::{Heap, JsValue};
use v12_native::{NativeId, Throw};

/// Handles needed to install built-ins. Constructed by `Realm::new` from its
/// materialized prototypes/constructors and passed to [`install_builtins`].
///
/// The macro's *grouped* entries (`Global`, `Math`, `Number`, `Array`, ...)
/// route to the corresponding field here by host name; prototypes and
/// singletons that always exist are plain handles, optional constructor
/// objects are `Option`.
pub struct BuiltinTargets {
    pub global: v12_heap::Handle<v12_heap::JsObject>,
    pub math: Option<v12_heap::Handle<v12_heap::JsObject>>,
    pub number: Option<v12_heap::Handle<v12_heap::JsObject>>,
    pub number_proto: v12_heap::Handle<v12_heap::JsObject>,
    pub string: Option<v12_heap::Handle<v12_heap::JsObject>>,
    pub string_proto: v12_heap::Handle<v12_heap::JsObject>,
    pub array: Option<v12_heap::Handle<v12_heap::JsObject>>,
    pub array_proto: v12_heap::Handle<v12_heap::JsObject>,
    pub object: Option<v12_heap::Handle<v12_heap::JsObject>>,
    pub object_proto: v12_heap::Handle<v12_heap::JsObject>,
    pub function_proto: v12_heap::Handle<v12_heap::JsObject>,
    pub json: Option<v12_heap::Handle<v12_heap::JsObject>>,
    pub boolean_proto: v12_heap::Handle<v12_heap::JsObject>,
    pub symbol: Option<v12_heap::Handle<v12_heap::JsObject>>,
    pub symbol_proto: v12_heap::Handle<v12_heap::JsObject>,
}

pub(crate) fn builtin_install_prop(
    heap: &mut Heap,
    obj: v12_heap::Handle<v12_heap::JsObject>,
    name: &str,
    value: JsValue,
) {
    use v12_heap::{Attrs, PropKey, V12Str};
    let h = if name.is_ascii() {
        heap.intern_string(V12Str::latin1_slice(name.as_bytes()))
    } else {
        heap.intern_string(V12Str::utf16(name.encode_utf16().collect()))
    };
    let key = PropKey::from_string(h);
    let shape = heap.shape_of_mut(obj);
    let child = heap.add_property(shape, key, Attrs::BUILTIN);
    heap.bind_shape(obj, child);
    heap.get_mut(obj).properties.push(value);
    heap.get_mut(obj).property_keys.push(Some(key));
}

/// Allocates the native function object for `id` and installs it as a
/// shape-bound property `name` on `target`.
///
/// A `None` target installs nothing: optional constructors that this realm
/// has not materialized, and the reserved future hosts (`Json`, `Map`, …)
/// whose target fields do not exist yet. This is the one install shape —
/// every `__builtin_emit_install!` arm routes through it.
pub(crate) fn install_native(
    heap: &mut Heap,
    target: Option<v12_heap::Handle<v12_heap::JsObject>>,
    name: &str,
    id: NativeId,
) {
    let Some(obj) = target else { return };
    let func = heap.alloc(v12_heap::JsObject {
        kind: v12_heap::Kind::Function,
        callable: v12_heap::FunctionTarget::Bytecode(u32::from(id)),
        ..Default::default()
    });
    heap.add_root(JsValue::object(func));
    builtin_install_prop(heap, obj, name, JsValue::object(func));
}

/// Installs the *value* a handler produces (a `Math.PI` constant, a
/// well-known symbol, …) as a shape-bound property `name` on `target`.
///
/// The grouped entry still declares `name => id => handler`; the handler is
/// evaluated once at install time through [`builtin_dispatch`], so constants
/// stay single-source with their dispatch arm. `None`/unevaluatable targets
/// install nothing.
pub(crate) fn install_value(
    heap: &mut Heap,
    target: Option<v12_heap::Handle<v12_heap::JsObject>>,
    name: &str,
    id: NativeId,
) {
    let Some(obj) = target else { return };
    if let Some(Ok(value)) = builtin_dispatch(id, heap, JsValue::undefined(), &[]) {
        builtin_install_prop(heap, obj, name, value);
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! __builtin_emit_install {
    // Plain-handle targets: the prototype/singleton always exists.
    (Global, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.global), $name, $id)
    };
    (NumberProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.number_proto), $name, $id)
    };
    (StringProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.string_proto), $name, $id)
    };
    (ArrayProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.array_proto), $name, $id)
    };
    (ObjectProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.object_proto), $name, $id)
    };
    (FunctionProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.function_proto), $name, $id)
    };
    // Optional constructor targets: skipped until the realm materializes them.
    (Math, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.math, $name, $id)
    };
    (Number, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.number, $name, $id)
    };
    (Array, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.array, $name, $id)
    };
    (Object, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.object, $name, $id)
    };
    // Reserved future hosts — no target field yet, nothing to install.
    (Json, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.json, $name, $id)
    };
    (BooleanProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.boolean_proto), $name, $id)
    };
    (ErrorProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, None, $name, $id)
    };
    (RegExp, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, None, $name, $id)
    };
    (RegExpProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, None, $name, $id)
    };
    (Map, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, None, $name, $id)
    };
    (MapProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, None, $name, $id)
    };
    (Set, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, None, $name, $id)
    };
    (SetProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, None, $name, $id)
    };
    (Iterator, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, None, $name, $id)
    };
    (IteratorProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, None, $name, $id)
    };
    // Value-constant groups: the handler is evaluated once at install time
    // (see `install_value`) and the result is stored as a plain data property.
    (GlobalValue, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_value($heap, Some($targets.global), $name, $id)
    };
    (MathValue, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_value($heap, $targets.math, $name, $id)
    };
    (NumberValue, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_value($heap, $targets.number, $name, $id)
    };
    (StringCtor, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.string, $name, $id)
    };
    (Symbol, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.symbol, $name, $id)
    };
    (SymbolProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.symbol_proto), $name, $id)
    };
}

/// Unified builtin declaration: single source of truth for dispatch + install.
///
/// *Grouped* entries `Target { "jsName" => Variant => handler }` emit both a
/// `builtin_dispatch` match arm and a straight-line `install_builtins` call.
/// Grouped targets distinguish **static** (constructor) vs **dynamic**
/// (prototype) installs: `Array { "isArray" => ... }` installs on the `Array`
/// constructor, `ArrayProto { "push" => ... }` installs on `Array.prototype`.
/// No intermediate `BUILTIN_INSTALLS` array is stored — the macro expands to
/// direct `install_prop` calls (zero rodata, no iteration).
/// *Bare* entries `Variant => handler` (after `;`) emit only a dispatch arm
/// for truly internal / non-JS-visible natives (e.g. `Eval`, `ModuleImport`,
/// `ConsoleLog`). They are not installed on any JS object.
///
/// Example:
/// ```ignore
/// define_builtins! {
///     Global { "isNaN" => GlobalIsNaN => number::global_is_nan },
///     Math { "floor" => MathFloor => math::math_floor },
///     Array { "isArray" => ArrayIsArray => array::array_is_array },
///     ArrayProto { "push" => ArrayPush => array::array_push };
///     Eval => eval_stub,
/// }
/// ```
macro_rules! define_builtins {
    (
        $( $target:ident { $($name:literal => $id:ident => $handler:expr),* $(,)? } ),* $(,)? ;
        $( $bare_id:ident => $bare_handler:expr ),* $(,)?
    ) => {
        /// Compile-time dispatch over every builtin. `None` means "not a
        /// builtin" — the caller falls through to the runtime registry.
        ///
        /// This is the O(1) static lookup: `match` lowers to a jump table on
        /// the discriminants, so a builtin call costs no hashing and no index
        /// tables. A `phf`/perfect-hash table is deliberately NOT used here —
        /// the match runs faster than any hash (neither hashing nor table
        /// memory) — and the runtime `handlers` map cannot be `phf` anyway,
        /// because host closures register at runtime and `phf` keys must be
        /// known at compile time.
        pub fn builtin_dispatch(
            id: NativeId,
            heap: &mut Heap,
            this: JsValue,
            args: &[JsValue],
        ) -> Option<Result<JsValue, Throw>> {
            match id {
                $( $( NativeId::$id => Some(($handler)(heap, this, args)), )* )*
                $( NativeId::$bare_id => Some(($bare_handler)(heap, this, args)), )*
                _ => None,
            }
        }

        /// Installs all grouped built-ins as shape-bound properties. This is
        /// the only install path — there is no `BUILTIN_INSTALLS` array. Each
        /// grouped entry expands to a straight-line [`install_native`] call,
        /// so the compiler can inline and no rodata table is emitted.
        pub fn install_builtins(heap: &mut Heap, targets: &BuiltinTargets) {
            $( $( $crate::__builtin_emit_install!($target, heap, targets, $name, NativeId::$id); )* )*
            // Bare ids are dispatch-only; silence unused warnings.
            $( let _ = NativeId::$bare_id; )*
        }
    };
}

define_builtins! {
    Global {
        "isNaN" => GlobalIsNaN => |heap, this, args| call_ctx(number::global_is_nan, heap, this, args),
        "isFinite" => GlobalIsFinite => |heap, this, args| call_ctx(number::global_is_finite, heap, this, args),
        "parseInt" => GlobalParseInt => |heap, this, args| call_ctx(number::global_parse_int, heap, this, args),
        "parseFloat" => GlobalParseFloat => |heap, this, args| call_ctx(number::global_parse_float, heap, this, args),
        "encodeURI" => GlobalEncodeUri => global::global_encode_uri,
        "decodeURI" => GlobalDecodeUri => global::global_decode_uri,
        "encodeURIComponent" => GlobalEncodeUriComponent => global::global_encode_uri_component,
        "decodeURIComponent" => GlobalDecodeUriComponent => global::global_decode_uri_component,
    },
    Math {
        "abs" => MathAbs => |heap, this, args| call_ctx(math::math_abs, heap, this, args),
        "floor" => MathFloor => |heap, this, args| call_ctx(math::math_floor, heap, this, args),
        "ceil" => MathCeil => |heap, this, args| call_ctx(math::math_ceil, heap, this, args),
        "trunc" => MathTrunc => |heap, this, args| call_ctx(math::math_trunc, heap, this, args),
        "pow" => MathPow => |heap, this, args| call_ctx(math::math_pow, heap, this, args),
        "max" => MathMax => |heap, this, args| call_ctx(math::math_max, heap, this, args),
        "min" => MathMin => |heap, this, args| call_ctx(math::math_min, heap, this, args),
        "random" => MathRandom => |heap, this, args| call_ctx(math::math_random, heap, this, args),
        "round" => MathRound => |heap, this, args| call_ctx(math::math_round, heap, this, args),
        "sqrt" => MathSqrt => |heap, this, args| call_ctx(math::math_sqrt, heap, this, args),
        "sign" => MathSign => |heap, this, args| call_ctx(math::math_sign, heap, this, args),
        "cbrt" => MathCbrt => |heap, this, args| call_ctx(math::math_cbrt, heap, this, args),
        "exp" => MathExp => |heap, this, args| call_ctx(math::math_exp, heap, this, args),
        "expm1" => MathExpm1 => |heap, this, args| call_ctx(math::math_expm1, heap, this, args),
        "log" => MathLog => |heap, this, args| call_ctx(math::math_log, heap, this, args),
        "log1p" => MathLog1p => |heap, this, args| call_ctx(math::math_log1p, heap, this, args),
        "log2" => MathLog2 => |heap, this, args| call_ctx(math::math_log2, heap, this, args),
        "log10" => MathLog10 => |heap, this, args| call_ctx(math::math_log10, heap, this, args),
        "sin" => MathSin => |heap, this, args| call_ctx(math::math_sin, heap, this, args),
        "cos" => MathCos => |heap, this, args| call_ctx(math::math_cos, heap, this, args),
        "tan" => MathTan => |heap, this, args| call_ctx(math::math_tan, heap, this, args),
        "asin" => MathAsin => |heap, this, args| call_ctx(math::math_asin, heap, this, args),
        "acos" => MathAcos => |heap, this, args| call_ctx(math::math_acos, heap, this, args),
        "atan" => MathAtan => |heap, this, args| call_ctx(math::math_atan, heap, this, args),
        "atan2" => MathAtan2 => |heap, this, args| call_ctx(math::math_atan2, heap, this, args),
        "sinh" => MathSinh => |heap, this, args| call_ctx(math::math_sinh, heap, this, args),
        "cosh" => MathCosh => |heap, this, args| call_ctx(math::math_cosh, heap, this, args),
        "tanh" => MathTanh => |heap, this, args| call_ctx(math::math_tanh, heap, this, args),
        "asinh" => MathAsinh => |heap, this, args| call_ctx(math::math_asinh, heap, this, args),
        "acosh" => MathAcosh => |heap, this, args| call_ctx(math::math_acosh, heap, this, args),
        "atanh" => MathAtanh => |heap, this, args| call_ctx(math::math_atanh, heap, this, args),
        "hypot" => MathHypot => |heap, this, args| call_ctx(math::math_hypot, heap, this, args),
        "clz32" => MathClz32 => |heap, this, args| call_ctx(math::math_clz32, heap, this, args),
        "imul" => MathImul => |heap, this, args| call_ctx(math::math_imul, heap, this, args),
        "fround" => MathFround => |heap, this, args| call_ctx(math::math_fround, heap, this, args),
    },
    MathValue {
        "E" => MathConstE => |heap, this, args| call_ctx(math::math_const_e, heap, this, args),
        "LN2" => MathConstLn2 => |heap, this, args| call_ctx(math::math_const_ln2, heap, this, args),
        "LN10" => MathConstLn10 => |heap, this, args| call_ctx(math::math_const_ln10, heap, this, args),
        "LOG2E" => MathConstLog2e => |heap, this, args| call_ctx(math::math_const_log2e, heap, this, args),
        "LOG10E" => MathConstLog10e => |heap, this, args| call_ctx(math::math_const_log10e, heap, this, args),
        "PI" => MathConstPi => |heap, this, args| call_ctx(math::math_const_pi, heap, this, args),
        "SQRT1_2" => MathConstSqrt1_2 => |heap, this, args| call_ctx(math::math_const_sqrt1_2, heap, this, args),
        "SQRT2" => MathConstSqrt2 => |heap, this, args| call_ctx(math::math_const_sqrt2, heap, this, args),
    },
    Number {
        "isNaN" => NumberIsNan => |heap, this, args| call_ctx(number::number_is_nan, heap, this, args),
        "isFinite" => NumberIsFinite => |heap, this, args| call_ctx(number::number_is_finite, heap, this, args),
        "parseInt" => NumberParseInt => |heap, this, args| call_ctx(number::global_parse_int, heap, this, args),
        "parseFloat" => NumberParseFloat => |heap, this, args| call_ctx(number::global_parse_float, heap, this, args),
        "isInteger" => NumberIsInteger => |heap, this, args| call_ctx(number::number_is_integer, heap, this, args),
        "isSafeInteger" => NumberIsSafeInteger => |heap, this, args| call_ctx(number::number_is_safe_integer, heap, this, args),
    },
    NumberValue {
        "MAX_SAFE_INTEGER" => NumberConstMaxSafeInteger => |heap, this, args| call_ctx(number::number_const_max_safe_integer, heap, this, args),
        "MIN_SAFE_INTEGER" => NumberConstMinSafeInteger => |heap, this, args| call_ctx(number::number_const_min_safe_integer, heap, this, args),
        "EPSILON" => NumberConstEpsilon => |heap, this, args| call_ctx(number::number_const_epsilon, heap, this, args),
        "MAX_VALUE" => NumberConstMaxValue => |heap, this, args| call_ctx(number::number_const_max_value, heap, this, args),
        "MIN_VALUE" => NumberConstMinValue => |heap, this, args| call_ctx(number::number_const_min_value, heap, this, args),
        "POSITIVE_INFINITY" => NumberConstPositiveInfinity => |heap, this, args| call_ctx(number::number_const_positive_infinity, heap, this, args),
        "NEGATIVE_INFINITY" => NumberConstNegativeInfinity => |heap, this, args| call_ctx(number::number_const_negative_infinity, heap, this, args),
        "NaN" => NumberConstNaN => |heap, this, args| call_ctx(number::number_const_nan, heap, this, args),
    },
    NumberProto {
        "toString" => NumberProtoToString => |heap, this, args| call_ctx(number::number_proto_to_string, heap, this, args),
        "toFixed" => NumberToFixed => |heap, this, args| call_ctx(number::number_to_fixed, heap, this, args),
        "toPrecision" => NumberToPrecision => |heap, this, args| call_ctx(number::number_to_precision, heap, this, args),
        "toExponential" => NumberToExponential => |heap, this, args| call_ctx(number::number_to_exponential, heap, this, args),
        "valueOf" => NumberProtoValueOf => |heap, this, args| call_ctx(number::number_proto_value_of, heap, this, args),
    },
    Array {
        "isArray" => ArrayIsArray => |heap, this, args| call_ctx(array::array_is_array, heap, this, args),
    },
    ArrayProto {
        "push" => ArrayPush => |heap, this, args| call_ctx(array::array_push, heap, this, args),
        "pop" => ArrayPop => |heap, this, args| call_ctx(array::array_pop, heap, this, args),
        "join" => ArrayJoin => |heap, this, args| call_ctx(array_join, heap, this, args),
        "slice" => ArraySlice => |heap, this, args| call_ctx(array::array_slice, heap, this, args),
        "sort" => ArraySort => |heap, this, args| call_ctx(array::array_sort, heap, this, args),
        "entries" => ArrayIteratorEntries => iterator::array_iterator_entries,
        "keys" => ArrayIteratorKeys => iterator::array_iterator_keys,
        "values" => ArrayIterator => iterator::array_iterator,
        "indexOf" => ArrayIndexOf => |heap, this, args| call_ctx(array::array_index_of, heap, this, args),
        "lastIndexOf" => ArrayLastIndexOf => |heap, this, args| call_ctx(array::array_last_index_of, heap, this, args),
        "includes" => ArrayIncludes => |heap, this, args| call_ctx(array::array_includes, heap, this, args),
        "concat" => ArrayConcat => |heap, this, args| call_ctx(array::array_concat, heap, this, args),
        "at" => ArrayAt => |heap, this, args| call_ctx(array::array_at, heap, this, args),
        "reverse" => ArrayReverse => |heap, this, args| call_ctx(array::array_reverse, heap, this, args),
        "shift" => ArrayShift => |heap, this, args| call_ctx(array::array_shift, heap, this, args),
        "unshift" => ArrayUnshift => |heap, this, args| call_ctx(array::array_unshift, heap, this, args),
        "splice" => ArraySplice => |heap, this, args| call_ctx(array::array_splice, heap, this, args),
        "fill" => ArrayFill => |heap, this, args| call_ctx(array::array_fill, heap, this, args),
        "copyWithin" => ArrayCopyWithin => |heap, this, args| call_ctx(array::array_copy_within, heap, this, args),
        "flat" => ArrayFlat => |heap, this, args| call_ctx(array::array_flat, heap, this, args),
        "toString" => ArrayToString => |heap, this, args| call_ctx(array::array_to_string, heap, this, args),
        // Callback-taking methods run at the interpreter seam
        // (`Interp::run_callback_builtin`); these stubs are never dispatched
        // from JS but carry the install.
        "forEach" => ArrayForEach => callback_stub,
        "map" => ArrayMap => callback_stub,
        "filter" => ArrayFilter => callback_stub,
        "some" => ArraySome => callback_stub,
        "every" => ArrayEvery => callback_stub,
        "find" => ArrayFind => callback_stub,
        "findIndex" => ArrayFindIndex => callback_stub,
        "findLast" => ArrayFindLast => callback_stub,
        "findLastIndex" => ArrayFindLastIndex => callback_stub,
        "reduce" => ArrayReduce => callback_stub,
        "reduceRight" => ArrayReduceRight => callback_stub,
        "flatMap" => ArrayFlatMap => callback_stub,
    },
    Array {
        "of" => ArrayOf => |heap, this, args| call_ctx(array::array_of, heap, this, args),
        "from" => ArrayFrom => |heap, this, args| call_ctx(array::array_from, heap, this, args),
    },
    Object {
        "assign" => ObjectAssign => |heap, this, args| call_ctx(object::object_assign, heap, this, args),
        "is" => ObjectIs => |heap, this, args| call_ctx(object::object_is, heap, this, args),
        "hasOwn" => ObjectHasOwn => |heap, this, args| call_ctx(object::object_has_own, heap, this, args),
        "freeze" => ObjectFreeze => |heap, this, args| call_ctx(object::object_freeze, heap, this, args),
        "isFrozen" => ObjectIsFrozen => |heap, this, args| call_ctx(object::object_is_frozen, heap, this, args),
        "seal" => ObjectSeal => |heap, this, args| call_ctx(object::object_seal, heap, this, args),
        "isSealed" => ObjectIsSealed => |heap, this, args| call_ctx(object::object_is_sealed, heap, this, args),
        "preventExtensions" => ObjectPreventExtensions => |heap, this, args| call_ctx(object::object_prevent_extensions, heap, this, args),
        "isExtensible" => ObjectIsExtensible => |heap, this, args| call_ctx(object::object_is_extensible, heap, this, args),
        "fromEntries" => ObjectFromEntries => |heap, this, args| call_ctx(object::object_from_entries, heap, this, args),
        "getOwnPropertyNames" => ObjectGetOwnPropertyNames => |heap, this, args| call_ctx(object::object_get_own_property_names, heap, this, args),
        "getOwnPropertySymbols" => ObjectGetOwnPropertySymbols => |heap, this, args| call_ctx(object::object_get_own_property_symbols, heap, this, args),
        "getOwnPropertyDescriptor" => ObjectGetOwnPropertyDescriptor => |heap, this, args| call_ctx(object::object_get_own_property_descriptor, heap, this, args),
        "setPrototypeOf" => ObjectSetPrototypeOf => |heap, this, args| call_ctx(object::object_set_prototype_of, heap, this, args),
        "create" => ObjectCreate => |heap, this, args| call_ctx(object::object_create, heap, this, args),
        "getPrototypeOf" => ObjectGetPrototypeOf => |heap, this, args| call_ctx(object::object_get_prototype_of, heap, this, args),
        "defineProperty" => ObjectDefineProperty => |heap, this, args| call_ctx(object::object_define_property, heap, this, args),
        "keys" => ObjectKeys => |heap, this, args| call_ctx(object::object_keys, heap, this, args),
        "values" => ObjectValues => |heap, this, args| call_ctx(object::object_values, heap, this, args),
        "entries" => ObjectEntries => |heap, this, args| call_ctx(object::object_entries, heap, this, args),
    },
    ObjectProto {
        "hasOwnProperty" => ObjectHasOwnProperty => |heap, this, args| call_ctx(object::object_has_own_property, heap, this, args),
        "toString" => ObjectProtoToString => |heap, this, args| call_ctx(object::object_proto_to_string, heap, this, args),
        "valueOf" => ObjectProtoValueOf => |heap, this, args| call_ctx(object::object_proto_value_of, heap, this, args),
    },
    FunctionProto {
        "toString" => FunctionProtoToString => |heap, this, args| call_ctx(object::function_proto_to_string, heap, this, args),
    },
    StringProto {
        "charAt" => StringCharAt => string::string_char_at,
        "slice" => StringSlice => string::string_slice,
        "charCodeAt" => StringCharCodeAt => string::string_char_code_at,
        "codePointAt" => StringCodePointAt => string::string_code_point_at,
        "at" => StringAt => string::string_at,
        "indexOf" => StringIndexOf => string::string_index_of,
        "lastIndexOf" => StringLastIndexOf => string::string_last_index_of,
        "includes" => StringIncludes => string::string_includes,
        "startsWith" => StringStartsWith => string::string_starts_with,
        "endsWith" => StringEndsWith => string::string_ends_with,
        "concat" => StringConcat => string::string_concat,
        "repeat" => StringRepeat => string::string_repeat,
        "padStart" => StringPadStart => string::string_pad_start,
        "padEnd" => StringPadEnd => string::string_pad_end,
        "trim" => StringTrim => string::string_trim,
        "trimStart" => StringTrimStart => string::string_trim_start,
        "trimEnd" => StringTrimEnd => string::string_trim_end,
        "toLowerCase" => StringToLowerCase => string::string_to_lower_case,
        "toUpperCase" => StringToUpperCase => string::string_to_upper_case,
        "substring" => StringSubstring => string::string_substring,
        "substr" => StringSubstr => string::string_substr,
        "toString" => StringToString => string::string_to_string,
        "valueOf" => StringValueOf => string::string_value_of,
        "localeCompare" => StringLocaleCompare => string::string_locale_compare,
        "replaceAll" => StringReplaceAll => string::string_replace_all,
    },
    StringCtor {
        "fromCharCode" => StringFromCharCode => string::string_from_char_code,
        "fromCodePoint" => StringFromCodePoint => string::string_from_code_point,
    },
    Json {
        "parse" => JsonParse => json::json_parse,
        "stringify" => JsonStringify => json::json_stringify,
    },
    BooleanProto {
        "toString" => BooleanProtoToString => |heap, this, args| call_ctx(boolean::boolean_proto_to_string, heap, this, args),
        "valueOf" => BooleanProtoValueOf => |heap, this, args| call_ctx(boolean::boolean_proto_value_of, heap, this, args),
    },
    Symbol {
        "for" => SymbolFor => symbol::symbol_for,
        "keyFor" => SymbolKeyFor => symbol::symbol_key_for,
        "iterator" => SymbolWellKnownIterator => symbol::symbol_well_known,
        "asyncIterator" => SymbolWellKnownAsyncIterator => symbol::symbol_well_known,
        "hasInstance" => SymbolWellKnownHasInstance => symbol::symbol_well_known,
        "isConcatSpreadable" => SymbolWellKnownIsConcatSpreadable => symbol::symbol_well_known,
        "match" => SymbolWellKnownMatch => symbol::symbol_well_known,
        "replace" => SymbolWellKnownReplace => symbol::symbol_well_known,
        "search" => SymbolWellKnownSearch => symbol::symbol_well_known,
        "species" => SymbolWellKnownSpecies => symbol::symbol_well_known,
        "split" => SymbolWellKnownSplit => symbol::symbol_well_known,
        "toPrimitive" => SymbolWellKnownToPrimitive => symbol::symbol_well_known,
        "toStringTag" => SymbolWellKnownToStringTag => symbol::symbol_well_known,
        "unscopables" => SymbolWellKnownUnscopables => symbol::symbol_well_known,
    },
    SymbolProto {
        "toString" => SymbolProtoToString => symbol::symbol_proto_to_string,
        "valueOf" => SymbolProtoValueOf => symbol::symbol_proto_value_of,
        "description" => SymbolProtoDescription => symbol::symbol_proto_description,
    };
    // Truly internal / non-JS-visible dispatch-only natives (not installed).
    StringConstruct => string_construct,
    NumberConstruct => |heap, this, args| call_ctx(number::number_construct, heap, this, args),
    BooleanConstruct => |heap, this, args| call_ctx(boolean::boolean_construct, heap, this, args),
    ErrorCreate => error::error_create,
    Eval => eval_stub,
    Function => function_stub,
    ConsoleLog => console_log,
    MapConstruct => map::map_construct,
    MapGet => map::map_get,
    MapSet => map::map_set,
    MapHas => map::map_has,
    MapDelete => map::map_delete,
    MapSize => map::map_size,
    MapClear => map::map_clear,
    MapEntries => map::map_entries,
    MapKeys => map::map_keys,
    MapValues => map::map_values,
    MapForEach => callback_stub,
    SetConstruct => map::set_construct,
    SetAdd => map::set_add,
    SetHas => map::set_has,
    SetDelete => map::set_delete,
    SetSize => map::set_size,
    SetClear => map::set_clear,
    SetEntries => map::set_entries,
    SetKeys => map::set_keys,
    SetValues => map::set_values,
    SetForEach => callback_stub,
    IteratorNext => iterator::iterator_next,
    IteratorToArray => iterator::iterator_to_array,
    IteratorTake => iterator::iterator_take,
    IteratorDrop => iterator::iterator_drop,
    IteratorFrom => iterator::iterator_from,
    IteratorMap => callback_stub,
    IteratorFilter => callback_stub,
    IteratorFlatMap => callback_stub,
    IteratorReduce => callback_stub,
    IteratorForEach => callback_stub,
    IteratorSome => callback_stub,
    IteratorEvery => callback_stub,
    IteratorFind => callback_stub,
    SymbolConstruct => symbol::symbol_construct,
    MapIterator => iterator::map_iterator,
    SetIterator => iterator::set_iterator,
    IteratorSelf => iterator::iterator_self,
    RegExpConstruct => regexp::regexp_construct,
    RegExpToString => regexp::regexp_to_string,
    ModuleImport => module_import,
    ObjectEnumerableOwnKeys => |heap, this, args| call_ctx(object::object_enumerable_own_keys, heap, this, args),
}

/// Installs the core built-ins into `registry`.
///
/// `String(x)`: ES ToString subset for the callable `String` intrinsic.
/// The realm points the `String` placeholder's `elements[0]` at this index.
fn string_construct(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = match args.first() {
        Some(&v) => helpers::value_text(heap, v),
        None => "undefined".to_string(),
    };
    Ok(JsValue::string(heap.intern_text(&text)))
}

/// `Array.prototype.join(separator?)`: element display strings joined by
/// `separator` (default `","`). `undefined`/`null` elements render empty,
/// matching ES `Array.prototype.join`.
fn array_join(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let Some(arr) = this.as_object() else {
        return Err(ctx.type_error("TypeError: Array.prototype.join requires an array"));
    };
    let sep = match args.first() {
        Some(&v) if !v.is_undefined() => ctx.to_string(v),
        _ => ",".to_string(),
    };
    // Snapshot before formatting: the display helpers may allocate (and thus
    // collect), invalidating a live borrow of the element store.
    let heap = &mut *ctx.heap;
    let elements: Vec<JsValue> = heap.get(arr).elements_snapshot();
    let mut parts = Vec::with_capacity(elements.len());
    for &v in &elements {
        if v.is_undefined() || v.is_null() {
            parts.push(String::new());
        } else {
            parts.push(helpers::value_text(heap, v));
        }
    }
    let text = parts.join(&sep);
    Ok(JsValue::string(heap.intern_text(&text)))
}

/// Placeholder handler for the callback-taking built-ins (`map`, `forEach`,
/// …). Calls from JS are intercepted at the interpreter seam
/// (`Interp::run_callback_builtin`) before any dispatch happens, so this is
/// unreachable in practice — it exists only so `define_builtins!` can carry
/// the install.
fn callback_stub(heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    Err(Throw::type_error(
        heap,
        "TypeError: built-in method must be invoked through the interpreter",
    ))
}

fn eval_stub(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    // v1 stub: non-string args return as-is; string args are syntax-checked
    // via the compiler and return `undefined` on success. The full
    // heap-sharing `eval` path is exercised via `Engine::eval_direct`.
    if let Some(first) = args.first() {
        if let Some(h) = first.as_string() {
            let text = helpers::string_text(heap, h);
            if let Err(err) = v12_bccompiler::compile_source_with_strings(&text) {
                let msg = err.message;
                let handle = heap.intern_text(&msg);
                return Err((JsValue::string(handle)).into());
            }
            Ok(JsValue::undefined())
        } else {
            Ok(*first)
        }
    } else {
        Ok(JsValue::undefined())
    }
}

fn function_stub(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    // v1 stub for `new Function`: validate syntax and return a placeholder
    // function object. Full compilation is via `Engine::create_function`.
    if args.is_empty() {
        let func = helpers::alloc_obj(
            heap,
            v12_heap::JsObject::function(v12_heap::FunctionTarget::Bytecode(0), None),
        );
        return Ok(JsValue::object(func));
    }
    let mut param_parts = Vec::new();
    for &arg in &args[..args.len() - 1] {
        if let Some(h) = arg.as_string() {
            param_parts.push(helpers::string_text(heap, h));
        }
    }
    let param_str = param_parts.join(",");
    let body = args
        .last()
        .and_then(|v| v.as_string())
        .map(|h| helpers::string_text(heap, h))
        .unwrap_or_default();
    let src = format!("function __f({param_str}){{{body}}}");
    if let Err(err) = v12_bccompiler::compile_source_with_strings(&src) {
        let msg = err.message;
        let handle = heap.intern_text(&msg);
        return Err((JsValue::string(handle)).into());
    }
    let func = helpers::alloc_obj(
        heap,
        v12_heap::JsObject::function(v12_heap::FunctionTarget::Bytecode(1), None),
    );
    Ok(JsValue::object(func))
}

fn console_log(heap: &mut Heap, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let mut parts = Vec::with_capacity(args.len());
    for &v in args {
        parts.push(helpers::value_text(heap, v));
    }
    println!("{}", parts.join(" "));
    Ok(JsValue::undefined())
}

fn module_import(heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    // Spec shape: `import()` returns a promise. There is no module loader
    // yet (ModuleMap/resolve/link/evaluate are v1 work-in-progress), so the
    // promise rejects — `import(x).catch(...)` observes a real rejection
    // instead of a synchronous throw.
    let err = Throw::type_error(heap, "dynamic import: no module loader in this context");
    match err {
        Throw::Value(v) => Ok(promise::make_rejected_promise(heap, v)),
        other => Err(other),
    }
}

fn intern_type_error(heap: &mut Heap, msg: &str) -> JsValue {
    let h = heap.intern_string(v12_heap::V12Str::latin1(msg.as_bytes().to_vec()));
    JsValue::string(h)
}
