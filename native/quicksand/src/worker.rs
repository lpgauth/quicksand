//! The thread that owns a QuickJS runtime and evaluates code on it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use rquickjs::{CatchResultExt, CaughtError, Context, Ctx, Function, Runtime, Value};
use rustler::LocalPid;

use crate::atoms;
use crate::convert::{self, JsValue};
use crate::runtime::{self, CallbackRegistry};

type EvalResult = Result<JsValue, String>;

const GC_THRESHOLD: usize = 4 * 1024 * 1024;

// QuickJS cannot discard queued jobs, so after an interrupt the remaining ones
// still run (and abort at their next interrupt check) instead of leaking into
// the next eval. Short jobs can keep queueing more, so the drain is capped.
const MAX_JOBS_AFTER_INTERRUPT: usize = 10_000;

// Installed once per runtime and frozen so user JS cannot replace it. Callback
// names are passed in as an argument, never interpolated into source.
const WRAPPER_FACTORY: &str = r"Object.defineProperty(globalThis, '__quicksand_make_wrapper', {
    value: function(name) {
        return function() {
            globalThis.__quicksand_cb_args = Array.from(arguments);
            globalThis.__quicksand_dispatch(name);
            var r = globalThis.__quicksand_cb_result;
            delete globalThis.__quicksand_cb_result;
            return r;
        };
    },
    writable: false,
    configurable: false,
    enumerable: false
})";

