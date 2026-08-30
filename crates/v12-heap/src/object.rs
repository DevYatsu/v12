//! Minimal heap-object types: `JsObject`, `V12Symbol`, `V12BigInt`. Strings
//! live in their own module ([`crate::string`]); real BigInt semantics over
//! malachite arrive with the built-ins.

use crate::gc::{MarkSink, Trace};
use crate::handle::{Handle, HeapSpace, Space};
use crate::shape::ValidityCellId;

/// The kind of a heap object.
///
/// Replaces the flat `KIND_*` u8 constants: a single namespace, so no two
/// kinds can collide, and `match` over a `Kind` is exhaustive (adding a kind
/// is a compile error everywhere it must be handled).
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, strum::FromRepr)]
pub enum Kind {
    /// Default `JsObject` kind: the ordinary object. Further kinds are
    /// assigned as they become needed (function, array, Proxy, …).
    Ordinary = 0,
    /// Object kind for user functions created by `Closure`.
    Function = 1,
    /// Object kind for array literals.
    Array = 2,
    /// Arguments exotic objects (ES `ArgumentsExoticObject`).
    ///
    /// Layout: `properties` holds named properties (`length`, `callee`),
    /// `elements` holds indexed arguments, and `arguments_mapped` tracks the
    /// exotic parameter alias (see `JsObject::arguments_mapped`).
    Arguments = 3,
    /// Generator object. Interpreter contract (v12-interp/src/lib.rs):
    /// properties[0]=fn_idx, [1]=resume_pc, [2]=done (0.0 running / 1.0
    /// completed / 2.0 suspended), [3]=yield_dst; elements=saved register
    /// window snapshot; prototype=captured Env. DO NOT reorder without
    /// updating interp.
    Generator = 4,
    /// Promise object. Internal slots live in `properties[0..3]` =
    /// `[state, value, reactions]` (see [`PromiseExt`]). A dedicated kind
    /// makes `is_promise` a single field test instead of a structural check.
    Promise = 5,
    /// Error object. Internal slots: `properties[0]` = name, `properties[1]`
    /// = message. A dedicated kind lets `is_error` be a field test and keeps
    /// error display (`message` extraction) off the shape path.
    Error = 6,
    /// Map object. Entries live in `elements` as `[k1, v1, k2, v2, …]` pairs
    /// (see `v12-engine/src/builtins/map.rs`).
    Map = 7,
    /// Set object. Values live in `elements` (see
    /// `v12-engine/src/builtins/map.rs`).
    Set = 8,
    /// Iterator object (Array/Map/Set iterators backing `Symbol.iterator`).
    /// Internal state lives in `elements` as `[kind, source, index]` — see
    /// `v12-engine/src/builtins/iterator.rs`. The `next` method is a native
    /// routed through the registry.
    Iterator = 9,
    /// RegExp object. Internal slots live in `properties` as
    /// `[source, flags, lastIndex]` — see `v12-engine/src/builtins/regexp.rs`.
    /// The compiled pattern is cached inside the object's `elements[0]` via
    /// the native registry's side table; the object itself only carries the
    /// source text and flag string.
    RegExp = 10,
    /// Proxy object: every internal method traps (the engine's dispatch
    /// reports a `TypeError` for the v1 stub).
    Proxy = 99,
    /// Receiver pseudo-kind for primitive strings in the built-in method
    /// table. Never stored on a `JsObject`; used only to key `methods!`
    /// tables.
    StringPrim = 0xFF,
}

/// The raw `u8` discriminant of a kind (for the `JsObject.kind` field and
/// serialized layout).
impl From<Kind> for u8 {
    #[inline]
    fn from(k: Kind) -> u8 {
        k as u8
    }
}

