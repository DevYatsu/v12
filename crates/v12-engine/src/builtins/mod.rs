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
pub mod proxy;
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
    /// The `Proxy` constructor. Statics install on it (`Proxy.revocable`);
    /// `Proxy` intentionally has no `prototype` (test262
    /// `built-ins/Proxy/proxy-no-prototype.js`), so there is no
    /// `proxy_proto` field.
    pub proxy: Option<v12_heap::Handle<v12_heap::JsObject>>,
}

pub(crate) fn builtin_install_prop(
    heap: &mut Heap,
    obj: v12_heap::Handle<v12_heap::JsObject>,
    name: &str,
    value: JsValue,
) {
    let mut ctx = Ctx::new(heap, None, None);
    ctx.define_data_prop(obj, name, value);
}

/// Declared arity table for `define_builtins!` entries. `None` = legacy
/// entry without an explicit `(len)` yet: no `length` prop is installed,
/// preserving current observable behavior. Entries that gain `(len)` in the
/// macro get `Some(len)` here and the install family stamps the prop.
pub fn builtin_length(id: NativeId) -> Option<u32> {
    match id {
        NativeId::ArrayAt => Some(1),
        NativeId::ArrayConcat => Some(1),
        NativeId::ArrayCopyWithin => Some(2),
        NativeId::ArrayEvery => Some(1),
        NativeId::ArrayFill => Some(1),
        NativeId::ArrayFilter => Some(1),
        NativeId::ArrayFind => Some(1),
        NativeId::ArrayFindIndex => Some(1),
        NativeId::ArrayFindLast => Some(1),
        NativeId::ArrayFindLastIndex => Some(1),
        NativeId::ArrayFlat => Some(0),
        NativeId::ArrayFlatMap => Some(1),
        NativeId::ArrayForEach => Some(1),
        NativeId::ArrayFrom => Some(1),
        NativeId::ArrayIncludes => Some(1),
        NativeId::ArrayIndexOf => Some(1),
        NativeId::ArrayIsArray => Some(1),
        NativeId::ArrayIterator => Some(0),
        NativeId::ArrayIteratorEntries => Some(0),
        NativeId::ArrayIteratorKeys => Some(0),
        NativeId::ArrayJoin => Some(1),
        NativeId::ArrayLastIndexOf => Some(1),
        NativeId::ArrayMap => Some(1),
        NativeId::ArrayOf => Some(0),
        NativeId::ArrayPop => Some(0),
        NativeId::ArrayPush => Some(1),
        NativeId::ArrayReduce => Some(1),
        NativeId::ArrayReduceRight => Some(1),
        NativeId::ArrayReverse => Some(0),
        NativeId::ArrayShift => Some(0),
        NativeId::ArraySlice => Some(2),
        NativeId::ArraySome => Some(1),
        NativeId::ArraySort => Some(1),
        NativeId::ArraySplice => Some(2),
        NativeId::ArrayToString => Some(0),
        NativeId::ArrayUnshift => Some(1),
        NativeId::BooleanProtoToString => Some(0),
        NativeId::BooleanProtoValueOf => Some(0),
        NativeId::FunctionProtoToString => Some(0),
        NativeId::GlobalDecodeUri => Some(1),
        NativeId::GlobalDecodeUriComponent => Some(1),
        NativeId::GlobalEncodeUri => Some(1),
        NativeId::GlobalEncodeUriComponent => Some(1),
        NativeId::GlobalIsFinite => Some(1),
        NativeId::GlobalIsNaN => Some(1),
        NativeId::GlobalParseFloat => Some(1),
        NativeId::GlobalParseInt => Some(2),
        NativeId::JsonParse => Some(2),
        NativeId::JsonStringify => Some(3),
        NativeId::MathAbs => Some(1),
        NativeId::MathAcos => Some(1),
        NativeId::MathAcosh => Some(1),
        NativeId::MathAsin => Some(1),
        NativeId::MathAsinh => Some(1),
        NativeId::MathAtan => Some(1),
        NativeId::MathAtan2 => Some(2),
        NativeId::MathAtanh => Some(1),
        NativeId::MathCbrt => Some(1),
        NativeId::MathCeil => Some(1),
        NativeId::MathClz32 => Some(1),
        NativeId::MathCos => Some(1),
        NativeId::MathCosh => Some(1),
        NativeId::MathExp => Some(1),
        NativeId::MathExpm1 => Some(1),
        NativeId::MathFloor => Some(1),
        NativeId::MathFround => Some(1),
        NativeId::MathHypot => Some(2),
        NativeId::MathImul => Some(2),
        NativeId::MathLog => Some(1),
        NativeId::MathLog10 => Some(1),
        NativeId::MathLog1p => Some(1),
        NativeId::MathLog2 => Some(1),
        NativeId::MathMax => Some(2),
        NativeId::MathMin => Some(2),
        NativeId::MathPow => Some(2),
        NativeId::MathRandom => Some(0),
        NativeId::MathRound => Some(1),
        NativeId::MathSign => Some(1),
        NativeId::MathSin => Some(1),
        NativeId::MathSinh => Some(1),
        NativeId::MathSqrt => Some(1),
        NativeId::MathTan => Some(1),
        NativeId::MathTanh => Some(1),
        NativeId::MathTrunc => Some(1),
        NativeId::NumberIsFinite => Some(1),
        NativeId::NumberIsInteger => Some(1),
        NativeId::NumberIsNan => Some(1),
        NativeId::NumberIsSafeInteger => Some(1),
        NativeId::NumberParseFloat => Some(1),
        NativeId::NumberParseInt => Some(2),
        NativeId::NumberProtoToString => Some(1),
        NativeId::NumberProtoValueOf => Some(0),
        NativeId::NumberToExponential => Some(1),
        NativeId::NumberToFixed => Some(1),
        NativeId::NumberToPrecision => Some(1),
        NativeId::ObjectAssign => Some(2),
        NativeId::ObjectCreate => Some(1),
        NativeId::ObjectDefineProperty => Some(3),
        NativeId::ObjectEntries => Some(1),
        NativeId::ObjectFreeze => Some(1),
        NativeId::ObjectFromEntries => Some(1),
        NativeId::ObjectGetOwnPropertyDescriptor => Some(2),
        NativeId::ObjectGetOwnPropertyNames => Some(1),
        NativeId::ObjectGetOwnPropertySymbols => Some(1),
        NativeId::ObjectProtoPropertyIsEnumerable => Some(1),
        NativeId::ObjectGetPrototypeOf => Some(1),
        NativeId::ObjectHasOwn => Some(2),
        NativeId::ObjectHasOwnProperty => Some(1),
        NativeId::ObjectIs => Some(2),
        NativeId::ObjectIsExtensible => Some(1),
        NativeId::ObjectIsFrozen => Some(1),
        NativeId::ObjectIsSealed => Some(1),
        NativeId::ObjectKeys => Some(1),
        NativeId::ObjectPreventExtensions => Some(1),
        NativeId::ObjectProtoToString => Some(0),
        NativeId::ObjectProtoValueOf => Some(0),
        NativeId::ObjectSeal => Some(1),
        NativeId::ObjectSetPrototypeOf => Some(2),
        NativeId::ObjectValues => Some(1),
        NativeId::StringAt => Some(1),
        NativeId::StringCharAt => Some(1),
        NativeId::StringCharCodeAt => Some(1),
        NativeId::StringCodePointAt => Some(1),
        NativeId::StringConcat => Some(1),
        NativeId::StringEndsWith => Some(1),
        NativeId::StringFromCharCode => Some(1),
        NativeId::StringFromCodePoint => Some(1),
        NativeId::StringIncludes => Some(1),
        NativeId::StringIndexOf => Some(1),
        NativeId::StringLastIndexOf => Some(1),
        NativeId::StringLocaleCompare => Some(1),
        NativeId::StringPadEnd => Some(1),
        NativeId::StringPadStart => Some(1),
        NativeId::StringRepeat => Some(1),
        NativeId::StringReplaceAll => Some(2),
        NativeId::StringSlice => Some(2),
        NativeId::StringStartsWith => Some(1),
        NativeId::StringSubstr => Some(2),
        NativeId::StringSubstring => Some(2),
        NativeId::StringToLowerCase => Some(0),
        NativeId::StringToString => Some(0),
        NativeId::StringToUpperCase => Some(0),
        NativeId::StringTrim => Some(0),
        NativeId::StringTrimEnd => Some(0),
        NativeId::StringTrimStart => Some(0),
        NativeId::StringValueOf => Some(0),
        NativeId::SymbolFor => Some(1),
        NativeId::SymbolKeyFor => Some(1),
        NativeId::SymbolProtoToString => Some(0),
        NativeId::SymbolProtoValueOf => Some(0),
        NativeId::ProxyRevocable => Some(2),
        _ => None,
    }
}

