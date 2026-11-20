//! The thread that owns a QuickJS runtime and evaluates code on it.

use std::borrow::Cow;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

use rquickjs::function::Rest;
use rquickjs::object::Property;
use rquickjs::{
    CatchResultExt, CaughtError, Context, Ctx, Exception, Function, Object, Persistent, Runtime,
    Value,
};
use rustler::LocalPid;

use crate::atoms;
use crate::convert::{self, Converter, JsValue};
use crate::interrupt::Interrupt;
use crate::runtime::{self, CallbackRegistry};

type EvalResult = Result<JsValue, String>;

pub const DEAD_RUNTIME: &str = "dead_runtime";
const TIMEOUT: &str = "timeout";

// QuickJS collects cycles only once the heap passes this threshold, and never
// when an allocation fails, so it has to start below the memory limit or
// collectable garbage runs the eval out of memory.
const GC_THRESHOLD: usize = 4 * 1024 * 1024;

// QuickJS cannot discard queued jobs, so once an eval has to stop the
// remaining ones still run (and abort at their next interrupt check) instead
// of leaking into the next eval. Short jobs can keep queueing more, so the
// drain is capped.
const MAX_JOBS_AFTER_INTERRUPT: usize = 10_000;

pub struct EvalRequest {
    pub code: String,
    pub callback_names: Vec<String>,
    pub caller: LocalPid,
    pub tag: u64,
    pub deadline: Option<Instant>,
}

/// Work sent from NIF calls to a runtime's worker thread.
pub enum Message {
    Eval(EvalRequest),
    Stop { reply: Sender<()> },
}

#[derive(Clone, Debug, Default)]
pub struct WorkerOptions {
    pub memory_limit: usize,
    pub max_stack_size: usize,
}

pub struct Worker {
    // Captured before any user code runs, so a script replacing them cannot
    // break how callbacks save and restore the globals they shadow. Declared
    // before `context` because fields drop in order, and these must be
    // released before the QuickJS runtime is freed.
    describe_property: Persistent<Function<'static>>,
    define_property: Persistent<Function<'static>>,
    context: Context,
    registry: Arc<CallbackRegistry>,
    interrupt: Arc<Interrupt>,
    memory_limit: usize,
}

impl Worker {
    pub fn try_new(
        options: &WorkerOptions,
        registry: Arc<CallbackRegistry>,
        interrupt: Arc<Interrupt>,
    ) -> Result<Self, String> {
        let runtime =
            Runtime::new().map_err(|error| format!("Failed to create QuickJS runtime: {error}"))?;
        runtime.set_memory_limit(options.memory_limit);
        runtime.set_max_stack_size(options.max_stack_size);
        runtime.set_gc_threshold(gc_threshold(options.memory_limit));
        let handler_interrupt = Arc::clone(&interrupt);
        runtime.set_interrupt_handler(Some(Box::new(move || handler_interrupt.is_due())));

        let context = Context::full(&runtime)
            .map_err(|error| format!("Failed to create QuickJS context: {error}"))?;
        let (describe_property, define_property) = context
            .with(|ctx| {
                let object: Object = ctx.globals().get("Object")?;
                let describe: Function = object.get("getOwnPropertyDescriptor")?;
                let define: Function = object.get("defineProperty")?;
                Ok::<_, rquickjs::Error>((
                    Persistent::save(&ctx, describe),
                    Persistent::save(&ctx, define),
                ))
            })
            .map_err(|error| format!("Failed to capture Object functions: {error}"))?;

        Ok(Self {
            describe_property,
            define_property,
            context,
            registry,
            interrupt,
            memory_limit: options.memory_limit,
        })
    }

    // Takes the receiver by value so the channel closes as soon as the loop
    // ends. Sends then fail with dead_runtime instead of queueing while the
    // QuickJS runtime is being freed.
    #[allow(clippy::needless_pass_by_value)]
    pub fn run(&self, receiver: Receiver<Message>) {
        while let Ok(message) = receiver.recv() {
            match message {
                Message::Eval(request) => self.handle_eval(&request),
                Message::Stop { reply } => {
                    let _ = reply.send(());
                    break;
                }
            }
        }

        // Requests queued behind the stop would otherwise vanish, leaving
        // their callers to wait out their deadlines.
        for message in receiver.try_iter() {
            if let Message::Eval(request) = message {
                self.reply(&request, Err(DEAD_RUNTIME.to_string()));
            }
        }
    }

