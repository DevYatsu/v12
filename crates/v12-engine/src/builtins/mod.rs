//! Built-in objects and functions.
//!
//! Each built-in is a native function registered with the interpreter's
//! `NativeRegistry`. The functions operate directly on the heap, using shape
//! transitions and string primitives.

pub mod array;
pub mod boolean;
pub mod ctx;
pub mod date;
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
pub mod reflect;
pub mod regexp;
pub mod registry;
pub mod string;
pub mod symbol;

pub use ctx::{BuiltinFn, Ctx, call_ctx, call_legacy};
pub use registry::{HostClosure, HostFn, NativeHandler, NativeRegistry};

use v12_heap::{Handle, Heap, JsValue};
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
    pub map: Option<v12_heap::Handle<v12_heap::JsObject>>,
    pub map_proto: v12_heap::Handle<v12_heap::JsObject>,
    pub set: Option<v12_heap::Handle<v12_heap::JsObject>>,
    pub set_proto: v12_heap::Handle<v12_heap::JsObject>,
    /// The `Proxy` constructor. Statics install on it (`Proxy.revocable`);
    /// `Proxy` intentionally has no `prototype` (test262
    /// `built-ins/Proxy/proxy-no-prototype.js`), so there is no
    /// `proxy_proto` field.
    pub proxy: Option<v12_heap::Handle<v12_heap::JsObject>>,
    /// The `Date` constructor. Statics install on it (`Date.now`, `Date.UTC`,
    /// `Date.parse`); prototype methods install on `date_proto`.
    pub date: Option<v12_heap::Handle<v12_heap::JsObject>>,
    /// `%Date.prototype%`.
    pub date_proto: v12_heap::Handle<v12_heap::JsObject>,
    /// The `Reflect` global object. An ordinary, non-callable object whose 13
    /// static methods install on it (same mechanism as `Date`: a plain
    /// shape-bound global property, not a `GLOBAL_INTRINSICS` slot).
    pub reflect: Option<v12_heap::Handle<v12_heap::JsObject>>,
    /// The `Iterator` constructor (ES2025 iterator helpers). `Iterator.from`
    /// installs on it; the constructor's own callable is wired by the realm
    /// (like `Promise`), so no method is installed here.
    pub iterator: Option<v12_heap::Handle<v12_heap::JsObject>>,
    /// `%Iterator.prototype%`. Always exists (the realm allocates it before
    /// the install pass), so it is a plain handle like `array_proto`. The
    /// eleven helper methods install here.
    pub iterator_proto: v12_heap::Handle<v12_heap::JsObject>,
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
        // `Function.prototype.call` / `apply` / `bind` (ES 10.2.3-10.2.5). These
        // three have no `define_builtins!` entry — their bodies live in the
        // interpreter router (`Interp::dispatch_native`), because `call` and
        // `apply` must re-enter the machine to invoke the target — so nothing
        // else declares their arity. Without these arms
        // `install_function_proto_methods` could not stamp a `length`, and
        // `Function.prototype.call.length` read `undefined`.
        NativeId::FunctionCall => Some(1),
        NativeId::FunctionApply => Some(2),
        NativeId::FunctionBind => Some(1),
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
        NativeId::MapGet => Some(1),
        NativeId::MapSet => Some(2),
        NativeId::MapHas => Some(1),
        NativeId::MapDelete => Some(1),
        NativeId::MapSize => Some(0),
        NativeId::MapClear => Some(0),
        NativeId::MapForEach => Some(1),
        NativeId::MapEntries => Some(0),
        NativeId::MapKeys => Some(0),
        NativeId::MapValues => Some(0),
        NativeId::SetAdd => Some(1),
        NativeId::SetHas => Some(1),
        NativeId::SetDelete => Some(1),
        NativeId::SetSize => Some(0),
        NativeId::SetClear => Some(0),
        NativeId::SetForEach => Some(1),
        NativeId::SetEntries => Some(0),
        NativeId::SetKeys => Some(0),
        NativeId::SetValues => Some(0),
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
        NativeId::ObjectDefineProperties => Some(2),
        NativeId::ObjectEntries => Some(1),
        NativeId::ObjectFreeze => Some(1),
        NativeId::ObjectFromEntries => Some(1),
        NativeId::ObjectGetOwnPropertyDescriptor => Some(2),
        NativeId::ObjectGetOwnPropertyDescriptors => Some(1),
        NativeId::ObjectGetOwnPropertyNames => Some(1),
        NativeId::ObjectGetOwnPropertySymbols => Some(1),
        NativeId::ObjectProtoPropertyIsEnumerable => Some(1),
        NativeId::ObjectProtoDefineGetter => Some(2),
        NativeId::ObjectProtoDefineSetter => Some(2),
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
        NativeId::StringAnchor => Some(1),
        NativeId::StringBig => Some(0),
        NativeId::StringBlink => Some(0),
        NativeId::StringBold => Some(0),
        NativeId::StringCharAt => Some(1),
        NativeId::StringCharCodeAt => Some(1),
        NativeId::StringCodePointAt => Some(1),
        NativeId::StringConcat => Some(1),
        NativeId::StringEndsWith => Some(1),
        NativeId::StringFixed => Some(0),
        NativeId::StringFontcolor => Some(1),
        NativeId::StringFontsize => Some(1),
        NativeId::StringFromCharCode => Some(1),
        NativeId::StringFromCodePoint => Some(1),
        NativeId::StringIncludes => Some(1),
        NativeId::StringIndexOf => Some(1),
        NativeId::StringIsWellFormed => Some(0),
        NativeId::StringItalics => Some(0),
        NativeId::StringLastIndexOf => Some(1),
        NativeId::StringLink => Some(1),
        NativeId::StringLocaleCompare => Some(1),
        NativeId::StringMatch => Some(1),
        NativeId::StringMatchAll => Some(1),
        NativeId::StringPadEnd => Some(1),
        NativeId::StringPadStart => Some(1),
        NativeId::StringRaw => Some(1),
        NativeId::StringRepeat => Some(1),
        NativeId::StringReplace => Some(2),
        NativeId::StringReplaceAll => Some(2),
        NativeId::StringSearch => Some(1),
        NativeId::StringSlice => Some(2),
        NativeId::StringSmall => Some(0),
        NativeId::StringSplit => Some(2),
        NativeId::StringStartsWith => Some(1),
        NativeId::StringStrike => Some(0),
        NativeId::StringSub => Some(0),
        NativeId::StringSubstring => Some(2),
        NativeId::StringSup => Some(0),
        NativeId::StringToLocaleLowerCase => Some(0),
        NativeId::StringToLocaleUpperCase => Some(0),
        NativeId::StringSubstr => Some(2),
        NativeId::StringToLowerCase => Some(0),
        NativeId::StringToString => Some(0),
        NativeId::StringToUpperCase => Some(0),
        NativeId::StringToWellFormed => Some(0),
        NativeId::StringTrim => Some(0),
        NativeId::StringTrimEnd => Some(0),
        NativeId::StringTrimLeft => Some(0),
        NativeId::StringTrimRight => Some(0),
        NativeId::StringTrimStart => Some(0),
        NativeId::StringValueOf => Some(0),
        NativeId::SymbolFor => Some(1),
        NativeId::SymbolKeyFor => Some(1),
        NativeId::SymbolProtoToString => Some(0),
        NativeId::SymbolProtoValueOf => Some(0),
        NativeId::ProxyRevocable => Some(2),
        NativeId::DateNow => Some(0),
        NativeId::DateUtc => Some(7),
        NativeId::DateParse => Some(1),
        NativeId::DateProtoValueOf => Some(0),
        NativeId::DateProtoGetTime => Some(0),
        NativeId::DateProtoGetFullYear => Some(0),
        NativeId::DateProtoGetUtcFullYear => Some(0),
        NativeId::DateProtoGetMonth => Some(0),
        NativeId::DateProtoGetUtcMonth => Some(0),
        NativeId::DateProtoGetDate => Some(0),
        NativeId::DateProtoGetUtcDate => Some(0),
        NativeId::DateProtoGetDay => Some(0),
        NativeId::DateProtoGetUtcDay => Some(0),
        NativeId::DateProtoGetHours => Some(0),
        NativeId::DateProtoGetUtcHours => Some(0),
        NativeId::DateProtoGetMinutes => Some(0),
        NativeId::DateProtoGetUtcMinutes => Some(0),
        NativeId::DateProtoGetSeconds => Some(0),
        NativeId::DateProtoGetUtcSeconds => Some(0),
        NativeId::DateProtoGetMilliseconds => Some(0),
        NativeId::DateProtoGetUtcMilliseconds => Some(0),
        NativeId::DateProtoGetTimezoneOffset => Some(0),
        NativeId::DateProtoSetTime => Some(1),
        NativeId::DateProtoSetFullYear => Some(3),
        NativeId::DateProtoSetUtcFullYear => Some(3),
        NativeId::DateProtoSetMonth => Some(2),
        NativeId::DateProtoSetUtcMonth => Some(2),
        NativeId::DateProtoSetDate => Some(1),
        NativeId::DateProtoSetUtcDate => Some(1),
        NativeId::DateProtoSetHours => Some(4),
        NativeId::DateProtoSetUtcHours => Some(4),
        NativeId::DateProtoSetMinutes => Some(3),
        NativeId::DateProtoSetUtcMinutes => Some(3),
        NativeId::DateProtoSetSeconds => Some(2),
        NativeId::DateProtoSetUtcSeconds => Some(2),
        NativeId::DateProtoSetMilliseconds => Some(1),
        NativeId::DateProtoSetUtcMilliseconds => Some(1),
        NativeId::DateProtoToIsoString => Some(0),
        NativeId::DateProtoToString => Some(0),
        NativeId::DateProtoToDateString => Some(0),
        NativeId::DateProtoToTimeString => Some(0),
        NativeId::DateProtoToUtcString => Some(0),
        NativeId::DateProtoToGmtString => Some(0),
        NativeId::DateProtoToLocaleString => Some(0),
        NativeId::DateProtoToLocaleDateString => Some(0),
        NativeId::DateProtoToLocaleTimeString => Some(0),
        NativeId::DateProtoToJson => Some(1),
        NativeId::DateProtoToPrimitive => Some(1),
        NativeId::DateProtoGetYear => Some(0),
        NativeId::DateProtoSetYear => Some(1),
        NativeId::DateProtoToTemporalInstant => Some(0),
        NativeId::PromiseAll => Some(1),
        NativeId::PromiseRace => Some(1),
        NativeId::PromiseAllSettled => Some(1),
        NativeId::PromiseAny => Some(1),
        NativeId::PromiseWithResolvers => Some(0),
        NativeId::PromiseTry => Some(1),
        NativeId::PromiseResolve => Some(1),
        NativeId::PromiseReject => Some(1),
        NativeId::PromiseThen => Some(2),
        NativeId::PromiseCatch => Some(1),
        NativeId::PromiseFinally => Some(1),
        NativeId::PromiseSymbolSpeciesGet => Some(0),
        NativeId::ErrorIsError => Some(1),
        NativeId::ErrorProtoToString => Some(0),
        NativeId::ReflectApply => Some(3),
        NativeId::ReflectConstruct => Some(2),
        NativeId::ReflectDefineProperty => Some(3),
        NativeId::ReflectDeleteProperty => Some(2),
        NativeId::ReflectGet => Some(2),
        NativeId::ReflectGetOwnPropertyDescriptor => Some(2),
        NativeId::ReflectGetPrototypeOf => Some(1),
        NativeId::ReflectHas => Some(2),
        NativeId::ReflectIsExtensible => Some(1),
        NativeId::ReflectOwnKeys => Some(1),
        NativeId::ReflectPreventExtensions => Some(1),
        NativeId::ReflectSet => Some(3),
        NativeId::ReflectSetPrototypeOf => Some(2),
        // Iterator helpers (ES2025). `toArray` is the only arity-0 member;
        // every other helper takes exactly one argument (`reduce`'s
        // `initialValue` is optional, so it does not count).
        NativeId::IteratorConstruct => Some(0),
        NativeId::IteratorSelf => Some(0),
        NativeId::IteratorFrom => Some(1),
        NativeId::IteratorMap => Some(1),
        NativeId::IteratorFilter => Some(1),
        NativeId::IteratorTake => Some(1),
        NativeId::IteratorDrop => Some(1),
        NativeId::IteratorFlatMap => Some(1),
        NativeId::IteratorReduce => Some(1),
        NativeId::IteratorToArray => Some(0),
        NativeId::IteratorForEach => Some(1),
        NativeId::IteratorSome => Some(1),
        NativeId::IteratorEvery => Some(1),
        NativeId::IteratorFind => Some(1),
        // Set relational methods (24.2.3). All seven take exactly one
        // argument, `other`; the three predicates are not key-mapped (the
        // proposal's rejected `*ByKey` variants are not part of the standard).
        NativeId::SetUnion => Some(1),
        NativeId::SetIntersection => Some(1),
        NativeId::SetDifference => Some(1),
        NativeId::SetSymmetricDifference => Some(1),
        NativeId::SetIsSubsetOf => Some(1),
        NativeId::SetIsSupersetOf => Some(1),
        NativeId::SetIsDisjointFrom => Some(1),
        // Iterator helpers added by the same proposal round. `includes` and
        // `join` default their argument but are declared with one parameter.
        // `windows`/`chunks` take one; `windows` takes a second, optional
        // `undersized`. `zip`/`zipKeyed` take `(iterables, options)` and so
        // report 1, while `concat` is a rest-parameter static and reports 0.
        NativeId::IteratorIncludes => Some(1),
        NativeId::IteratorJoin => Some(1),
        NativeId::IteratorWindows => Some(1),
        NativeId::IteratorChunks => Some(1),
        NativeId::IteratorZip => Some(1),
        NativeId::IteratorZipKeyed => Some(1),
        NativeId::IteratorConcat => Some(0),
        // Keyed promise combinators (27.2.4).
        NativeId::PromiseAllKeyed => Some(1),
        NativeId::PromiseAllSettledKeyed => Some(1),
        // RegExp: the 22.2.6 accessors and the `@@species` getter are all
        // zero-argument; the 22.2.5 methods and 22.2.7 protocol methods carry
        // their spec arity. These are the arities the `define_named_accessor`
        // debug assertion and the `length` own props are checked against, so
        // they must not drift.
        NativeId::RegExpProtoGetSource
        | NativeId::RegExpProtoGetFlags
        | NativeId::RegExpProtoGetGlobal
        | NativeId::RegExpProtoGetIgnoreCase
        | NativeId::RegExpProtoGetMultiline
        | NativeId::RegExpProtoGetDotAll
        | NativeId::RegExpProtoGetUnicode
        | NativeId::RegExpProtoGetUnicodeSets
        | NativeId::RegExpProtoGetSticky
        | NativeId::RegExpProtoGetHasIndices
        | NativeId::RegExpProtoGetLastIndex
        | NativeId::RegExpProtoSetLastIndex
        | NativeId::RegExpSymbolSpeciesGet
        | NativeId::RegExpToString => Some(0),
        NativeId::RegExpExec
        | NativeId::RegExpTest
        | NativeId::RegExpSymbolMatch
        | NativeId::RegExpSymbolMatchAll
        | NativeId::RegExpSymbolSearch => Some(1),
        NativeId::RegExpCompile | NativeId::RegExpSymbolReplace | NativeId::RegExpSymbolSplit => {
            Some(2)
        }
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