/// Work sent from NIF calls to a runtime's worker thread.
pub enum Message {
    Eval {
        code: String,
        reply: Sender<EvalResult>,
    },
    EvalWithCallbacks {
        code: String,
        callback_names: Vec<String>,
        registry: Arc<CallbackRegistry>,
        caller: LocalPid,
        timeout: Duration,
        tag: u64,
    },
    Stop {
        reply: Sender<()>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct WorkerOptions {
    pub memory_limit: usize,
    pub max_stack_size: usize,
}

pub struct Worker {
    runtime: Runtime,
    context: Context,
    interrupt: Arc<AtomicBool>,
}

impl Worker {
    pub fn try_new(options: &WorkerOptions, interrupt: Arc<AtomicBool>) -> Result<Self, String> {
        let runtime =
            Runtime::new().map_err(|error| format!("Failed to create QuickJS runtime: {error}"))?;
        runtime.set_memory_limit(options.memory_limit);
        runtime.set_max_stack_size(options.max_stack_size);
        runtime.set_gc_threshold(GC_THRESHOLD);
        let handler_interrupt = Arc::clone(&interrupt);
        runtime.set_interrupt_handler(Some(Box::new(move || {
            handler_interrupt.load(Ordering::Relaxed)
        })));

        let context = Context::full(&runtime)
            .map_err(|error| format!("Failed to create QuickJS context: {error}"))?;
        context.with(|ctx| {
            ctx.eval::<(), _>(WRAPPER_FACTORY)
                .catch(&ctx)
                .map_err(|error| format!("Failed to install wrapper factory: {error:?}"))
        })?;

        Ok(Self {
            runtime,
            context,
            interrupt,
        })
    }

    // Takes the receiver by value so the channel closes as soon as the loop
    // ends. Sends then fail with dead_runtime instead of queueing while the
    // QuickJS runtime is being freed.
    #[allow(clippy::needless_pass_by_value)]
    pub fn run(&self, receiver: Receiver<Message>) {
        while let Ok(message) = receiver.recv() {
            match message {
                Message::Eval { code, reply } => {
                    self.interrupt.store(false, Ordering::Relaxed);
                    let _ = reply.send(self.eval(&code));
                }
                Message::EvalWithCallbacks {
                    code,
                    callback_names,
                    registry,
                    caller,
                    timeout,
                    tag,
                } => {
                    self.interrupt.store(false, Ordering::Relaxed);
                    let result = self
                        .install_callbacks(&callback_names, &registry, caller, timeout, tag)
                        .and_then(|()| self.eval(&code));
                    self.remove_callbacks(&callback_names);
                    registry.clear();
                    if !registry.is_abandoned(tag) {
                        let message = (atoms::quicksand_result(), tag, result);
                        let _ = runtime::send_to_pid(caller, message);
                    }
                }
                Message::Stop { reply } => {
                    let _ = reply.send(());
                    break;
                }
            }
        }
    }

    fn drain_jobs(&self) {
        let mut budget = MAX_JOBS_AFTER_INTERRUPT;
        while !matches!(self.runtime.execute_pending_job(), Ok(false)) {
            if self.interrupt.load(Ordering::Relaxed) {
                budget -= 1;
                if budget == 0 {
                    break;
                }
            }
        }
    }

    fn eval(&self, code: &str) -> EvalResult {
        let result = self
            .context
            .with(|ctx| match ctx.eval::<Value, _>(code).catch(&ctx) {
                Ok(value) => convert::js_to_term(&ctx, value),
                Err(error) => Err(format_caught_error(error)),
            });
        self.drain_jobs();
        result
    }

    fn install_callbacks(
        &self,
        callback_names: &[String],
        registry: &Arc<CallbackRegistry>,
        caller: LocalPid,
        timeout: Duration,
        tag: u64,
    ) -> Result<(), String> {
        let registry = Arc::clone(registry);
        let interrupt = Arc::clone(&self.interrupt);

        self.context.with(|ctx| {
            let globals = ctx.globals();

            // The wrapper hands arguments over in `__quicksand_cb_args` and reads
            // the result back from `__quicksand_cb_result`, so no JS value has to
            // outlive this closure's context borrow.
            let dispatch = Function::new(
                ctx.clone(),
                move |ctx: Ctx<'_>, name: String| -> rquickjs::Result<()> {
                    if interrupt.load(Ordering::Relaxed) {
                        let message = format!("Callback '{name}' called after interrupt");
                        return Err(throw_string(&ctx, &message));
                    }

                    let arguments: Value = ctx
                        .globals()
                        .get("__quicksand_cb_args")
                        .unwrap_or_else(|_| Value::new_undefined(ctx.clone()));
                    let callback_arguments = match convert::js_to_term(&ctx, arguments) {
                        Ok(JsValue::Array(items)) => items,
                        Ok(other) => vec![other],
                        Err(_) => vec![],
                    };

                    let (id, reply) = registry.register();
                    let message = (
                        atoms::quicksand_callback(),
                        tag,
                        id,
                        name.as_str(),
                        callback_arguments,
                    );
                    let _ = runtime::send_to_pid(caller, message);

                    match reply.recv_timeout(timeout) {
                        Ok(Ok(term_value)) => {
                            let value = convert::intermediate_to_js(&ctx, &term_value)
                                .map_err(|_| rquickjs::Error::Exception)?;
                            ctx.globals()
                                .set("__quicksand_cb_result", value)
                                .map_err(|_| rquickjs::Error::Exception)?;
                            Ok(())
                        }
                        Ok(Err(reason)) => Err(throw_string(&ctx, &reason)),
                        Err(_) => {
                            let message = format!("Callback '{name}' timed out");
                            Err(throw_string(&ctx, &message))
                        }
                    }
                },
            )
            .map_err(|error| format!("Failed to create __quicksand_dispatch: {error}"))?;

            globals
                .set("__quicksand_dispatch", dispatch)
                .map_err(|error| format!("Failed to set __quicksand_dispatch: {error}"))?;

            let make_wrapper: Function = globals
                .get("__quicksand_make_wrapper")
                .map_err(|error| format!("Failed to get wrapper factory: {error}"))?;

            for name in callback_names {
                let wrapper: Function = make_wrapper
                    .call((name.as_str(),))
                    .catch(&ctx)
                    .map_err(|error| format!("Failed to create wrapper for {name}: {error:?}"))?;
                globals
                    .set(name.as_str(), wrapper)
                    .map_err(|error| format!("Failed to set wrapper for {name}: {error}"))?;
            }

            Ok(())
        })
    }

    fn remove_callbacks(&self, callback_names: &[String]) {
        self.context.with(|ctx| {
            let globals = ctx.globals();
            let _ = globals.remove("__quicksand_dispatch");
            let _ = globals.remove("__quicksand_cb_args");
            let _ = globals.remove("__quicksand_cb_result");
            for name in callback_names {
                let _ = globals.remove(name.as_str());
            }
        });
    }
}

/// Throws `message` as a JS string and returns the error that tells rquickjs
/// an exception is pending.
fn throw_string(ctx: &Ctx<'_>, message: &str) -> rquickjs::Error {
    match rquickjs::String::from_str(ctx.clone(), message) {
        Ok(string) => ctx.throw(string.into_value()),
        Err(error) => error,
    }
}

fn format_caught_error(error: CaughtError<'_>) -> String {
    match error {
        CaughtError::Exception(exception) => {
            let message: String = exception.get("message").unwrap_or_default();
            let stack: String = exception.get("stack").unwrap_or_default();
            if stack.is_empty() {
                message
            } else {
                format!("{message}\n{stack}")
            }
        }
        // QuickJS throws null when it cannot allocate the out-of-memory error
        // itself. A script throwing null literally is reported the same way.
        CaughtError::Value(value) if value.is_null() => "out of memory".to_string(),
        CaughtError::Value(value) => format!("Thrown value: {value:?}"),
        CaughtError::Error(error) => error.to_string(),
    }
}
