//! Object built-ins.
//!
//! Phase 3 step 3 migration (`docs/builtins-arch-plan.md` §5.3): bodies take
//! `&mut Ctx`; the legacy `&mut Heap` dispatch site reaches them through
//! `ctx::call_ctx`, so dispatch IDs and install paths are unchanged.

use v12_heap::{Handle, JsObject, JsValue, PropKey, V12Str};
use v12_native::Throw;

use super::ctx::Ctx;
use super::helpers;

/// `Object.create(proto)` – creates a new ordinary object with `proto` as
/// its prototype. `proto` may be an object or `null`.
pub fn object_create(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let proto = args.first().copied().unwrap_or(JsValue::null());
    let proto_handle = if proto.is_null() {
        None
    } else if let Some(h) = proto.as_object() {
        Some(h)
    } else {
        return Err(ctx.type_error("TypeError: Object.create prototype must be object or null"));
    };
    let obj = ctx.heap.alloc(JsObject::environment(0, proto_handle));
    Ok(JsValue::object(obj))
}

/// `Object([value])` – callable/constructible form: `undefined`/`null`
/// produce a fresh ordinary object; a primitive wraps into its kind object
/// (v1: a plain object carrying the primitive as storage is not modeled, so
/// primitives return a fresh object); an object passes through.
pub fn object_construct(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let value = args.first().copied().unwrap_or(JsValue::undefined());
    if let Some(obj) = value.as_object() {
        return Ok(JsValue::object(obj));
    }
    let obj = ctx.heap.alloc(JsObject::default());
    Ok(JsValue::object(obj))
}

/// `Object.getPrototypeOf(obj)` – returns the prototype.
pub fn object_get_prototype_of(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| ctx.type_error("TypeError: Object.getPrototypeOf called on non-object"))?;
    match ctx.heap.get(obj).prototype {
        Some(p) => Ok(JsValue::object(p)),
        None => Ok(JsValue::null()),
    }
}

/// Minimal `ToPropertyDescriptor` for data descriptors: reads `value`,
/// `writable`, `enumerable`, `configurable` with spec-default `false` for
/// absent flags. `PropertyDescriptor::default()` is all-`true`, so it is
/// deliberately not used here.
fn parse_data_descriptor(ctx: &mut Ctx, v: JsValue) -> Result<crate::internal_methods::PropertyDescriptor, Throw> {
    let mut desc = crate::internal_methods::PropertyDescriptor {
        value: None,
        writable: false,
        enumerable: false,
        configurable: false,
    };
    let Some(obj) = v.as_object() else {
        return Err(ctx.type_error("TypeError: Property description must be an object"));
    };
    for name in ["value", "writable", "enumerable", "configurable"] {
        let key = ctx.heap.intern_text(name);
        let present = {
            let shape = ctx.heap.shape_of(obj);
            ctx.heap.lookup_property(shape, PropKey::from_string(key)).is_some()
        };
        if !present {
            continue;
        }
        let got = crate::internal_methods::dispatch_get(
            &mut *ctx.heap,
            obj,
            PropKey::from_string(key),
            JsValue::object(obj),
        )
        .map_err(Throw::Value)?;
        match name {
            "value" => desc.value = Some(got),
            "writable" => desc.writable = super::boolean::to_boolean(ctx, got),
            "enumerable" => desc.enumerable = super::boolean::to_boolean(ctx, got),
            "configurable" => desc.configurable = super::boolean::to_boolean(ctx, got),
            _ => unreachable!(),
        }
    }
    Ok(desc)
}

