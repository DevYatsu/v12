//! The generic-receiver array methods, run where the machine is.
//!
//! # Why this file exists
//!
//! `docs/adr/0006-accessor-capability-at-the-property-seam.md` records that
//! `v12-engine`'s `[[Get]]` (`internal_methods::ordinary_get`) and this
//! crate's `[[Get]]` (`Interp::get_property`) are two implementations of one
//! spec operation that share no code, and that they disagree.
//!
//! The disagreement is not "one is faster". It is **capability**:
//! `internal_methods::invoke_accessor_getter` can run `FunctionTarget::Native`
//! and `::Host` from a bare `&mut Heap`, and for a **user** closure —
//! `FunctionTarget::Bytecode(index_into_Program_functions)` — it has no
//! `Program` table to index and no frame to run in, so it answers
//! `undefined`. `Interp::get_property` has all three, so it answers the
//! truth. The observable symptom:
//!
//! ```js
//! var o = { 0: "a", 1: "b", get length() { return 3 } };
//! Array.prototype.slice.call(o).length   // node 3, engine-native path 0
//! ```
//!
//! # Why this is the seam and not a fix inside `ordinary_get`
//!
//! `NativeHandler` is `fn(&mut Heap, JsValue, &[JsValue])`
//! (`v12_engine::builtins::registry::NativeHandler`). It already holds the
//! one exclusive `&mut Heap` borrow, so handing it a `&mut Interp` would be
//! an aliasing violation, not a signature tweak. And ADR-0006's Rule 1 —
//! delete `invoke_accessor_getter` and make the case a `Throw` — was measured
//! and **reverted**: it broke 15 tests in `crates/v12-engine` (5 in
//! `regexp_prototype`, 9 in `es2025_surface`, 1 in this ledger) because
//! symbol-keyed and `size`-keyed accessors on engine-owned prototypes
//! (`Array.prototype[Symbol.iterator]`, `Set.prototype.size`) are legitimately
//! `Bytecode`-targeted.
//!
//! That is the case ADR-0006's amendment could not tell apart, and the
//! resolution is **not** in this file: `v12-engine` now distinguishes them
//! with `JsObject::FLAG_ENGINE_ACCESSOR`, stamped by the engine's own
//! accessor allocator, and dispatches a flagged payload through
//! `builtin_dispatch`. That change keeps engine-owned prototype accessors
//! working *and* keeps them correct.
//!
//! What is left — and what this file closes — is the other half: the methods
//! that read a **user-supplied receiver** must read its `length` and indices
//! through `Interp::get_property`, which is the only reader in the codebase
//! that can invoke a user getter. So each body here is a thin spec-step
//! sequence over the shared triple below, and `dispatch_native` routes to it
//! before the registry native is consulted.
//!
//! # The triple
//!
//! Three operations, and they are the ONLY index/length readers in
//! `v12-interp` for these methods (ADR-0006 Rule 3's anti-drift requirement —
//! ADR-0004 asked for one reader and produced six):
//!
//! * [`Interp::generic_len`] — ES `LengthOfArrayLike(O)`.
//! * [`Interp::generic_index`] — ES `HasProperty` + `Get` at an index, as one
//!   answer, because `[[Get]]` alone cannot distinguish a hole from a present
//!   `undefined` and `splice`/`fill`/`copyWithin` must.
//! * [`Interp::generic_set`] — ES `Set(O, k, v, true)`; `true` is `Throw`, so
//!   a refusal is a `TypeError` rather than a silent no-op.
//!
//! Every body below is `LengthOfArrayLike` → per-index `HasProperty`/`Get` →
//! `Set`, with no other property access on the receiver.

use v12_heap::{Handle, JsObject, JsValue};
use v12_native::NativeId;

use crate::ops;
use crate::JSException;
use crate::Kind;

/// Upper bound on one whole-array read (`MAX_SCAN_SPAN` in
/// `v12_engine::builtins::array`, and `MAX_CALLBACK_SCAN` for the
/// callback-taking methods). A sparse receiver with a huge `length` clamps
/// here instead of hanging the runner. A **declared** deviation, matching
/// the two existing bounds.
const MAX_SCAN_SPAN: i64 = 1_000_000;
const MAX_WRITE_SPAN: i64 = 1_000_000;