/// The `%RegExp.prototype%` handle, resolved through the realm `RegExp`
/// constructor's `prototype` field (the same source `link_regexp_proto` uses).
///
/// Every `RegExp.prototype` install funnels here so the four install passes
/// below share one resolution and one bail-out rule: a realm that has not
/// linked the prototype (a bare-heap engine test) installs nothing rather than
/// panicking. Requires `install_ctor` to have run, which is why all four are
/// called from the tail of `install_builtins`.
///
/// `global` is passed explicitly rather than left to [`Ctx::intrinsic`]'s
/// `realm_globals().first()` fallback: a *second* `Realm::new` on the same heap
/// (what `$262.createRealm()` does) would otherwise resolve the primary
/// realm's `RegExp`, and the duplicate-install assertion in
/// [`Ctx::define_named_accessor`] would fire on the primary realm's prototype.
fn regexp_proto(
    heap: &mut Heap,
    global: v12_heap::Handle<v12_heap::JsObject>,
) -> Option<(
    v12_heap::Handle<v12_heap::JsObject>,
    v12_heap::Handle<v12_heap::JsObject>,
)> {
    let ctx = Ctx::new(heap, Some(global), None);
    let ctor = ctx.intrinsic("RegExp").and_then(|v| v.as_object())?;
    let proto = ctx.heap.get(ctor).prototype?;
    Some((ctor, proto))
}