/// `Object.defineProperty(obj, key, descriptor)` – defines a property via
/// shape, honoring the descriptor's `value`/`writable`/`enumerable`/
/// `configurable` flags (absent flags default to `false`).
pub fn object_define_property(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    if args.len() < 2 {
        return Err(ctx.type_error("TypeError: Object.defineProperty requires 2 arguments"));
    }
    let obj = args[0]
        .as_object()
        .ok_or_else(|| ctx.type_error("TypeError: Object.defineProperty called on non-object"))?;
    let key = property_key(ctx, args[1]).map_err(Throw::Value)?;

    // Delegate to the ordinary [[DefineOwnProperty]] implementation: it
    // dispatches on object kind (arrays, arguments exotics) and handles the
    // shape extension + binding for new keys.
    let descriptor = if args.len() >= 3 {
        parse_data_descriptor(ctx, args[2])?
    } else {
        crate::internal_methods::PropertyDescriptor {
            value: Some(JsValue::undefined()),
            writable: false,
            enumerable: false,
            configurable: false,
        }
    };
    let defined = crate::internal_methods::ordinary_define_own_property(
        &mut *ctx.heap,
        obj,
        key,
        descriptor,
    )
    .map_err(Throw::Value)?;
    if !defined {
        return Err(ctx.type_error(
            "TypeError: Cannot redefine property: Invalid property definition",
        ));
    }
    Ok(JsValue::object(obj))
}

pub fn object_keys(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = args.first().and_then(|v| v.as_object()).ok_or_else(|| ctx.type_error("TypeError: Object.keys called on non-object"))?;
    let keys = collect_own_string_keys(&ctx.heap, obj);
    let arr = ctx.heap.alloc(v12_heap::JsObject::array(keys.iter().map(|&k| JsValue::string(k)).collect()));
    ctx.add_root(JsValue::object(arr));
    Ok(JsValue::object(arr))
}

pub fn object_values(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = args.first().and_then(|v| v.as_object()).ok_or_else(|| ctx.type_error("TypeError: Object.values called on non-object"))?;
    let vals = collect_own_values(&ctx.heap, obj);
    let arr = ctx.heap.alloc(v12_heap::JsObject::array(vals));
    ctx.add_root(JsValue::object(arr));
    Ok(JsValue::object(arr))
}

pub fn object_entries(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = args.first().and_then(|v| v.as_object()).ok_or_else(|| ctx.type_error("TypeError: Object.entries called on non-object"))?;
    let keys = collect_own_string_keys(&ctx.heap, obj);
    let vals = collect_own_values(&ctx.heap, obj);
    let pairs: Vec<JsValue> = keys.into_iter().zip(vals).map(|(k, v)| {
        let ks = JsValue::string(k);
        let pair = ctx.heap.alloc(v12_heap::JsObject::array(vec![ks, v]));
        ctx.add_root(JsValue::object(pair));
        JsValue::object(pair)
    }).collect();
    let arr = ctx.heap.alloc(v12_heap::JsObject::array(pairs));
    ctx.add_root(JsValue::object(arr));
    Ok(JsValue::object(arr))
}

/// Internal `for-in` helper: own *enumerable* string keys in spec order
/// (array indices ascending, then named keys in insertion order).
/// `null`/`undefined` yield an empty array (the loop body never runs).
/// Dispatch-only (never installed on a JS object); see the bare entries in
/// `define_builtins!`.
pub fn object_enumerable_own_keys(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let Some(obj) = args.first().and_then(|v| v.as_object()) else {
        return Ok(array_value(ctx, Vec::new()));
    };
    let mut items: Vec<JsValue> = Vec::new();
    // Integer-indexed elements (array indices come first, ascending).
    let o = ctx.heap.get(obj);
    if o.kind == v12_heap::Kind::Array {
        let len = o.element_len() as u32;
        for i in 0..len {
            items.push(JsValue::string(ctx.heap.intern_text(&i.to_string())));
        }
    } else {
        let elems = o.elements.clone();
        for (i, v) in elems.iter().enumerate() {
            if !v.is_hole() {
                items.push(JsValue::string(ctx.heap.intern_text(&(i as u32).to_string())));
            }
        }
    }
    // Named string keys, enumerable only (data or accessor alike).
    // Arrays carry a `length` shape descriptor (installed with default
    // attrs); per spec `length` is non-enumerable, so skip it here.
    let shape = ctx.heap.shape_of(obj);
    let is_array = ctx.heap.get(obj).kind == v12_heap::Kind::Array;
    let handles: Vec<v12_heap::Handle<v12_heap::V12Str>> = ctx
        .heap
        .get(shape)
        .descriptors
        .as_slice()
        .iter()
        .filter(|d| d.attrs().enumerable())
        .filter_map(|d| d.key().string())
        .collect();
    for h in handles {
        if is_array && ctx.string_text(h) == "length" {
            continue;
        }
        items.push(JsValue::string(h));
    }
    // Dictionary-rung overflow keys, enumerable only, insertion order.
    // (Length-skip is shape business; overflow keys are user data.)
    if let Some(map) = ctx.heap.get(obj).dictionary.as_ref() {
        let mut overflow: Vec<(u32, v12_heap::Handle<v12_heap::V12Str>)> = map
            .iter()
            .filter(|(_, e)| e.attrs.enumerable())
            .filter_map(|(k, e)| k.string().map(|h| (e.seq, h)))
            .collect();
        overflow.sort_by_key(|&(seq, _)| seq);
        items.extend(overflow.into_iter().map(|(_, h)| JsValue::string(h)));
    }
    Ok(array_value(ctx, items))
}