    fn handle_eval(&self, request: &EvalRequest) {
        self.interrupt.set_deadline(request.deadline);
        let result = match self.interrupted_reason() {
            Some(reason) => Err(reason.to_string()),
            None => self.eval(request),
        };
        self.interrupt.clear_deadline();
        if matches!(&result, Err(message) if message.contains(convert::OUT_OF_MEMORY)) {
            self.collect_garbage();
        }
        self.registry.clear();
        self.reply(request, result);
    }

    // After each collection QuickJS moves its threshold to 1.5 times the live
    // heap. Once that is past the memory limit it never collects cycles again,
    // so the garbage that filled the heap would fail every later eval.
    fn collect_garbage(&self) {
        let runtime = self.context.runtime();
        runtime.run_gc();
        runtime.set_gc_threshold(gc_threshold(self.memory_limit));
    }

    fn reply(&self, request: &EvalRequest, result: EvalResult) {
        if self.registry.finish(request.tag) {
            let message = (atoms::quicksand_result(), request.tag, result);
            let _ = runtime::send_to_pid(request.caller, message);
        }
    }

    fn interrupted_reason(&self) -> Option<&'static str> {
        if self.interrupt.is_stopping() {
            Some(DEAD_RUNTIME)
        } else if self.interrupt.deadline_passed() {
            Some(TIMEOUT)
        } else {
            None
        }
    }

    fn eval(&self, request: &EvalRequest) -> EvalResult {
        let code = escape_nul(&request.code);

        self.context.with(|ctx| {
            let mut shadowed = Vec::new();
            let result = self
                .install_callbacks(&ctx, request, &mut shadowed)
                .and_then(|()| self.evaluate(&ctx, &code));
            // An eval that ran past its deadline timed out, even if its own
            // code returned before its jobs were interrupted.
            let result = match self.interrupted_reason() {
                Some(reason) => Err(reason.to_string()),
                None => result,
            };
            // Cleared before cleanup so restoring globals is not interrupted.
            self.interrupt.clear_deadline();
            self.remove_callbacks(&ctx, &request.callback_names, shadowed);
            result
        })
    }

    fn evaluate<'js>(&self, ctx: &Ctx<'js>, code: &str) -> EvalResult {
        let evaluated = ctx.eval::<Value, _>(code).catch(ctx);
        self.drain_jobs(ctx);
        let value = evaluated.map_err(|error| format_caught_error(ctx, error))?;
        let value = settle(ctx, value)?;
        Converter::new(ctx, &self.interrupt, self.memory_limit).convert(value)
    }

    fn drain_jobs(&self, ctx: &Ctx<'_>) {
        let mut budget = MAX_JOBS_AFTER_INTERRUPT;
        while ctx.execute_pending_job() {
            if self.interrupt.is_due() {
                budget -= 1;
                if budget == 0 {
                    break;
                }
            }
        }
    }

    /// Defines a global function per callback, pushing the property
    /// descriptor each one replaces (or `undefined`) onto `shadowed`.
    fn install_callbacks<'js>(
        &self,
        ctx: &Ctx<'js>,
        request: &EvalRequest,
        shadowed: &mut Vec<Value<'js>>,
    ) -> Result<(), String> {
        if request.callback_names.is_empty() {
            return Ok(());
        }

        let globals = ctx.globals();
        let describe_property = self
            .describe_property
            .clone()
            .restore(ctx)
            .map_err(|error| format!("Failed to restore getOwnPropertyDescriptor: {error}"))?;

        for name in &request.callback_names {
            let previous: Value = describe_property
                .call((globals.clone(), name.as_str()))
                .catch(ctx)
                .map_err(|error| format!("Failed to read global {name}: {error:?}"))?;
            let callback = self.callback_function(ctx, name, request.caller, request.tag)?;
            // Defined rather than assigned, so a name like `__proto__` becomes
            // an own property instead of replacing the global's prototype.
            let defined = if previous.is_undefined() {
                let property = Property::from(callback).writable().configurable();
                globals.prop(name.as_str(), property)
            } else {
                globals.prop(name.as_str(), callback)
            };
            defined
                .catch(ctx)
                .map_err(|error| format!("Failed to define callback {name}: {error:?}"))?;
            shadowed.push(previous);
        }

        Ok(())
    }

    /// Builds the JS function for callback `name`: it sends its arguments to
    /// the caller and returns the reply. Being native, it depends on no JS
    /// global that user code could replace.
    fn callback_function<'js>(
        &self,
        ctx: &Ctx<'js>,
        name: &str,
        caller: LocalPid,
        tag: u64,
    ) -> Result<Function<'js>, String> {
        let registry = Arc::clone(&self.registry);
        let interrupt = Arc::clone(&self.interrupt);
        let memory_limit = self.memory_limit;
        let callback_name = name.to_string();

        let callback = move |ctx: Ctx<'js>, arguments: Rest<Value<'js>>| {
            let name = callback_name.as_str();
            if interrupt.is_due() {
                let message = format!("Callback '{name}' called after the eval was interrupted");
                return Err(Exception::throw_message(&ctx, &message));
            }
            if registry.is_abandoned(tag) {
                let message = format!("Callback '{name}' called after its eval ended");
                return Err(Exception::throw_message(&ctx, &message));
            }

            let mut converter = Converter::new(&ctx, &interrupt, memory_limit);
            let mut converted = Vec::with_capacity(arguments.len());
            for argument in arguments.into_inner() {
                match converter.convert(argument) {
                    Ok(value) => converted.push(value),
                    Err(reason) => {
                        let message = format!("Failed to convert arguments for '{name}': {reason}");
                        return Err(Exception::throw_message(&ctx, &message));
                    }
                }
            }

            let (id, reply) = registry.register();
            let message = (atoms::quicksand_callback(), tag, id, name, converted);
            if !runtime::send_to_pid(caller, message) {
                let message = format!("Callback '{name}' has no caller to run it");
                return Err(Exception::throw_message(&ctx, &message));
            }

            match reply.recv_timeout(interrupt.remaining()) {
                Ok(Ok(term_value)) => {
                    convert::intermediate_to_js(&ctx, &term_value).map_err(|reason| {
                        let message = format!("Failed to convert result of '{name}': {reason}");
                        Exception::throw_message(&ctx, &message)
                    })
                }
                Ok(Err(reason)) => Err(Exception::throw_message(&ctx, &reason)),
                Err(_) => {
                    let message = format!("Callback '{name}' timed out");
                    Err(Exception::throw_message(&ctx, &message))
                }
            }
        };

        Function::new(ctx.clone(), callback)
            .and_then(|function| function.with_name(name))
            .map_err(|error| format!("Failed to create callback {name}: {error}"))
    }

    /// Puts back what each callback global shadowed.
    fn remove_callbacks<'js>(
        &self,
        ctx: &Ctx<'js>,
        callback_names: &[String],
        shadowed: Vec<Value<'js>>,
    ) {
        if callback_names.is_empty() {
            return;
        }

        let globals = ctx.globals();
        let define_property = self.define_property.clone().restore(ctx).ok();
        for (name, previous) in callback_names.iter().zip(shadowed) {
            match (&define_property, previous.is_undefined()) {
                (Some(define_property), false) => {
                    let _ = define_property
                        .call::<_, Value>((globals.clone(), name.as_str(), previous))
                        .catch(ctx);
                }
                _ => {
                    let _ = globals.remove(name.as_str());
                }
            }
        }
    }
}