/// One element of a receiver: `Absent` is a hole, which is not the same as a
/// present `undefined` and must travel with the value through `splice`,
/// `reverse` and `copyWithin`.
#[derive(Clone, Copy)]
enum Slot {
    Present(JsValue),
    Absent,
}

impl Slot {
    fn of(answer: Option<JsValue>) -> Self {
        match answer {
            Some(v) => Slot::Present(v),
            None => Slot::Absent,
        }
    }
    fn value(self) -> JsValue {
        match self {
            Slot::Present(v) => v,
            Slot::Absent => JsValue::undefined(),
        }
    }
}

impl<'i> crate::Interp<'i> {
    // -- the triple ---------------------------------------------------------

    /// ES `LengthOfArrayLike(O)`: `ToLength(Get(O, "length"))`, through
    /// `[[Get]]`.
    ///
    /// The reader that can invoke a user getter, which is the whole point:
    /// `{ get length() { return 3 } }` answers 3 here and 0 through
    /// `internal_methods::array_like_len`.
    pub(crate) fn generic_len(&mut self, base: JsValue) -> Result<i64, JSException> {
        let key = self.new_temp_key("length");
        let v = self.get_property(0, 0, base, key)?;
        let n = self.to_number_value(v)?;
        let n = if n.is_finite() && n > 0.0 { n.trunc() } else { 0.0 };
        Ok(n.clamp(0.0, MAX_SCAN_SPAN as f64) as i64)
    }

    /// ES `HasProperty(O, ToString(i))` + `Get(O, ToString(i))` as one
    /// answer. `None` means "the index is not there".
    pub(crate) fn generic_index(
        &mut self,
        base: JsValue,
        i: i64,
    ) -> Result<Option<JsValue>, JSException> {
        if i < 0 || i > i64::from(u32::MAX) - 1 {
            // 2^32-1 is the last valid array index; nothing can be there.
            return Ok(None);
        }
        let key = JsValue::string(self.heap.intern_text(&i.to_string()));
        if !self.op_in(key, base)? {
            return Ok(None);
        }
        Ok(Some(self.get_property(0, 0, base, key)?))
    }

    /// ES `Set(O, ToString(i), v, true)` — the `true` is `Throw`, so a
    /// refusal raises rather than silently dropping the write.
    pub(crate) fn generic_set(
        &mut self,
        base: JsValue,
        i: i64,
        v: JsValue,
    ) -> Result<(), JSException> {
        if i < 0 || i > i64::from(u32::MAX) - 1 {
            return Ok(());
        }
        let key = JsValue::string(self.heap.intern_text(&i.to_string()));
        self.set_property(base, key, v, true)
    }

    /// `Delete(O, ToString(i))` or throw — the mutators' third operation, and
    /// the reason `index_set` alone is not the triple: `splice` and `shift`
    /// remove indices rather than writing them.
    fn generic_delete(&mut self, base: JsValue, i: i64) -> Result<(), JSException> {
        if i < 0 || i > i64::from(u32::MAX) - 1 {
            return Ok(());
        }
        let key = JsValue::string(self.heap.intern_text(&i.to_string()));
        self.delete_property_or_throw(base, key)
    }

    /// `Set(O, "length", n, true)` — `ArraySetLength`, so `pop`/`shift`/
    /// `splice` finish by truncating through the same `[[Set]]` any other
    /// write uses.
    fn generic_set_len(&mut self, base: JsValue, n: i64) -> Result<(), JSException> {
        let key = self.new_temp_key("length");
        self.set_property(base, key, ops::box_number(n as f64), true)
    }

    /// `ToIntegerOrInfinity`, then the relative-index fold the spec's
    /// `GetRelativeIndex`/`GetIntegerIndex` perform.
    fn relative_index(&mut self, v: Option<JsValue>, len: i64, default: i64) -> i64 {
        let Some(v) = v else { return default };
        if v.is_undefined() {
            return default;
        }
        let n = self.to_number_value(v).unwrap_or(f64::NAN);
        let n = if n.is_nan() { 0.0 } else { n.trunc() };
        let n = n.clamp(i64::MIN as f64, i64::MAX as f64) as i64;
        if n < 0 { (len + n).max(0) } else { n.min(len) }
    }

    // -- the methods -------------------------------------------------------