/// Whether `desc` is a live own property of `obj`. `delete` stores `hole` in a
/// data property's slot while leaving the shared shape descriptor in place, so
/// a holed data descriptor is not observable. Accessors have no slot and are
/// always live.
fn descriptor_is_live(heap: &v12_heap::Heap, obj: Handle<v12_heap::JsObject>, desc: &v12_heap::Descriptor) -> bool {
    match desc {
        v12_heap::Descriptor::Data { slot, .. } => heap
            .get(obj)
            .properties
            .get(*slot as usize)
            .is_some_and(|v| !v.is_hole()),
        v12_heap::Descriptor::Accessor { .. } => true,
    }
}

/// Dictionary-rung analog: data entries read non-hole storage,
/// accessors are always live.
fn dict_entry_is_live(
    heap: &v12_heap::Heap,
    obj: Handle<v12_heap::JsObject>,
    entry: &v12_heap::DictEntry,
) -> bool {
    if entry.is_accessor {
        return true;
    }
    heap.get(obj)
        .properties
        .get(entry.slot as usize)
        .is_some_and(|v| !v.is_hole())
}

pub fn object_has_own_property(ctx: &mut Ctx, this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let this_obj = this.as_object().ok_or_else(|| ctx.type_error("TypeError: Object.prototype.hasOwnProperty called on non-object"))?;
    let key = args.first().copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key).map_err(Throw::Value)?;
    // Dictionary rung first (overflow keys live only here).
    if let Some((entry, _)) = crate::internal_methods::dict_lookup(ctx.heap, this_obj, pk) {
        return Ok(JsValue::from_bool(dict_entry_is_live(
            ctx.heap,
            this_obj,
            &entry,
        )));
    }
    let shape = ctx.heap.shape_of(this_obj);
    let found = ctx
        .heap
        .lookup_property(shape, pk)
        .is_some_and(|d| descriptor_is_live(ctx.heap, this_obj, d));
    Ok(JsValue::from_bool(found))
}