/// ES integrity levels ([`JsObject`]): how far an object has been locked
/// down. Transitions are monotone — sealing then freezing is legal, nothing
/// un-seals — so [`Heap::set_integrity_level`] only ever raises flags.
///
/// [`Heap::set_integrity_level`]: crate::Heap::set_integrity_level
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrityLevel {
    /// Non-extensible; existing properties may remain configurable.
    Sealed,
    /// Sealed, and every own property additionally non-writable.
    Frozen,
}

/// A heap object: a kind/flags header plus dense property and element
/// vectors and a prototype link. Shapes describe the layout; this carries
/// the storage.
#[derive(Clone, Debug, PartialEq)]
pub struct JsObject {
    /// Object kind; [`Kind::Ordinary`] by default.
    pub kind: Kind,
    /// Kind-specific flag bits (see the `FLAG_*` constants).
    pub flags: u8,
    /// For `KIND_FUNCTION` objects: what calling this object invokes (a
    /// bytecode index, a native handler, or a host closure). One word.
    /// Meaningless for other kinds (defaults to the root bytecode index 0).
    pub callable: crate::function::FunctionTarget,
    /// The program whose function table a `Bytecode` callable indexes.
    /// 0 is the interpreter's own program; eval-registered programs get
    /// higher ids. Lets a closure created in one program (e.g. `eval`) be
    /// invoked from another by resolving through the program registry.
    pub program_id: u32,
    /// For `KIND_FUNCTION` objects: the captured lexical environment the
    /// closure closes over. Distinct from `prototype` (the [[Prototype]]
    /// link, which classes' `extends` rewires). Kept separate so a function
    /// can have both a real prototype chain and a captured env.
    pub captured_env: Option<Handle<JsObject>>,
    /// The shape describing this object's property layout. Stored directly
    /// on the object (one word) so `Heap::shape_of` is a single field read
    /// — no side-table indirection on the property-access hot path. Defaults
    /// to the pinned empty-object root shape; `Heap::bind_shape` transitions
    /// it as properties are added.
    pub shape: crate::shape::ShapeHandle,
    /// In-object property slots (V8-style): shape descriptor slots below
    /// [`Self::IN_OBJECT_PROP_CAP`] live here, avoiding a separate backing
    /// allocation for the common few-property case. Access through
    /// [`Self::prop_slot`].
    pub inline_props: [crate::JsValue; Self::IN_OBJECT_PROP_CAP],
    /// Out-of-line property backing store for slots at or beyond
    /// [`Self::IN_OBJECT_PROP_CAP`]. `None` until a shape grows past the
    /// inline cap. Access through [`Self::prop_slot`].
    pub overflow: Option<Box<[crate::JsValue]>>,
    /// Named-property slots (shape-backed layout arrives with the object
    /// model work).
    pub properties: Vec<crate::JsValue>,
    /// Property keys (names) parallel to `properties` vector. `None` for
    /// empty slots; `Some(PropKey)` for populated slots. Used by native
    /// handlers that need to enumerate property names without interpreter's
    /// shape cache.
    pub property_keys: Vec<Option<crate::PropKey>>,
    /// Integer-indexed element slots (legacy `Vec` used for the overloaded
    /// slots: function index, generator register window, promise reactions).
    pub elements: Vec<crate::JsValue>,
    /// Integer-indexed element storage for *arrays*, backed by the
    /// [`ElementsArray`] kind lattice (PackedSmi → … → Dictionary). Array
    /// element reads/writes route through here; `elements` remains for the
    /// non-array overloaded uses above.
    pub elements_array: crate::elements::ElementsArray,
    /// Prototype link; traced as a strong reference. Guards over this chain
    /// watch a validity-cell serial, not this field alone.
    pub prototype: Option<Handle<JsObject>>,
    /// The validity cell watching assumptions about this object (prototype
    /// identity, attribute stability), assigned lazily on first use. The
    /// registry holding serials lives on the heap. Not on the shape-lookup
    /// hot path — `shape` is the shape binding.
    pub validity_cell: ValidityCellId,
    /// Arguments exotic mapping: `Some(map)` where `map[i]` is `Some(slot)`
    /// when indexed property `i` is aliased to the `slot`-th parameter slot,
    /// `None` for mapped holes and `None` for unmapped (strict) arguments.
    ///
    /// Only meaningful when `kind == KIND_ARGUMENTS`.
    pub arguments_mapped: Option<Box<[Option<u32>]>>,
}

