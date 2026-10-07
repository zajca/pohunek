//! Readiness waits bounded by one hang-guard ceiling.
//!
//! A test never asserts a duration against real time. When the deadline is the
//! behavior under test, the test uses virtual time (`tokio::time::pause`,
//! `start_paused`, or an injected clock). When the deadline is incidental, the
//! test waits for a readiness signal (a socket accepting, a journal marker, a
//! state transition observed on the same view the test asserts) and this module
//! bounds that wait.
//!
//! [`HANG_GUARD`] is the single ceiling for every such wait. It is a hang
//! guard, not a timing assertion: it exists so a signal that never arrives
//! fails the test with a message naming what was awaited, instead of the test
//! running until the nextest `slow-timeout` backstop terminates it silently.
//!
//! - [`poll_until`] re-checks a state condition from synchronous tests.
//! - [`wait_until`] is the same poll for async tests.
//! - [`guard`] bounds an arbitrary future, such as a channel receive or a
//!   `Notify::notified` wait, that is already its own readiness signal.
//!
//! Under `tokio::time::pause` the ceiling, the poll interval and the elapsed
//! time the async helpers measure are virtual: they then fail as soon as the
//! runtime is idle for that long.
//! Use paused time only in tests whose awaited signals are timers or in-memory
//! channels, never real sockets or child processes.
//!
//! # Examples
//!
//! ```
//! use std::cell::Cell;
//!
//! let probes = Cell::new(0_u32);
//! let ready = pohunek_test_support::wait::poll_until("third probe", || {
//!     probes.set(probes.get() + 1);
//!     (probes.get() == 3).then_some(probes.get())
//! });
//! assert_eq!(ready, 3);
//! ```

// Rust guideline compliant 2026-10-01

use std::future::Future;
use std::time::{Duration, Instant};

/// Ceiling on any single readiness wait: a hang guard, never a timing assertion.
///
/// Healthy readiness signals arrive within milliseconds to a few seconds even
/// on a loaded CI runner, so this value is two orders of magnitude above them
/// and a wait that reaches it is a hang. It is also below the nextest
/// `terminate-after` of every profile in `.config/nextest.toml` (`default`,
/// `ci`, `fast`, `heavy` and `relay-db` terminate a test after 3 periods of
/// 60 s, 180 s in total), so the panic naming the awaited condition is what the
/// developer sees rather than a nextest termination. The `local` profile
/// only marks tests slow after 30 s and never terminates them.
///
/// The ceiling bounds one wait, not a whole test: a test that spends more than
/// the 60 s margin before a hanging wait is terminated by nextest first. Raising
/// this value to or past the smallest `terminate-after` product removes the
/// diagnostic; the unit test `hang_guard_is_below_every_nextest_termination`
/// enforces that bound against the nextest configuration.
pub const HANG_GUARD: Duration = Duration::from_secs(120);

/// Interval between probes of [`poll_until`] and [`wait_until`].
///
/// A readiness poll, not a duration check: short enough that a condition that
/// is already near-ready adds little latency, long enough that a probe doing
/// file or process I/O does not saturate a loaded runner.
pub const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Re-checks `probe` until it yields a value, then returns that value.
///
/// `what` names the awaited condition in the panic message. Express a boolean
/// condition as `condition.then_some(())`.
///
/// # Panics
///
/// Panics, naming `what` and the elapsed time, when [`HANG_GUARD`] has elapsed
/// before `probe` yields a value. A value from a probe call that returns after
/// the ceiling is discarded: reaching the ceiling is a hang, as it is for
/// [`wait_until`] and [`guard`].
///
/// # Examples
///
/// ```
/// let marker = pohunek_test_support::wait::poll_until("marker", || Some("ready"));
/// assert_eq!(marker, "ready");
/// ```
#[track_caller]
pub fn poll_until<T>(what: &str, probe: impl FnMut() -> Option<T>) -> T {
    poll_until_within(HANG_GUARD, POLL_INTERVAL, what, probe)
}

