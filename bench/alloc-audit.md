# Allocation & Zerocopy Audit — v12

> Generated 2026-08-27 from parallel explorer (clone grep) + librarian (zerocopy crate) lanes.
> Tell me which rows to fix — nothing is changed until you pick.

## Executive summary

- **In-process sharing is already zerocopy where it matters:** `Arc<[FunctionBytecode]>` / `Arc<[String]>` compiler→interp, `Handle<T>` (Copy), `lasso::RodeoResolver` key→str. No deep copy on `Arc::clone`.
- **Biggest remaining wins are `SHOULD_BORROW` slices, not new crates:** 70% of hot `.clone()`/`.to_vec()` sites can take `&[JsValue]` / `&str` instead of owning.
- **Zerocopy crates help only at the snapshot/file boundary,** not the hot interpreter loop. `bytemuck` is the cheapest win; `rkyv` is v2 feature.

---

## 1. Zerocopy crate verdict (where to put it / where not)

| Crate | Put it here (zerocopy wins) | Do NOT put it here | Cost | Verdict |
|-------|------------------------------|---------------------|------|---------|
| **bytemuck 1.x** | `crates/v12-bytecode/src/lib.rs`: `Vec<Instr>` ↔ `Vec<u8>` file I/O, `HandlerRange`/`PcMapEntry`/`SpanPair` as `Pod` | `Heap::V12Str` `Cons`/`Sliced` (GC-traced, mutable, owned), `hashbrown` maps | Very low — 1 dep, wrapper crate keeps `#![forbid(unsafe_code)]` green | **Use** (cheapest) |
| **zerocopy 0.8** | Same as bytemuck + bulk `&[u8]` mmap → `&[Instr]` without `copy_from_slice`, `Const::F64` `to_bits`/`from_bits` | Same as above + in-process `Arc` already zero-copy | Low | **Use IF snapshotting**, else prefer bytemuck |
| **rkyv 0.8** | Cross-process compiler daemon / persistent bytecode cache: `rkyv::to_bytes` on `Vec<FunctionBytecode>` + string table → `&ArchivedProgram` mmap, strings as `&str` views | All GC strings (`V12Str` must be owned, flattened, hash-cached, freed via `alive` bitmap), `Shape` hashbrown internals, `Interned_strings` rooted map | High — lifetime `ArchivedProgram<'a>` infects `Interp`, +200KB, MSRV churn | **Skip v1**, revisit v2 behind feature |
| **bincode/postcard** | Quick diagnostic serde for harness `cargo run` dump | Hot path — always copies, no mmap gain | Minimal | **Skip** unless you need quick persistence |
| **allocator_api2/bumpalo** | Already optimal — `oxc_parser` arena via `oxc_allocator`; AST dropped after `collect` | `RodeoResolver` must outlive many compiles, `Heap::V12Str` needs mark-sweep not bump | High lifetime tax | **Skip** |

**Single-sentence rule:** `Instr`/`ConstantPool` wire bytes can be zerocopy files; GC-traced heap strings and hash maps must stay owned.

---

## 2. Clone / allocation hotspots — triage

Legend: **MUST_CLONE** = owns data, required · **SHOULD_BORROW** = fix wins (no alloc) · **CHEAP** = Copy/small

### A. Heap / GC — fix `SHOULD_BORROW` first (highest impact, per-GC)

| File:line | What is cloned | Class | Fix (1 line) |
|-----------|----------------|-------|--------------|
| `v12-heap/src/gc.rs:856` | `marked[Shapes].clone()` bitmap per GC | **SHOULD_BORROW** | `&collector.marked[Space::Shapes]` or `mem::swap` — saves `Vec<bool>` alloc ×5 spaces per GC |
| `v12-heap/src/gc.rs:458,494` | `parent_shape.descriptors.clone()` in `add_property` / `define_accessor` | **SHOULD_BORROW** | Iterate `&parent.descriptors` or use `Arc<[Descriptor]>` Cow |
| `v12-heap/src/string.rs:488` `v12-interp/src/lib.rs:184` `v12-engine/src/realm.rs:134` | `text.as_bytes().to_vec()` for interning Latin1 | **MUST_CLONE** but borrow-hit path exists | Keep `intern(&str)` that hashes `&str` before alloc; only `to_vec` on miss. Already partially done — keep. |

### B. Interp hot loops — slice instead of `to_vec` (avoid heap per op)