impl Default for JsObject {
    /// Defaults to the pinned empty-object root shape (shape-slot 0), which
    /// every heap pins at construction — so a fresh object is born with the
    /// canonical shape and `Heap::shape_of` returns the root until the first
    /// `bind_shape`.
    fn default() -> Self {
        Self {
            kind: Kind::Ordinary,
            flags: 0,
            callable: crate::function::FunctionTarget::Bytecode(0),
            program_id: 0,
            captured_env: None,
            shape: crate::shape::ShapeHandle::new(0),
            inline_props: [crate::JsValue::undefined(); Self::IN_OBJECT_PROP_CAP],
            overflow: None,
            properties: Vec::new(),
            property_keys: Vec::new(),
            elements: Vec::new(),
            elements_array: crate::elements::ElementsArray::new(),
            prototype: None,
            validity_cell: ValidityCellId::NONE,
            arguments_mapped: None,
        }
    }
}

impl JsObject {
    /// An ordinary object with empty storage.
    pub fn new() -> Self {
        Self::default()
    }
    /// `[[Extensible]] == false`. Implied by both integrity transitions.
    pub const FLAG_NOT_EXTENSIBLE: u8 = 0b0000_0001;
    /// Every own property non-configurable: sealed or stricter.
    pub const FLAG_SEALED: u8 = 0b0000_0010;
    /// Every own property also non-writable: frozen.
    pub const FLAG_FROZEN: u8 = 0b0000_0100;

    /// True once sealed (or frozen): no property may be removed or
    /// reconfigured.
    pub fn is_sealed(&self) -> bool {
        self.flags & Self::FLAG_SEALED != 0
    }

    /// True only when frozen: sealed plus every own property non-writable.
    pub fn is_frozen(&self) -> bool {
        self.flags & Self::FLAG_FROZEN != 0
    }

    /// Number of property slots stored inline in the object header.
    ///
    /// This is the V8-style in-object/overflow split: shape descriptor slots
    /// below this cap live in `inline_props`, slots at or above it live in
    /// `overflow`. Kept small (V8 uses 3–4); benchmark and tune.
    pub const IN_OBJECT_PROP_CAP: usize = 4;

    /// Reads the property value at shape-slot `slot`.
    ///
    /// Slots below [`Self::IN_OBJECT_PROP_CAP`] come from the inline array,
    /// slots at or above it from the overflow backing store. This is the
    /// single accessor for shape-derived slots — the interpreter's
    /// `GetProperty`/`SetProperty` fast paths index through it.
    ///
    /// # Panics
    /// Panics when `slot` is out of range of both stores (corrupt shape).
    #[inline]
    pub fn prop_slot(&self, slot: usize) -> crate::JsValue {
        if slot < Self::IN_OBJECT_PROP_CAP {
            self.inline_props[slot]
        } else {
            self.overflow
                .as_deref()
                .and_then(|o| o.get(slot - Self::IN_OBJECT_PROP_CAP))
                .copied()
                .unwrap_or(crate::JsValue::undefined())
        }
    }

    /// Writes the property value at shape-slot `slot`. See
    /// [`Self::prop_slot`] for the store layout.
    #[inline]
    pub fn set_prop_slot(&mut self, slot: usize, value: crate::JsValue) {
        if slot < Self::IN_OBJECT_PROP_CAP {
            self.inline_props[slot] = value;
        } else {
            let idx = slot - Self::IN_OBJECT_PROP_CAP;
            if let Some(overflow) = self.overflow.as_deref_mut() {
                overflow[idx] = value;
            }
        }
    }