/// Unified install: single canonical path for every builtin function
/// property. `install_native` (legacy name) delegates here with
/// `length = builtin_length(id)` so grouped installs and hand-rolled sites
/// share one code path.
pub(crate) fn install_native_with_length(
    heap: &mut Heap,
    target: Option<v12_heap::Handle<v12_heap::JsObject>>,
    name: &str,
    id: NativeId,
    length: Option<u32>,
) -> Option<v12_heap::Handle<v12_heap::JsObject>> {
    let mut ctx = Ctx::new(heap, None, None);
    ctx.define_method(target, name, id, length)
}

/// Allocates the native function object for `id` and installs it as a
/// shape-bound property `name` on `target`.
///
/// A `None` target installs nothing: optional constructors that this realm
/// has not materialized, and the reserved future hosts (`Json`, `Map`, …)
/// whose target fields do not exist yet. Retired as the canonical path —
/// it now delegates to the unified [`Ctx::define_method`] family so there
/// is exactly one install shape; kept (not deleted) because `realm.rs` and
/// hand-rolled sites still call it.
pub(crate) fn install_native(
    heap: &mut Heap,
    target: Option<v12_heap::Handle<v12_heap::JsObject>>,
    name: &str,
    id: NativeId,
) -> Option<v12_heap::Handle<v12_heap::JsObject>> {
    let length = builtin_length(id);
    install_native_with_length(heap, target, name, id, length)
}

