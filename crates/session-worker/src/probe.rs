//! Answers the OSC 7501 program-status probe of the PTY child.
//!
//! A probe-waiting program (Claude Code, Pi) enables its status reports only
//! after the terminal echoes `ESC ] 7501 ; ? ESC \`. The output hub queues one
//! token per answerable probe; a dedicated task writes the reply through the
//! [`WriteCoordinator`], so the reader thread never writes and a reply never
//! lands inside a paced input plan.

// Rust guideline compliant 2026-06-26

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;
use tracing::{event, Level};

use pohunek_terminal::PROBE_REPLY;

use crate::input::{InputFragment, WriteCoordinator};

/// One-slot queue of pending probe replies.
///
/// A probe that arrives while a reply is pending coalesces into it.
#[derive(Debug, Default)]
pub(crate) struct ProbeReplies {
    pending: AtomicBool,
    closed: AtomicBool,
    wake: Notify,
}

impl ProbeReplies {
    /// Queues one reply, merging with a pending one.
    pub(crate) fn queue(&self) {
        self.pending.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    /// Ends the queue; a waiting task observes it and stops.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    /// Waits until a reply is pending, or returns `false` once the queue is
    /// closed. The pending mark stays set until [`Self::take`], so probes that
    /// arrive while the reply waits for the input lock join it.
    async fn wait(&self) -> bool {
        loop {
            if self.closed.load(Ordering::Acquire) {
                return false;
            }
            if self.pending.load(Ordering::Acquire) {
                return true;
            }
            self.wake.notified().await;
        }
    }

    /// Clears the pending mark; probes queued afterwards ask for a new reply.
    fn take(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

/// Writes one reply per queued token until the queue closes.
///
/// A reply the PTY does not accept within the control stall bound is dropped:
/// the program then reads "no 7501" for this process and falls back to its
/// screen output.
pub(crate) async fn answer_probes(replies: Arc<ProbeReplies>, input: WriteCoordinator) {
    while replies.wait().await {
        let reply = InputFragment {
            bytes: PROBE_REPLY.to_vec(),
            delay_after: std::time::Duration::ZERO,
        };
        if let Err(error) = input.execute_reply(reply, || replies.take()).await {
            replies.take();
            event!(
                name: "worker.probe.reply.dropped",
                Level::DEBUG,
                error.message = %error,
                "program status probe reply dropped: {{error.message}}",
            );
        }
    }
}