    /// An ordinary object with the given properties and their keys.
    pub fn ordinary(
        properties: Vec<crate::JsValue>,
        property_keys: Vec<Option<crate::PropKey>>,
    ) -> Self {
        Self {
            kind: Kind::Ordinary,
            properties,
            property_keys,
            ..Self::default()
        }
    }

    /// A function object: `callable` is what invoking it runs, `captured_env`
    /// is the closure environment.
    pub fn function(
        target: crate::function::FunctionTarget,
        env: Option<Handle<JsObject>>,
    ) -> Self {
        Self {
            kind: Kind::Function,
            callable: target,
            captured_env: env,
            ..Self::default()
        }
    }

    /// An array object with the given elements and length property.
    ///
    /// The length is stored as a Smi to match the interpreter's array
    /// allocations (`NewArray`/`CopyArrayRest` use `ops::box_number`, which
    /// boxes small integers as Smi). The elements are mirrored into both the
    /// legacy `elements` vec (overloaded uses read it) and the live
    /// `elements_array` lattice that array element accesses route through.
    // Element count fits the i31 Smi bound; audited invariant.
    #[allow(clippy::expect_used)]
    pub fn array(elements: Vec<crate::JsValue>) -> Self {
        let len = elements.len();
        let mut elements_array = crate::elements::ElementsArray::new();
        for (i, &v) in elements.iter().enumerate() {
            elements_array.set(i as u32, v);
        }
        Self {
            kind: Kind::Array,
            properties: vec![
                crate::JsValue::from_i32_smi(len as i32).expect("array length fits Smi"),
            ],
            property_keys: vec![Some(crate::PropKey::from_parts(false, 0))], // length property key (index 0 placeholder; caller should intern "length" when heap is available)
            elements,
            elements_array,
            ..Self::default()
        }
    }

    /// An arguments exotic object with the given properties and elements.
    pub fn arguments(
        properties: Vec<crate::JsValue>,
        elements: Vec<crate::JsValue>,
        mapped: Option<Box<[Option<u32>]>>,
    ) -> Self {
        let prop_len = properties.len();
        Self {
            kind: Kind::Arguments,
            properties,
            property_keys: vec![None; prop_len],
            elements,
            arguments_mapped: mapped,
            ..Self::default()
        }
    }

    /// A RegExp object with `properties = [source, flags, lastIndex]`.
    /// `source` and `flags` are heap strings; `lastIndex` starts at 0.
    /// The compiled pattern lives in the engine's regexp side table, keyed by
    /// the object handle — this object carries only the source text.
    // lastIndex starts at 0; always within the i31 Smi bound.
    #[allow(clippy::expect_used)]
    pub fn regexp(
        source: crate::Handle<crate::V12Str>,
        flags: crate::Handle<crate::V12Str>,
    ) -> Self {
        Self {
            kind: Kind::RegExp,
            properties: vec![
                crate::JsValue::string(source),
                crate::JsValue::string(flags),
                crate::JsValue::from_i32_smi(0).expect("0 fits Smi"),
            ],
            property_keys: vec![None; 3],
            ..Self::default()
        }
    }

    /// A generator object (suspended frame).
    pub fn generator() -> Self {
        Self {
            kind: Kind::Generator,
            ..Default::default()
        }
    }

    /// A Promise object (ordinary kind with Promise-specific fields set later).
    pub fn promise() -> Self {
        Self {
            kind: Kind::Ordinary,
            ..Self::default()
        }
    }

    /// Pending promise with `properties = [state=0, value=undefined, reactions]`.
    pub fn pending_promise(reactions: crate::Handle<JsObject>) -> Self {
        Self {
            kind: Kind::Promise,
            properties: vec![
                crate::JsValue::from_f64(0.0),
                crate::JsValue::undefined(),
                crate::JsValue::object(reactions),
            ],
            property_keys: vec![None; 3],
            ..Self::default()
        }
    }

