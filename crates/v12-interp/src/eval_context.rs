//! Direct-eval caller context and child-frame execution.

use v12_bytecode::{BindingLocation, EvalBindingKind, EvalCallerFacts, EvalPhase, EvalVarTarget};
use v12_heap::{Handle, JsObject, JsValue};
use v12_native::{CompiledEval, EvalCompileFacts};

use super::{Frame, Interp, JSException};

#[derive(Debug, Clone)]
pub(crate) enum EvalBindingRef {
    Register { base: usize, reg: u16 },
    Env { env: Handle<JsObject>, slot: u16 },
    EvalLocal { env: Handle<JsObject>, slot: u16 },
    Global,
    Arguments { frame: usize },
}

#[derive(Debug, Clone)]
pub(crate) struct EvalCallerContext {
    pub local_env: Option<Handle<JsObject>>,
    pub var_target: EvalVarTarget,
    pub this: JsValue,
    pub env: Option<Handle<JsObject>>,
    pub new_target: Option<JsValue>,
    pub arguments: Option<EvalBindingRef>,
    pub strict: bool,
    pub caller: EvalCallerFacts,
    pub bindings: Vec<(String, EvalBindingRef)>,
}

impl EvalCallerContext {
    pub(crate) fn compile_facts(&self) -> EvalCompileFacts {
        EvalCompileFacts {
            direct: true,
            strict: self.strict,
            var_target: self.var_target,
            caller: self.caller.clone(),
        }
    }
}

