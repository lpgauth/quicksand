//! The `Runtime` resource handed to Elixir and the registry of pending callbacks.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SendError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rustler::{Encoder, LocalPid, OwnedEnv, Resource};

use crate::convert::CallbackReply;
use crate::worker::Message;

/// Pending `eval/3` callback replies, shared by NIF calls and the worker thread.
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

    pub fn clear(&self) {
        self.pending().clear();
    }

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
    pub interrupt: Arc<AtomicBool>,
    pub alive: AtomicBool,
    pub timeout: Duration,
}

impl Runtime {
    pub fn new(sender: Sender<Message>, interrupt: Arc<AtomicBool>, timeout: Duration) -> Self {
        Self {
            sender,
            registry: Arc::new(CallbackRegistry::default()),
            interrupt,
            alive: AtomicBool::new(true),
            timeout,
        }
    }

    pub fn send(&self, message: Message) -> Result<(), SendError<Message>> {
        self.sender.send(message)
    }
}

#[rustler::resource_impl]
impl Resource for Runtime {}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        self.interrupt.store(true, Ordering::Relaxed);
        let (tx, _rx) = mpsc::channel();
        let _ = self.sender.send(Message::Stop { reply: tx });
    }
}

/// Sends `data` to `pid` from a thread that is not a BEAM scheduler. Returns
/// whether the process was alive to receive it.
#[must_use]
pub fn send_to_pid<T: Encoder>(pid: LocalPid, data: T) -> bool {
    OwnedEnv::new().send_and_clear(&pid, |_env| data).is_ok()
}