/// [`poll_until`] with an explicit ceiling and interval.
#[track_caller]
fn poll_until_within<T>(
    ceiling: Duration,
    interval: Duration,
    what: &str,
    mut probe: impl FnMut() -> Option<T>,
) -> T {
    let start = Instant::now();
    loop {
        let found = probe();
        let elapsed = start.elapsed();
        assert!(
            elapsed < ceiling,
            "hang guard of {ceiling:?} elapsed after {elapsed:?} waiting for {what}"
        );
        if let Some(value) = found {
            return value;
        }
        std::thread::sleep(interval.min(ceiling.saturating_sub(elapsed)));
    }
}

/// Re-checks the async `probe` until it yields a value, then returns that value.
///
/// The whole loop, including every `probe` future, is bounded by [`HANG_GUARD`],
/// so a probe that itself never completes also fails the wait.
///
/// # Panics
///
/// Panics, naming `what` and the elapsed time, when [`HANG_GUARD`] has elapsed
/// before `probe` yields a value, including when a single poll blocks past the
/// ceiling and then returns a value.
///
/// # Examples
///
/// ```
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// let marker = pohunek_test_support::wait::wait_until("marker", || async { Some("ready") }).await;
/// assert_eq!(marker, "ready");
/// # }
/// ```
pub async fn wait_until<T, F, Fut>(what: &str, probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    wait_until_within(HANG_GUARD, POLL_INTERVAL, what, probe).await
}

/// [`wait_until`] with an explicit ceiling and interval.
async fn wait_until_within<T, F, Fut>(
    ceiling: Duration,
    interval: Duration,
    what: &str,
    mut probe: F,
) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    guard_within(ceiling, what, async {
        loop {
            if let Some(value) = probe().await {
                return value;
            }
            tokio::time::sleep(interval).await;
        }
    })
    .await
}

/// Bounds `future` by [`HANG_GUARD`] and returns its output.
///
/// Use it around a wait that is already a readiness signal, such as a channel
/// receive, an accept, or a `Notify::notified` call.
///
/// # Panics
///
/// Panics, naming `what` and the elapsed time, when [`HANG_GUARD`] has elapsed
/// before `future` completes, including when a single poll blocks past the
/// ceiling and then completes.
///
/// # Examples
///
/// ```
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// let (tx, rx) = tokio::sync::oneshot::channel();
/// tx.send(7).unwrap();
/// let value = pohunek_test_support::wait::guard("the reply", rx).await.unwrap();
/// assert_eq!(value, 7);
/// # }
/// ```
pub async fn guard<F: Future>(what: &str, future: F) -> F::Output {
    guard_within(HANG_GUARD, what, future).await
}