impl Interp<'_> {
    pub(crate) fn drop_frame_context(&mut self, base: usize) {
        self.eval_frame_contexts.remove(&base);
    }

    pub(crate) fn capture_eval_context(&mut self, call_pc: u32) -> EvalCallerContext {
        let frame_idx = self.frames.len().saturating_sub(1);
        let (this, env, new_target, strict, own_bindings, site_caller, site_phase, is_arrow) = {
            let frame = &self.frames[frame_idx];
            let funcs = self.functions_for_program(frame.program);
            let function = funcs.get(frame.fn_idx as usize);
            let metadata = function.and_then(|f| f.eval_metadata.as_ref());
            let site =
                metadata.and_then(|m| m.direct_eval_sites.iter().find(|site| site.pc == call_pc));
            (
                self.stack
                    .get(frame.base)
                    .copied()
                    .unwrap_or_else(JsValue::undefined),
                frame.env,
                frame.new_target,
                function.is_some_and(|f| f.is_strict),
                metadata.map(|m| m.bindings.clone()).unwrap_or_default(),
                site.map(|site| site.caller.clone()).unwrap_or_default(),
                site.map(|site| site.phase).unwrap_or(EvalPhase::Body),
                function.is_some_and(|f| f.is_arrow),
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
        let owner_is_ordinary = self
            .frames
            .get(owner)
            .and_then(|frame| {
                self.functions_for_program(frame.program)
                    .get(frame.fn_idx as usize)
                    .map(|function| !function.is_arrow && !function.is_top_level)
            })
            .unwrap_or(false);
        let local_env = self.frames.get(owner).and_then(|frame| frame.env);
        let inherited = self.frames.get(frame_idx).and_then(|frame| {
            frame
                .env
                .and_then(|env| self.eval_contexts.get(&(env.index() as usize)))
                .or_else(|| self.eval_frame_contexts.get(&frame.base))
        });
        let var_target = inherited.map(|ctx| ctx.var_target).unwrap_or_else(|| {
            if owner_is_ordinary {
                EvalVarTarget::CallerFunction
            } else if is_arrow {
                EvalVarTarget::EvalLocal
            } else {
                EvalVarTarget::Global
            }
        });
        let mut bindings = inherited
            .map(|ctx| ctx.bindings.clone())
            .unwrap_or_default();
        let mut caller = inherited.map(|ctx| ctx.caller.clone()).unwrap_or_default();
        for (name, kind) in &site_caller.bindings {
            caller.bindings.retain(|(old, _)| old != name);
            caller.bindings.push((name.clone(), *kind));
        }
        caller.strict |= site_caller.strict || strict;
        caller.arguments_object_needed |= site_caller.arguments_object_needed;

        // Arrows inherit the nearest ordinary owner's facts, not their own
        // parameter list's implicit arguments binding.  Keep lexical this/env
        // storage from the current frame while resolving the owner's values.
        if is_arrow {
            let owner_frame = &self.frames[owner];
            let owner_funcs = self.functions_for_program(owner_frame.program);
            let owner_metadata = owner_funcs
                .get(owner_frame.fn_idx as usize)
                .and_then(|f| f.eval_metadata.as_ref());
            if let Some(metadata) = owner_metadata {
                for (name, _) in &metadata.bindings {
                    let reference = metadata
                        .bindings
                        .iter()
                        .find(|(binding_name, _)| binding_name == name)
                        .map(|(_, location)| match location {
                            BindingLocation::Register(reg) => EvalBindingRef::Register {
                                base: owner_frame.base,
                                reg: *reg,
                            },
                            BindingLocation::Env(slot) => EvalBindingRef::Env {
                                env: owner_frame.env.expect("owner env binding has environment"),
                                slot: *slot,
                            },
                            BindingLocation::Global => EvalBindingRef::Global,
                        })
                        .unwrap_or_else(|| EvalBindingRef::Arguments { frame: owner });
                    bindings.retain(|(old, _)| old != name);
                    bindings.push((name.clone(), reference));
                }
            }
        }

        for (name, location) in own_bindings {
            if site_phase == EvalPhase::Parameter
                && !site_caller.bindings.iter().any(|(old, _)| old == &name)
            {
                continue;
            }
            let kind = site_caller
                .bindings
                .iter()
                .find(|(old, _)| old == &name)
                .map(|(_, kind)| *kind)
                .unwrap_or(EvalBindingKind::Var);
            // In sloppy nested eval, `var` metadata is global in the eval
            // unit, but an inherited caller binding remains the variable
            // environment target.  Lexical locations still shadow it.
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
                BindingLocation::Env(slot) => {
                    let mut binding_env = env.expect("eval-reachable env binding has environment");
                    while self
                        .heap
                        .get(binding_env)
                        .properties
                        .get(usize::from(slot))
                        .is_none()
                    {
                        binding_env = self
                            .heap
                            .get(binding_env)
                            .prototype
                            .expect("eval-reachable env binding has a parent environment");
                    }
                    EvalBindingRef::Env {
                        env: binding_env,
                        slot,
                    }
                }
                BindingLocation::Global => EvalBindingRef::Global,
            };
            bindings.retain(|(old, _)| old != &name);
            bindings.push((name.clone(), reference));
            caller.bindings.retain(|(old, _)| old != &name);
            caller.bindings.push((name, kind));
        }

        let arguments = EvalBindingRef::Arguments { frame: owner };
        if !bindings.iter().any(|(name, _)| name == "arguments") && owner_is_ordinary {
            // Keep the runtime arguments binding available for eval reads even
            // when the caller has no *declaration conflict* fact for a simple
            // parameter list.  The fact remains separate so `var arguments`
            // can stay legal in that case.
            bindings.push(("arguments".to_string(), arguments.clone()));
        }
        EvalCallerContext {
            local_env,
            var_target,
            this,
            env,
            new_target,
            arguments: Some(arguments),
            strict,
            caller,
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
        let var_names = function
            .eval_metadata
            .as_ref()
            .map(|metadata| metadata.var_declared_names.clone())
            .unwrap_or_default();
        let caller_base = self.frames.last().map(|frame| frame.base);
        let local_env = match context.var_target {
            EvalVarTarget::Global => None,
            EvalVarTarget::EvalLocal => Some(home_env),
            EvalVarTarget::CallerFunction => {
                if context.local_env.is_some() {
                    context.local_env
                } else {
                    // A direct eval call from an ordinary function is a
                    // declaration-instantiation boundary. Ensure that even a
                    // function without a pre-existing environment gets a
                    // durable caller-local environment.
                    let env = self.heap.alloc(JsObject::environment(0, None));
                    self.heap.add_root(JsValue::object(env));
                    let env_display = self.env_display_for(Some(env));
                    if let Some(frame) = self.frames.last_mut() {
                        frame.env = Some(env);
                        frame.env_display = env_display;
                    }
                    Some(env)
                }
            }
        };
        if let Some(local_env) = local_env {
            for name in var_names {
                if context.bindings.iter().any(|(old, _)| old == &name) {
                    continue;
                }
                let slot = u16::try_from(self.heap.get(local_env).properties.len())
                    .expect("eval-local environment fits u16");
                self.heap
                    .get_mut(local_env)
                    .properties
                    .push(JsValue::undefined());
                context.bindings.push((
                    name,
                    EvalBindingRef::EvalLocal {
                        env: local_env,
                        slot,
                    },
                ));
            }
        }
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
            .insert(home_env.index() as usize, context.clone());
        if let Some(local_env) = local_env
            && local_env != home_env
        {
            self.eval_contexts
                .insert(local_env.index() as usize, context.clone());
        }
        if context.var_target == EvalVarTarget::EvalLocal
            && let Some(caller_base) = caller_base
        {
            self.eval_frame_contexts
                .insert(caller_base, context.clone());
        }
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
        let frame = self.frames.last()?;
        let context = frame
            .env
            .and_then(|env| self.eval_contexts.get(&(env.index() as usize)))
            .or_else(|| self.eval_frame_contexts.get(&frame.base))?;
        context
            .bindings
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
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
            EvalBindingRef::Env { env, slot } => Ok(Some(self.read_env_slot(env, slot)?)),
            EvalBindingRef::EvalLocal { env, slot } => {
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
                self.write_env_slot(env, slot, value)?;
            }
            EvalBindingRef::EvalLocal { env, slot } => {
                let idx = usize::from(slot);
                if idx >= self.heap.get(env).properties.len() {
                    return Err(JSException(
                        self.error_value("ReferenceError: invalid eval-local binding"),
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