    /// Fulfilled promise `properties = [1, value, reactions]`.
    // Promise state constants 1 and 2 fit the i31 Smi bound.
    #[allow(clippy::expect_used)]
    pub fn fulfilled_promise(value: crate::JsValue, reactions: crate::Handle<JsObject>) -> Self {
        Self {
            kind: Kind::Promise,
            properties: vec![
                crate::JsValue::from_i32_smi(1).expect("1 fits"),
                value,
                crate::JsValue::object(reactions),
            ],
            property_keys: vec![None; 3],
            ..Self::default()
        }
    }

    /// Rejected promise `properties = [2, reason, reactions]`.
    // Promise state constants 1 and 2 fit the i31 Smi bound.
    #[allow(clippy::expect_used)]
    pub fn rejected_promise(reason: crate::JsValue, reactions: crate::Handle<JsObject>) -> Self {
        Self {
            kind: Kind::Promise,
            properties: vec![
                crate::JsValue::from_i32_smi(2).expect("2 fits"),
                reason,
                crate::JsValue::object(reactions),
            ],
            property_keys: vec![None; 3],
            ..Self::default()
        }
    }

    /// An error object with `properties = [name, message]`. `name` is the
    /// error class (e.g. `"TypeError"`); `message` is the human-readable
    /// text. Both are heap strings.
    pub fn error(
        name: crate::Handle<crate::V12Str>,
        message: crate::Handle<crate::V12Str>,
    ) -> Self {
        Self {
            kind: Kind::Error,
            properties: vec![
                crate::JsValue::string(name),
                crate::JsValue::string(message),
            ],
            property_keys: vec![None; 2],
            ..Self::default()
        }
    }

    /// Generator object with full slot contract.
    /// properties[0]=fn_idx, [1]=resume_pc, [2]=done, [3]=yield_dst, [4]=async_promise?
    pub fn generator_with(
        fn_idx: u32,
        resume_pc: u32,
        done: f64,
        yield_dst: u16,
        window: Vec<crate::JsValue>,
        env: Option<crate::Handle<JsObject>>,
        async_promise: Option<crate::JsValue>,
    ) -> Self {
        let mut props = vec![
            crate::JsValue::from_f64(f64::from(fn_idx)),
            crate::JsValue::from_f64(f64::from(resume_pc)),
            crate::JsValue::from_f64(done),
            crate::JsValue::from_f64(f64::from(yield_dst)),
        ];
        if let Some(p) = async_promise {
            props.push(p);
        }
        let prop_len = props.len();
        Self {
            kind: Kind::Generator,
            properties: props,
            property_keys: vec![None; prop_len],
            elements: window,
            prototype: env,
            ..Self::default()
        }
    }

