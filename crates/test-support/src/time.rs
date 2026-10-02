//! Helpers for tests that drive tokio's paused clock while awaiting real I/O.
//!
//! A `#[tokio::test(start_paused = true)]` test advances the clock by itself
//! whenever the runtime is idle. That is what makes a pure timer test fast, and
//! what breaks a test that also awaits a real socket, PTY or child process: the
//! runtime idles on the I/O, the clock races ahead to the next timer, and every
//! deadline in the code under test expires before the I/O completes.
//!
//! [`AutoAdvanceInhibitor`] turns that automatic advance off, so virtual time
//! moves only through explicit `tokio::time::advance` calls. [`TIMER_TICK`] is
//! the margin to add to such an advance.
//!
//! # Examples
//!
//! ```
//! use std::time::Duration;
//!
//! use pohunek_test_support::time::{AutoAdvanceInhibitor, TIMER_TICK};
//!
//! # #[tokio::main(flavor = "current_thread", start_paused = true)]
//! # async fn main() {
//! let inhibitor = AutoAdvanceInhibitor::new();
//! let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
//! // ... await real I/O here; the clock stays at its current instant ...
//! tokio::time::advance(Duration::from_secs(5) + TIMER_TICK).await;
//! assert!(tokio::time::Instant::now() >= deadline);
//! inhibitor.release().await;
//! # }
//! ```

// Rust guideline compliant 2026-10-02

use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::wait::HANG_GUARD;

/// Margin added to every `tokio::time::advance` of a paused clock.
///
/// Tokio's timer wheel has a resolution of one millisecond and rounds a
/// timer's deadline up to the next tick, so advancing by exactly a timer's
/// duration can leave it one tick short. One millisecond is that resolution;
/// a smaller margin reintroduces the off-by-one tick, a larger one only moves
/// virtual time further than the timer requires.
pub const TIMER_TICK: Duration = Duration::from_millis(1);

/// Keeps tokio's paused clock from auto-advancing while real I/O is awaited.
///
/// A `spawn_blocking` task in flight makes a current-thread runtime with a
/// paused clock skip its automatic advance, so the clock moves only through
/// `tokio::time::advance`, even while the runtime idles on a socket or a PTY.
/// The inhibitor parks such a task.
///
/// The task ends when the inhibitor is dropped, when [`release`] is awaited, or
/// after [`HANG_GUARD`] of real time, so a stuck test then fails on its own
/// virtual hang guards instead of waiting for nextest to terminate it. Ending
/// the task wakes the runtime, so auto-advance resumes at once.
///
/// The inhibition applies to a current-thread runtime, which is the only kind
/// `start_paused` creates.
///
/// [`release`]: AutoAdvanceInhibitor::release
#[derive(Debug)]
pub struct AutoAdvanceInhibitor {
    release: Sender<()>,
    task: JoinHandle<()>,
}

impl AutoAdvanceInhibitor {
    /// Starts inhibiting auto-advance of the current runtime's paused clock.
    ///
    /// # Panics
    ///
    /// Panics when called outside a tokio runtime.
    #[must_use = "auto-advance resumes as soon as the inhibitor is dropped"]
    pub fn new() -> Self {
        Self::within(HANG_GUARD)
    }

    /// [`AutoAdvanceInhibitor::new`] with an explicit real-time ceiling.
    fn within(ceiling: Duration) -> Self {
        let (release, released) = channel::<()>();
        let task = tokio::task::spawn_blocking(move || {
            // A disconnected sender (drop or release) and the ceiling both end
            // the wait; either way the inhibition is over.
            let _ = released.recv_timeout(ceiling);
        });
        Self { release, task }
    }

    /// Ends the inhibition and waits until the blocking task has finished.
    ///
    /// Dropping the inhibitor ends it too, without waiting.
    ///
    /// # Panics
    ///
    /// Panics when the blocking task cannot be joined, which means it panicked
    /// or the runtime is shutting down.
    pub async fn release(self) {
        let Self { release, task } = self;
        drop(release);
        task.await.expect("auto-advance inhibitor task joins");
    }
}

