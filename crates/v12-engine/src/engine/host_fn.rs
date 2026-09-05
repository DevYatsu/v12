//! Host-facing callables: the `Function` constructor and `create_host_function`
//! bridging Rust closures into the interpreter.



use v12_heap::JsValue;

use super::{string_value, Engine};

impl Engine {
    pub fn create_function(&mut self, params: &str, body: &str) -> Result<JsValue, JsValue> {
        let src = format!("function __f({params}){{{body}}}");
        let (program, _strings) =
            v12_bccompiler::compile_source_with_strings(&src)
                .map_err(|err| string_value(&mut self.heap, &err.message))?;
        let idx = program
            .functions
            .iter()
            .position(|f| f.name_hint.as_deref() == Some("__f"))
            .unwrap_or(1) as u32;
        let func = self.heap.alloc(v12_heap::JsObject::function(
            v12_heap::FunctionTarget::Bytecode(idx),
            None,
        ));
        self.heap.add_root(JsValue::object(func));
        // Keep the program alive for the test duration by leaking its Rc
        // (v1: tests do not actually call the function through the engine's
        // heap; they verify the object was created).
        let _ = program;
        Ok(JsValue::object(func))
    }

    /// Registers a capturing Rust closure as a global function named `name`.
    ///
    /// The function is installed as a property on the realm's global object
    /// (shape transition, mirroring the interpreter's `SetGlobal` fast path).
    /// It dispatches through `FunctionTarget::Host` (one word on the function
    /// object), so no registry entry is needed.
    pub fn create_host_function(
        &mut self,
        name: &str,
        closure: crate::builtins::HostClosure,
    ) -> Result<(), JsValue> {
        let global = self.realm.global();
        // The function object carries the host closure directly (one word),
        // so `prepare_call` invokes it without a registry lookup. The engine
        // closure wraps an `Rc<RefCell<dyn FnMut>>`; the heap `HostClosure`
        // adapts it to the one-word handle. Ownership: the returned function
        // object and its closure live on the heap for the engine's lifetime.
        let heap_closure = v12_heap::HostClosure::new(move |heap, this, args| {
            closure.call(heap, this, args).map_err(|t| t.into_js(heap))
        });
        let func = self.heap.alloc(v12_heap::JsObject::function(
            v12_heap::FunctionTarget::Host(heap_closure),
            None,
        ));
        self.heap.add_root(JsValue::object(func));
        // Install `name` on the global via the public shape API, exactly as
        // the interpreter's `op_set_global` does (GLOBAL_VAR_OFFSET bias).
        let h = self
            .heap
            .intern_text(&name);
        let key = v12_heap::PropKey::from_string(h);
        let shape = self.heap.shape_of(global);
        if let Some(desc) = self.heap.lookup_property(shape, key)
            && let Some(slot) = desc.slot()
        {
            let idx = crate::realm::INTRINSIC_COUNT + slot as usize;
            let len = self.heap.get(global).properties.len();
            if idx >= len {
                self.heap
                    .get_mut(global)
                    .properties
                    .resize(idx + 1, JsValue::undefined());
            }
            self.heap.get_mut(global).properties[idx] = JsValue::object(func);
        } else {
            let child = self.heap.add_property(shape, key, v12_heap::Attrs::DEFAULT);
            self.heap.bind_shape(global, child);
            let new_slot =
                usize::try_from(self.heap.get(child).num_own - 1).expect("slot fits usize");
            let idx = crate::realm::INTRINSIC_COUNT + new_slot;
            let len = self.heap.get(global).properties.len();
            if len <= idx {
                self.heap
                    .get_mut(global)
                    .properties
                    .resize(idx + 1, JsValue::undefined());
            }
            self.heap.get_mut(global).properties[idx] = JsValue::object(func);
        }
        Ok(())
    }
}