    /// Environment object with `slots` undefined slots.
    pub fn environment(slots: usize, parent: Option<crate::Handle<JsObject>>) -> Self {
        Self {
            properties: vec![crate::JsValue::undefined(); slots],
            property_keys: vec![None; slots],
            prototype: parent,
            ..Self::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Slot-hiding traits
// ---------------------------------------------------------------------------

/// Promise state discriminant stored in `properties[0]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromiseState {
    Pending,
    Fulfilled,
    Rejected,
}

/// Generator suspension state hiding `properties[0..4]` slot contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeneratorState {
    pub fn_idx: u32,
    pub resume_pc: u32,
    /// 0.0 running, 1.0 done, 2.0 suspended
    pub done_raw: u32,
    pub yield_dst: u16,
    pub done: bool,
    pub suspended: bool,
}

/// Extension hiding Promise slot layout.
pub trait PromiseExt {
    fn promise_state(&self, heap: &crate::Heap) -> Option<PromiseState>;
    fn promise_value(&self, heap: &crate::Heap) -> Option<crate::JsValue>;
    fn settle_promise(
        &mut self,
        heap: &mut crate::Heap,
        state: PromiseState,
        value: crate::JsValue,
    );
}

impl PromiseExt for crate::Handle<JsObject> {
    fn promise_state(&self, heap: &crate::Heap) -> Option<PromiseState> {
        let v = heap.get(*self).properties.first()?;
        if let Some(n) = v.as_smi() {
            match n {
                0 => return Some(PromiseState::Pending),
                1 => return Some(PromiseState::Fulfilled),
                2 => return Some(PromiseState::Rejected),
                _ => return None,
            }
        }
        if let Some(f) = v.as_f64() {
            match f as i32 {
                0 => return Some(PromiseState::Pending),
                1 => return Some(PromiseState::Fulfilled),
                2 => return Some(PromiseState::Rejected),
                _ => return None,
            }
        }
        None
    }
    fn promise_value(&self, heap: &crate::Heap) -> Option<crate::JsValue> {
        heap.get(*self).properties.get(1).copied()
    }
    // State constants 0–2 fit the i31 Smi bound.
    #[allow(clippy::expect_used)]
    fn settle_promise(
        &mut self,
        heap: &mut crate::Heap,
        state: PromiseState,
        value: crate::JsValue,
    ) {
        let s = match state {
            PromiseState::Pending => 0,
            PromiseState::Fulfilled => 1,
            PromiseState::Rejected => 2,
        };
        let o = heap.get_mut(*self);
        if o.properties.len() >= 2 {
            o.properties[0] = crate::JsValue::from_i32_smi(s).expect("fits");
            o.properties[1] = value;
        }
    }
}

/// Extension hiding Generator slot layout.
pub trait GeneratorExt {
    fn generator_state(&self, heap: &crate::Heap) -> Option<GeneratorState>;
    fn set_generator_state(&mut self, heap: &mut crate::Heap, s: GeneratorState);
}

impl GeneratorExt for crate::Handle<JsObject> {
    fn generator_state(&self, heap: &crate::Heap) -> Option<GeneratorState> {
        let o = heap.get(*self);
        if o.kind != Kind::Generator || o.properties.len() < 4 {
            return None;
        }
        let fn_idx = o.properties[0].as_smi().unwrap_or(0) as u32;
        let resume_pc = o.properties[1]
            .as_smi()
            .unwrap_or(o.properties[1].as_f64().unwrap_or(0.0) as i32)
            as u32;
        // done is stored as f64
        let done_f = o.properties[2].as_f64().unwrap_or(0.0);
        let done_raw = done_f as u32;
        let yield_dst = o.properties[3].as_f64().unwrap_or(0.0) as u16;
        Some(GeneratorState {
            fn_idx,
            resume_pc,
            done_raw,
            yield_dst,
            done: done_raw == 1,
            suspended: done_raw == 2,
        })
    }
    fn set_generator_state(&mut self, heap: &mut crate::Heap, s: GeneratorState) {
        let o = heap.get_mut(*self);
        if o.properties.len() >= 4 {
            o.properties[0] = crate::JsValue::from_f64(f64::from(s.fn_idx));
            o.properties[1] = crate::JsValue::from_f64(f64::from(s.resume_pc));
            o.properties[2] = crate::JsValue::from_f64(f64::from(s.done_raw));
            o.properties[3] = crate::JsValue::from_f64(f64::from(s.yield_dst));
        }
    }
}

/// Heap allocation helpers that hide `add_root` + `property_keys` invariants.
pub trait HeapExt {
    fn alloc_array(&mut self, elements: Vec<crate::JsValue>) -> crate::Handle<JsObject>;
    fn alloc_function(
        &mut self,
        target: crate::function::FunctionTarget,
        env: Option<crate::Handle<JsObject>>,
    ) -> crate::Handle<JsObject>;
    fn alloc_ordinary(&mut self, props: Vec<crate::JsValue>) -> crate::Handle<JsObject>;
    fn alloc_pending_promise(&mut self) -> crate::Handle<JsObject>;
    fn alloc_array_with_roots(&mut self, elements: Vec<crate::JsValue>) -> crate::Handle<JsObject> {
        self.alloc_array(elements)
    }
}

impl HeapExt for crate::Heap {
    fn alloc_array(&mut self, elements: Vec<crate::JsValue>) -> crate::Handle<JsObject> {
        let h = self.alloc(JsObject::array(elements));
        self.add_root(crate::JsValue::object(h));
        h
    }
    fn alloc_function(
        &mut self,
        target: crate::function::FunctionTarget,
        env: Option<crate::Handle<JsObject>>,
    ) -> crate::Handle<JsObject> {
        let h = self.alloc(JsObject::function(target, env));
        self.add_root(crate::JsValue::object(h));
        h
    }
    fn alloc_ordinary(&mut self, props: Vec<crate::JsValue>) -> crate::Handle<JsObject> {
        let n = props.len();
        let h = self.alloc(JsObject::ordinary(props, vec![None; n]));
        self.add_root(crate::JsValue::object(h));
        h
    }
    fn alloc_pending_promise(&mut self) -> crate::Handle<JsObject> {
        let reactions = self.alloc(JsObject::array(Vec::new()));
        self.add_root(crate::JsValue::object(reactions));
        let h = self.alloc(JsObject::pending_promise(reactions));
        self.add_root(crate::JsValue::object(h));
        h
    }
}

/// Engine-side promise allocation seam: Interp should call through this trait
/// rather than `heap.alloc(JsObject{...})` directly. Provided for layering;
/// Interp currently documents the TODO and calls `HeapExt::alloc_pending_promise`.
pub trait EnginePromise {
    fn new_pending_promise(&mut self) -> crate::Handle<JsObject>;
}

impl EnginePromise for crate::Heap {
    fn new_pending_promise(&mut self) -> crate::Handle<JsObject> {
        self.alloc_pending_promise()
    }
}
/// A heap symbol. Identity *is* the handle for now; descriptions,
/// well-known singletons, and `#private` names come with interning work.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct V12Symbol;

/// A heap BigInt placeholder: sign + little-endian base-256 magnitude. The
/// malachite-backed implementation lands with built-ins; this carries the
/// space and allocation accounting until then.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct V12BigInt {
    /// `true` when negative. Zero is canonically positive (`sign == false`).
    pub sign: bool,
    /// Magnitude, little-endian base 256, no trailing zero bytes for zero.
    pub magnitude_le: Vec<u8>,
}

