//! Interpreter capability for evaluating deferred module namespaces.

use std::rc::Rc;

use v12_heap::{Handle, JsObject, JsValue, Kind, ModuleNamespaceVariant, PropKey};

use crate::{Interp, JSException};

/// Host capability used when a module namespace observation requires the
/// deferred module body to be evaluated.
pub trait ModuleEvaluator {
    /// Ensures the module behind `namespace` has been evaluated.
    fn ensure_evaluated(
        &self,
        interp: &mut Interp<'_>,
        namespace: JsValue,
    ) -> Result<(), JSException>;
}

impl Interp<'_> {
    /// Installs the host-side module evaluator used by namespace observations.
    pub fn set_module_evaluator(&mut self, evaluator: Rc<dyn ModuleEvaluator>) {
        self.module_evaluator = Some(evaluator);
    }

    /// Triggers deferred evaluation for a namespace operation.
    ///
    /// `key` is `None` only for an unkeyed operation such as
    /// `OwnPropertyKeys`; keyed string reads deliberately skip the `then`
    /// assimilation probe, and symbol keys never trigger the body.
    pub(crate) fn trigger_module_namespace(
        &mut self,
        namespace: Handle<JsObject>,
        key: Option<PropKey>,
        own_keys: bool,
    ) -> Result<(), JSException> {
        let deferred = self
            .heap
            .get(namespace)
            .module_namespace
            .as_ref()
            .is_some_and(|record| record.variant == ModuleNamespaceVariant::Deferred);
        if self.heap.get(namespace).kind != Kind::ModuleNamespace || !deferred {
            return Ok(());
        }
        if !own_keys {
            let Some(key) = key else { return Ok(()) };
            if !key.is_string() {
                return Ok(());
            }
            if key
                .string()
                .is_some_and(|handle| self.string_text(handle) == "then")
            {
                return Ok(());
            }
        }
        // Clone the capability before entering the adapter: it may execute a
        // module body through this same interpreter and must not retain a
        // borrow of the capability field.
        let evaluator = self.module_evaluator.as_ref().map(Rc::clone);
        if let Some(evaluator) = evaluator {
            evaluator.ensure_evaluated(self, JsValue::object(namespace))?;
        }
        Ok(())
    }
}