/// Constructor/prototype linkage for an already-materialized pair (realm
/// placeholders). Thin wrapper over [`Ctx::install_ctor_link`] so realm
/// construction routes through the install family instead of per-site
/// `builtin_install_prop("prototype", …)` calls.
pub(crate) fn install_ctor(
    heap: &mut Heap,
    ctor: v12_heap::Handle<v12_heap::JsObject>,
    proto: v12_heap::Handle<v12_heap::JsObject>,
) {
    let mut ctx = Ctx::new(heap, None, None);
    ctx.install_ctor_link(ctor, proto);
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
    (Proxy, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.proxy, $name, $id)
    };
    (SymbolProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.symbol_proto), $name, $id)
    };
    // Explicit-length forms (plan §5 step 4): `define_builtins!` entries may
    // declare `"name" (len) => Id => handler`. These route through
    // `install_native_with_length` with `Some(len)` so `length` is stamped at
    // install time; entries without `(len)` keep the 5-arg form (no `length`
    // prop — current observable behavior) until their arity is audited.
    (Global, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, Some($targets.global), $name, $id, Some($len))
    };
    (NumberProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, Some($targets.number_proto), $name, $id, Some($len))
    };
    (StringProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, Some($targets.string_proto), $name, $id, Some($len))
    };
    (ArrayProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, Some($targets.array_proto), $name, $id, Some($len))
    };
    (ObjectProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, Some($targets.object_proto), $name, $id, Some($len))
    };
    (FunctionProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, Some($targets.function_proto), $name, $id, Some($len))
    };
    (Math, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, $targets.math, $name, $id, Some($len))
    };
    (Number, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, $targets.number, $name, $id, Some($len))
    };
    (Array, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, $targets.array, $name, $id, Some($len))
    };
    (Object, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, $targets.object, $name, $id, Some($len))
    };
    (Json, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, $targets.json, $name, $id, Some($len))
    };
    (BooleanProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, Some($targets.boolean_proto), $name, $id, Some($len))
    };
    (StringCtor, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, $targets.string, $name, $id, Some($len))
    };
    (Symbol, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, $targets.symbol, $name, $id, Some($len))
    };
    (SymbolProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, Some($targets.symbol_proto), $name, $id, Some($len))
    };
    (Proxy, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, $targets.proxy, $name, $id, Some($len))
    };
    // Value-constant groups ignore length (constants, not functions).
    (GlobalValue, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        { let _ = $len; $crate::builtins::install_value($heap, Some($targets.global), $name, $id) }
    };
    (MathValue, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        { let _ = $len; $crate::builtins::install_value($heap, $targets.math, $name, $id) }
    };
    (NumberValue, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        { let _ = $len; $crate::builtins::install_value($heap, $targets.number, $name, $id) }
    };
}

/// Unified builtin declaration: single source of truth for dispatch + install.
///
/// *Grouped* entries `Target { "jsName" [(len)] => Variant => handler }`
/// emit both a `builtin_dispatch` match arm and a straight-line install call.
/// The optional `(len)` declares the ES `length` arity: entries carrying it
/// route through `install_native_with_length` with `Some(len)` (the install
/// family stamps spec-attr `length` + `name` on the function object);
/// entries without it install no `length` prop until their arity is audited
/// (current observable behavior preserved).
/// Grouped targets distinguish **static** (constructor) vs **dynamic**
/// (prototype) installs: `Array { "isArray" => ... }` installs on the `Array`
/// constructor, `ArrayProto { "push" => ... }` installs on `Array.prototype`.
/// No intermediate `BUILTIN_INSTALLS` array is stored — the macro expands to
/// direct `install_prop` calls (zero rodata, no iteration).
/// *Bare* entries `Variant => handler` (after `;`) emit only a dispatch arm
/// for truly internal / non-JS-visible natives (e.g. `ModuleImport`,
/// `MapSize`, `IteratorNext`). They are not installed on any JS object.
///
/// Example:
/// ```ignore
/// define_builtins! {
///     Global { "isNaN" => GlobalIsNaN => number::global_is_nan },
///     Math { "floor" => MathFloor => math::math_floor },
///     Array { "isArray" => ArrayIsArray => array::array_is_array },
///     ArrayProto { "push" => ArrayPush => array::array_push };
///     ModuleImport => module_import,
/// }
/// ```
macro_rules! define_builtins {
    (
        $( $target:ident { $($name:literal $(($len:literal))? => $id:ident => $handler:expr),* $(,)? } ),* $(,)? ;
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
        /// grouped entry expands to a straight-line install call through the
        /// unified [`Ctx::define_method`] family (`install_native` for legacy
        /// entries, `install_native_with_length` for `(len)` entries), so the
        /// compiler can inline and no rodata table is emitted.
        pub fn install_builtins(heap: &mut Heap, targets: &BuiltinTargets) {
            $( $( $crate::__builtin_emit_install!($target, heap, targets, $name, NativeId::$id $(, $len)?); )* )*
            // Bare ids are dispatch-only; silence unused warnings.
            $( let _ = NativeId::$bare_id; )*
        }
    };
}