impl HeapSpace for JsObject {
    const SPACE: Space = Space::Objects;
}
impl HeapSpace for V12Symbol {
    const SPACE: Space = Space::Symbols;
}
impl HeapSpace for V12BigInt {
    const SPACE: Space = Space::Bigints;
}

/// Rough retained-bytes estimate used by the GC growth trigger and live-byte
/// accounting. Deliberately approximate: capacities count, exact allocator
/// overhead does not. Implemented by the four heap-space payload types;
/// not meant to be implemented elsewhere.
pub trait SizeEstimate {
    fn approx_size(&self) -> usize;
}

const VALUE_BYTES: usize = core::mem::size_of::<crate::JsValue>();

impl SizeEstimate for JsObject {
    fn approx_size(&self) -> usize {
        // Header (kind, flags, prototype) + capacity of all slot vectors.
        let mapped = self
            .arguments_mapped
            .as_ref()
            .map_or(0, |m| m.len() * core::mem::size_of::<Option<u32>>());
        16 + self.properties.capacity() * VALUE_BYTES
            + self.elements.capacity() * VALUE_BYTES
            + self.property_keys.capacity() * core::mem::size_of::<Option<crate::PropKey>>()
            + self.elements_array.retained_bytes()
            + mapped
    }
}

impl SizeEstimate for V12Symbol {
    fn approx_size(&self) -> usize {
        1 // opaque unit payload
    }
}