/// `Object.prototype.propertyIsEnumerable(key)` – whether `key` is an own
/// enumerable data property of `this`.
pub fn object_proto_property_is_enumerable(
    ctx: &mut Ctx,
    this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = this.as_object().ok_or_else(|| {
        ctx.type_error("TypeError: Object.prototype.propertyIsEnumerable called on non-object")
    })?;
    let key_v = args.first().copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v).map_err(Throw::Value)?;
    // Dictionary rung first (overflow keys live only here).
    if let Some((entry, _)) = crate::internal_methods::dict_lookup(ctx.heap, obj, pk) {
        if entry.is_accessor {
            return Ok(JsValue::from_bool(entry.attrs.enumerable()));
        }
        let populated = ctx
            .heap
            .get(obj)
            .properties
            .get(entry.slot as usize)
            .is_some_and(|v| !v.is_hole());
        return Ok(JsValue::from_bool(populated && entry.attrs.enumerable()));
    }
    let shape = ctx.heap.shape_of(obj);
    let enumerable = match ctx.heap.lookup_property(shape, pk) {
        Some(desc) if desc.is_data() => {
            let populated = desc
                .slot()
                .and_then(|slot| ctx.heap.get(obj).properties.get(slot as usize))
                .is_some_and(|v| !v.is_hole());
            populated && desc.attrs().enumerable()
        }
        Some(desc) => desc.attrs().enumerable(),
        None => false,
    };
    Ok(JsValue::from_bool(enumerable))
}

pub fn object_proto_to_string(ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    let text = if this.is_object() && ctx.heap.get(this.as_object().unwrap()).kind == v12_heap::Kind::Array { "[object Array]" } else { "[object Object]" };
    Ok(JsValue::string(ctx.heap.intern_text(text)))
}

pub fn object_proto_value_of(_ctx: &mut Ctx, this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    Ok(this)
}

pub fn function_proto_to_string(ctx: &mut Ctx, _this: JsValue, _args: &[JsValue]) -> Result<JsValue, Throw> {
    Ok(JsValue::string(ctx.heap.intern_text("function() {}")))
}

/// Own string keys from the shape descriptors (the shape is authoritative:
/// properties defined via [[DefineOwnProperty]] never touch the parallel
/// `property_keys` vec), plus dictionary-rung overflow keys in insertion
/// order (the two stores never overlap).
fn collect_own_string_keys(heap: &v12_heap::Heap, obj: Handle<v12_heap::JsObject>) -> Vec<Handle<V12Str>> {
    let shape = heap.shape_of(obj);
    let mut keys: Vec<Handle<V12Str>> = heap
        .get(shape)
        .descriptors
        .as_slice()
        .iter()
        .filter_map(|d| d.key().string())
        .collect();
    if let Some(map) = heap.get(obj).dictionary.as_ref() {
        let mut overflow: Vec<(u32, Handle<V12Str>)> = map
            .iter()
            .filter_map(|(k, e)| k.string().map(|h| (e.seq, h)))
            .collect();
        overflow.sort_by_key(|&(seq, _)| seq);
        keys.extend(overflow.into_iter().map(|(_, h)| h));
    }
    keys
}

fn collect_own_values(heap: &v12_heap::Heap, obj: Handle<v12_heap::JsObject>) -> Vec<JsValue> {
    let shape = heap.shape_of(obj);
    let mut vals: Vec<JsValue> = heap
        .get(shape)
        .descriptors
        .as_slice()
        .iter()
        .filter_map(|d| {
            d.slot()
                .and_then(|slot| heap.get(obj).properties.get(slot as usize))
                .copied()
        })
        .collect();
    // Overflow values in the same insertion order as the keys above, so
    // `entries` zipping stays aligned. No hole filtering here either —
    // the shape path includes holes positionally.
    if let Some(map) = heap.get(obj).dictionary.as_ref() {
        let mut overflow: Vec<(u32, JsValue)> = map
            .iter()
            .map(|(_, e)| {
                (
                    e.seq,
                    heap.get(obj)
                        .properties
                        .get(e.slot as usize)
                        .copied()
                        .unwrap_or(JsValue::undefined()),
                )
            })
            .collect();
        overflow.sort_by_key(|&(seq, _)| seq);
        vals.extend(overflow.into_iter().map(|(_, v)| v));
    }
    vals
}