define_builtins! {
    Global {
        "isNaN" (1) => GlobalIsNaN => |heap, this, args| call_ctx(number::global_is_nan, heap, this, args),
        "isFinite" (1) => GlobalIsFinite => |heap, this, args| call_ctx(number::global_is_finite, heap, this, args),
        "parseInt" (2) => GlobalParseInt => |heap, this, args| call_ctx(number::global_parse_int, heap, this, args),
        "parseFloat" (1) => GlobalParseFloat => |heap, this, args| call_ctx(number::global_parse_float, heap, this, args),
        "encodeURI" (1) => GlobalEncodeUri => |heap, this, args| call_ctx(global::global_encode_uri, heap, this, args),
        "decodeURI" (1) => GlobalDecodeUri => |heap, this, args| call_ctx(global::global_decode_uri, heap, this, args),
        "encodeURIComponent" (1) => GlobalEncodeUriComponent => |heap, this, args| call_ctx(global::global_encode_uri_component, heap, this, args),
        "decodeURIComponent" (1) => GlobalDecodeUriComponent => |heap, this, args| call_ctx(global::global_decode_uri_component, heap, this, args),
    },
    Math {
        "abs" (1) => MathAbs => |heap, this, args| call_ctx(math::math_abs, heap, this, args),
        "floor" (1) => MathFloor => |heap, this, args| call_ctx(math::math_floor, heap, this, args),
        "ceil" (1) => MathCeil => |heap, this, args| call_ctx(math::math_ceil, heap, this, args),
        "trunc" (1) => MathTrunc => |heap, this, args| call_ctx(math::math_trunc, heap, this, args),
        "pow" (2) => MathPow => |heap, this, args| call_ctx(math::math_pow, heap, this, args),
        "max" (2) => MathMax => |heap, this, args| call_ctx(math::math_max, heap, this, args),
        "min" (2) => MathMin => |heap, this, args| call_ctx(math::math_min, heap, this, args),
        "random" (0) => MathRandom => |heap, this, args| call_ctx(math::math_random, heap, this, args),
        "round" (1) => MathRound => |heap, this, args| call_ctx(math::math_round, heap, this, args),
        "sqrt" (1) => MathSqrt => |heap, this, args| call_ctx(math::math_sqrt, heap, this, args),
        "sign" (1) => MathSign => |heap, this, args| call_ctx(math::math_sign, heap, this, args),
        "cbrt" (1) => MathCbrt => |heap, this, args| call_ctx(math::math_cbrt, heap, this, args),
        "exp" (1) => MathExp => |heap, this, args| call_ctx(math::math_exp, heap, this, args),
        "expm1" (1) => MathExpm1 => |heap, this, args| call_ctx(math::math_expm1, heap, this, args),
        "log" (1) => MathLog => |heap, this, args| call_ctx(math::math_log, heap, this, args),
        "log1p" (1) => MathLog1p => |heap, this, args| call_ctx(math::math_log1p, heap, this, args),
        "log2" (1) => MathLog2 => |heap, this, args| call_ctx(math::math_log2, heap, this, args),
        "log10" (1) => MathLog10 => |heap, this, args| call_ctx(math::math_log10, heap, this, args),
        "sin" (1) => MathSin => |heap, this, args| call_ctx(math::math_sin, heap, this, args),
        "cos" (1) => MathCos => |heap, this, args| call_ctx(math::math_cos, heap, this, args),
        "tan" (1) => MathTan => |heap, this, args| call_ctx(math::math_tan, heap, this, args),
        "asin" (1) => MathAsin => |heap, this, args| call_ctx(math::math_asin, heap, this, args),
        "acos" (1) => MathAcos => |heap, this, args| call_ctx(math::math_acos, heap, this, args),
        "atan" (1) => MathAtan => |heap, this, args| call_ctx(math::math_atan, heap, this, args),
        "atan2" (2) => MathAtan2 => |heap, this, args| call_ctx(math::math_atan2, heap, this, args),
        "sinh" (1) => MathSinh => |heap, this, args| call_ctx(math::math_sinh, heap, this, args),
        "cosh" (1) => MathCosh => |heap, this, args| call_ctx(math::math_cosh, heap, this, args),
        "tanh" (1) => MathTanh => |heap, this, args| call_ctx(math::math_tanh, heap, this, args),
        "asinh" (1) => MathAsinh => |heap, this, args| call_ctx(math::math_asinh, heap, this, args),
        "acosh" (1) => MathAcosh => |heap, this, args| call_ctx(math::math_acosh, heap, this, args),
        "atanh" (1) => MathAtanh => |heap, this, args| call_ctx(math::math_atanh, heap, this, args),
        "hypot" (2) => MathHypot => |heap, this, args| call_ctx(math::math_hypot, heap, this, args),
        "clz32" (1) => MathClz32 => |heap, this, args| call_ctx(math::math_clz32, heap, this, args),
        "imul" (2) => MathImul => |heap, this, args| call_ctx(math::math_imul, heap, this, args),
        "fround" (1) => MathFround => |heap, this, args| call_ctx(math::math_fround, heap, this, args),
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
        "isNaN" (1) => NumberIsNan => |heap, this, args| call_ctx(number::number_is_nan, heap, this, args),
        "isFinite" (1) => NumberIsFinite => |heap, this, args| call_ctx(number::number_is_finite, heap, this, args),
        "parseInt" (2) => NumberParseInt => |heap, this, args| call_ctx(number::global_parse_int, heap, this, args),
        "parseFloat" (1) => NumberParseFloat => |heap, this, args| call_ctx(number::global_parse_float, heap, this, args),
        "isInteger" (1) => NumberIsInteger => |heap, this, args| call_ctx(number::number_is_integer, heap, this, args),
        "isSafeInteger" (1) => NumberIsSafeInteger => |heap, this, args| call_ctx(number::number_is_safe_integer, heap, this, args),
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
        "toString" (1) => NumberProtoToString => |heap, this, args| call_ctx(number::number_proto_to_string, heap, this, args),
        "toFixed" (1) => NumberToFixed => |heap, this, args| call_ctx(number::number_to_fixed, heap, this, args),
        "toPrecision" (1) => NumberToPrecision => |heap, this, args| call_ctx(number::number_to_precision, heap, this, args),
        "toExponential" (1) => NumberToExponential => |heap, this, args| call_ctx(number::number_to_exponential, heap, this, args),
        "valueOf" (0) => NumberProtoValueOf => |heap, this, args| call_ctx(number::number_proto_value_of, heap, this, args),
    },
    Array {
        "isArray" (1) => ArrayIsArray => |heap, this, args| call_ctx(array::array_is_array, heap, this, args),
    },
    ArrayProto {
        "push" (1) => ArrayPush => |heap, this, args| call_ctx(array::array_push, heap, this, args),
        "pop" (0) => ArrayPop => |heap, this, args| call_ctx(array::array_pop, heap, this, args),
        "join" (1) => ArrayJoin => |heap, this, args| call_ctx(array_join, heap, this, args),
        "slice" (2) => ArraySlice => |heap, this, args| call_ctx(array::array_slice, heap, this, args),
        "sort" (1) => ArraySort => |heap, this, args| call_ctx(array::array_sort, heap, this, args),
        "entries" (0) => ArrayIteratorEntries => |heap, this, args| call_ctx(iterator::array_iterator_entries, heap, this, args),
        "keys" (0) => ArrayIteratorKeys => |heap, this, args| call_ctx(iterator::array_iterator_keys, heap, this, args),
        "values" (0) => ArrayIterator => |heap, this, args| call_ctx(iterator::array_iterator, heap, this, args),
        "indexOf" (1) => ArrayIndexOf => |heap, this, args| call_ctx(array::array_index_of, heap, this, args),
        "lastIndexOf" (1) => ArrayLastIndexOf => |heap, this, args| call_ctx(array::array_last_index_of, heap, this, args),
        "includes" (1) => ArrayIncludes => |heap, this, args| call_ctx(array::array_includes, heap, this, args),
        "concat" (1) => ArrayConcat => |heap, this, args| call_ctx(array::array_concat, heap, this, args),
        "at" (1) => ArrayAt => |heap, this, args| call_ctx(array::array_at, heap, this, args),
        "reverse" (0) => ArrayReverse => |heap, this, args| call_ctx(array::array_reverse, heap, this, args),
        "shift" (0) => ArrayShift => |heap, this, args| call_ctx(array::array_shift, heap, this, args),
        "unshift" (1) => ArrayUnshift => |heap, this, args| call_ctx(array::array_unshift, heap, this, args),
        "splice" (2) => ArraySplice => |heap, this, args| call_ctx(array::array_splice, heap, this, args),
        "fill" (1) => ArrayFill => |heap, this, args| call_ctx(array::array_fill, heap, this, args),
        "copyWithin" (2) => ArrayCopyWithin => |heap, this, args| call_ctx(array::array_copy_within, heap, this, args),
        "flat" (0) => ArrayFlat => |heap, this, args| call_ctx(array::array_flat, heap, this, args),
        "toString" (0) => ArrayToString => |heap, this, args| call_ctx(array::array_to_string, heap, this, args),
        // Callback-taking methods run at the interpreter seam
        // (`Interp::run_callback_builtin`); these stubs are never dispatched
        // from JS but carry the install.
        "forEach" (1) => ArrayForEach => callback_stub,
        "map" (1) => ArrayMap => callback_stub,
        "filter" (1) => ArrayFilter => callback_stub,
        "some" (1) => ArraySome => callback_stub,
        "every" (1) => ArrayEvery => callback_stub,
        "find" (1) => ArrayFind => callback_stub,
        "findIndex" (1) => ArrayFindIndex => callback_stub,
        "findLast" (1) => ArrayFindLast => callback_stub,
        "findLastIndex" (1) => ArrayFindLastIndex => callback_stub,
        "reduce" (1) => ArrayReduce => callback_stub,
        "reduceRight" (1) => ArrayReduceRight => callback_stub,
        "flatMap" (1) => ArrayFlatMap => callback_stub,
    },
    Array {
        "of" (0) => ArrayOf => |heap, this, args| call_ctx(array::array_of, heap, this, args),
        "from" (1) => ArrayFrom => |heap, this, args| call_ctx(array::array_from, heap, this, args),
    },
    Object {
        "assign" (2) => ObjectAssign => |heap, this, args| call_ctx(object::object_assign, heap, this, args),
        "is" (2) => ObjectIs => |heap, this, args| call_ctx(object::object_is, heap, this, args),
        "hasOwn" (2) => ObjectHasOwn => |heap, this, args| call_ctx(object::object_has_own, heap, this, args),
        "freeze" (1) => ObjectFreeze => |heap, this, args| call_ctx(object::object_freeze, heap, this, args),
        "isFrozen" (1) => ObjectIsFrozen => |heap, this, args| call_ctx(object::object_is_frozen, heap, this, args),
        "seal" (1) => ObjectSeal => |heap, this, args| call_ctx(object::object_seal, heap, this, args),
        "isSealed" (1) => ObjectIsSealed => |heap, this, args| call_ctx(object::object_is_sealed, heap, this, args),
        "preventExtensions" (1) => ObjectPreventExtensions => |heap, this, args| call_ctx(object::object_prevent_extensions, heap, this, args),
        "isExtensible" (1) => ObjectIsExtensible => |heap, this, args| call_ctx(object::object_is_extensible, heap, this, args),
        "fromEntries" (1) => ObjectFromEntries => |heap, this, args| call_ctx(object::object_from_entries, heap, this, args),
        "getOwnPropertyNames" (1) => ObjectGetOwnPropertyNames => |heap, this, args| call_ctx(object::object_get_own_property_names, heap, this, args),
        "getOwnPropertySymbols" (1) => ObjectGetOwnPropertySymbols => |heap, this, args| call_ctx(object::object_get_own_property_symbols, heap, this, args),
        "getOwnPropertyDescriptor" (2) => ObjectGetOwnPropertyDescriptor => |heap, this, args| call_ctx(object::object_get_own_property_descriptor, heap, this, args),
        "setPrototypeOf" (2) => ObjectSetPrototypeOf => |heap, this, args| call_ctx(object::object_set_prototype_of, heap, this, args),
        "create" (1) => ObjectCreate => |heap, this, args| call_ctx(object::object_create, heap, this, args),
        "getPrototypeOf" (1) => ObjectGetPrototypeOf => |heap, this, args| call_ctx(object::object_get_prototype_of, heap, this, args),
        "defineProperty" (3) => ObjectDefineProperty => |heap, this, args| call_ctx(object::object_define_property, heap, this, args),
        "keys" (1) => ObjectKeys => |heap, this, args| call_ctx(object::object_keys, heap, this, args),
        "values" (1) => ObjectValues => |heap, this, args| call_ctx(object::object_values, heap, this, args),
        "entries" (1) => ObjectEntries => |heap, this, args| call_ctx(object::object_entries, heap, this, args),
    },
    ObjectProto {
        "hasOwnProperty" (1) => ObjectHasOwnProperty => |heap, this, args| call_ctx(object::object_has_own_property, heap, this, args),
        "propertyIsEnumerable" (1) => ObjectProtoPropertyIsEnumerable => |heap, this, args| call_ctx(object::object_proto_property_is_enumerable, heap, this, args),
        "toString" (0) => ObjectProtoToString => |heap, this, args| call_ctx(object::object_proto_to_string, heap, this, args),
        "valueOf" (0) => ObjectProtoValueOf => |heap, this, args| call_ctx(object::object_proto_value_of, heap, this, args),
    },
    FunctionProto {
        "toString" (0) => FunctionProtoToString => |heap, this, args| call_ctx(object::function_proto_to_string, heap, this, args),
    },
    StringProto {
        "charAt" (1) => StringCharAt => |heap, this, args| call_ctx(string::string_char_at, heap, this, args),
        "slice" (2) => StringSlice => |heap, this, args| call_ctx(string::string_slice, heap, this, args),
        "charCodeAt" (1) => StringCharCodeAt => |heap, this, args| call_ctx(string::string_char_code_at, heap, this, args),
        "codePointAt" (1) => StringCodePointAt => |heap, this, args| call_ctx(string::string_code_point_at, heap, this, args),
        "at" (1) => StringAt => |heap, this, args| call_ctx(string::string_at, heap, this, args),
        "indexOf" (1) => StringIndexOf => |heap, this, args| call_ctx(string::string_index_of, heap, this, args),
        "lastIndexOf" (1) => StringLastIndexOf => |heap, this, args| call_ctx(string::string_last_index_of, heap, this, args),
        "includes" (1) => StringIncludes => |heap, this, args| call_ctx(string::string_includes, heap, this, args),
        "startsWith" (1) => StringStartsWith => |heap, this, args| call_ctx(string::string_starts_with, heap, this, args),
        "endsWith" (1) => StringEndsWith => |heap, this, args| call_ctx(string::string_ends_with, heap, this, args),
        "concat" (1) => StringConcat => |heap, this, args| call_ctx(string::string_concat, heap, this, args),
        "repeat" (1) => StringRepeat => |heap, this, args| call_ctx(string::string_repeat, heap, this, args),
        "padStart" (1) => StringPadStart => |heap, this, args| call_ctx(string::string_pad_start, heap, this, args),
        "padEnd" (1) => StringPadEnd => |heap, this, args| call_ctx(string::string_pad_end, heap, this, args),
        "trim" (0) => StringTrim => |heap, this, args| call_ctx(string::string_trim, heap, this, args),
        "trimStart" (0) => StringTrimStart => |heap, this, args| call_ctx(string::string_trim_start, heap, this, args),
        "trimEnd" (0) => StringTrimEnd => |heap, this, args| call_ctx(string::string_trim_end, heap, this, args),
        "toLowerCase" (0) => StringToLowerCase => |heap, this, args| call_ctx(string::string_to_lower_case, heap, this, args),
        "toUpperCase" (0) => StringToUpperCase => |heap, this, args| call_ctx(string::string_to_upper_case, heap, this, args),
        "substring" (2) => StringSubstring => |heap, this, args| call_ctx(string::string_substring, heap, this, args),
        "substr" (2) => StringSubstr => |heap, this, args| call_ctx(string::string_substr, heap, this, args),
        "toString" (0) => StringToString => |heap, this, args| call_ctx(string::string_to_string, heap, this, args),
        "valueOf" (0) => StringValueOf => |heap, this, args| call_ctx(string::string_value_of, heap, this, args),
        "localeCompare" (1) => StringLocaleCompare => |heap, this, args| call_ctx(string::string_locale_compare, heap, this, args),
        "replaceAll" (2) => StringReplaceAll => |heap, this, args| call_ctx(string::string_replace_all, heap, this, args),
    },
    StringCtor {
        "fromCharCode" (1) => StringFromCharCode => |heap, this, args| call_ctx(string::string_from_char_code, heap, this, args),
        "fromCodePoint" (1) => StringFromCodePoint => |heap, this, args| call_ctx(string::string_from_code_point, heap, this, args),
    },
    Json {
        "parse" (2) => JsonParse => |heap, this, args| call_ctx(json::json_parse, heap, this, args),
        "stringify" (3) => JsonStringify => |heap, this, args| call_ctx(json::json_stringify, heap, this, args),
    },
    BooleanProto {
        "toString" (0) => BooleanProtoToString => |heap, this, args| call_ctx(boolean::boolean_proto_to_string, heap, this, args),
        "valueOf" (0) => BooleanProtoValueOf => |heap, this, args| call_ctx(boolean::boolean_proto_value_of, heap, this, args),
    },
    Symbol {
        "for" (1) => SymbolFor => |heap, this, args| call_ctx(symbol::symbol_for, heap, this, args),
        "keyFor" (1) => SymbolKeyFor => |heap, this, args| call_ctx(symbol::symbol_key_for, heap, this, args),
        "iterator" => SymbolWellKnownIterator => |heap, this, args| call_ctx(symbol::symbol_well_known, heap, this, args),
        "asyncIterator" => SymbolWellKnownAsyncIterator => |heap, this, args| call_ctx(symbol::symbol_well_known, heap, this, args),
        "hasInstance" => SymbolWellKnownHasInstance => |heap, this, args| call_ctx(symbol::symbol_well_known, heap, this, args),
        "isConcatSpreadable" => SymbolWellKnownIsConcatSpreadable => |heap, this, args| call_ctx(symbol::symbol_well_known, heap, this, args),
        "match" => SymbolWellKnownMatch => |heap, this, args| call_ctx(symbol::symbol_well_known, heap, this, args),
        "replace" => SymbolWellKnownReplace => |heap, this, args| call_ctx(symbol::symbol_well_known, heap, this, args),
        "search" => SymbolWellKnownSearch => |heap, this, args| call_ctx(symbol::symbol_well_known, heap, this, args),
        "species" => SymbolWellKnownSpecies => |heap, this, args| call_ctx(symbol::symbol_well_known, heap, this, args),
        "split" => SymbolWellKnownSplit => |heap, this, args| call_ctx(symbol::symbol_well_known, heap, this, args),
        "toPrimitive" => SymbolWellKnownToPrimitive => |heap, this, args| call_ctx(symbol::symbol_well_known, heap, this, args),
        "toStringTag" => SymbolWellKnownToStringTag => |heap, this, args| call_ctx(symbol::symbol_well_known, heap, this, args),
        "unscopables" => SymbolWellKnownUnscopables => |heap, this, args| call_ctx(symbol::symbol_well_known, heap, this, args),
    },
    SymbolProto {
        "toString" (0) => SymbolProtoToString => |heap, this, args| call_ctx(symbol::symbol_proto_to_string, heap, this, args),
        "valueOf" (0) => SymbolProtoValueOf => |heap, this, args| call_ctx(symbol::symbol_proto_value_of, heap, this, args),
        "description" => SymbolProtoDescription => |heap, this, args| call_ctx(symbol::symbol_proto_description, heap, this, args),
    },
    Proxy {
        "revocable" (2) => ProxyRevocable => |heap, this, args| call_ctx(proxy::proxy_revocable, heap, this, args),
    };
    // Truly internal / non-JS-visible dispatch-only natives (not installed).
    StringConstruct => |heap, this, args| call_ctx(string_construct, heap, this, args),
    NumberConstruct => |heap, this, args| call_ctx(number::number_construct, heap, this, args),
    BooleanConstruct => |heap, this, args| call_ctx(boolean::boolean_construct, heap, this, args),
    ErrorCreate => |heap, this, args| call_ctx(error::error_create, heap, this, args),
    TypeErrorCreate => |heap, this, args| call_ctx(error::type_error_create, heap, this, args),
    RangeErrorCreate => |heap, this, args| call_ctx(error::range_error_create, heap, this, args),
    ReferenceErrorCreate => |heap, this, args| call_ctx(error::reference_error_create, heap, this, args),
    SyntaxErrorCreate => |heap, this, args| call_ctx(error::syntax_error_create, heap, this, args),
    ObjectConstruct => |heap, this, args| call_ctx(object::object_construct, heap, this, args),
    ArrayConstruct => |heap, this, args| call_ctx(array::array_construct, heap, this, args),
    MapConstruct => |heap, this, args| call_ctx(map::map_construct, heap, this, args),
    MapGet => |heap, this, args| call_ctx(map::map_get, heap, this, args),
    MapSet => |heap, this, args| call_ctx(map::map_set, heap, this, args),
    MapHas => |heap, this, args| call_ctx(map::map_has, heap, this, args),
    MapDelete => |heap, this, args| call_ctx(map::map_delete, heap, this, args),
    MapSize => |heap, this, args| call_ctx(map::map_size, heap, this, args),
    MapClear => |heap, this, args| call_ctx(map::map_clear, heap, this, args),
    MapEntries => |heap, this, args| call_ctx(map::map_entries, heap, this, args),
    MapKeys => |heap, this, args| call_ctx(map::map_keys, heap, this, args),
    MapValues => |heap, this, args| call_ctx(map::map_values, heap, this, args),
    MapForEach => callback_stub,
    SetConstruct => |heap, this, args| call_ctx(map::set_construct, heap, this, args),
    SetAdd => |heap, this, args| call_ctx(map::set_add, heap, this, args),
    SetHas => |heap, this, args| call_ctx(map::set_has, heap, this, args),
    SetDelete => |heap, this, args| call_ctx(map::set_delete, heap, this, args),
    SetSize => |heap, this, args| call_ctx(map::set_size, heap, this, args),
    SetClear => |heap, this, args| call_ctx(map::set_clear, heap, this, args),
    SetEntries => |heap, this, args| call_ctx(map::set_entries, heap, this, args),
    SetKeys => |heap, this, args| call_ctx(map::set_keys, heap, this, args),
    SetValues => |heap, this, args| call_ctx(map::set_values, heap, this, args),
    SetForEach => callback_stub,
    IteratorNext => |heap, this, args| call_ctx(iterator::iterator_next, heap, this, args),
    IteratorToArray => |heap, this, args| call_ctx(iterator::iterator_to_array, heap, this, args),
    IteratorTake => |heap, this, args| call_ctx(iterator::iterator_take, heap, this, args),
    IteratorDrop => |heap, this, args| call_ctx(iterator::iterator_drop, heap, this, args),
    IteratorFrom => |heap, this, args| call_ctx(iterator::iterator_from, heap, this, args),
    IteratorMap => callback_stub,
    IteratorFilter => callback_stub,
    IteratorFlatMap => callback_stub,
    IteratorReduce => callback_stub,
    IteratorForEach => callback_stub,
    IteratorSome => callback_stub,
    IteratorEvery => callback_stub,
    IteratorFind => callback_stub,
    SymbolConstruct => |heap, this, args| call_ctx(symbol::symbol_construct, heap, this, args),
    ProxyConstruct => |heap, this, args| call_ctx(proxy::proxy_construct, heap, this, args),
    MapIterator => |heap, this, args| call_ctx(iterator::map_iterator, heap, this, args),
    SetIterator => |heap, this, args| call_ctx(iterator::set_iterator, heap, this, args),
    IteratorSelf => |heap, this, args| call_ctx(iterator::iterator_self, heap, this, args),
    RegExpConstruct => |heap, this, args| call_ctx(regexp::regexp_construct, heap, this, args),
    RegExpToString => |heap, this, args| call_ctx(regexp::regexp_to_string, heap, this, args),
    ObjectEnumerableOwnKeys => |heap, this, args| call_ctx(object::object_enumerable_own_keys, heap, this, args),
}

