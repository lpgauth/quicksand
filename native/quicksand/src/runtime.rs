//! The `Runtime` resource handed to Elixir and the registry of pending callbacks.

use std::collections::HashMap;
use std::mem;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SendError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rustler::{Encoder, LocalPid, OwnedEnv, Resource};

use crate::convert::CallbackReply;
use crate::interrupt::Interrupt;
use crate::worker::Message;

/// Pending callback replies, shared by NIF calls and the worker thread.
#[derive(Default)]
pub struct CallbackRegistry {
    pending: Mutex<HashMap<u64, Sender<CallbackReply>>>,
    next_id: AtomicU64,
    // Tags are positive, so the initial 0 matches no eval.
    abandoned_tag: AtomicU64,
}

impl CallbackRegistry {
    #[must_use]
    pub fn register(&self) -> (u64, Receiver<CallbackReply>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending().insert(id, tx);
        (id, rx)
    }

    pub fn respond(&self, id: u64, reply: CallbackReply) {
        let sender = self.pending().remove(&id);
        if let Some(sender) = sender {
            let _ = sender.send(reply);
        }
    }

    /// Fails every pending callback with `reason`, waking the worker.
    pub fn cancel_all(&self, reason: &str) {
        let pending = mem::take(&mut *self.pending());
        for sender in pending.into_values() {
            let _ = sender.send(Err(reason.to_string()));
        }
    }

    pub fn clear(&self) {
        self.pending().clear();
    }

    /// Marks the eval tagged `tag` as one its caller stopped waiting for, so
    /// the worker drops its result instead of sending it.
    pub fn abandon(&self, tag: u64) {
        self.abandoned_tag.store(tag, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_abandoned(&self, tag: u64) -> bool {
        self.abandoned_tag.load(Ordering::Relaxed) == tag
    }

    fn pending(&self) -> MutexGuard<'_, HashMap<u64, Sender<CallbackReply>>> {
        self.pending
            .lock()
            .expect("nothing panics while holding the callback registry lock")
    }
}

/// The resource behind a `Quicksand` runtime reference.
pub struct Runtime {
    sender: Sender<Message>,
    pub registry: Arc<CallbackRegistry>,
    pub interrupt: Arc<Interrupt>,
    // Shared with the worker thread, which clears it when it exits.
    pub alive: Arc<AtomicBool>,
    pub timeout: Duration,
}

impl Runtime {
    pub fn new(
        sender: Sender<Message>,
        registry: Arc<CallbackRegistry>,
        interrupt: Arc<Interrupt>,
        alive: Arc<AtomicBool>,
        timeout: Duration,
    ) -> Self {
        Self {
            sender,
            registry,
            interrupt,
            alive,
            timeout,
        }
    }

    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn send(&self, message: Message) -> Result<(), SendError<Message>> {
        self.sender.send(message)
    }

    /// Stops the worker, interrupting any running eval. Returns a receiver
    /// that gets a message once the worker has stopped, or `None` if it
    /// already had.
    pub fn stop(&self) -> Option<Receiver<()>> {
        self.alive.store(false, Ordering::Relaxed);
        self.interrupt.stop();
        self.registry.cancel_all("Runtime stopped");
        let (tx, rx) = mpsc::channel();
        self.sender.send(Message::Stop { reply: tx }).ok()?;
        Some(rx)
    }
}

#[rustler::resource_impl]
impl Resource for Runtime {}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Sends `data` to `pid` from a thread that is not a BEAM scheduler. Returns
/// whether the process was alive to receive it.
#[must_use]
pub fn send_to_pid<T: Encoder>(pid: LocalPid, data: T) -> bool {
    OwnedEnv::new().send_and_clear(&pid, |_env| data).is_ok()
}