fn property_key(ctx: &mut Ctx, v: JsValue) -> Result<PropKey, JsValue> {
    if let Some(h) = v.as_string() {
        // Intern: `PropKey` identity is handle identity, so a non-canonical
        // handle (concat, slice, computed key) must alias the canonical
        // instance or shape lookup misses. Flatten-once first, then a
        // single `intern_string` over the flat storage.
        ctx.heap.flatten(h);
        let owned = match &ctx.heap.get(h).storage {
            v12_heap::StrStorage::Latin1(bytes) => v12_heap::V12Str::latin1(bytes.clone()),
            v12_heap::StrStorage::Utf16(units) => v12_heap::V12Str::utf16(units.clone()),
            // Unreachable post-flatten (flatten materializes in place), but
            // degrade gracefully instead of assuming it: re-read lossy text.
            _ => v12_heap::V12Str::utf16(
                helpers::string_text(&mut *ctx.heap, h)
                    .encode_utf16()
                    .collect(),
            ),
        };
        return Ok(PropKey::from_string(ctx.heap.intern_string(owned)));
    }
    if let Some(sym) = v.as_symbol() {
        return Ok(PropKey::from_symbol(sym));
    }
    // Coerce via ToString.
    let h = to_string_handle(ctx, v)?;
    Ok(PropKey::from_string(h))
}

fn to_string_handle(ctx: &mut Ctx, v: JsValue) -> Result<Handle<V12Str>, JsValue> {
    let text = ctx.to_string(v);
    Ok(ctx.heap.intern_text(&text))
}


// ---------------------------------------------------------------------------
// Object statics
// ---------------------------------------------------------------------------

/// Allocates an array value from `items` (rooted, like every native-created
/// object that outlives the call).
fn array_value(ctx: &mut Ctx, items: Vec<JsValue>) -> JsValue {
    let arr = ctx.heap.alloc(v12_heap::JsObject::array(items));
    ctx.add_root(JsValue::object(arr));
    JsValue::object(arr)
}

/// `Object.is(a, b)` – ES `SameValue`: NaN is NaN, and ±0 differ.
pub fn object_is(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let a = args.first().copied().unwrap_or(JsValue::undefined());
    let b = args.get(1).copied().unwrap_or(JsValue::undefined());
    let same = match (a.as_smi().map(f64::from).or(a.as_f64()), b.as_smi().map(f64::from).or(b.as_f64())) {
        (Some(x), Some(y)) => {
            (x.is_nan() && y.is_nan())
                || (x == y && (x != 0.0 || x.to_bits() == y.to_bits()))
        }
        _ => helpers::strict_equals(&ctx.heap, a, b),
    };
    Ok(JsValue::from_bool(same))
}

/// `Object.hasOwn(obj, key)` – `hasOwnProperty` as a static.
pub fn object_has_own(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| ctx.type_error("TypeError: Object.hasOwn called on non-object"))?;
    let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v).map_err(Throw::Value)?;
    let shape = ctx.heap.shape_of(obj);
    Ok(JsValue::from_bool(
        ctx.heap
            .lookup_property(shape, pk)
            .is_some_and(|d| descriptor_is_live(ctx.heap, obj, d)),
    ))
}

/// `Object.assign(target, ...sources)` – copies own *enumerable* properties
/// from each source onto the target via [[DefineOwnProperty]].
pub fn object_assign(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    let target = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| ctx.type_error("TypeError: Object.assign called on non-object"))?;
    for &source in args.iter().skip(1) {
        let Some(src) = source.as_object() else { continue };
        // Integer-indexed elements: aligned element-store copy.
        if ctx.heap.get(src).kind == v12_heap::Kind::Array {
            let elems = ctx.heap.get(src).elements_snapshot();
            for (i, v) in elems.iter().enumerate() {
                if !v.is_hole() {
                    ctx.heap.get_mut(target).set_element(i as u32, *v);
                }
            }
        } else {
            let elems = ctx.heap.get(src).elements.clone();
            for (i, v) in elems.iter().enumerate() {
                if !v.is_hole() {
                    ctx.heap.get_mut(target).set_element(i as u32, *v);
                }
            }
        }
        // Named properties: snapshot from the shape descriptors (the shape
        // is authoritative — values defined via [[DefineOwnProperty]] never
        // touch the parallel `property_keys` vec), copying only the
        // enumerable ones. Snapshot first: defines may allocate.
        let shape = ctx.heap.shape_of(src);
        let slots: Vec<(PropKey, u32)> = ctx
            .heap
            .get(shape)
            .descriptors
            .as_slice()
            .iter()
            .filter(|d| d.attrs().enumerable())
            .filter_map(|d| d.slot().map(|slot| (d.key(), slot)))
            .collect();
        let mut pairs: Vec<(PropKey, JsValue)> = Vec::with_capacity(slots.len());
        for (pk, slot) in slots {
            if let Some(&v) = ctx.heap.get(src).properties.get(slot as usize) {
                pairs.push((pk, v));
            }
        }
        for (pk, value) in pairs {
            crate::internal_methods::ordinary_define_own_property(
                &mut *ctx.heap,
                target,
                pk,
                crate::internal_methods::PropertyDescriptor {
                    value: Some(value),
                    ..Default::default()
                },
            )
            .map_err(Throw::Value)?;
        }
    }
    Ok(JsValue::object(target))
}