/// Installs the core built-ins into `registry`.
///
/// `String(x)`: ES ToString subset for the callable `String` intrinsic.
/// The realm points the `String` placeholder's `elements[0]` at this index.
fn string_construct(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = match args.first() {
        Some(&v) => ctx.to_string(v),
        None => "undefined".to_string(),
    };
    Ok(JsValue::string(ctx.heap.intern_text(&text)))
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
/// (`Interp::run_callback_builtin`, reached via the single
/// `Interp::dispatch_native` router) before any dispatch happens, so this is
/// unreachable in practice — it exists only so `define_builtins!` can carry
/// the install.
fn callback_stub(heap: &mut Heap, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    Err(Throw::type_error(
        heap,
        "TypeError: built-in method must be invoked through the interpreter",
    ))
}

/// The interpreter router (`Interp::dispatch_native`) owns the REAL `eval` /
/// `Function` / `console.log` paths as explicit arms, and every interpreter
/// call site now funnels through it (`prepare_call`, `call_inline`,
/// `prepare_construct`, the `size` getter) before the registry fallback.
/// The former `eval_stub` / `function_stub` / `console_log` duplicates were
/// therefore unreachable from JS and are deleted: a direct
/// `NativeRegistry::call_native(Eval|Function|ConsoleLog)` now reports "not
/// registered", which no in-tree caller does.
/// No-loader `ModuleImport` fallback: spec shape (`import()` returns a
/// promise) with a rejection reason, so `.catch` observes a real rejection.
/// The loader-equipped path lives in `module_loader::handle_import` and is
/// intercepted in `NativeRegistry::call_native` before this fallback.
pub(crate) fn module_import(
    heap: &mut Heap,
    _this: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, Throw> {
    let err = Throw::type_error(heap, "dynamic import: no module loader in this context");
    match err {
        Throw::Value(v) => Ok(promise::make_rejected_promise(heap, v)),
        other => Err(other),
    }
}
