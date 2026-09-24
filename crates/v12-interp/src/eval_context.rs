//! Direct-eval caller context and child-frame execution.

use v12_bytecode::BindingLocation;
use v12_heap::{Handle, JsObject, JsValue};
use v12_native::{CompiledEval, EvalCompileFacts};

use super::{Frame, Interp, JSException};

#[derive(Debug, Clone)]
pub(crate) enum EvalBindingRef {
    Register { base: usize, reg: u16 },
    Env { env: Handle<JsObject>, slot: u16 },
    Global,
    Arguments { frame: usize },
}

#[derive(Debug, Clone)]
pub(crate) struct EvalCallerContext {
    pub this: JsValue,
    pub env: Option<Handle<JsObject>>,
    pub new_target: Option<JsValue>,
    pub arguments: Option<EvalBindingRef>,
    pub strict: bool,
    pub bindings: Vec<(String, EvalBindingRef)>,
}

impl EvalCallerContext {
    pub(crate) fn compile_facts(&self) -> EvalCompileFacts {
        let bindings = self
            .bindings
            .iter()
            .filter_map(|(name, location)| {
                let location = match location {
                    EvalBindingRef::Register { reg, .. } => BindingLocation::Register(*reg),
                    EvalBindingRef::Env { slot, .. } => BindingLocation::Env(*slot),
                    EvalBindingRef::Global => BindingLocation::Global,
                    EvalBindingRef::Arguments { .. } => BindingLocation::Global,
                };
                Some((name.clone(), location))
            })
            .collect();
        EvalCompileFacts {
            direct: true,
            strict: self.strict,
            non_simple_parameters: false,
            bindings,
        }
    }
}