/// `Object.freeze(obj)` / `Object.seal(obj)` share this shape; primitives
/// are returned unchanged (nothing to lock down).
fn set_integrity(
    ctx: &mut Ctx,
    args: &[JsValue],
    level: v12_heap::IntegrityLevel,
) -> Result<JsValue, Throw> {
    match args.first().and_then(|v| v.as_object()) {
        Some(obj) => {
            ctx.heap.set_integrity_level(obj, level);
            Ok(JsValue::object(obj))
        }
        None => Ok(args.first().copied().unwrap_or(JsValue::undefined())),
    }
}

pub fn object_freeze(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    set_integrity(ctx, args, v12_heap::IntegrityLevel::Frozen)
}

pub fn object_seal(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    set_integrity(ctx, args, v12_heap::IntegrityLevel::Sealed)
}

/// `Object.preventExtensions(obj)` – non-extensible flag only.
pub fn object_prevent_extensions(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    if let Some(obj) = args.first().and_then(|v| v.as_object()) {
        ctx.heap.get_mut(obj).flags |= v12_heap::JsObject::FLAG_NOT_EXTENSIBLE;
        let cell = ctx.heap.validity_cell_of(obj);
        ctx.heap.bump_validity(cell);
    }
    Ok(args.first().copied().unwrap_or(JsValue::undefined()))
}

/// `Object.isSealed(obj)` / `Object.isFrozen(obj)` share this shape;
/// non-objects are always sealed/frozen.
fn check_integrity(
    ctx: &mut Ctx,
    args: &[JsValue],
    check: fn(&v12_heap::JsObject) -> bool,
) -> Result<JsValue, Throw> {
    let result = match args.first().and_then(|v| v.as_object()) {
        Some(obj) => check(ctx.heap.get(obj)),
        None => true,
    };
    Ok(JsValue::from_bool(result))
}

pub fn object_is_sealed(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    check_integrity(ctx, args, v12_heap::JsObject::is_sealed)
}

pub fn object_is_frozen(ctx: &mut Ctx, _this: JsValue, args: &[JsValue]) -> Result<JsValue, Throw> {
    check_integrity(ctx, args, v12_heap::JsObject::is_frozen)
}

/// `Object.isExtensible(obj)` – false for primitives, flag test for objects.
pub fn object_is_extensible(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let extensible = match args.first().and_then(|v| v.as_object()) {
        Some(obj) => ctx.heap.get(obj).flags & v12_heap::JsObject::FLAG_NOT_EXTENSIBLE == 0,
        None => false,
    };
    Ok(JsValue::from_bool(extensible))
}