fn gc_threshold(memory_limit: usize) -> usize {
    GC_THRESHOLD.min(memory_limit / 2)
}

/// rquickjs 0.14 hands the source to QuickJS as a C string, so it rejects NUL
/// bytes. `\u0000` reads the same inside strings, comments and regexes, and a
/// raw NUL anywhere else is a syntax error anyway. After an odd run of
/// backslashes the last one already starts an escape, so only `u0000` is
/// added.
fn escape_nul(code: &str) -> Cow<'_, str> {
    if !code.contains('\0') {
        return Cow::Borrowed(code);
    }

    let mut escaped = String::with_capacity(code.len() + 16);
    let mut backslashes = 0_usize;
    for character in code.chars() {
        match character {
            '\0' if backslashes % 2 == 1 => escaped.push_str("u0000"),
            '\0' => escaped.push_str("\\u0000"),
            _ => escaped.push(character),
        }
        backslashes = if character == '\\' {
            backslashes + 1
        } else {
            0
        };
    }
    Cow::Owned(escaped)
}

/// Returns a promise's settled value, or the value itself if it is not one.
fn settle<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> Result<Value<'js>, String> {
    let Some(promise) = value.as_promise() else {
        return Ok(value);
    };
    match promise.result::<Value>() {
        None => Err("Promise did not settle".to_string()),
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(convert::describe_error(ctx, error)),
    }
}

fn format_caught_error<'js>(ctx: &Ctx<'js>, error: CaughtError<'js>) -> String {
    match error {
        CaughtError::Exception(exception) => convert::describe_thrown(ctx, exception.into_value()),
        CaughtError::Value(value) => convert::describe_thrown(ctx, value),
        CaughtError::Error(error) => error.to_string(),
    }
}
