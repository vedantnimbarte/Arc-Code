//! Steering: change a running turn's mind without killing it.
//!
//! Today the only way to redirect a turn in flight is to interrupt it. That
//! throws away the work in progress — the files it has read, what it has
//! concluded — to say something the model could have acted on directly, and
//! then it has to rebuild all of it from the transcript.
//!
//! A steer is a message dropped into the turn at the next safe boundary: not
//! mid-stream (the provider is still writing an assistant message, and the
//! history has to stay well-formed), but between one provider round-trip and
//! the next, where the loop is about to compose a fresh request anyway. That
//! is the same seam compaction and the learning hook already use.
//!
//! It is deliberately *not* a queue of prompts. A steer joins the turn that is
//! already running; if no turn is running there is nothing to steer, and the
//! caller should send an ordinary prompt instead. `wingman-cli` implements
//! exactly that fallback so `/steer` is never silently dropped.

use std::sync::Mutex;

/// A place to leave guidance for a turn that is already running.
///
/// Cheap to clone as an `Arc` and hand to a UI, an HTTP route, or anything
/// else holding a handle on the session while the loop owns the turn.
#[derive(Debug, Default)]
pub struct SteerInbox {
    pending: Mutex<Vec<String>>,
}

impl SteerInbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Leave a message for the running turn. Empty and whitespace-only
    /// messages are dropped here rather than becoming an empty user turn that
    /// the model has to interpret.
    pub fn push(&self, message: impl Into<String>) {
        let message = message.into();
        if message.trim().is_empty() {
            return;
        }
        self.lock().push(message);
    }

    /// Take everything waiting. The loop calls this once per round trip.
    pub fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.lock())
    }

    /// Whether anything is waiting, without taking it.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// A poisoned mutex here means some other thread panicked while holding
    /// it. The data is a `Vec<String>` with no invariant to violate, so
    /// recovering is strictly better than propagating the panic into the
    /// agent loop and killing a turn over a lock.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// How a steer is presented to the model.
///
/// Marked as an interjection rather than passed off as an ordinary user turn:
/// the model needs to know this arrived *during* its work, so that "actually,
/// skip the tests" reads as a change of plan rather than a new request that
/// supersedes the original one.
pub fn format(message: &str) -> String {
    format!(
        "[wingman steer] The user sent this while you were working. Take it as an \
         adjustment to what you are already doing, not a new task, and carry on:\n\n{message}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_in_order() {
        let inbox = SteerInbox::new();
        inbox.push("first");
        inbox.push("second");
        assert_eq!(inbox.drain(), vec!["first", "second"]);
    }

    #[test]
    fn draining_empties_the_inbox() {
        let inbox = SteerInbox::new();
        inbox.push("only");
        assert_eq!(inbox.drain().len(), 1);
        assert!(
            inbox.drain().is_empty(),
            "a steer must not be delivered twice"
        );
        assert!(inbox.is_empty());
    }

    #[test]
    fn blank_messages_are_dropped() {
        let inbox = SteerInbox::new();
        inbox.push("");
        inbox.push("   \n  ");
        assert!(inbox.is_empty());
    }

    #[test]
    fn the_formatted_steer_carries_the_message_and_frames_it() {
        let out = format("prefer the smaller patch");
        assert!(out.contains("prefer the smaller patch"));
        assert!(out.contains("not a new task"));
    }

    #[test]
    fn a_poisoned_lock_does_not_lose_the_inbox() {
        use std::sync::Arc;
        let inbox = Arc::new(SteerInbox::new());
        inbox.push("survives");
        let clone = inbox.clone();
        // Poison the mutex from another thread.
        let _ = std::thread::spawn(move || {
            let _guard = clone.pending.lock().unwrap();
            panic!("poisoning");
        })
        .join();
        assert_eq!(inbox.drain(), vec!["survives"]);
    }
}