/// Group 1 — the `%RegExp.prototype%` data accessors (ES 22.2.6.1-22.2.6.6,
/// 22.2.6.8-22.2.6.10): `source`, `flags`, and the eight single-flag getters.
///
/// Every one is an accessor `{ get f, set: undefined, enumerable: false,
/// configurable: true }` whose getter is a zero-argument function named
/// `get <name>`. `verifyProperty` on the prototype is a large share of the
/// `built-ins/RegExp/prototype` suite, so the attributes are load-bearing, not
/// incidental — hence [`Ctx::define_named_accessor`] asserting them.
fn install_regexp_accessors(heap: &mut Heap, global: v12_heap::Handle<v12_heap::JsObject>) {
    let Some((_ctor, proto)) = regexp_proto(heap, global) else {
        return;
    };
    let mut ctx = Ctx::new(heap, None, None);
    let flag_accessors: [(&str, NativeId); 8] = [
        ("global", NativeId::RegExpProtoGetGlobal),
        ("ignoreCase", NativeId::RegExpProtoGetIgnoreCase),
        ("multiline", NativeId::RegExpProtoGetMultiline),
        ("dotAll", NativeId::RegExpProtoGetDotAll),
        ("unicode", NativeId::RegExpProtoGetUnicode),
        ("unicodeSets", NativeId::RegExpProtoGetUnicodeSets),
        ("sticky", NativeId::RegExpProtoGetSticky),
        ("hasIndices", NativeId::RegExpProtoGetHasIndices),
    ];
    for (name, id) in flag_accessors {
        ctx.define_named_accessor(proto, name, Some((id, format!("get {name}"))), None);
    }
    ctx.define_named_accessor(
        proto,
        "source",
        Some((NativeId::RegExpProtoGetSource, "get source".to_string())),
        None,
    );
    ctx.define_named_accessor(
        proto,
        "flags",
        Some((NativeId::RegExpProtoGetFlags, "get flags".to_string())),
        None,
    );
}

/// Group 2 — the `%RegExp.prototype%` `lastIndex` accessor.
///
/// Modern ES gives each RegExp *instance* an own `lastIndex` data property
/// (`RegExpAlloc` step 2), which this engine models as the object's internal
/// slot: the interpreter's own-slot read serves it ahead of the shape lookup,
/// so an instance's observable `lastIndex` is already right. The prototype
/// accessor is the ES 5 shape, installed because it is the only way a
/// `get`/`set lastIndex` *pair* is reachable on the surface at all, and because
/// it gives the prototype itself the same brand check every other accessor has.
fn install_regexp_last_index(heap: &mut Heap, global: v12_heap::Handle<v12_heap::JsObject>) {
    let Some((_ctor, proto)) = regexp_proto(heap, global) else {
        return;
    };
    let mut ctx = Ctx::new(heap, None, None);
    ctx.define_named_accessor(
        proto,
        "lastIndex",
        Some((
            NativeId::RegExpProtoGetLastIndex,
            "get lastIndex".to_string(),
        )),
        Some((
            NativeId::RegExpProtoSetLastIndex,
            "set lastIndex".to_string(),
        )),
    );
}

/// Group 3 — the `%RegExp.prototype%` methods (ES 22.2.5): `exec`, `test`,
/// `toString`, `compile`.
///
/// All four already had `pub` bodies in `regexp.rs` and dispatch arms in the
/// registry; none was ever *installed*, which is why `RegExp.prototype.exec`
/// read `undefined` while the interpreter's `Kind::RegExp` surface kept
/// `/a/.exec` working. Installing them retires that asymmetry: the prototype
/// property and the instance read are now the same function object.
fn install_regexp_methods(heap: &mut Heap, global: v12_heap::Handle<v12_heap::JsObject>) {
    let Some((ctor, proto)) = regexp_proto(heap, global) else {
        return;
    };
    let mut ctx = Ctx::new(heap, None, None);
    for (name, id, len) in [
        ("exec", NativeId::RegExpExec, 1u32),
        ("test", NativeId::RegExpTest, 1),
        ("toString", NativeId::RegExpToString, 0),
        ("compile", NativeId::RegExpCompile, 2),
    ] {
        ctx.define_method(Some(proto), name, id, Some(len));
    }
    // `RegExp.length` is 2 and `RegExp.name` is `"RegExp"`
    // (`CreateBuiltinFunction`). The intrinsic placeholder is wired to
    // `RegExpConstruct` by the realm's `wire_callable` but, like every other
    // intrinsic ctor here, carries neither property — and `install_ctor` stamps
    // neither. Same two installs the realm hand-rolls for `Proxy`.
    for (prop, value) in [
        (
            "length",
            JsValue::from_i32_smi(2).expect("RegExp arity fits Smi"),
        ),
        ("name", JsValue::string(ctx.heap.intern_text("RegExp"))),
    ] {
        let key = v12_heap::PropKey::from_string(ctx.heap.intern_text(prop));
        if ctx
            .heap
            .lookup_property(ctx.heap.shape_of(ctor), key)
            .is_some()
        {
            continue;
        }
        ctx.define_data_prop_with_attrs(
            ctor,
            prop,
            value,
            v12_heap::Attrs::new(false, false, true),
        );
    }
}

/// Group 4 — the symbol-keyed protocol methods `%RegExp.prototype%[@@match]`,
/// `[@@matchAll]`, `[@@replace]`, `[@@search]`, `[@@split]` (ES 22.2.7).
///
/// Kept out of the `define_builtins!` table for the same reason as
/// [`install_string_regex_methods`]: a compile-time dispatch arm would pre-empt
/// the registry's step-2 match, the only place the per-registry compiled-pattern
/// cache is supplied. It is also the only part of the surface that needs a
/// *symbol* key, which the string-keyed macro cannot express.
///
/// The `@@match` family of well-known symbols does not exist yet when
/// `install_builtins` runs (the realm mints its canonical set strictly after the
/// main pass), so each one is minted here and written into the `Symbol`
/// constructor's slot, replacing the placeholder handler function the macro
/// installed — the same `overwrite_data_prop` swap the realm performs for
/// `toPrimitive` and `species`. Minting and keying must use the *same* handle or
/// the JS read `re[Symbol.match]` would compute a key the method is not stored
/// under. `@@species` is the exception: the realm's canonical handle must be
/// reused, so `RegExp[@@species]` is installed by [`install_regexp_species`].
fn install_regexp_protocol(heap: &mut Heap, targets: &BuiltinTargets) {
    let Some(symbol_ctor) = targets.symbol else {
        return;
    };
    let Some((_ctor, proto)) = regexp_proto(heap, targets.global) else {
        return;
    };
    let mut ctx = Ctx::new(heap, None, None);
    let protocol: [(&str, NativeId, u32); 5] = [
        ("match", NativeId::RegExpSymbolMatch, 1),
        ("matchAll", NativeId::RegExpSymbolMatchAll, 1),
        ("replace", NativeId::RegExpSymbolReplace, 2),
        ("search", NativeId::RegExpSymbolSearch, 1),
        ("split", NativeId::RegExpSymbolSplit, 2),
    ];
    for (name, id, len) in protocol {
        // Every name here is in `WellKnownSymbol::ALL`; the table is the same
        // one `install_well_known_singletons` just published, so the method
        // keyed under `Symbol.match` is keyed by the handle a script reads
        // back as `Symbol.match`.
        let Some(which) = v12_heap::WellKnownSymbol::from_name(name) else {
            unreachable!("RegExp protocol name {name} is not a well-known symbol")
        };
        let sym = ctx.heap.well_known_symbol(which);
        install_well_known_symbol(&mut ctx, symbol_ctor, name, sym);
        install_symbol_method(&mut ctx, proto, sym, id, &format!("[Symbol.{name}]"), len);
    }
}

/// Publishes a well-known symbol on the `Symbol` constructor with the spec
/// attributes `{ writable: false, enumerable: false, configurable: false }`.
///
/// The macro's `Symbol` group installed a *handler function* placeholder for
/// each name it knew about, so most of these overwrite an existing slot (the
/// same `overwrite_data_prop` swap the realm performs for `toPrimitive` and
/// `species`). `matchAll` is the exception: the macro never declared it, so the
/// slot has to be created here. Either way the value written is this function's
/// own handle, which is what makes `re[Symbol.match]` compute a key the method
/// is actually stored under.
fn install_well_known_symbol(
    ctx: &mut Ctx,
    symbol_ctor: v12_heap::Handle<v12_heap::JsObject>,
    name: &str,
    sym: Handle<v12_heap::V12Symbol>,
) {
    let spec_attrs = v12_heap::Attrs::new(false, false, false);
    if ctx.redefine_data_prop_with_attrs(symbol_ctor, name, JsValue::symbol(sym), spec_attrs) {
        return;
    }
    ctx.define_data_prop_with_attrs(symbol_ctor, name, JsValue::symbol(sym), spec_attrs);
}

/// Allocates a symbol-keyed method (`CreateBuiltinFunction` + the symbol
/// install) for the RegExp protocol surface. `length` and the `[Symbol.x]`
/// function name are stamped with the spec attrs before the install.
fn install_symbol_method(
    ctx: &mut Ctx,
    proto: v12_heap::Handle<v12_heap::JsObject>,
    sym: Handle<v12_heap::V12Symbol>,
    id: NativeId,
    fname: &str,
    len: u32,
) {
    let func = ctx.alloc_obj(v12_heap::JsObject {
        kind: v12_heap::Kind::Function,
        callable: v12_heap::FunctionTarget::Bytecode(u32::from(id)),
        ..Default::default()
    });
    let len_v = JsValue::from_i32_smi(len as i32).expect("protocol arity fits Smi");
    ctx.define_data_prop_with_attrs(
        func,
        "length",
        len_v,
        v12_heap::Attrs::new(false, false, true),
    );
    let name_h = ctx.heap.intern_text(fname);
    ctx.define_data_prop_with_attrs(
        func,
        "name",
        JsValue::string(name_h),
        v12_heap::Attrs::new(false, false, true),
    );
    // `RegExp.prototype[@@x]` is `{ writable: true, enumerable: false,
    // configurable: true }` — the same shape as every other prototype method.
    ctx.define_symbol_data_prop_with_attrs(
        proto,
        sym,
        JsValue::object(func),
        v12_heap::Attrs::BUILTIN,
    );
}