/// `Object.fromEntries(entries)` – array of `[key, value]` pairs → object.
pub fn object_from_entries(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = ctx.alloc_obj(JsObject::default());
    let Some(entries) = args.first().and_then(|v| v.as_object()) else {
        return Ok(JsValue::object(obj));
    };
    let len = ctx.heap.get(entries).element_len();
    for i in 0..len as u32 {
        let Some(pair) = ctx.heap.get(entries).get_element(i).and_then(|v| v.as_object()) else {
            continue;
        };
        let k = ctx
            .heap
            .get(pair)
            .get_element(0)
            .unwrap_or(JsValue::undefined());
        let v = ctx
            .heap
            .get(pair)
            .get_element(1)
            .unwrap_or(JsValue::undefined());
        let pk = property_key(ctx, k).map_err(Throw::Value)?;
        crate::internal_methods::ordinary_define_own_property(
            &mut *ctx.heap,
            obj,
            pk,
            crate::internal_methods::PropertyDescriptor {
                value: Some(v),
                ..Default::default()
            },
        )
        .map_err(Throw::Value)?;
    }
    Ok(JsValue::object(obj))
}

/// Own property names in spec order: array indices ascending, then named
/// string keys in insertion order.
pub fn own_property_names(ctx: &mut Ctx, obj: Handle<v12_heap::JsObject>) -> Vec<String> {
    let o = ctx.heap.get(obj);
    let mut names: Vec<String> = Vec::new();
    if o.kind == v12_heap::Kind::Array {
        let len = o.element_len() as u32;
        for i in 0..len {
            names.push(i.to_string());
        }
    } else {
        for (i, v) in o.elements.iter().enumerate() {
            if !v.is_hole() {
                names.push(i.to_string());
            }
        }
    }
    let handles: Vec<v12_heap::Handle<v12_heap::V12Str>> = {
        let shape = ctx.heap.shape_of(obj);
        ctx.heap.get(shape)
            .descriptors
            .as_slice()
            .iter()
            .filter_map(|d| d.key().string())
            .collect()
    };
    let mut named: Vec<String> = Vec::new();
    for h in handles {
        named.push(ctx.string_text(h));
    }
    names.extend(named);
    names
}

/// `Object.getOwnPropertyNames(obj)` – own string-keyed properties.
pub fn object_get_own_property_names(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| ctx.type_error("TypeError: Object.getOwnPropertyNames called on non-object"))?;
    let names = own_property_names(ctx, obj);
    let items: Vec<JsValue> = names.iter().map(|n| JsValue::string(ctx.heap.intern_text(n))).collect();
    Ok(array_value(ctx, items))
}

/// `Object.getOwnPropertySymbols(obj)` – own symbol-keyed properties.
pub fn object_get_own_property_symbols(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| ctx.type_error("TypeError: Object.getOwnPropertySymbols called on non-object"))?;
    let shape = ctx.heap.shape_of(obj);
    let items: Vec<JsValue> = ctx.heap.get(shape)
        .descriptors
        .as_slice()
        .iter()
        .filter_map(|d| d.key().symbol().map(|s| JsValue::symbol(s)))
        .collect();
    Ok(array_value(ctx, items))
}