impl Interp<'_> {
    pub(crate) fn capture_eval_context(&mut self) -> EvalCallerContext {
        let frame_idx = self.frames.len().saturating_sub(1);
        let (this, env, new_target, strict, own_bindings) = {
            let frame = &self.frames[frame_idx];
            let funcs = self.functions_for_program(frame.program);
            let function = funcs.get(frame.fn_idx as usize);
            let bindings = function
                .and_then(|f| f.eval_metadata.as_ref())
                .map(|m| m.bindings.clone())
                .unwrap_or_default();
            (
                self.stack
                    .get(frame.base)
                    .copied()
                    .unwrap_or_else(JsValue::undefined),
                frame.env,
                frame.new_target,
                function.is_some_and(|f| f.is_strict),
                bindings,
            )
        };
        let owner = self
            .frames
            .iter()
            .enumerate()
            .rev()
            .find(|(_, frame)| {
                self.functions_for_program(frame.program)
                    .get(frame.fn_idx as usize)
                    .is_some_and(|f| !f.is_arrow)
            })
            .map(|(idx, _)| idx)
            .unwrap_or(frame_idx);
        let mut bindings = self
            .frames
            .get(frame_idx)
            .and_then(|f| f.env)
            .and_then(|env| self.eval_contexts.get(&(env.index() as usize)))
            .map(|ctx| ctx.bindings.clone())
            .unwrap_or_default();
        for (name, location) in own_bindings {
            // In sloppy direct eval, a `var` declaration belongs to the
            // inherited variable environment when that environment already
            // has the name.  Do not let the eval unit's global descriptor
            // replace the caller's binding in a nested eval.  Lexical
            // Register/Env locations still shadow, and strict eval keeps the
            // normal global binding behavior.
            if !strict
                && matches!(location, BindingLocation::Global)
                && bindings.iter().any(|(old, _)| old == &name)
            {
                continue;
            }
            let reference = match location {
                BindingLocation::Register(reg) => EvalBindingRef::Register {
                    base: self.frames[frame_idx].base,
                    reg,
                },
                BindingLocation::Env(slot) => EvalBindingRef::Env {
                    env: env.expect("eval-reachable env binding has environment"),
                    slot,
                },
                BindingLocation::Global => EvalBindingRef::Global,
            };
            bindings.retain(|(old, _)| old != &name);
            bindings.push((name, reference));
        }
        let arguments = EvalBindingRef::Arguments { frame: owner };
        if !bindings.iter().any(|(name, _)| name == "arguments") {
            bindings.push(("arguments".to_string(), arguments.clone()));
        }
        EvalCallerContext {
            this,
            env,
            new_target,
            arguments: Some(arguments),
            strict,
            bindings,
        }
    }

    pub(crate) fn run_compiled_child(
        &mut self,
        compiled: CompiledEval,
        mut context: EvalCallerContext,
    ) -> Result<JsValue, JSException> {
        let _ = context.arguments.as_ref();
        let parent = self.frames.last().and_then(|f| f.env);
        self.gc_protect();
        let home_env = self.heap.alloc(JsObject::environment(0, parent));
        self.heap.add_root(JsValue::object(home_env));
        context.env = Some(home_env);
        let program_id = self.register_program(compiled.functions, compiled.strings);
        let funcs = self.functions_for_program(program_id);
        let function = funcs
            .get(compiled.main as usize)
            .ok_or_else(|| JSException(self.error_value("TypeError: invalid eval program")))?;
        let max_regs = function.max_regs;
        let new_base = self.stack.len();
        let end = new_base + usize::from(max_regs);
        self.stack.resize(end, JsValue::undefined());
        self.stack[new_base] = context.this;
        let env_display = self.env_display_for(Some(home_env));
        self.frames.push(Frame {
            fn_idx: compiled.main,
            program: program_id,
            pc: 0,
            base: new_base,
            max_regs,
            env: Some(home_env),
            env_display,
            generator: None,
            yield_dst: None,
            new_target: context.new_target,
            arguments: None,
        });
        self.eval_contexts
            .insert(home_env.index() as usize, context);
        let saved_program = self.program_id;
        self.program_id = program_id;
        let saved_stop = self.stop_at_frames;
        self.stop_at_frames = Some(self.frames.len() - 1);
        let result = self.execute();
        self.stop_at_frames = saved_stop;
        self.program_id = saved_program;
        result?;
        Ok(self.top_result.take().unwrap_or_else(JsValue::undefined))
    }

    fn eval_binding(&self, name: &str) -> Option<EvalBindingRef> {
        self.frames
            .last()
            .and_then(|f| f.env)
            .and_then(|env| self.eval_contexts.get(&(env.index() as usize)))
            .and_then(|ctx| {
                ctx.bindings
                    .iter()
                    .rev()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.clone())
            })
    }

    pub(crate) fn eval_read_global(
        &mut self,
        name: &str,
        lenient: bool,
    ) -> Result<Option<JsValue>, JSException> {
        let Some(binding) = self.eval_binding(name) else {
            return Ok(None);
        };
        match binding {
            EvalBindingRef::Register { base, reg } => Ok(Some(
                self.stack
                    .get(base + usize::from(reg))
                    .copied()
                    .unwrap_or_else(JsValue::undefined),
            )),
            EvalBindingRef::Env { env, slot } => {
                let value = self
                    .heap
                    .get(env)
                    .properties
                    .get(usize::from(slot))
                    .copied();
                Ok(Some(value.unwrap_or_else(JsValue::undefined)))
            }
            EvalBindingRef::Global => Ok(None),
            EvalBindingRef::Arguments { frame } => {
                let value = self.arguments_for_frame(frame)?;
                Ok(Some(value))
            }
        }
        .map(|value| {
            if lenient && value.is_none() {
                Some(JsValue::undefined())
            } else {
                value
            }
        })
    }

    pub(crate) fn eval_write_global(
        &mut self,
        name: &str,
        value: JsValue,
    ) -> Result<bool, JSException> {
        let Some(binding) = self.eval_binding(name) else {
            return Ok(false);
        };
        match binding {
            EvalBindingRef::Register { base, reg } => {
                if base + usize::from(reg) >= self.stack.len() {
                    self.stack
                        .resize(base + usize::from(reg) + 1, JsValue::undefined());
                }
                self.stack[base + usize::from(reg)] = value;
            }
            EvalBindingRef::Env { env, slot } => {
                let idx = usize::from(slot);
                if idx >= self.heap.get(env).properties.len() {
                    return Err(JSException(
                        self.error_value("ReferenceError: invalid eval binding"),
                    ));
                }
                self.heap.get_mut(env).properties[idx] = value;
            }
            EvalBindingRef::Global => return Ok(false),
            EvalBindingRef::Arguments { frame } => {
                let _ = self.arguments_for_frame(frame)?;
            }
        }
        Ok(true)
    }

    fn arguments_for_frame(&mut self, frame_idx: usize) -> Result<JsValue, JSException> {
        if let Some(value) = self.frames.get(frame_idx).and_then(|f| f.arguments) {
            return Ok(value);
        }
        let Some(frame) = self.frames.get(frame_idx) else {
            return Err(JSException(
                self.error_value("ReferenceError: arguments owner is unavailable"),
            ));
        };
        let base = frame.base;
        let Some((start, len)) = self.call_args.get(&base).copied() else {
            return Ok(JsValue::undefined());
        };
        let values: Vec<JsValue> = self.stack[start..start + len].to_vec();
        let value = self.make_arguments_object(&values);
        self.heap.add_root(value);
        if let Some(frame) = self.frames.get_mut(frame_idx) {
            frame.arguments = Some(value);
        }
        Ok(value)
    }
}
