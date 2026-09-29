//! Throwaway CI demonstration of the flaky-test policy; never merge.
//!
//! Fails on the first nextest attempt and passes on the retry, so the heavy
//! shard reports it as FLAKY and the run still fails (`flaky-result = "fail"`).

/// Environment variable nextest sets to the 1-based attempt number.
const ATTEMPT_VAR: &str = "NEXTEST_ATTEMPT";

#[test]
fn flaky_demo_fails_first_attempt() {
    let attempt: u32 = std::env::var(ATTEMPT_VAR)
        .expect("run under cargo nextest, which sets NEXTEST_ATTEMPT")
        .parse()
        .expect("NEXTEST_ATTEMPT is a positive integer");
    assert!(attempt > 1, "deliberate failure on attempt {attempt}");
}
