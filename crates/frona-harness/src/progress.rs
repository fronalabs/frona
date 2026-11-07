//! Optional, bounded observation. This transport never owns execution or reports.

use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex, Weak},
};

use tokio::sync::broadcast;

use super::{ActionKind, ActionPosition, ActionStatus};

/// Best-effort live updates. Use the result and execution report for authoritative
/// outcomes; this stream may lose any event, including the final update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Provider text updates, preserved as supplied by the adapter. Some adapters
    /// send cumulative text rather than deltas; these are not a final answer.
    Text {
        model: String,
        text: String,
    },
    Reasoning {
        model: String,
        text: String,
    },
    Action {
        position: ActionPosition,
        name: String,
        kind: ActionKind,
        status: ActionStatus,
    },
    Refinement {
        response: usize,
        stage: RefinementStage,
    },
    /// This receiver fell behind and lost this many buffered notifications.
    Missed(u64),
}

/// Progress carries no submission, retained value, arguments, or tool outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefinementStage {
    Ready,
    Repaired,
    Inspecting,
    Retained,
    Revised,
    Accepted,
    MissingSubmission,
}

/// Receiving observes only: it never polls the run or starts model/tool work.
/// Buffered notifications (and loss notices) drain before `recv` returns None.
pub struct ProgressReceiver(broadcast::Receiver<Progress>);

impl ProgressReceiver {
    pub async fn recv(&mut self) -> Option<Progress> {
        match self.0.recv().await {
            Ok(progress) => Some(progress),
            Err(broadcast::error::RecvError::Lagged(count)) => Some(Progress::Missed(count)),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

#[derive(Debug, Default)]
struct Subscribers {
    senders: Vec<broadcast::Sender<Progress>>,
    closed: bool,
}

/// Only this owner controls lifecycle. Publishers in retained conversations are
/// weak, and independent report handles never retain this transport.
#[derive(Debug, Default)]
pub(super) struct ProgressOwner(Arc<Mutex<Subscribers>>);

impl ProgressOwner {
    pub fn publisher(&self) -> ProgressPublisher {
        ProgressPublisher(Arc::downgrade(&self.0))
    }

    pub fn subscribe(&self, capacity: NonZeroUsize) -> ProgressReceiver {
        let (sender, receiver) = broadcast::channel(capacity.get());
        let mut subscribers = self.0.lock().unwrap();
        if !subscribers.closed {
            subscribers
                .senders
                .retain(|sender| sender.receiver_count() > 0);
            subscribers.senders.push(sender);
        }
        ProgressReceiver(receiver)
    }

    pub fn close(&self) {
        let mut subscribers = self.0.lock().unwrap();
        subscribers.closed = true;
        subscribers.senders.clear();
    }
}

impl Drop for ProgressOwner {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct ProgressPublisher(Weak<Mutex<Subscribers>>);

impl ProgressPublisher {
    pub fn has_subscribers(&self) -> bool {
        self.0.upgrade().is_some_and(|subscribers| {
            subscribers
                .lock()
                .unwrap()
                .senders
                .iter()
                .any(|sender| sender.receiver_count() > 0)
        })
    }

    pub fn emit(&self, progress: impl FnOnce() -> Progress) {
        let Some(subscribers) = self.0.upgrade() else {
            return;
        };
        let mut subscribers = subscribers.lock().unwrap();
        subscribers
            .senders
            .retain(|sender| sender.receiver_count() > 0);
        if !subscribers.senders.is_empty() {
            let progress = progress();
            for sender in &subscribers.senders {
                let _ = sender.send(progress.clone());
            }
        }
    }
}