impl SizeEstimate for V12BigInt {
    fn approx_size(&self) -> usize {
        16 + self.magnitude_le.capacity()
    }
}

// Tracing: objects expose their children; the other spaces are leaves this
// wave but participate uniformly so future fields need no collector changes.

impl Trace for JsObject {
    fn trace(&self, sink: &mut MarkSink<'_>) {
        self.properties.trace(sink);
        self.property_keys.trace(sink);
        self.elements.trace(sink);
        self.elements_array.trace(sink);
        self.prototype.trace(sink);
        self.captured_env.trace(sink);
    }
}

impl Trace for V12Symbol {
    fn trace(&self, _sink: &mut MarkSink<'_>) {}
}

impl Trace for V12BigInt {
    fn trace(&self, _sink: &mut MarkSink<'_>) {}
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::JsValue;

    #[test]
    fn object_traces_children_through_sink() {
        let mut heap = crate::Heap::new(crate::GcPolicy::NoGC);
        let child = heap.alloc(JsObject::default());
        let parent = heap.alloc(JsObject {
            properties: vec![JsValue::object(child)],
            elements: vec![JsValue::object(child)],
            prototype: Some(child),
            ..JsObject::default()
        });
        // Force a collect that only sees the parent: the child must survive
        // via all three child-bearing fields.
        heap.add_root(JsValue::object(parent));
        heap.force_collect();
        assert_eq!(heap.live_count::<JsObject>(), 2);
    }

    #[test]
    fn size_estimates_grow_with_capacity() {
        let mut o = JsObject::default();
        let base = o.approx_size();
        o.properties.reserve(100);
        assert!(o.approx_size() > base);
    }

    #[test]
    fn arguments_exotic_mapped_has_mapping() {
        let mut heap = crate::Heap::new(crate::GcPolicy::NoGC);
        let mapped: Box<[Option<u32>]> = vec![Some(0), Some(1), None].into_boxed_slice();
        let obj = heap.alloc(JsObject {
            kind: Kind::Arguments,
            elements: vec![
                JsValue::from_i32_smi(10).unwrap(),
                JsValue::from_i32_smi(20).unwrap(),
                JsValue::from_i32_smi(30).unwrap(),
            ],
            arguments_mapped: Some(mapped),
            ..JsObject::default()
        });
        heap.add_root(JsValue::object(obj));
        let stored = heap.get(obj);
        assert_eq!(stored.kind, Kind::Arguments);
        assert!(stored.arguments_mapped.is_some());
        let map = stored.arguments_mapped.as_ref().unwrap();
        assert_eq!(map[0], Some(0));
        assert_eq!(map[1], Some(1));
        assert_eq!(map[2], None);
        // Mapped arguments are exotic: indexed access via elements should work
        assert_eq!(stored.elements[0].as_smi(), Some(10));
    }

    #[test]
    fn arguments_exotic_unmapped_is_strict() {
        let mut heap = crate::Heap::new(crate::GcPolicy::NoGC);
        let obj = heap.alloc(JsObject {
            kind: Kind::Arguments,
            elements: vec![JsValue::from_i32_smi(1).unwrap()],
            arguments_mapped: None,
            ..JsObject::default()
        });
        heap.add_root(JsValue::object(obj));
        assert_eq!(heap.get(obj).arguments_mapped, None);
        // Unmapped (strict) arguments should not alias parameters
        assert_eq!(heap.get(obj).elements[0].as_smi(), Some(1));
    }

    #[test]
    fn generator_object_slots() {
        let mut heap = crate::Heap::new(crate::GcPolicy::NoGC);
        let h = heap.alloc(JsObject::generator());
        heap.get_mut(h).properties = vec![
            JsValue::from_f64(2.0),
            JsValue::from_f64(0.0),
            JsValue::from_f64(0.0),
        ];
        assert_eq!(heap.get(h).kind, Kind::Generator);
        assert_eq!(heap.get(h).properties.len(), 3);
    }
}
