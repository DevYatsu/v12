//! Registry of baseline-compiled functions.

use std::collections::HashMap;

use v12_bytecode::PcMapEntry;
use v12_heap::JsValue;

/// Identifier for a function in the program table.
pub type FunctionId = u32;

/// Executable closure for a baseline-compiled function.
pub type JitExecFn = Box<dyn Fn(&mut [JsValue]) -> JsValue + Send + Sync>;

/// A baseline-compiled function.
///
/// The struct owns the deoptimization map and the executable closure. The
/// closure captures the logic for the function and can be invoked via
/// [`CompiledFn::execute`].
pub struct CompiledFn {
    pc_map: Vec<PcMapEntry>,
    max_regs: u16,
    /// Executable logic: maps a mutable register window to the function's
    /// return value. Returns `JsValue::undefined()` for fall-off-the-end.
    exec: JitExecFn,
}

impl CompiledFn {
    /// Creates a new compiled function.
    #[allow(dead_code)]
    pub(crate) fn new(pc_map: Vec<PcMapEntry>, max_regs: u16, exec: JitExecFn) -> Self {
        Self {
            pc_map,
            max_regs,
            exec,
        }
    }

    /// Deoptimization map from JIT code offsets to bytecode PCs.
    ///
    /// Entries are sorted by `jit_pc` and mirror `FunctionBytecode::pc_map`.
    pub fn deopt_info(&self) -> &[PcMapEntry] {
        &self.pc_map
    }

    /// Number of registers the function expects.
    pub fn max_regs(&self) -> u16 {
        self.max_regs
    }

    /// Executes the compiled function over `regs`.
    ///
    /// `regs` must be at least `max_regs` long; the window is interpreted as
    /// the frame's register file. The return value is the value produced by
    /// the function's `Return` opcode, or `undefined` if execution falls off
    /// the end.
    pub fn execute(&self, regs: &mut [JsValue]) -> JsValue {
        (self.exec)(regs)
    }
}

impl std::fmt::Debug for CompiledFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledFn")
            .field("pc_map", &self.pc_map)
            .field("max_regs", &self.max_regs)
            .finish_non_exhaustive()
    }
}

/// Cache of baseline-compiled entry points keyed by function id.
///
/// The cache is separate from the interpreter's `FeedbackVector`, which
/// remains owned by `Interp`.
#[derive(Default)]
pub struct JitCache {
    inner: HashMap<FunctionId, CompiledFn>,
}

impl JitCache {
    /// Creates an empty cache.
    pub fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    /// Inserts a compiled function.
    pub fn insert(&mut self, id: FunctionId, func: CompiledFn) {
        self.inner.insert(id, func);
    }

    /// Looks up a compiled function.
    pub fn get(&self, id: FunctionId) -> Option<&CompiledFn> {
        self.inner.get(&id)
    }

    /// Removes a compiled function.
    pub fn remove(&mut self, id: FunctionId) -> Option<CompiledFn> {
        self.inner.remove(&id)
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Clears all entries.
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Iterates over cached ids.
    pub fn ids(&self) -> impl Iterator<Item = FunctionId> + '_ {
        self.inner.keys().copied()
    }
}

impl std::fmt::Debug for JitCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JitCache")
            .field("len", &self.inner.len())
            .finish()
    }
}