/// `get RegExp [ @@species ]` (ES 22.2.6.4).
///
/// Separate from [`install_regexp_surface`] because it must be keyed on the
/// *realm's* canonical `Symbol.species`, which only exists once the realm has
/// minted its well-known symbols — strictly after the main install pass, the
/// same ordering that forces `install_reflect_builtins` out of
/// `install_builtins`. The realm calls this where it installs
/// `Promise [ @@species ]`.
/// `global` is passed explicitly for the reason [`regexp_proto`] documents:
/// `Ctx::intrinsic`'s `realm_globals().first()` fallback would resolve the
/// *primary* realm's `RegExp` for every realm, and because `@@species` is now
/// one shared handle (ES 20.4.2.1) the second realm's install would collide
/// with the first's rather than merely being redundant — tripping
/// `Ctx::define_symbol_getter`'s duplicate-install assertion.
pub fn install_regexp_species(
    heap: &mut Heap,
    global: Handle<v12_heap::JsObject>,
    species: Handle<v12_heap::V12Symbol>,
) {
    let mut ctx = Ctx::new(heap, Some(global), None);
    let Some(ctor) = ctx.intrinsic("RegExp").and_then(|v| v.as_object()) else {
        return;
    };
    ctx.define_symbol_getter(
        ctor,
        species,
        NativeId::RegExpSymbolSpeciesGet,
        "get [Symbol.species]",
        0,
    );
}