impl Default for AutoAdvanceInhibitor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use tokio::time::Instant;

    use super::*;

    /// Virtual duration of the timer that is pending while the test awaits I/O.
    /// Shorter than [`HANG_GUARD`], so the nearest timer is never a guard.
    const PENDING_TIMER: Duration = Duration::from_secs(10);

    /// Real time the I/O thread waits for the pending timer to fire before it
    /// completes the I/O anyway. Only the held-inhibitor test relies on it, and
    /// only to give an erroneous auto-advance time to show; a longer or shorter
    /// wait cannot make that test fail, it can only weaken it.
    const IO_GRACE: Duration = Duration::from_millis(100);

    /// Real ceiling of the self-ending inhibitor test.
    const SHORT_CEILING: Duration = Duration::from_millis(20);

    /// How far the paused clock moved while the test awaited real I/O with a
    /// timer pending.
    ///
    /// A std thread completes the I/O once the pending timer has fired, or
    /// after `grace` of real time when it never does. Awaiting a oneshot has no
    /// timer of its own, so the only timer the idle runtime can advance to is
    /// [`PENDING_TIMER`].
    async fn clock_movement_during_io(grace: Duration) -> Duration {
        let start = Instant::now();
        let (fired, timer_fired) = channel::<()>();
        let sleeper = tokio::spawn(async move {
            tokio::time::sleep(PENDING_TIMER).await;
            let _ = fired.send(());
        });
        let (io_done, io) = tokio::sync::oneshot::channel::<()>();
        std::thread::spawn(move || {
            let _ = timer_fired.recv_timeout(grace);
            let _ = io_done.send(());
        });
        io.await.expect("the I/O thread completes the I/O");
        let moved = start.elapsed();
        sleeper.abort();
        moved
    }

    #[tokio::test(start_paused = true)]
    async fn clock_stays_put_while_the_inhibitor_is_held() {
        let inhibitor = AutoAdvanceInhibitor::new();
        let moved = clock_movement_during_io(IO_GRACE).await;
        assert_eq!(moved, Duration::ZERO);
        inhibitor.release().await;
    }

    #[tokio::test(start_paused = true)]
    async fn clock_auto_advances_without_an_inhibitor() {
        let moved = clock_movement_during_io(HANG_GUARD).await;
        assert!(moved >= PENDING_TIMER, "{moved:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn auto_advance_resumes_after_release() {
        let inhibitor = AutoAdvanceInhibitor::new();
        assert_eq!(clock_movement_during_io(IO_GRACE).await, Duration::ZERO);
        inhibitor.release().await;
        let moved = clock_movement_during_io(HANG_GUARD).await;
        assert!(moved >= PENDING_TIMER, "{moved:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn auto_advance_resumes_after_drop() {
        let inhibitor = AutoAdvanceInhibitor::new();
        assert_eq!(clock_movement_during_io(IO_GRACE).await, Duration::ZERO);
        drop(inhibitor);
        let moved = clock_movement_during_io(HANG_GUARD).await;
        assert!(moved >= PENDING_TIMER, "{moved:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_inhibition_ends_on_its_own_at_the_ceiling() {
        let _inhibitor = AutoAdvanceInhibitor::within(SHORT_CEILING);
        let moved = clock_movement_during_io(HANG_GUARD).await;
        assert!(moved >= PENDING_TIMER, "{moved:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_advance_moves_the_clock_while_the_inhibitor_is_held() {
        let inhibitor = AutoAdvanceInhibitor::new();
        let start = Instant::now();
        let timer = tokio::spawn(tokio::time::sleep(PENDING_TIMER));
        tokio::time::advance(PENDING_TIMER + TIMER_TICK).await;
        timer
            .await
            .expect("the timer fires once the clock is advanced");
        assert_eq!(start.elapsed(), PENDING_TIMER + TIMER_TICK);
        inhibitor.release().await;
    }
}