| File:line | What | Class | Fix |
|-----------|------|-------|-----|
| `v12-interp/src/lib.rs:581` `CopyObjectRestW` `excl_vals: Vec` | `stack[..].to_vec()` | **SHOULD_BORROW** | `op_copy_object_rest(&[JsValue])` take `&[JsValue]` slice |
| `v12-interp/src/lib.rs:951` narrow 1-elem `to_vec()` | same | **SHOULD_BORROW** | `Option<JsValue>` / `[JsValue;1]` no heap |
| `v12-interp/src/lib.rs:1990` `descriptors.as_slice().to_vec()` | `Vec<Descriptor>` snapshot | **SHOULD_BORROW** | Iterate `&sh.descriptors` directly |
| `v12-interp/src/lib.rs:1992` `src_props.clone()` | `Vec<JsValue>` | **SHOULD_BORROW** | Borrow `&[JsValue]` in loop, alloc dst only |
| `v12-interp/src/lib.rs:2062` `src_elements.clone()` | `Vec<JsValue>` | **SHOULD_BORROW** | `heap.get_mut(dst).elements.extend_from_slice(&heap.get(src).elements)` |
| `v12-interp/src/lib.rs:1915` `elements[start..].to_vec()` | slice for new array | **MUST_CLONE** | Keep — new array owns. Preallocate exact. |
| `v12-interp/src/lib.rs:844` `NewArray` `stack[..].to_vec()` | elements | **MUST_CLONE** | Keep (`Vec::from(&slice)` same). SmallVec for ≤4 not needed yet. |
| `v12-interp/src/lib.rs:2092` `strings.get(id).cloned()` | `String` clone in `op_get_global` fast path | **SHOULD_BORROW** | Return `&str` from `RodeoResolver`, avoid `String` clone; `intern_text` already takes `&str` |
| `v12-interp/src/lib.rs:2233,2262` `prepare_call_apply` `elements.clone()` + hole fixup | `Vec<JsValue>` | **SHOULD_BORROW** | Pass `&[JsValue]` to `call_native(&[JsValue])`; rest via slice iter |
| `v12-interp/src/lib.rs:1178` `prepare_call` rest_slice `to_vec()` | rest array | **MUST_CLONE** | Keep, preallocate exact. |

### C. BCCompiler imports — dedup clones

| File:line | What | Class | Fix |
|-----------|------|-------|-----|
| `v12-bccompiler/src/unit.rs:204` | `plans.imports.clone()` to iterate | **SHOULD_BORROW** | `&cx.comp.plans.imports` |
| `v12-bccompiler/src/unit.rs:205/206/209` | `e.specifier.clone()` ×3 (HashSet/HashMap/Vec) | **MUST_CLONE** but dedup | Clone once `let s = e.specifier.clone()` then move/reuse |
| `v12-bccompiler/src/collect.rs:331/346/358/370` | `specifier.clone()` per ImportEntry | **MUST_CLONE** | Required — `ImportEntry` owns `String` |
| `v12-bccompiler/src/lib.rs:415/416` | `plans.imports/exports.clone()` at compile entry | **MUST_CLONE** | Keep — Program owns; consider `mem::take` |
| `v12-heap/src/string.rs:572` `mid_units.clone()` | `Vec<u16>` utf16 alloc | **MUST_CLONE** | Keep — `V12Str::utf16` owns |
| `v12-jit-baseline/src/compiler.rs:71` `bytecode.clone()` | `FunctionBytecode` for `make_exec_closure` | **SHOULD_BORROW** | Take `&FunctionBytecode` or `Arc` share |
| `v12-jit-baseline/lib.rs:142/167...` `fb.clone()` ×6 | fallback interp | **SHOULD_BORROW** | `&fb` or `Arc`, clone only for owned tier-up |
| `v12-engine/src/engine.rs:114/153/245` `registry.clone()` | `NativeRegistry` HashMap per call | **CHEAP** | `Arc<Registry>` — clone is atomic inc, keep as is |

### D. Cold / cheap — keep as is

- `format!`/`to_string` in error paths, validation, tests: **MUST_CLONE / CHEAP**, not hot.
- `Handle` `clone()` is `Copy` (u32): **CHEAP** by design.
- `v12-heap/src/string.rs:758/759` `heap.get(h).clone()` — test helper only.

---

## 3. What to fix — pick your rows

**Suggested minimal `SHOULD_BORROW` batch (5 one-line fixes, biggest win, no API break):**
1. `v12-heap/src/gc.rs:856` — stop `marked.to_vec()` per GC
2. `v12-bccompiler/src/unit.rs:204` — iterate `&imports`
3. `v12-interp/src/lib.rs:1990` + `1992` + `2062` — borrow descriptor/props/elements slices
4. `v12-interp/src/lib.rs:2092` — `&str` from resolver, no `String` clone in `GetGlobal`
5. `v12-jit-baseline/src/compiler.rs:71` — `&FunctionBytecode` in `make_exec_closure`

**Next batch (need small API tweak, still low risk):**
6. `v12-interp/src/lib.rs:581,951` — `&[JsValue]` for `CopyObjectRest`
7. `v12-interp/src/lib.rs:2233,2262` — borrowed `prepare_call_apply`

**Zerocopy crate (only if you want snapshot file):**
8. Add `bytemuck` wrapper crate for `Instr↔u8` casts (keep `#![forbid(unsafe_code)]` green — derives in wrapper, not in `v12-bytecode`).

Say e.g. `fix 1,3,4` or `fix all SHOULD_BORROW` or `add bytemuck` and I will dispatch the fixer lanes in parallel (one lane per file group, no overlap).