/// [`guard`] with an explicit ceiling.
///
/// `tokio::time::timeout` polls the future before it checks the timer, so a
/// poll that blocks past the ceiling and then completes would be accepted; the
/// elapsed time is therefore checked again after completion.
async fn guard_within<F: Future>(ceiling: Duration, what: &str, future: F) -> F::Output {
    let start = tokio::time::Instant::now();
    let outcome = tokio::time::timeout(ceiling, future).await;
    let elapsed = start.elapsed();
    match outcome {
        Ok(output) if elapsed < ceiling => output,
        _ => panic!("hang guard of {ceiling:?} elapsed after {elapsed:?} waiting for {what}"),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// Ceiling for the panic-path tests; real time, so kept to a few milliseconds.
    const TINY_CEILING: Duration = Duration::from_millis(30);

    /// Interval for the panic-path tests, below [`TINY_CEILING`].
    const TINY_INTERVAL: Duration = Duration::from_millis(5);

    /// Virtual ceiling for the async tests under paused time.
    const VIRTUAL_CEILING: Duration = Duration::from_secs(10);

    /// Virtual interval for the async tests under paused time.
    const VIRTUAL_INTERVAL: Duration = Duration::from_secs(1);

    #[test]
    fn poll_until_returns_once_the_condition_becomes_ready() {
        let probes = Cell::new(0_u32);
        let value = poll_until_within(HANG_GUARD, TINY_INTERVAL, "fourth probe", || {
            probes.set(probes.get() + 1);
            (probes.get() == 4).then_some(probes.get())
        });
        assert_eq!(value, 4);
        assert_eq!(probes.get(), 4);
    }

    #[test]
    #[should_panic(expected = "waiting for the never-ready widget")]
    fn poll_until_panic_names_the_awaited_condition() {
        poll_until_within(
            TINY_CEILING,
            TINY_INTERVAL,
            "the never-ready widget",
            || None::<()>,
        );
    }

    #[test]
    #[should_panic(expected = "hang guard of 30ms elapsed after")]
    fn poll_until_rejects_a_condition_that_becomes_ready_only_after_the_ceiling() {
        let first_probe = Cell::new(None::<Instant>);
        poll_until_within(TINY_CEILING, TINY_INTERVAL, "a late condition", || {
            let first = first_probe.get().unwrap_or_else(Instant::now);
            first_probe.set(Some(first));
            (first.elapsed() >= TINY_CEILING).then_some(())
        });
    }

    #[test]
    #[should_panic(expected = "waiting for a probe that blocks")]
    fn poll_until_rejects_a_single_probe_that_returns_after_the_ceiling() {
        poll_until_within(TINY_CEILING, TINY_INTERVAL, "a probe that blocks", || {
            // timing-allowed: #362 blocks one poll past the ceiling on purpose to test the overrun check
            std::thread::sleep(TINY_CEILING * 2);
            Some(())
        });
    }

    #[tokio::test(start_paused = true)]
    async fn wait_until_returns_once_the_condition_becomes_ready() {
        let probes = Cell::new(0_u32);
        let value = wait_until_within(VIRTUAL_CEILING, VIRTUAL_INTERVAL, "third probe", || {
            probes.set(probes.get() + 1);
            let probe = probes.get();
            async move { (probe == 3).then_some(probe) }
        })
        .await;
        assert_eq!(value, 3);
        assert_eq!(probes.get(), 3);
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "waiting for the never-ready socket")]
    async fn wait_until_panic_names_the_awaited_condition() {
        wait_until_within(
            VIRTUAL_CEILING,
            VIRTUAL_INTERVAL,
            "the never-ready socket",
            || async { None::<()> },
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "waiting for a probe that never completes")]
    async fn wait_until_bounds_a_probe_that_never_completes() {
        wait_until_within(
            VIRTUAL_CEILING,
            VIRTUAL_INTERVAL,
            "a probe that never completes",
            std::future::pending::<Option<()>>,
        )
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn guard_returns_the_output_of_a_future_that_completes_before_the_ceiling() {
        let value = guard_within(VIRTUAL_CEILING, "a delayed future", async {
            tokio::time::sleep(VIRTUAL_INTERVAL).await;
            9_u8
        })
        .await;
        assert_eq!(value, 9);
    }

    #[tokio::test]
    #[should_panic(expected = "hang guard of 30ms elapsed after")]
    async fn guard_rejects_a_future_whose_single_poll_blocks_past_the_ceiling() {
        guard_within(TINY_CEILING, "a blocking future", async {
            // timing-allowed: #362 blocks one poll past the ceiling on purpose to test the overrun check
            std::thread::sleep(TINY_CEILING * 2);
        })
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "waiting for a blocking probe")]
    async fn wait_until_rejects_a_probe_whose_single_poll_blocks_past_the_ceiling() {
        wait_until_within(TINY_CEILING, TINY_INTERVAL, "a blocking probe", || async {
            // timing-allowed: #362 blocks one poll past the ceiling on purpose to test the overrun check
            std::thread::sleep(TINY_CEILING * 2);
            Some(())
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "hang guard of 10s elapsed after 10s waiting for the stuck reply")]
    async fn guard_panic_names_the_awaited_future_and_ceiling() {
        guard_within(
            VIRTUAL_CEILING,
            "the stuck reply",
            std::future::pending::<()>(),
        )
        .await;
    }

    #[test]
    fn hang_guard_is_below_every_nextest_termination() {
        let path = crate::workspace_root().join(".config/nextest.toml");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let terminations = terminations_in(&text);
        assert!(
            !terminations.is_empty(),
            "{} defines no `terminate-after`; the hang guard has no backstop to stay below",
            path.display()
        );
        for (location, total) in terminations {
            assert!(
                HANG_GUARD < total,
                "HANG_GUARD {HANG_GUARD:?} is not below the {total:?} termination of {location} in {}",
                path.display()
            );
        }
    }

    #[test]
    fn terminations_in_multiplies_period_by_count() {
        let text = r#"
            [profile.a]
            slow-timeout = { period = "60s", terminate-after = 3 }
            [profile.b]
            slow-timeout = { period = "30s" }
            [profile.c]
            slow-timeout = "45s"
        "#;
        assert_eq!(
            terminations_in(text),
            [(
                "profile.a.slow-timeout".to_owned(),
                Duration::from_secs(180)
            )]
        );
    }

    #[test]
    fn terminations_in_covers_override_entries() {
        let text = r#"
            [profile.a]
            slow-timeout = { period = "10s" }
            [[profile.a.overrides]]
            filter = "test(x)"
            slow-timeout = { period = "2m", terminate-after = 2 }
            [[profile.a.overrides]]
            filter = "test(y)"
            slow-timeout = { period = "5s", terminate-after = 4 }
        "#;
        assert_eq!(
            terminations_in(text),
            [
                (
                    "profile.a.overrides[0].slow-timeout".to_owned(),
                    Duration::from_secs(240)
                ),
                (
                    "profile.a.overrides[1].slow-timeout".to_owned(),
                    Duration::from_secs(20)
                ),
            ]
        );
    }

    #[test]
    #[should_panic(expected = "profile.a.slow-timeout")]
    fn terminations_in_rejects_an_unrecognized_period() {
        terminations_in("[profile.a]\nslow-timeout = { period = \"1h\", terminate-after = 3 }\n");
    }

    #[test]
    #[should_panic(expected = "profile.a.slow-timeout")]
    fn terminations_in_rejects_a_non_integer_count() {
        terminations_in(
            "[profile.a]\nslow-timeout = { period = \"60s\", terminate-after = \"3\" }\n",
        );
    }

    /// Returns the location and total runtime (`period * terminate-after`) of
    /// every `profile.<name>.slow-timeout` and
    /// `profile.<name>.overrides[<i>].slow-timeout` table that sets
    /// `terminate-after`.
    ///
    /// Panics on a TOML syntax error or on a `terminate-after` it cannot
    /// interpret, so an unrecognized form fails the test instead of being skipped.
    fn terminations_in(text: &str) -> Vec<(String, Duration)> {
        let config: toml::Table = text
            .parse()
            .unwrap_or_else(|error| panic!("nextest configuration is not valid TOML: {error}"));
        let mut found = Vec::new();
        let profiles = config.get("profile").and_then(toml::Value::as_table);
        for (name, profile) in profiles.into_iter().flatten() {
            let Some(profile) = profile.as_table() else {
                continue;
            };
            let location = format!("profile.{name}.slow-timeout");
            found.extend(termination_of(&location, profile.get("slow-timeout")));
            let overrides = profile.get("overrides").and_then(toml::Value::as_array);
            for (index, entry) in overrides.into_iter().flatten().enumerate() {
                let location = format!("profile.{name}.overrides[{index}].slow-timeout");
                found.extend(termination_of(&location, entry.get("slow-timeout")));
            }
        }
        found
    }

    /// Returns the termination of one `slow-timeout` value, or `None` when it
    /// sets no `terminate-after`.
    fn termination_of(
        location: &str,
        slow_timeout: Option<&toml::Value>,
    ) -> Option<(String, Duration)> {
        let table = slow_timeout?.as_table()?;
        let count = table.get("terminate-after")?;
        let count = count
            .as_integer()
            .and_then(|count| u32::try_from(count).ok())
            .unwrap_or_else(|| {
                panic!("cannot parse `terminate-after` of {location}: {count} is not a count")
            });
        let period = table
            .get("period")
            .and_then(toml::Value::as_str)
            .and_then(period_of)
            .unwrap_or_else(|| {
                panic!(
                    "cannot parse `period` of {location}: expected `<N>s` or `<N>m`, got {:?}",
                    table.get("period")
                )
            });
        Some((location.to_owned(), period * count))
    }

    /// Parses a nextest period of the form `<N>s` or `<N>m`.
    fn period_of(text: &str) -> Option<Duration> {
        let (digits, unit) = text.split_at(text.len().checked_sub(1)?);
        let amount: u64 = digits.parse().ok()?;
        match unit {
            "s" => Some(Duration::from_secs(amount)),
            "m" => Some(Duration::from_secs(amount.checked_mul(60)?)),
            _ => None,
        }
    }
}
