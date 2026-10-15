//! The daemon and the session worker take their default PTY stop grace and
//! post-kill reap deadline from the single definitions in
//! `pohunek-service-config`.

use pohunek_daemon::session::SessionRegistryConfig;
use pohunek_service_config::{DEFAULT_KILL_REAP_DEADLINE, DEFAULT_STOP_GRACE};
use pohunek_session_worker::WorkerConfig;
use pohunek_worker_protocol::StopPolicy;

#[test]
fn daemon_and_worker_default_to_the_service_config_stop_grace() {
    let daemon = SessionRegistryConfig::default().stop_grace;
    let worker = WorkerConfig::new().stop_grace;

    assert_eq!(daemon, DEFAULT_STOP_GRACE);
    assert_eq!(worker, DEFAULT_STOP_GRACE);
    assert_eq!(daemon, worker);
    assert_eq!(
        SessionRegistryConfig::default().kill_reap_deadline,
        DEFAULT_KILL_REAP_DEADLINE
    );
    assert_eq!(
        WorkerConfig::new().kill_reap_deadline,
        DEFAULT_KILL_REAP_DEADLINE
    );
}

#[test]
fn the_shared_default_is_a_valid_worker_policy_and_wire_stop_policy() {
    WorkerConfig::new()
        .validate()
        .expect("the production worker policy is valid");
    let grace_ms = u64::try_from(DEFAULT_STOP_GRACE.as_millis()).expect("grace fits the wire");
    let kill_reap_ms =
        u64::try_from(DEFAULT_KILL_REAP_DEADLINE.as_millis()).expect("deadline fits the wire");
    let policy = StopPolicy::with_kill_reap(grace_ms, kill_reap_ms)
        .expect("the daemon can send the default grace and reap deadline");
    assert_eq!(policy.grace_ms(), grace_ms);
    assert_eq!(policy.kill_reap_ms(), Some(kill_reap_ms));
}
