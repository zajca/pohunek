//! Answers the OSC 7501 program-status probe of the PTY child.
//!
//! A probe-waiting program (Claude Code, Pi) enables its status reports only
//! after the terminal echoes `ESC ] 7501 ; ? ESC \`. The output hub queues one
//! token per answerable probe; a dedicated task writes the reply through the
//! [`WriteCoordinator`], so the reader thread never writes and a reply never
//! lands inside a fragment. A reply may land between the fragments of a paced
//! input plan while the plan's delay runs.

// Rust guideline compliant 2026-06-26

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;
use tracing::{event, Level};

use pohunek_platform::process::{HostInspector, Pid, ProcessInspector, ProcessMatchers};
use pohunek_terminal::PROBE_REPLY;
use pohunek_worker_protocol::ProcessPatterns;

use crate::input::{InputFragment, WriteCoordinator};

/// One-slot queue of pending probe replies.
///
/// A probe that arrives while a reply is pending coalesces into it.
#[derive(Debug, Default)]
pub(crate) struct ProbeReplies {
    pending: AtomicBool,
    closed: AtomicBool,
    /// Replies the foreground filter refused.
    withheld: AtomicU64,
    wake: Notify,
}

impl ProbeReplies {
    /// Queues one reply, merging with a pending one.
    pub(crate) fn queue(&self) {
        self.pending.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    /// Counts one refused reply, after its decision is final.
    pub(crate) fn note_withheld(&self) {
        self.withheld.fetch_add(1, Ordering::Release);
    }

    /// How many replies the foreground filter refused so far.
    pub(crate) fn withheld(&self) -> u64 {
        self.withheld.load(Ordering::Acquire)
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

/// Decides whether the program in the PTY foreground may be answered.
///
/// A session whose own runtime does not answer the probe (a shell) answers it
/// only for the programs the launch plan lists, judged by the same
/// [`ProcessMatchers`] the daemon uses to recognise them. The check runs when a
/// reply is about to be written, against the members of the terminal's
/// foreground process group at that moment.
#[derive(Debug)]
pub(crate) struct ForegroundProbeFilter {
    programs: Vec<ProcessMatchers>,
    /// The PTY's root process, known once the child is spawned.
    root_pid: Option<Pid>,
    inspector: Arc<HostInspector>,
}

impl ForegroundProbeFilter {
    /// Compiles the listed programs.
    ///
    /// # Errors
    ///
    /// Returns the diagnostic of the first pattern the regex engine rejects.
    pub(crate) fn new(programs: &[ProcessPatterns]) -> Result<Self, String> {
        let programs = programs
            .iter()
            .map(|entry| ProcessMatchers::from_patterns(&entry.comm, &entry.cmdline))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            programs,
            root_pid: None,
            inspector: Arc::new(HostInspector::new()),
        })
    }

    /// Binds the filter to the PTY whose root process is `root_pid`.
    pub(crate) fn bound_to(mut self, root_pid: Pid) -> Self {
        self.root_pid = Some(root_pid);
        self
    }

    /// Whether a process of the current foreground group matches a listed
    /// program. Any failure to read the group answers no.
    fn allows(&self) -> bool {
        let Some(root_pid) = self.root_pid else {
            return false;
        };
        let group = match self.inspector.foreground_process_group(root_pid) {
            Ok(Some(group)) => group,
            Ok(None) => return false,
            Err(error) => {
                event!(
                    name: "worker.probe.foreground.unreadable",
                    Level::DEBUG,
                    error.message = %error,
                    "foreground process group unreadable, probe not answered: {{error.message}}",
                );
                return false;
            }
        };
        // A shell without job control keeps a started program in the shell's own
        // process group, so every member of the foreground group counts.
        let mut members = match self.inspector.descendants(root_pid) {
            Ok(members) => members,
            Err(error) => {
                event!(
                    name: "worker.probe.foreground.unreadable",
                    Level::DEBUG,
                    error.message = %error,
                    "foreground processes unreadable, probe not answered: {{error.message}}",
                );
                return false;
            }
        };
        if let Ok(Some(root)) = self.inspector.process(root_pid) {
            members.push(root);
        }
        members
            .iter()
            .filter(|member| member.pgid == group)
            .any(|member| self.programs.iter().any(|program| program.matches(member)))
    }
}

/// Writes one reply per queued token until the queue closes.
///
/// A reply the PTY does not accept within the control stall bound is dropped:
/// the program then reads "no 7501" for this process and falls back to its
/// screen output.
pub(crate) async fn answer_probes(
    replies: Arc<ProbeReplies>,
    input: WriteCoordinator,
    filter: Option<Arc<ForegroundProbeFilter>>,
) {
    while replies.wait().await {
        if let Some(filter) = &filter {
            let checked = Arc::clone(filter);
            let allowed = tokio::task::spawn_blocking(move || checked.allows())
                .await
                .unwrap_or(false);
            if !allowed {
                replies.take();
                replies.note_withheld();
                event!(
                    name: "worker.probe.reply.withheld",
                    Level::DEBUG,
                    "program status probe not answered: the foreground program is not listed",
                );
                continue;
            }
        }
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