    /// `Array.prototype.slice(start?, end?)`.
    pub(crate) fn generic_slice(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.slice")?;
        let len = self.generic_len(obj)?;
        let start = self.relative_index(args.first().copied(), len, 0);
        let end = self.relative_index(args.get(1).copied(), len, len);
        let mut out = Vec::new();
        for i in start..end.max(start) {
            out.push(self.generic_index(obj, i)?.unwrap_or_else(JsValue::undefined));
        }
        Ok(crate::call::alloc_array(self, out))
    }

    /// `Array.prototype.at(index)`.
    pub(crate) fn generic_at(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.at")?;
        let len = self.generic_len(obj)?;
        let idx = self.relative_index(args.first().copied(), len, 0);
        if idx < 0 || idx >= len {
            return Ok(JsValue::undefined());
        }
        Ok(self.generic_index(obj, idx)?.unwrap_or_else(JsValue::undefined))
    }

    /// `Array.prototype.indexOf(searchElement, fromIndex?)`.
    pub(crate) fn generic_index_of(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.indexOf")?;
        let target = args.first().copied().unwrap_or_else(JsValue::undefined);
        let len = self.generic_len(obj)?;
        let mut k = if args.len() < 2 {
            0
        } else {
            let n = self.relative_index(args.get(1).copied(), len, 0);
            if n >= len { len } else { n }
        };
        while k < len {
            // Steps 5-7: `HasProperty` first — a hole is skipped even when it
            // would `===` the target, which is why this cannot be a bare `Get`.
            if let Some(v) = self.generic_index(obj, k)?
                && self.strict_equals(v, target)
            {
                return Ok(ops::box_number(k as f64));
            }
            k += 1;
        }
        Ok(ops::box_number(-1.0))
    }

    /// `Array.prototype.lastIndexOf(searchElement, fromIndex?)`.
    pub(crate) fn generic_last_index_of(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.lastIndexOf")?;
        let target = args.first().copied().unwrap_or_else(JsValue::undefined);
        let len = self.generic_len(obj)?;
        if len == 0 {
            return Ok(ops::box_number(-1.0));
        }
        let mut k = len - 1;
        if args.len() >= 2 {
            let n = self.to_number_value(args[1]).unwrap_or(f64::NAN);
            let n = if n.is_nan() { (len - 1) as f64 } else { n.trunc() };
            let n = n.clamp(i64::MIN as f64, i64::MAX as f64) as i64;
            k = if n < 0 { (len + n).max(0) } else { n.min(len - 1) };
        }
        while k >= 0 {
            if let Some(v) = self.generic_index(obj, k)?
                && self.strict_equals(v, target)
            {
                return Ok(ops::box_number(k as f64));
            }
            k -= 1;
        }
        Ok(ops::box_number(-1.0))
    }

    /// `Array.prototype.includes(searchElement, fromIndex?)`.
    pub(crate) fn generic_includes(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.includes")?;
        let target = args.first().copied().unwrap_or_else(JsValue::undefined);
        let len = self.generic_len(obj)?;
        if len == 0 {
            return Ok(JsValue::from_bool(false));
        }
        let mut k = if args.len() < 2 {
            0
        } else {
            let n = self.to_number_value(args[1]).unwrap_or(f64::NAN);
            let n = if n.is_nan() { 0.0 } else { n.trunc() };
            let n = n.clamp(i64::MIN as f64, i64::MAX as f64) as i64;
            if n >= len {
                return Ok(JsValue::from_bool(false));
            }
            if n < 0 { (len + n).max(0) } else { n }
        };
        // Steps 6-7: a hole is a *match* here, unlike `indexOf` — the spec
        // compares `undefined` with no `HasProperty` step. `same_value_zero`
        // also makes `NaN` match `NaN`.
        while k < len {
            let present = self.generic_index(obj, k)?.is_some();
            let v = if present {
                self.generic_index(obj, k)?.unwrap_or_else(JsValue::undefined)
            } else {
                JsValue::undefined()
            };
            if self.same_value_zero(v, target) {
                return Ok(JsValue::from_bool(true));
            }
            k += 1;
        }
        Ok(JsValue::from_bool(false))
    }

    /// `Array.prototype.fill(value, start?, end?)`.
    pub(crate) fn generic_fill(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.fill")?;
        let len = self.generic_len(obj)?;
        let value = args.first().copied().unwrap_or_else(JsValue::undefined);
        let start = self.relative_index(args.get(1).copied(), len, 0);
        let end = self.relative_index(args.get(2).copied(), len, len);
        let hi = end.min(start + MAX_WRITE_SPAN);
        for i in start..hi.max(start) {
            self.generic_set(obj, i, value)?;
        }
        Ok(this_v)
    }