/// `Object.getOwnPropertyDescriptor(obj, key)` – a plain descriptor object,
/// or `undefined` when the property is absent.
pub fn object_get_own_property_descriptor(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let obj = args.first().and_then(|v| v.as_object()).ok_or_else(|| {
        ctx.type_error("TypeError: Object.getOwnPropertyDescriptor called on non-object")
    })?;
    let key_v = args.get(1).copied().unwrap_or(JsValue::undefined());
    let pk = property_key(ctx, key_v).map_err(Throw::Value)?;
    let shape = ctx.heap.shape_of(obj);
    enum SlotKind {
        Data { slot: u32, writable: bool, enumerable: bool, configurable: bool },
        Accessor { get: Option<v12_heap::Handle<v12_heap::JsObject>>, set: Option<v12_heap::Handle<v12_heap::JsObject>>, enumerable: bool, configurable: bool },
    }
    // Dictionary rung first (overflow keys live only here), with the
    // same liveness filter as the shape path below. Either arm resolves
    // to a `SlotKind` for the shared tail.
    let kind: SlotKind = if let Some((entry, _)) = crate::internal_methods::dict_lookup(ctx.heap, obj, pk) {
        if !dict_entry_is_live(ctx.heap, obj, &entry) {
            return Ok(JsValue::undefined());
        }
        if entry.is_accessor {
            SlotKind::Accessor {
                get: entry.getter,
                set: entry.setter,
                enumerable: entry.attrs.enumerable(),
                configurable: entry.attrs.configurable(),
            }
        } else {
            SlotKind::Data {
                slot: entry.slot,
                writable: entry.attrs.writable(),
                enumerable: entry.attrs.enumerable(),
                configurable: entry.attrs.configurable(),
            }
        }
    } else {
        let Some(desc) = ctx
            .heap
            .lookup_property(shape, pk)
            .filter(|d| descriptor_is_live(ctx.heap, obj, d))
        else {
            return Ok(JsValue::undefined());
        };
        // Copy the descriptor out before any allocation (heap borrows nest).
        if let Some(slot) = desc.slot() {
            SlotKind::Data {
                slot,
                writable: desc.attrs().writable(),
                enumerable: desc.attrs().enumerable(),
                configurable: desc.attrs().configurable(),
            }
        } else {
            SlotKind::Accessor {
                get: desc.getter(),
                set: desc.setter(),
                enumerable: desc.attrs().enumerable(),
                configurable: desc.attrs().configurable(),
            }
        }
    };
    let d = ctx.alloc_obj(JsObject::default());
    match kind {
        SlotKind::Data { slot, writable, enumerable, configurable } => {
            let value = ctx
                .heap
                .get(obj)
                .properties
                .get(slot as usize)
                .copied()
                .unwrap_or(JsValue::undefined());
            define_plain_prop(ctx, d, "value", value);
            define_plain_prop(ctx, d, "writable", JsValue::from_bool(writable));
            define_plain_prop(ctx, d, "enumerable", JsValue::from_bool(enumerable));
            define_plain_prop(ctx, d, "configurable", JsValue::from_bool(configurable));
        }
        SlotKind::Accessor { get, set, enumerable, configurable } => {
            define_plain_prop(ctx, d, "get", get.map(JsValue::object).unwrap_or(JsValue::undefined()));
            define_plain_prop(ctx, d, "set", set.map(JsValue::object).unwrap_or(JsValue::undefined()));
            define_plain_prop(ctx, d, "enumerable", JsValue::from_bool(enumerable));
            define_plain_prop(ctx, d, "configurable", JsValue::from_bool(configurable));
        }
    }
    Ok(JsValue::object(d))
}

/// Defines a plain data property on a fresh descriptor object.
fn define_plain_prop(
    ctx: &mut Ctx,
    obj: v12_heap::Handle<v12_heap::JsObject>,
    name: &str,
    value: JsValue,
) {
    let h = ctx.heap.intern_text(name);
    let key = PropKey::from_string(h);
    let shape = ctx.heap.shape_of_mut(obj);
    let child = ctx.heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
    ctx.heap.bind_shape(obj, child);
    ctx.heap.get_mut(obj).properties.push(value);
    ctx.heap.get_mut(obj).property_keys.push(Some(key));
}

/// `Object.setPrototypeOf(obj, proto)` – rewires the [[Prototype]] link.
pub fn object_set_prototype_of(
    ctx: &mut Ctx,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, Throw> {
    let Some(obj) = args.first().and_then(|v| v.as_object()) else {
        return Ok(args.first().copied().unwrap_or(JsValue::undefined()));
    };
    let proto = args.get(1).copied().unwrap_or(JsValue::undefined());
    let link = if proto.is_null() {
        None
    } else if let Some(h) = proto.as_object() {
        Some(h)
    } else {
        return Err(ctx.type_error("TypeError: Object.setPrototypeOf prototype must be object or null"));
    };
    ctx.heap.get_mut(obj).prototype = link;
    Ok(JsValue::object(obj))
}
