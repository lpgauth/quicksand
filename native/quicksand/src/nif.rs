//! NIF entry points called by `Quicksand.Native`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rustler::{Atom, Env, OwnedEnv, ResourceArc, Term};

use crate::atoms;
use crate::convert::{self, CallbackReply, JsValue};
use crate::runtime::Runtime;
use crate::worker::{Message, Worker, WorkerOptions};

const DEAD_RUNTIME: &str = "dead_runtime";

#[rustler::nif]
fn start_runtime(
    env: Env,
    start_ref: Term,
    timeout_ms: u64,
    memory_limit: usize,
    max_stack_size: usize,
) -> Atom {
    let caller = env.pid();
    let (sender, receiver) = mpsc::channel::<Message>();
    let interrupt = Arc::new(AtomicBool::new(false));
    let worker_interrupt = Arc::clone(&interrupt);
    let timeout = Duration::from_millis(timeout_ms);
    let options = WorkerOptions {
        memory_limit,
        max_stack_size,
    };

    let mut reply_env = OwnedEnv::new();
    let saved_ref = reply_env.save(start_ref);

    thread::spawn(move || match Worker::try_new(&options, worker_interrupt) {
        Ok(worker) => {
            let sent = reply_env.send_and_clear(&caller, |env| {
                let resource = ResourceArc::new(Runtime::new(sender, interrupt, timeout));
                let reply = (atoms::ok(), resource);
                (atoms::quicksand_start(), saved_ref.load(env), reply)
            });
            if sent.is_ok() {
                worker.run(receiver);
            }
        }
        Err(reason) => {
            let _ = reply_env.send_and_clear(&caller, |env| {
                let reply = (atoms::error(), reason);
                (atoms::quicksand_start(), saved_ref.load(env), reply)
            });
        }
    });

    atoms::ok()
}

#[rustler::nif(schedule = "DirtyIo")]
fn eval_sync(resource: ResourceArc<Runtime>, code: String) -> Result<JsValue, String> {
    let (tx, rx) = mpsc::channel();
    if resource.send(Message::Eval { code, reply: tx }).is_err() {
        return Err(DEAD_RUNTIME.to_string());
    }

    match rx.recv_timeout(resource.timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => {
            resource.interrupt.store(true, Ordering::Relaxed);
            Err("timeout".to_string())
        }
        Err(RecvTimeoutError::Disconnected) => Err(DEAD_RUNTIME.to_string()),
    }
}

#[rustler::nif]
fn eval_with_callbacks(
    env: Env,
    resource: ResourceArc<Runtime>,
    code: String,
    callback_names: Vec<String>,
    tag: u64,
) -> Result<JsValue, String> {
    let message = Message::EvalWithCallbacks {
        code,
        callback_names,
        registry: Arc::clone(&resource.registry),
        caller: env.pid(),
        timeout: resource.timeout,
        tag,
    };
    if resource.send(message).is_err() {
        return Err(DEAD_RUNTIME.to_string());
    }
    Ok(JsValue::Null)
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
fn get_timeout(resource: ResourceArc<Runtime>) -> u64 {
    u64::try_from(resource.timeout.as_millis()).unwrap_or(u64::MAX)
}

#[rustler::nif]
fn is_alive(resource: ResourceArc<Runtime>) -> bool {
    resource.alive.load(Ordering::Relaxed)
}

#[rustler::nif]
fn interrupt(resource: ResourceArc<Runtime>, tag: u64) -> Atom {
    resource.registry.abandon(tag);
    resource.interrupt.store(true, Ordering::Relaxed);
    atoms::ok()
}

#[rustler::nif(schedule = "DirtyIo")]
fn stop_runtime(resource: ResourceArc<Runtime>) -> Atom {
    resource.alive.store(false, Ordering::Relaxed);
    resource.interrupt.store(true, Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    let _ = resource.send(Message::Stop { reply: tx });
    let _ = rx.recv_timeout(resource.timeout);
    atoms::ok()
}