    /// `Array.prototype.copyWithin(target, start, end?)`.
    pub(crate) fn generic_copy_within(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.copyWithin")?;
        let len = self.generic_len(obj)?;
        let to = self.relative_index(args.first().copied(), len, 0);
        let from = self.relative_index(args.get(1).copied(), len, 0);
        let final_ = self.relative_index(args.get(2).copied(), len, len);
        let count = (final_ - from).min(len - to).max(0).min(MAX_WRITE_SPAN);
        // Steps 8-11: `direction` picks ascending vs descending, and the
        // descending case is what makes an overlapping copy correct.
        let (mut from, mut to, mut count) = (from, to, count);
        if from < to && to < from + count {
            from += count - 1;
            to += count - 1;
            while count > 0 {
                let slot = Slot::of(self.generic_index(obj, from)?);
                self.write_slot(obj, to, slot)?;
                from -= 1;
                to -= 1;
                count -= 1;
            }
        } else {
            while count > 0 {
                let slot = Slot::of(self.generic_index(obj, from)?);
                self.write_slot(obj, to, slot)?;
                from += 1;
                to += 1;
                count -= 1;
            }
        }
        Ok(this_v)
    }

    /// `Array.prototype.reverse()` — in place, returns the receiver.
    pub(crate) fn generic_reverse(
        &mut self,
        this_v: JsValue,
    ) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.reverse")?;
        let len = self.generic_len(obj)?;
        let middle = len / 2;
        for lower in 0..middle {
            let upper = len - lower - 1;
            let lower_slot = Slot::of(self.generic_index(obj, lower)?);
            let upper_slot = Slot::of(self.generic_index(obj, upper)?);
            self.write_slot(obj, lower, upper_slot)?;
            self.write_slot(obj, upper, lower_slot)?;
        }
        Ok(this_v)
    }

    /// `Array.prototype.pop()`.
    pub(crate) fn generic_pop(&mut self, this_v: JsValue) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.pop")?;
        let len = self.generic_len(obj)?;
        if len == 0 {
            self.generic_set_len(obj, 0)?;
            return Ok(JsValue::undefined());
        }
        let idx = len - 1;
        let slot = Slot::of(self.generic_index(obj, idx)?);
        self.generic_delete(obj, idx)?;
        self.generic_set_len(obj, idx)?;
        Ok(slot.value())
    }

    /// `Array.prototype.shift()`.
    pub(crate) fn generic_shift(&mut self, this_v: JsValue) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.shift")?;
        let len = self.generic_len(obj)?;
        if len == 0 {
            self.generic_set_len(obj, 0)?;
            return Ok(JsValue::undefined());
        }
        let first = Slot::of(self.generic_index(obj, 0)?);
        for k in 1..len {
            let slot = Slot::of(self.generic_index(obj, k)?);
            self.write_slot(obj, k - 1, slot)?;
        }
        self.generic_delete(obj, len - 1)?;
        self.generic_set_len(obj, len - 1)?;
        Ok(first.value())
    }

    /// `Array.prototype.unshift(...items)`.
    pub(crate) fn generic_unshift(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.unshift")?;
        let len = self.generic_len(obj)?;
        let count = args.len() as i64;
        if count > 0 {
            for k in (0..len).rev() {
                let slot = Slot::of(self.generic_index(obj, k)?);
                self.write_slot(obj, k + count, slot)?;
            }
            for (i, &item) in args.iter().enumerate() {
                self.generic_set(obj, i as i64, item)?;
            }
        }
        self.generic_set_len(obj, len + count)?;
        Ok(ops::box_number((len + count) as f64))
    }

    /// `Array.prototype.splice(start, deleteCount?, ...items)`.
    pub(crate) fn generic_splice(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.splice")?;
        let len = self.generic_len(obj)?;
        let start = self.relative_index(args.first().copied(), len, 0);
        let delete_count = match args.get(1).copied() {
            None => len - start,
            Some(v) if v.is_undefined() => len - start,
            Some(v) => {
                let d = self.to_number_value(v).unwrap_or(f64::NAN);
                let d = if d.is_nan() { 0.0 } else { d.trunc() };
                let d = d.clamp(0.0, len as f64) as i64;
                d.min(len - start)
            }
        };
        let items: Vec<JsValue> = args.iter().skip(2).copied().collect();
        let insert_count = items.len() as i64;

        // Steps 5-6: an index the receiver does not have leaves a HOLE in the
        // removed array — `splice.call(new Array(3), 0, 3)` is
        // `[<3 empty items>]`, not three `undefined`s.
        let mut removed = Vec::new();
        for j in 0..delete_count.min(MAX_SCAN_SPAN) {
            removed.push(self.generic_index(obj, start + j)?);
        }
        let removed_array: Vec<JsValue> = removed
            .iter()
            .map(|slot| slot.unwrap_or_else(JsValue::undefined))
            .collect();

        if insert_count < delete_count {
            for k in start..len - delete_count {
                let slot = Slot::of(self.generic_index(obj, k + delete_count)?);
                self.write_slot(obj, k, slot)?;
            }
            for k in (len - delete_count)..(len - insert_count) {
                self.generic_delete(obj, k)?;
            }
        } else if insert_count > delete_count {
            // Back to front, so a read is never clobbered by a write.
            let mut k = len - delete_count;
            while k > start {
                let slot = Slot::of(self.generic_index(obj, k + delete_count - 1)?);
                self.write_slot(obj, k + insert_count - 1, slot)?;
                k -= 1;
            }
        }
        for (i, &item) in items.iter().enumerate() {
            self.generic_set(obj, start + i as i64, item)?;
        }
        self.generic_set_len(obj, (len - delete_count + insert_count).max(0))?;
        Ok(crate::call::alloc_array(self, removed_array))
    }

    /// `Array.prototype.flat(depth?)`.
    pub(crate) fn generic_flat(
        &mut self,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        let obj = self.receiver_base(this_v, "Array.prototype.flat")?;
        let depth = match args.first().copied() {
            None => 1.0,
            Some(v) if v.is_undefined() => 1.0,
            Some(v) => self.to_number_value(v).unwrap_or(1.0),
        };
        let mut out: Vec<JsValue> = Vec::new();
        self.flatten_into(obj, 1.0_f64.max(depth), &mut out)?;
        Ok(crate::call::alloc_array(self, out))
    }

    /// `FlattenIntoArray(target, source, sourceLen, start, depth)`.
    fn flatten_into(
        &mut self,
        source: JsValue,
        depth: f64,
        target: &mut Vec<JsValue>,
    ) -> Result<(), JSException> {
        let len = self.generic_len(source)?;
        for i in 0..len {
            let Some(element) = self.generic_index(source, i)? else {
                continue;
            };
            let is_array = element
                .as_object()
                .is_some_and(|h| self.heap.get(h).kind == v12_heap::Kind::Array);
            if depth > 0.0 && is_array {
                self.flatten_into(element, depth - 1.0, target)?;
            } else {
                target.push(element);
            }
        }
        Ok(())
    }

    /// `Set(O, k, slot, true)` with the hole distinction preserved: an absent
    /// index is deleted rather than written as `undefined`, which is what
    /// keeps `reverse(new Array(3))` hole-free and `copyWithin` faithful.
    fn write_slot(&mut self, base: JsValue, i: i64, slot: Slot) -> Result<(), JSException> {
        match slot {
            Slot::Present(v) => self.generic_set(base, i, v),
            Slot::Absent => self.generic_delete(base, i),
        }
    }

    /// `ToObject(O)`, the step every one of these methods opens with: a
    /// primitive receiver is boxed and a nullish one throws.
    ///
    /// Returns the **value**, not a handle: a boxed string has no
    /// `Handle<JsObject>` the reader can be handed without allocating a
    /// wrapper that `get_property` would allocate again. `get_property` and
    /// `op_in` both handle a primitive base themselves (`string_prim_surface`),
    /// so the value is the correct thing to thread.
    fn receiver_base(&mut self, this_v: JsValue, what: &str) -> Result<JsValue, JSException> {
        if this_v.is_null() || this_v.is_undefined() {
            return Err(JSException(
                self.type_error(&format!("{what} called on null or undefined")),
            ));
        }
        Ok(this_v)
    }

    /// `IsStrictlyEqual` — the spec's comparison for `indexOf`/`lastIndexOf`
    /// (22.1.3.18/19 step 5). Heap-only and total: no operand conversion, so
    /// no user code runs and no re-entrancy question arises.
    fn strict_equals(&self, a: JsValue, b: JsValue) -> bool {
        ops::strict_equals(self.heap, a, b)
    }

    /// `SameValueZero` — the spec's comparison for `includes` (22.1.3.16
    /// step 5): `NaN` matches `NaN` and `+0` matches `-0`.
    fn same_value_zero(&self, a: JsValue, b: JsValue) -> bool {
        // `as_smi`/`as_f64` rather than an `is_number` predicate: a Smi is
        // not `is_f64`, and SameValueZero must treat both as numbers.
        let numeric = |v: JsValue| v.as_smi().map(|i| i as f64).or_else(|| v.as_f64());
        if let (Some(x), Some(y)) = (numeric(a), numeric(b)) {
            return x == y || (x.is_nan() && y.is_nan());
        }
        ops::strict_equals(self.heap, a, b)
    }

    /// Does routing `id` to this seam change anything?
    ///
    /// A `Some` means "run the body in `generic_receiver.rs`", and it is only
    /// returned when the **registry native would get a different answer** —
    /// i.e. when resolving the receiver's `length`, or one of its indices,
    /// reaches an accessor the engine's `[[Get]]` cannot invoke.
    ///
    /// That predicate is what keeps this batch's blast radius at exactly the
    /// E2 rows: a plain array, a dense object, a sparse array and a
    /// string-`length` receiver all answer `false` here and keep running the
    /// registry body, so the 1414-test gate is testing the same code it was
    /// testing before for every one of them.
    ///
    /// Two things it deliberately does **not** try to be:
    ///
    /// * **Not a proxy check.** A `Proxy` receiver throws in both paths
    ///   (`proxy_op_get` is the unimplemented-trap stub here, and
    ///   `dispatch_get` reaches the same stub), so routing it would change
    ///   the message, not the outcome.
    /// * **Not a completeness proof.** It inspects `length` and the indices
    ///   within a bounded window, which is what these methods read; a getter
    ///   planted at an index past the window is not seen. That is a coverage
    ///   limit of the *probe*, not of the bodies — and it is the conservative
    ///   direction, because a `false` only means "the old, already-tested path
    ///   runs".
    pub(crate) fn generic_receiver_seam_applies(
        &mut self,
        id: v12_native::NativeId,
        this_v: JsValue,
    ) -> bool {
        if !matches!(
            id,
            NativeId::ArraySlice
                | NativeId::ArrayAt
                | NativeId::ArrayIndexOf
                | NativeId::ArrayLastIndexOf
                | NativeId::ArrayIncludes
                | NativeId::ArrayFill
                | NativeId::ArrayCopyWithin
                | NativeId::ArrayReverse
                | NativeId::ArrayPop
                | NativeId::ArrayShift
                | NativeId::ArrayUnshift
                | NativeId::ArraySplice
                | NativeId::ArrayFlat
        ) {
            return false;
        }
        let Some(obj) = this_v.as_object() else {
            return false;
        };
        // A real array and an `arguments` object read their `length` from the
        // element store through the first rung of both `[[Get]]`s, so an
        // accessor can never be what answers. `dispatch_get`'s first rung is
        // the same test, and keeping it identical is why a real array is not
        // rerouted.
        if matches!(self.heap.get(obj).kind, Kind::Array | Kind::Arguments) {
            return false;
        }
        if self.heap.get(obj).kind == Kind::Proxy {
            return false;
        }
        let length_key = self.new_temp_key("length");
        self.unreadable_accessor_at(obj, length_key) || self.any_unreadable_index(obj)
    }

    /// Walks `obj`'s prototype chain for `key` and reports whether the winning
    /// descriptor is an accessor whose getter the engine cannot invoke.
    ///
    /// The test is `FLAG_ENGINE_ACCESSOR`, the same bit
    /// `internal_methods::invoke_accessor_getter` uses: an accessor with that
    /// flag is dispatched by `NativeId` and answers correctly from
    /// `&mut Heap`, so it is **not** unreadable and must not force the seam.
    /// An accessor without it is a user closure — the E2 case.
    fn unreadable_accessor_at(&mut self, obj: Handle<JsObject>, key: JsValue) -> bool {
        let Ok(prop_key) = self.property_key(key) else {
            return false;
        };
        let mut cur = Some(obj);
        while let Some(o) = cur {
            let shape = self.heap.shape_of(o);
            let d = self.heap.get(shape).descriptors.find(prop_key).copied();
            let entry_getter = d.as_ref().map(|d| match d {
                v12_heap::Descriptor::Accessor { getter, .. } => *getter,
                v12_heap::Descriptor::Data { .. } => None,
            });
            let winner = match (entry_getter, &d) {
                (Some(g), _) => Some(g),
                (None, Some(v12_heap::Descriptor::Data { slot, .. })) => {
                    let _ = slot;
                    None
                }
                (None, None) => {
                    // Dictionary rung: accessors live only here.
                    self.heap
                        .get(o)
                        .dictionary
                        .as_ref()
                        .and_then(|m| m.get(&prop_key))
                        .filter(|e| e.is_accessor)
                        .map(|e| e.getter)
                }
                (None, _) => None,
            };
            match winner {
                // A data descriptor won the walk: no accessor is involved.
                None if d.is_some() => return false,
                Some(Some(getter)) => {
                    return !self.getter_is_engine_owned(getter);
                }
                // `get: undefined` reads `undefined`, which the engine answers
                // correctly — nothing to reroute.
                Some(None) => return false,
                None => {}
            }
            cur = self.heap.get(o).prototype;
        }
        false
    }

    /// Bounded scan for an accessor at an integer index on the receiver's
    /// chain. The window is deliberately small: it only has to cover what a
    /// method actually reads before it gives up, and a `false` here is the
    /// safe direction (the pre-existing registry body runs).
    fn any_unreadable_index(&mut self, obj: Handle<JsObject>) -> bool {
        const INDEX_PROBE_SPAN: i64 = 64;
        for i in 0..INDEX_PROBE_SPAN {
            let key = JsValue::string(self.heap.intern_text(&i.to_string()));
            if self.unreadable_accessor_at(obj, key) {
                return true;
            }
        }
        false
    }

    /// True when `getter` is an engine-allocated accessor, which
    /// `builtin_dispatch` can run from `&mut Heap` alone.
    fn getter_is_engine_owned(&self, getter: Handle<JsObject>) -> bool {
        self.heap.get(getter).flags & v12_heap::JsObject::FLAG_ENGINE_ACCESSOR != 0
    }
}
impl<'i> crate::Interp<'i> {
    /// `dispatch_native`'s router arm: run `id`'s spec body against a
    /// receiver read through `Interp::get_property`.
    ///
    /// Split from `generic_receiver_seam_applies` so the probe is a pure
    /// question ("would the registry native answer differently?") and this is
    /// the answer. The match is exhaustive over the ids the probe accepts, so
    /// adding a method to one without the other is a compile error rather than
    /// a silent fall-through.
    pub(crate) fn run_generic_receiver(
        &mut self,
        id: NativeId,
        this_v: JsValue,
        args: &[JsValue],
    ) -> Result<JsValue, JSException> {
        match id {
            NativeId::ArraySlice => self.generic_slice(this_v, args),
            NativeId::ArrayAt => self.generic_at(this_v, args),
            NativeId::ArrayIndexOf => self.generic_index_of(this_v, args),
            NativeId::ArrayLastIndexOf => self.generic_last_index_of(this_v, args),
            NativeId::ArrayIncludes => self.generic_includes(this_v, args),
            NativeId::ArrayFill => self.generic_fill(this_v, args),
            NativeId::ArrayCopyWithin => self.generic_copy_within(this_v, args),
            NativeId::ArrayReverse => self.generic_reverse(this_v),
            NativeId::ArrayPop => self.generic_pop(this_v),
            NativeId::ArrayShift => self.generic_shift(this_v),
            NativeId::ArrayUnshift => self.generic_unshift(this_v, args),
            NativeId::ArraySplice => self.generic_splice(this_v, args),
            NativeId::ArrayFlat => self.generic_flat(this_v, args),
            // Unreachable by construction: `generic_receiver_seam_applies`
            // returns `false` for every other id, and it is `pub(crate)` so no
            // other caller can reach this arm. Named rather than folded into a
            // catch-all so a future id cannot silently answer `undefined`
            // here (ADR-0006 consequence 3).
            other => Err(JSException(
                self.type_error(&format!("{other:?} has no generic-receiver body")),
            )),
        }
    }
}
