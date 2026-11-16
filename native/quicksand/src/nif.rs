//! NIF entry points called by `Quicksand.Native`.

use std::str;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rustler::{Atom, Binary, Encoder, Env, OwnedEnv, ResourceArc, Term};

use crate::atoms;
use crate::convert::{self, CallbackReply};
use crate::interrupt::Interrupt;
use crate::runtime::{CallbackRegistry, Runtime};
use crate::worker::{EvalRequest, Message, Worker, WorkerOptions, DEAD_RUNTIME};

// rquickjs treats a larger `max_stack_size` as no limit at all.
const MAX_STACK_SIZE: usize = 1 << 24;

// QuickJS only enforces `max_stack_size` on JS frames. The native frames
// around them (the worker loop, conversions, callbacks) need room too, and
// without it deep recursion overflows the thread's stack and crashes the VM.
const STACK_HEADROOM: usize = 2 * 1024 * 1024;

/// Clears the runtime's alive flag when the worker thread exits, including by
/// panic.
struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Creates a runtime whose worker thread starts in the background and then
/// sends `{:quicksand_start, start_ref, :ok | {:error, reason}}`.
#[rustler::nif]
fn start_runtime(
    env: Env,
    start_ref: Term,
    timeout_ms: u64,
    memory_limit: usize,
    max_stack_size: usize,
) -> Result<ResourceArc<Runtime>, String> {
    if max_stack_size == 0 || max_stack_size > MAX_STACK_SIZE {
        return Err(format!(
            "max_stack_size must be between 1 and {MAX_STACK_SIZE}"
        ));
    }

    let (sender, receiver) = mpsc::channel::<Message>();
    let registry = Arc::new(CallbackRegistry::default());
    let interrupt = Arc::new(Interrupt::default());
    let alive = Arc::new(AtomicBool::new(true));
    let timeout = Duration::from_millis(timeout_ms);
    let resource = ResourceArc::new(Runtime::new(
        sender,
        Arc::clone(&registry),
        Arc::clone(&interrupt),
        Arc::clone(&alive),
        timeout,
    ));

    let options = WorkerOptions {
        memory_limit,
        max_stack_size,
    };
    let caller = env.pid();
    let mut reply_env = OwnedEnv::new();
    let saved_ref = reply_env.save(start_ref);

    let run_worker = move || {
        let _alive_guard = AliveGuard(alive);
        let started = Worker::try_new(&options, registry, Arc::clone(&interrupt));
        // A caller that gave up waiting has already stopped the runtime.
        if interrupt.is_stopping() {
            return;
        }
        let sent = reply_env.send_and_clear(&caller, |env| {
            let reply = match &started {
                Ok(_) => atoms::ok().encode(env),
                Err(reason) => (atoms::error(), reason).encode(env),
            };
            (atoms::quicksand_start(), saved_ref.load(env), reply)
        });
        if let (Ok(worker), Ok(())) = (started, sent) {
            worker.run(receiver);
        }
    };

    thread::Builder::new()
        .name("quicksand-worker".to_string())
        .stack_size(max_stack_size.saturating_add(STACK_HEADROOM))
        .spawn(run_worker)
        .map_err(|error| format!("Failed to start worker thread: {error}"))?;
    Ok(resource)
}

/// Queues `code` on the runtime and returns its timeout in milliseconds. The
/// result arrives as `{:quicksand_result, tag, result}`, after any
/// `{:quicksand_callback, tag, id, name, args}` requests.
#[rustler::nif]
fn eval(
    env: Env,
    resource: ResourceArc<Runtime>,
    code: Binary,
    callback_names: Vec<String>,
    tag: u64,
) -> Result<u64, String> {
    if !resource.is_alive() {
        return Err(DEAD_RUNTIME.to_string());
    }
    let code = str::from_utf8(code.as_slice())
        .map_err(|_| "JavaScript source must be valid UTF-8".to_string())?;

    let request = EvalRequest {
        code: code.to_string(),
        callback_names,
        caller: env.pid(),
        tag,
        deadline: Instant::now().checked_add(resource.timeout),
    };
    resource
        .send(Message::Eval(request))
        .map_err(|_| DEAD_RUNTIME.to_string())?;
    Ok(u64::try_from(resource.timeout.as_millis()).unwrap_or(u64::MAX))
}

#[rustler::nif]
fn respond_callback(resource: ResourceArc<Runtime>, callback_id: u64, result: Term) -> Atom {
    resource
        .registry
        .respond(callback_id, decode_callback_reply(result));
    atoms::ok()
}

fn decode_callback_reply(term: Term) -> CallbackReply {
    let Ok((tag, value)) = term.decode::<(Atom, Term)>() else {
        return Err("Invalid callback result: expected {atom, term} tuple".to_string());
    };

    if tag == atoms::ok() {
        convert::term_to_intermediate(value)
            .map_err(|reason| format!("Failed to convert callback result: {reason}"))
    } else if tag == atoms::error() {
        Err(value
            .decode::<String>()
            .unwrap_or_else(|_| "Callback returned error with non-string reason".to_string()))
    } else {
        Err("Invalid callback result: expected :ok or :error tag".to_string())
    }
}

#[rustler::nif]
fn abandon(resource: ResourceArc<Runtime>, tag: u64) -> Atom {
    resource.registry.abandon(tag);
    atoms::ok()
}

#[rustler::nif]
fn is_alive(resource: ResourceArc<Runtime>) -> bool {
    resource.is_alive()
}

#[rustler::nif(schedule = "DirtyIo")]
fn stop_runtime(resource: ResourceArc<Runtime>) -> Atom {
    if let Some(stopped) = resource.stop() {
        let _ = stopped.recv_timeout(resource.timeout);
    }
    atoms::ok()
}