/// Installs the regexp-backed `String.prototype` methods
/// (`match`/`matchAll`/`replace`/`search`/`split`).
///
/// They have handlers and registry dispatch arms
/// ([`NativeRegistry::call_native`] step 2), but were never installed on
/// `String.prototype`. They are intentionally NOT `define_builtins!` entries:
/// a compile-time dispatch arm would pre-empt the registry's step-2 arm,
/// which is the only path that supplies the per-registry compiled-pattern
/// cache (`Ctx::regex_cache`) these bodies require — without it
/// `regexp.rs::regexp_exec` panics. The arities live in [`builtin_length`].
fn install_string_regex_methods(heap: &mut Heap, targets: &BuiltinTargets) {
    for (name, id) in [
        ("match", NativeId::StringMatch),
        ("matchAll", NativeId::StringMatchAll),
        ("replace", NativeId::StringReplace),
        ("search", NativeId::StringSearch),
        ("split", NativeId::StringSplit),
    ] {
        install_native(heap, Some(targets.string_proto), name, id);
    }
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
        $crate::builtins::install_native($heap, $targets.map, $name, $id)
    };
    (MapProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.map_proto), $name, $id)
    };
    (Set, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.set, $name, $id)
    };
    (SetProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.set_proto), $name, $id)
    };
    (Iterator, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.iterator, $name, $id)
    };
    (IteratorProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.iterator_proto), $name, $id)
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
    // Well-known `Symbol` statics: the property *value* is the realm's
    // singleton symbol, minted once here, with spec 20.4.2's non-writable
    // attribute triple. See `install_value_with_attrs`.
    (SymbolValue, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_value_with_attrs(
            $heap,
            $targets.symbol,
            $name,
            $id,
            v12_heap::Attrs::new(false, false, true),
        )
    };
    (Proxy, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.proxy, $name, $id)
    };
    (Date, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.date, $name, $id)
    };
    (DateProto, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, Some($targets.date_proto), $name, $id)
    };
    (Reflect, $heap:expr, $targets:expr, $name:expr, $id:expr) => {
        $crate::builtins::install_native($heap, $targets.reflect, $name, $id)
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
        $crate::builtins::install_native_with_length(
            $heap,
            Some($targets.global),
            $name,
            $id,
            Some($len),
        )
    };
    (NumberProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length(
            $heap,
            Some($targets.number_proto),
            $name,
            $id,
            Some($len),
        )
    };
    (StringProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length(
            $heap,
            Some($targets.string_proto),
            $name,
            $id,
            Some($len),
        )
    };
    (ArrayProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length(
            $heap,
            Some($targets.array_proto),
            $name,
            $id,
            Some($len),
        )
    };
    (ObjectProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length(
            $heap,
            Some($targets.object_proto),
            $name,
            $id,
            Some($len),
        )
    };
    (FunctionProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length(
            $heap,
            Some($targets.function_proto),
            $name,
            $id,
            Some($len),
        )
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
        $crate::builtins::install_native_with_length(
            $heap,
            Some($targets.boolean_proto),
            $name,
            $id,
            Some($len),
        )
    };
    (StringCtor, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, $targets.string, $name, $id, Some($len))
    };
    (Symbol, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, $targets.symbol, $name, $id, Some($len))
    };
    (SymbolValue, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_value_with_attrs(
            $heap,
            $targets.symbol,
            $name,
            $id,
            v12_heap::Attrs::new(false, false, true),
        )
    };
    (SymbolProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length(
            $heap,
            Some($targets.symbol_proto),
            $name,
            $id,
            Some($len),
        )
    };
    (MapProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length(
            $heap,
            Some($targets.map_proto),
            $name,
            $id,
            Some($len),
        )
    };
    (SetProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length(
            $heap,
            Some($targets.set_proto),
            $name,
            $id,
            Some($len),
        )
    };
    (Proxy, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, $targets.proxy, $name, $id, Some($len))
    };
    (Date, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length($heap, $targets.date, $name, $id, Some($len))
    };
    (DateProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length(
            $heap,
            Some($targets.date_proto),
            $name,
            $id,
            Some($len),
        )
    };
    (Reflect, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length(
            $heap,
            $targets.reflect,
            $name,
            $id,
            Some($len),
        )
    };
    (Iterator, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length(
            $heap,
            $targets.iterator,
            $name,
            $id,
            Some($len),
        )
    };
    (IteratorProto, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {
        $crate::builtins::install_native_with_length(
            $heap,
            Some($targets.iterator_proto),
            $name,
            $id,
            Some($len),
        )
    };
    // Value-constant groups ignore length (constants, not functions).
    (GlobalValue, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {{
        let _ = $len;
        $crate::builtins::install_value($heap, Some($targets.global), $name, $id)
    }};
    (MathValue, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {{
        let _ = $len;
        $crate::builtins::install_value($heap, $targets.math, $name, $id)
    }};
    (NumberValue, $heap:expr, $targets:expr, $name:expr, $id:expr, $len:expr) => {{
        let _ = $len;
        $crate::builtins::install_value($heap, $targets.number, $name, $id)
    }};
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
            // `%Function.prototype%`'s own identity: its `[[Prototype]]` link
            // and its `length`/`name` own properties. The macro's
            // `FunctionProto` group can only *add* named methods, and it runs
            // before this, so this pass is what makes the object the spec
            // prototype object rather than a bare callable. See
            // `install_function_proto_identity`.
            install_function_proto_identity(heap, targets);
            // `Function.prototype.call` / `apply` / `bind`, installed rather
            // than macro-declared. See `install_function_proto_methods`.
            install_function_proto_methods(heap, targets);
            // Bare ids are dispatch-only; silence unused warnings.
            $( let _ = NativeId::$bare_id; )*
            // Regex-backed `String.prototype` methods: installed here, but
            // deliberately absent from the dispatch table above so
            // `builtin_dispatch` returns `None` for their ids. That routes
            // them through `NativeRegistry::call_native`'s step-2 arms, which
            // supply the per-registry compiled-pattern cache these bodies
            // require (`Ctx::regex_cache`).
            install_string_regex_methods(heap, targets);
            // Keyed promise combinators: the one install this pass makes that
            // the macro's target table cannot express (see the function's doc).
            install_keyed_promise_combinators(heap);
            // The `%RegExp.prototype%` surface, one pass per spec group. Same
            // reasoning as the string methods above: the bodies need the
            // registry's step-2 arms (regex cache), and the symbol-keyed ones
            // cannot ride the string-keyed macro at all.
            install_regexp_accessors(heap, targets.global);
            install_regexp_last_index(heap, targets.global);
            install_regexp_methods(heap, targets.global);
            // The shared well-known symbols, published on the `Symbol`
            // constructor. Runs *before* the RegExp protocol install so the
            // two agree by construction: `install_regexp_protocol` keys
            // `RegExp.prototype[@@match]` and friends on the same table, and
            // the realm's own `Symbol.species` / `toPrimitive` /
            // `toStringTag` / `iterator` reads read the same table afterwards.
            if let Some(symbol_ctor) = targets.symbol {
                symbol::install_well_known_singletons(heap, symbol_ctor);
            }
            install_regexp_protocol(heap, targets);
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
    MapProto {
        "get" (1) => MapGet => |heap, this, args| call_ctx(map::map_get, heap, this, args),
        "set" (2) => MapSet => |heap, this, args| call_ctx(map::map_set, heap, this, args),
        "has" (1) => MapHas => |heap, this, args| call_ctx(map::map_has, heap, this, args),
        "delete" (1) => MapDelete => |heap, this, args| call_ctx(map::map_delete, heap, this, args),
        "clear" (0) => MapClear => |heap, this, args| call_ctx(map::map_clear, heap, this, args),
        "forEach" (1) => MapForEach => callback_stub,
        "entries" (0) => MapEntries => |heap, this, args| call_ctx(map::map_entries, heap, this, args),
        "keys" (0) => MapKeys => |heap, this, args| call_ctx(map::map_keys, heap, this, args),
        "values" (0) => MapValues => |heap, this, args| call_ctx(map::map_values, heap, this, args),
    },
    SetProto {
        "add" (1) => SetAdd => |heap, this, args| call_ctx(map::set_add, heap, this, args),
        "has" (1) => SetHas => |heap, this, args| call_ctx(map::set_has, heap, this, args),
        "delete" (1) => SetDelete => |heap, this, args| call_ctx(map::set_delete, heap, this, args),
        "clear" (0) => SetClear => |heap, this, args| call_ctx(map::set_clear, heap, this, args),
        "forEach" (1) => SetForEach => callback_stub,
        "entries" (0) => SetEntries => |heap, this, args| call_ctx(map::set_entries, heap, this, args),
        "keys" (0) => SetKeys => |heap, this, args| call_ctx(map::set_keys, heap, this, args),
        "values" (0) => SetValues => |heap, this, args| call_ctx(map::set_values, heap, this, args),
        // 24.2.3. The seven relational methods. They are pure `&mut Heap`
        // natives: each drains `other` through
        // `iterator::drain_iterable`, which covers the iterable kinds whose
        // iteration needs no user code (see that function's doc for the
        // boundary).
        "union" (1) => SetUnion => |heap, this, args| call_ctx(map::set_union, heap, this, args),
        "intersection" (1) => SetIntersection => |heap, this, args| call_ctx(map::set_intersection, heap, this, args),
        "difference" (1) => SetDifference => |heap, this, args| call_ctx(map::set_difference, heap, this, args),
        "symmetricDifference" (1) => SetSymmetricDifference => |heap, this, args| call_ctx(map::set_symmetric_difference, heap, this, args),
        "isSubsetOf" (1) => SetIsSubsetOf => |heap, this, args| call_ctx(map::set_is_subset_of, heap, this, args),
        "isSupersetOf" (1) => SetIsSupersetOf => |heap, this, args| call_ctx(map::set_is_superset_of, heap, this, args),
        "isDisjointFrom" (1) => SetIsDisjointFrom => |heap, this, args| call_ctx(map::set_is_disjoint_from, heap, this, args),
    },
    // The `Iterator` constructor's one static (spec 27.1.3).
    Iterator {
        "from" (1) => IteratorFrom => |heap, this, args| call_ctx(iterator::iterator_from, heap, this, args),
        // Joint iteration (spec 27.1.3). `zip`/`zipKeyed` take
        // `(iterables, options)`; `concat` is the rest-parameter one.
        "zip" (1) => IteratorZip => |heap, this, args| call_ctx(iterator::iterator_zip, heap, this, args),
        "zipKeyed" (1) => IteratorZipKeyed => |heap, this, args| call_ctx(iterator::iterator_zip_keyed, heap, this, args),
        "concat" (0) => IteratorConcat => |heap, this, args| call_ctx(iterator::iterator_concat, heap, this, args),
    },
    // `%Iterator.prototype%`: the ES2025 iterator helpers (spec 27.1.4-27.1.5).
    // `take`/`drop`/`toArray` are pure-heap (a lazy wrapper / a drain) and run
    // as registry natives.
    //
    // `map`/`filter`/`flatMap` carry a real body: the interpreter intercepts
    // the JS call (the callback must be *stored*, not called, and `next` needs
    // the machine) and then calls this builder through the registry seam with
    // `args = [callback, source next method]`, so the wrapper layout has a
    // single home. `reduce`/`forEach`/`some`/`every`/`find` are consumed on the
    // seam and keep the `callback_stub` placeholder.
    IteratorProto {
        "take" (1) => IteratorTake => |heap, this, args| call_ctx(iterator::iterator_take, heap, this, args),
        "drop" (1) => IteratorDrop => |heap, this, args| call_ctx(iterator::iterator_drop, heap, this, args),
        "toArray" (0) => IteratorToArray => |heap, this, args| call_ctx(iterator::iterator_to_array, heap, this, args),
        "map" (1) => IteratorMap => |heap, this, args| call_ctx(iterator::iterator_map, heap, this, args),
        "filter" (1) => IteratorFilter => |heap, this, args| call_ctx(iterator::iterator_filter, heap, this, args),
        "flatMap" (1) => IteratorFlatMap => |heap, this, args| call_ctx(iterator::iterator_flatmap, heap, this, args),
        "reduce" (1) => IteratorReduce => callback_stub,
        "forEach" (1) => IteratorForEach => callback_stub,
        "some" (1) => IteratorSome => callback_stub,
        "every" (1) => IteratorEvery => callback_stub,
        "find" (1) => IteratorFind => callback_stub,
        // Eager / lazy helpers added by the same proposal round. `includes` and
        // `join` drain through the engine's own `pull`, so they work over any
        // machine-free iterator chain; `windows`/`chunks` build a buffer wrapper
        // that this engine steps itself (see `iterator::is_windowing_wrapper`).
        "includes" (1) => IteratorIncludes => |heap, this, args| call_ctx(iterator::iterator_includes, heap, this, args),
        "join" (1) => IteratorJoin => |heap, this, args| call_ctx(iterator::iterator_join, heap, this, args),
        "windows" (1) => IteratorWindows => |heap, this, args| call_ctx(iterator::iterator_windows, heap, this, args),
        "chunks" (1) => IteratorChunks => |heap, this, args| call_ctx(iterator::iterator_chunks, heap, this, args),
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
        "getOwnPropertyDescriptors" (1) => ObjectGetOwnPropertyDescriptors => |heap, this, args| call_ctx(object::object_get_own_property_descriptors, heap, this, args),
        "setPrototypeOf" (2) => ObjectSetPrototypeOf => |heap, this, args| call_ctx(object::object_set_prototype_of, heap, this, args),
        "create" (1) => ObjectCreate => |heap, this, args| call_ctx(object::object_create, heap, this, args),
        "getPrototypeOf" (1) => ObjectGetPrototypeOf => |heap, this, args| call_ctx(object::object_get_prototype_of, heap, this, args),
        "defineProperty" (3) => ObjectDefineProperty => |heap, this, args| call_ctx(object::object_define_property, heap, this, args),
        "defineProperties" (2) => ObjectDefineProperties => |heap, this, args| call_ctx(object::object_define_properties, heap, this, args),
        "keys" (1) => ObjectKeys => |heap, this, args| call_ctx(object::object_keys, heap, this, args),
        "values" (1) => ObjectValues => |heap, this, args| call_ctx(object::object_values, heap, this, args),
        "entries" (1) => ObjectEntries => |heap, this, args| call_ctx(object::object_entries, heap, this, args),
    },
    ObjectProto {
        "hasOwnProperty" (1) => ObjectHasOwnProperty => |heap, this, args| call_ctx(object::object_has_own_property, heap, this, args),
        "propertyIsEnumerable" (1) => ObjectProtoPropertyIsEnumerable => |heap, this, args| call_ctx(object::object_proto_property_is_enumerable, heap, this, args),
        "__defineGetter__" (2) => ObjectProtoDefineGetter => |heap, this, args| call_ctx(object::object_proto_define_getter, heap, this, args),
        "__defineSetter__" (2) => ObjectProtoDefineSetter => |heap, this, args| call_ctx(object::object_proto_define_setter, heap, this, args),
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
        "toWellFormed" (0) => StringToWellFormed => |heap, this, args| call_ctx(string::string_to_well_formed, heap, this, args),
        "isWellFormed" (0) => StringIsWellFormed => |heap, this, args| call_ctx(string::string_is_well_formed, heap, this, args),
        "trimLeft" (0) => StringTrimLeft => |heap, this, args| call_ctx(string::string_trim_left, heap, this, args),
        "trimRight" (0) => StringTrimRight => |heap, this, args| call_ctx(string::string_trim_right, heap, this, args),
        "toLocaleLowerCase" (0) => StringToLocaleLowerCase => |heap, this, args| call_ctx(string::string_to_locale_lower_case, heap, this, args),
        "toLocaleUpperCase" (0) => StringToLocaleUpperCase => |heap, this, args| call_ctx(string::string_to_locale_upper_case, heap, this, args),
        "anchor" (1) => StringAnchor => |heap, this, args| call_ctx(string::string_anchor, heap, this, args),
        "big" (0) => StringBig => |heap, this, args| call_ctx(string::string_big, heap, this, args),
        "blink" (0) => StringBlink => |heap, this, args| call_ctx(string::string_blink, heap, this, args),
        "bold" (0) => StringBold => |heap, this, args| call_ctx(string::string_bold, heap, this, args),
        "fixed" (0) => StringFixed => |heap, this, args| call_ctx(string::string_fixed, heap, this, args),
        "fontcolor" (1) => StringFontcolor => |heap, this, args| call_ctx(string::string_fontcolor, heap, this, args),
        "fontsize" (1) => StringFontsize => |heap, this, args| call_ctx(string::string_fontsize, heap, this, args),
        "italics" (0) => StringItalics => |heap, this, args| call_ctx(string::string_italics, heap, this, args),
        "link" (1) => StringLink => |heap, this, args| call_ctx(string::string_link, heap, this, args),
        "small" (0) => StringSmall => |heap, this, args| call_ctx(string::string_small, heap, this, args),
        "strike" (0) => StringStrike => |heap, this, args| call_ctx(string::string_strike, heap, this, args),
        "sub" (0) => StringSub => |heap, this, args| call_ctx(string::string_sub, heap, this, args),
        "sup" (0) => StringSup => |heap, this, args| call_ctx(string::string_sup, heap, this, args),
    },
    StringCtor {
        "fromCharCode" (1) => StringFromCharCode => |heap, this, args| call_ctx(string::string_from_char_code, heap, this, args),
        "fromCodePoint" (1) => StringFromCodePoint => |heap, this, args| call_ctx(string::string_from_code_point, heap, this, args),
        "raw" (1) => StringRaw => |heap, this, args| call_ctx(string::string_raw, heap, this, args),
    },
    Json {
        "parse" (2) => JsonParse => |heap, this, args| call_ctx(json::json_parse, heap, this, args),
        "stringify" (3) => JsonStringify => |heap, this, args| call_ctx(json::json_stringify, heap, this, args),
    },
    BooleanProto {
        "toString" (0) => BooleanProtoToString => |heap, this, args| call_ctx(boolean::boolean_proto_to_string, heap, this, args),
        "valueOf" (0) => BooleanProtoValueOf => |heap, this, args| call_ctx(boolean::boolean_proto_value_of, heap, this, args),
    },
    Reflect {
        "getPrototypeOf" (1) => ReflectGetPrototypeOf => |heap, this, args| call_ctx(reflect::reflect_get_prototype_of, heap, this, args),
        "setPrototypeOf" (2) => ReflectSetPrototypeOf => |heap, this, args| call_ctx(reflect::reflect_set_prototype_of, heap, this, args),
        "isExtensible" (1) => ReflectIsExtensible => |heap, this, args| call_ctx(reflect::reflect_is_extensible, heap, this, args),
        "preventExtensions" (1) => ReflectPreventExtensions => |heap, this, args| call_ctx(reflect::reflect_prevent_extensions, heap, this, args),
        "has" (2) => ReflectHas => |heap, this, args| call_ctx(reflect::reflect_has, heap, this, args),
        "deleteProperty" (2) => ReflectDeleteProperty => |heap, this, args| call_ctx(reflect::reflect_delete_property, heap, this, args),
        "ownKeys" (1) => ReflectOwnKeys => |heap, this, args| call_ctx(reflect::reflect_own_keys, heap, this, args),
        "getOwnPropertyDescriptor" (2) => ReflectGetOwnPropertyDescriptor => |heap, this, args| call_ctx(reflect::reflect_get_own_property_descriptor, heap, this, args),
        "defineProperty" (3) => ReflectDefineProperty => |heap, this, args| call_ctx(reflect::reflect_define_property, heap, this, args),
        "get" (2) => ReflectGet => |heap, this, args| call_ctx(reflect::reflect_get, heap, this, args),
        "set" (3) => ReflectSet => |heap, this, args| call_ctx(reflect::reflect_set, heap, this, args),
        "apply" (3) => ReflectApply => |heap, this, args| call_ctx(reflect::reflect_apply, heap, this, args),
        "construct" (2) => ReflectConstruct => |heap, this, args| call_ctx(reflect::reflect_construct, heap, this, args),
    },
    Symbol {
        "for" (1) => SymbolFor => |heap, this, args| call_ctx(symbol::symbol_for, heap, this, args),
        "keyFor" (1) => SymbolKeyFor => |heap, this, args| call_ctx(symbol::symbol_key_for, heap, this, args),
    },
    SymbolProto {
        "toString" (0) => SymbolProtoToString => |heap, this, args| call_ctx(symbol::symbol_proto_to_string, heap, this, args),
        "valueOf" (0) => SymbolProtoValueOf => |heap, this, args| call_ctx(symbol::symbol_proto_value_of, heap, this, args),
        "description" => SymbolProtoDescription => |heap, this, args| call_ctx(symbol::symbol_proto_description, heap, this, args),
    },
    Proxy {
        "revocable" (2) => ProxyRevocable => |heap, this, args| call_ctx(proxy::proxy_revocable, heap, this, args),
    },
    Date {
        "now" (0) => DateNow => |heap, this, args| call_ctx(date::date_now, heap, this, args),
        "UTC" (7) => DateUtc => |heap, this, args| call_ctx(date::date_utc, heap, this, args),
        "parse" (1) => DateParse => |heap, this, args| call_ctx(date::date_parse, heap, this, args),
    },
    DateProto {
        "valueOf" (0) => DateProtoValueOf => |heap, this, args| call_ctx(date::date_proto_value_of, heap, this, args),
        "getTime" (0) => DateProtoGetTime => |heap, this, args| call_ctx(date::date_proto_value_of, heap, this, args),
        "getFullYear" (0) => DateProtoGetFullYear => |heap, this, args| call_ctx(date::date_proto_get_full_year, heap, this, args),
        "getUTCFullYear" (0) => DateProtoGetUtcFullYear => |heap, this, args| call_ctx(date::date_proto_get_utc_full_year, heap, this, args),
        "getMonth" (0) => DateProtoGetMonth => |heap, this, args| call_ctx(date::date_proto_get_month, heap, this, args),
        "getUTCMonth" (0) => DateProtoGetUtcMonth => |heap, this, args| call_ctx(date::date_proto_get_utc_month, heap, this, args),
        "getDate" (0) => DateProtoGetDate => |heap, this, args| call_ctx(date::date_proto_get_date, heap, this, args),
        "getUTCDate" (0) => DateProtoGetUtcDate => |heap, this, args| call_ctx(date::date_proto_get_utc_date, heap, this, args),
        "getDay" (0) => DateProtoGetDay => |heap, this, args| call_ctx(date::date_proto_get_day, heap, this, args),
        "getUTCDay" (0) => DateProtoGetUtcDay => |heap, this, args| call_ctx(date::date_proto_get_utc_day, heap, this, args),
        "getHours" (0) => DateProtoGetHours => |heap, this, args| call_ctx(date::date_proto_get_hours, heap, this, args),
        "getUTCHours" (0) => DateProtoGetUtcHours => |heap, this, args| call_ctx(date::date_proto_get_utc_hours, heap, this, args),
        "getMinutes" (0) => DateProtoGetMinutes => |heap, this, args| call_ctx(date::date_proto_get_minutes, heap, this, args),
        "getUTCMinutes" (0) => DateProtoGetUtcMinutes => |heap, this, args| call_ctx(date::date_proto_get_utc_minutes, heap, this, args),
        "getSeconds" (0) => DateProtoGetSeconds => |heap, this, args| call_ctx(date::date_proto_get_seconds, heap, this, args),
        "getUTCSeconds" (0) => DateProtoGetUtcSeconds => |heap, this, args| call_ctx(date::date_proto_get_utc_seconds, heap, this, args),
        "getMilliseconds" (0) => DateProtoGetMilliseconds => |heap, this, args| call_ctx(date::date_proto_get_milliseconds, heap, this, args),
        "getUTCMilliseconds" (0) => DateProtoGetUtcMilliseconds => |heap, this, args| call_ctx(date::date_proto_get_utc_milliseconds, heap, this, args),
        "getTimezoneOffset" (0) => DateProtoGetTimezoneOffset => |heap, this, args| call_ctx(date::date_proto_get_timezone_offset, heap, this, args),
        "setTime" (1) => DateProtoSetTime => |heap, this, args| call_ctx(date::date_proto_set_time, heap, this, args),
        "setFullYear" (3) => DateProtoSetFullYear => |heap, this, args| call_ctx(date::date_proto_set_full_year, heap, this, args),
        "setUTCFullYear" (3) => DateProtoSetUtcFullYear => |heap, this, args| call_ctx(date::date_proto_set_utc_full_year, heap, this, args),
        "setMonth" (2) => DateProtoSetMonth => |heap, this, args| call_ctx(date::date_proto_set_month, heap, this, args),
        "setUTCMonth" (2) => DateProtoSetUtcMonth => |heap, this, args| call_ctx(date::date_proto_set_utc_month, heap, this, args),
        "setDate" (1) => DateProtoSetDate => |heap, this, args| call_ctx(date::date_proto_set_date, heap, this, args),
        "setUTCDate" (1) => DateProtoSetUtcDate => |heap, this, args| call_ctx(date::date_proto_set_utc_date, heap, this, args),
        "setHours" (4) => DateProtoSetHours => |heap, this, args| call_ctx(date::date_proto_set_hours, heap, this, args),
        "setUTCHours" (4) => DateProtoSetUtcHours => |heap, this, args| call_ctx(date::date_proto_set_utc_hours, heap, this, args),
        "setMinutes" (3) => DateProtoSetMinutes => |heap, this, args| call_ctx(date::date_proto_set_minutes, heap, this, args),
        "setUTCMinutes" (3) => DateProtoSetUtcMinutes => |heap, this, args| call_ctx(date::date_proto_set_utc_minutes, heap, this, args),
        "setSeconds" (2) => DateProtoSetSeconds => |heap, this, args| call_ctx(date::date_proto_set_seconds, heap, this, args),
        "setUTCSeconds" (2) => DateProtoSetUtcSeconds => |heap, this, args| call_ctx(date::date_proto_set_utc_seconds, heap, this, args),
        "setMilliseconds" (1) => DateProtoSetMilliseconds => |heap, this, args| call_ctx(date::date_proto_set_milliseconds, heap, this, args),
        "setUTCMilliseconds" (1) => DateProtoSetUtcMilliseconds => |heap, this, args| call_ctx(date::date_proto_set_utc_milliseconds, heap, this, args),
        "toISOString" (0) => DateProtoToIsoString => |heap, this, args| call_ctx(date::date_proto_to_iso_string, heap, this, args),
        "toString" (0) => DateProtoToString => |heap, this, args| call_ctx(date::date_proto_to_string, heap, this, args),
        "toDateString" (0) => DateProtoToDateString => |heap, this, args| call_ctx(date::date_proto_to_date_string, heap, this, args),
        "toTimeString" (0) => DateProtoToTimeString => |heap, this, args| call_ctx(date::date_proto_to_time_string, heap, this, args),
        "toUTCString" (0) => DateProtoToUtcString => |heap, this, args| call_ctx(date::date_proto_to_utc_string, heap, this, args),
        "toLocaleString" (0) => DateProtoToLocaleString => |heap, this, args| call_ctx(date::date_proto_to_locale_string, heap, this, args),
        "toLocaleDateString" (0) => DateProtoToLocaleDateString => |heap, this, args| call_ctx(date::date_proto_to_locale_date_string, heap, this, args),
        "toLocaleTimeString" (0) => DateProtoToLocaleTimeString => |heap, this, args| call_ctx(date::date_proto_to_locale_time_string, heap, this, args),
        "toJSON" (1) => DateProtoToJson => |heap, this, args| call_ctx(date::date_proto_to_json, heap, this, args),
        // Annex B.
        "getYear" (0) => DateProtoGetYear => |heap, this, args| call_ctx(date::date_proto_get_year, heap, this, args),
        "setYear" (1) => DateProtoSetYear => |heap, this, args| call_ctx(date::date_proto_set_year, heap, this, args),
    };
    // Truly internal / non-JS-visible dispatch-only natives (not installed).
    StringConstruct => |heap, this, args| call_ctx(string_construct, heap, this, args),
    NumberConstruct => |heap, this, args| call_ctx(number::number_construct, heap, this, args),
    BooleanConstruct => |heap, this, args| call_ctx(boolean::boolean_construct, heap, this, args),
    DateConstruct => |heap, this, args| call_ctx(date::date_construct, heap, this, args),
    ErrorCreate => |heap, this, args| call_ctx(error::error_create, heap, this, args),
    TypeErrorCreate => |heap, this, args| call_ctx(error::type_error_create, heap, this, args),
    RangeErrorCreate => |heap, this, args| call_ctx(error::range_error_create, heap, this, args),
    ReferenceErrorCreate => |heap, this, args| call_ctx(error::reference_error_create, heap, this, args),
    SyntaxErrorCreate => |heap, this, args| call_ctx(error::syntax_error_create, heap, this, args),
    EvalErrorCreate => |heap, this, args| call_ctx(error::eval_error_create, heap, this, args),
    UriErrorCreate => |heap, this, args| call_ctx(error::uri_error_create, heap, this, args),
    ErrorIsError => |heap, this, args| call_ctx(error::error_is_error, heap, this, args),
    ErrorProtoToString => |heap, this, args| call_ctx(error::error_proto_to_string, heap, this, args),
    AggregateErrorCreate => {
        |heap, this, args| call_ctx(error::aggregate_error_create, heap, this, args)
    },
    ObjectConstruct => |heap, this, args| call_ctx(object::object_construct, heap, this, args),
    ArrayConstruct => |heap, this, args| call_ctx(array::array_construct, heap, this, args),
    MapConstruct => |heap, this, args| call_ctx(map::map_construct, heap, this, args),
    MapSize => |heap, this, args| call_ctx(map::map_size, heap, this, args),
    SetConstruct => |heap, this, args| call_ctx(map::set_construct, heap, this, args),
    SetSize => |heap, this, args| call_ctx(map::set_size, heap, this, args),
    IteratorNext => |heap, this, args| call_ctx(iterator::iterator_next, heap, this, args),
    // The `Iterator` constructor's body and the shared
    // `%Iterator.prototype%[Symbol.iterator]()`. Both are wired by the realm
    // (a callable field / a symbol key), so they are dispatch-only ids.
    IteratorConstruct => |heap, this, args| call_ctx(iterator::iterator_construct, heap, this, args),
    IteratorSelf => |heap, this, args| call_ctx(iterator::iterator_self, heap, this, args),
    SymbolConstruct => |heap, this, args| call_ctx(symbol::symbol_construct, heap, this, args),
    ProxyConstruct => |heap, this, args| call_ctx(proxy::proxy_construct, heap, this, args),
    MapIterator => |heap, this, args| call_ctx(iterator::map_iterator, heap, this, args),
    SetIterator => |heap, this, args| call_ctx(iterator::set_iterator, heap, this, args),
    RegExpConstruct => |heap, this, args| call_ctx(regexp::regexp_construct, heap, this, args),
    RegExpToString => |heap, this, args| call_ctx(regexp::regexp_to_string, heap, this, args),
    ObjectEnumerableOwnKeys => |heap, this, args| call_ctx(object::object_enumerable_own_keys, heap, this, args),
    // Date: installed manually by the realm rather than by the macro.
    // `toGMTString` must be the *same function object* as `toUTCString`
    // (test262 `annexB/.../toGMTString/value.js`), and
    // `[Symbol.toPrimitive]` needs a symbol key the string-based install
    // cannot express.
    DateProtoToGmtString => |heap, this, args| call_ctx(date::date_proto_to_utc_string, heap, this, args),
    DateProtoToPrimitive => |heap, this, args| call_ctx(date::date_proto_to_primitive, heap, this, args),
    DateProtoToTemporalInstant => |heap, this, args| call_ctx(date::date_proto_to_temporal_instant, heap, this, args),
    // Keyed promise combinators (27.2.4). Dispatch arms only: the *install*
    // happens in `install_keyed_promise_combinators` below, because the
    // `Promise` constructor is not a `BuiltinTargets` field — the realm
    // installs the `Promise` statics by hand, since all of them need the
    // interpreter seam. These two do not (they read own keys and `Get`, both
    // pure heap), so the install pass reaches the constructor through the
    // global's `GLOBAL_INTRINSICS` slot instead of adding a targets field.
    PromiseAllKeyed => |heap, this, args| call_ctx(promise::promise_all_keyed, heap, this, args),
    PromiseAllSettledKeyed => |heap, this, args| call_ctx(promise::promise_all_settled_keyed, heap, this, args),
}

/// Installs `Promise.allKeyed` and `Promise.allSettledKeyed` on the realm's
/// `Promise` constructor.
///
/// Why not the `Promise` group in `define_builtins!`:
/// [`BuiltinTargets`] has no `promise` field, and adding one would mean
/// editing `realm.rs`'s `BuiltinTargets` literal — outside this lane's file
/// ownership. The `Promise` statics are installed by the realm rather than by
/// the macro precisely because `all`/`race`/`any`/`allSettled`/`withResolvers`/
/// `try`/`then`/`catch`/`finally` all need the interpreter seam; these two
/// need only own-key enumeration, a `[[Get]]`, and the existing reaction-record
/// machinery, so the macro's install path is a better fit.
///
/// Idempotent by construction: the pass is skipped when the realm global is
/// absent, and `define_method` is only reached for a `Promise` that does not
/// already carry the names, so a later move of the `Promise` statics into the
/// macro cannot double-install.
fn install_keyed_promise_combinators(heap: &mut Heap) {
    let Some(idx) = v12_bytecode::GLOBAL_INTRINSICS
        .iter()
        .position(|n| *n == "Promise")
    else {
        return;
    };
    let Some(global) = heap.realm_globals().first().copied() else {
        return;
    };
    let Some(ctor) = heap
        .get(global)
        .properties
        .get(idx)
        .copied()
        .and_then(|v| v.as_object())
    else {
        return;
    };
    let mut ctx = Ctx::new(heap, Some(global), None);
    for (name, id) in [
        ("allKeyed", NativeId::PromiseAllKeyed),
        ("allSettledKeyed", NativeId::PromiseAllSettledKeyed),
    ] {
        let key = v12_heap::PropKey::from_string(ctx.heap.intern_text(name));
        if ctx
            .heap
            .lookup_property(ctx.heap.shape_of(ctor), key)
            .is_some()
        {
            continue;
        }
        ctx.define_method(Some(ctor), name, id, Some(1));
    }
}

/// Installs the `Reflect` static methods onto `targets.reflect`.
///
/// Separate from [`install_builtins`] because `Reflect` is materialized by
/// the realm *after* the main install pass runs (with `reflect: None`), so a
/// second `install_builtins` call would double-install every other group.
/// The dispatch arms still live in the macro's `Reflect` group — this only
/// performs the installs.
pub fn install_reflect_builtins(heap: &mut Heap, targets: &BuiltinTargets) {
    let Some(reflect) = targets.reflect else {
        return;
    };
    for (name, id) in [
        ("getPrototypeOf", NativeId::ReflectGetPrototypeOf),
        ("setPrototypeOf", NativeId::ReflectSetPrototypeOf),
        ("isExtensible", NativeId::ReflectIsExtensible),
        ("preventExtensions", NativeId::ReflectPreventExtensions),
        ("has", NativeId::ReflectHas),
        ("deleteProperty", NativeId::ReflectDeleteProperty),
        ("ownKeys", NativeId::ReflectOwnKeys),
        (
            "getOwnPropertyDescriptor",
            NativeId::ReflectGetOwnPropertyDescriptor,
        ),
        ("defineProperty", NativeId::ReflectDefineProperty),
        ("get", NativeId::ReflectGet),
        ("set", NativeId::ReflectSet),
        ("apply", NativeId::ReflectApply),
        ("construct", NativeId::ReflectConstruct),
    ] {
        install_native(heap, Some(reflect), name, id);
    }
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

/// Gives `%Function.prototype%` the identity the spec gives it (ES 10.2.4):
/// the `[[Prototype]]` link to `%Object.prototype%`, plus the `length` and
/// `name` own data properties every built-in function object carries.
///
/// **Why a separate pass and not macro entries.** The realm allocates
/// `%Function.prototype%` as a bare `Kind::Function`
/// (`realm.rs`: `JsObject::function(FunctionTarget::Bytecode(u32::MAX), None)`)
/// and links only the `constructor` back-link through `install_ctor`. Two of
/// the three things above are not *named methods*, so `define_builtins!` has no
/// syntax for them: the `[[Prototype]]` is a heap field, not a property, and
/// `length`/`name` are per-object attributes that must precede the method
/// group for `built-ins/Function/prototype/property-order.js` (which asserts
/// `name` immediately follows `length` in `getOwnPropertyNames`). The realm
/// owns the allocation and is not this lane's file, so the pass runs from
/// `install_builtins`, which the realm calls with both handles already wired.
///
/// `length`/`name` carry the standard built-in-function attributes
/// `{ writable: false, enumerable: false, configurable: true }` (ES 10.2.4 /
/// the "Properties of the Function Prototype Object" clause), which is
/// `define_method`'s own shape — so the install goes through
/// `Ctx::define_method` on the prototype object itself rather than
/// `define_data_prop_with_attrs` by hand, keeping one install shape.
pub(crate) fn install_function_proto_identity(heap: &mut Heap, targets: &BuiltinTargets) {
    let proto = targets.function_proto;
    // ES 10.2.4: "%Function.prototype%'s [[Prototype]] internal slot is a
    // reference to %Object.prototype%". Without this link every inherited
    // `Object.prototype` member (`valueOf`, `hasOwnProperty`, and a
    // user-installed `indicator`) reads `undefined` off the function
    // prototype object, and `Object.getPrototypeOf(Function.prototype)`
    // answers `null`.
    heap.get_mut(proto).prototype = Some(targets.object_proto);
    // The `constructor` back-link that `install_ctor` already installed is the
    // prototype's *only* own property here; `length` then `name` follow it in
    // insertion order, which is what `property-order.js` measures.
    define_method_length_and_name(heap, proto);
}

/// Installs the `length` and `name` own data properties on an *existing*
/// function object, with the standard built-in-function attributes
/// `{ writable: false, enumerable: false, configurable: true }` and in that
/// order.
///
/// `Ctx::define_method` stamps the same two properties, but it also *allocates*
/// the function object from a `NativeId` — `%Function.prototype%` is
/// allocated by the realm and has no id of its own (its callable is the
/// `u32::MAX` placeholder). This is the same shape, factored over the two
/// cases.
///
/// Order is load-bearing, not cosmetic: `CreateBuiltinFunction` (ES 10.1.8)
/// defines `length` then `name`, and
/// `built-ins/Function/prototype/property-order.js` asserts that
/// `getOwnPropertyNames` lists them adjacently in that order.
fn define_method_length_and_name(heap: &mut Heap, func: Handle<v12_heap::JsObject>) {
    let mut ctx = Ctx::new(heap, None, None);
    // `length` first: see the order note above.
    ctx.define_data_prop_with_attrs(
        func,
        "length",
        // +0 as a plain `f64`: `SetFunctionLength` stores a Number, and `+0`
        // is its only legal zero, so the Smi and double spellings are
        // indistinguishable to `SameValue` and to `typeof`.
        JsValue::from_f64(0.0),
        v12_heap::Attrs::new(false, false, true),
    );
    // `%Function.prototype%.name` is the empty string (ES 10.2.4), not the
    // absent property the object had before.
    let empty = ctx.heap.intern_text("");
    ctx.define_data_prop_with_attrs(
        func,
        "name",
        JsValue::string(empty),
        v12_heap::Attrs::new(false, false, true),
    );
}

/// Installs `Function.prototype.call` (arity 1), `.apply` (arity 2) and
/// `.bind` (arity 1) as real own properties of `%Function.prototype%`
/// (ES 10.2.3, 10.2.4, 10.2.5).
///
/// **Why not a `define_builtins!` `FunctionProto` group entry.** The macro is
/// two things at once: it emits an install *and* a `builtin_dispatch` arm whose
/// body is a `(&mut Heap, JsValue, &[JsValue]) -> Result<JsValue, Throw>`
/// closure. These three have no such body — they are implemented by the
/// interpreter router (`Interp::dispatch_native`, `v12-interp/src/call_setup.rs`),
/// because `call` and `apply` have to *re-enter the machine* to invoke their
/// target with a computed `this` and argument list, and `bind` has to compile
/// and register nothing but must read the target's `length`/`name`. A macro
/// entry would need a duplicate second implementation behind it, so this pass
/// borrows the established hand-written-install shape instead — the same
/// reason `install_string_regex_methods` and `install_regexp_methods` exist.
///
/// Each goes through [`Ctx::define_method`], so each gets the same
/// `{ writable: false, enumerable: false, configurable: true }` `length` and
/// `name` own properties every other built-in function object has, and the
/// arity is read from [`builtin_length`] rather than repeated here (that is
/// also what `define_method`'s own `debug_assert` checks). The install is
/// `{ writable: true, enumerable: false, configurable: true }` — `BUILTIN`,
/// which `define_method`'s final `define_data_prop` uses.
///
/// Before this pass the only way to reach these three was the interpreter's
/// synthesized `function_method_surface`, which allocated a bare function
/// object per read: no `length`, no `name`, and no identity
/// (`Function.prototype.call !== Function.prototype.call`).
pub(crate) fn install_function_proto_methods(heap: &mut Heap, targets: &BuiltinTargets) {
    let mut ctx = Ctx::new(heap, None, None);
    let proto = Some(targets.function_proto);
    for (name, id) in [
        ("call", NativeId::FunctionCall),
        ("apply", NativeId::FunctionApply),
        ("bind", NativeId::FunctionBind),
    ] {
        ctx.define_method(proto, name, id, builtin_length(id));
    }
}
