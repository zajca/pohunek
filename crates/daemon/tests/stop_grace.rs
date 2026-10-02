//! The daemon and the session worker take their default PTY stop grace from the
//! single definition in `pohunek-service-config`.

use pohunek_daemon::session::SessionRegistryConfig;
use pohunek_service_config::DEFAULT_STOP_GRACE;
use pohunek_session_worker::WorkerConfig;
use pohunek_worker_protocol::StopPolicy;

#[test]
fn daemon_and_worker_default_to_the_service_config_stop_grace() {
    let daemon = SessionRegistryConfig::default().stop_grace;
    let worker = WorkerConfig::new().stop_grace;

    assert_eq!(daemon, DEFAULT_STOP_GRACE);
    assert_eq!(worker, DEFAULT_STOP_GRACE);
    assert_eq!(daemon, worker);
}

#[test]
fn the_shared_default_is_a_valid_worker_policy_and_wire_stop_policy() {
    WorkerConfig::new()
        .validate()
        .expect("the production worker policy is valid");
    let grace_ms = u64::try_from(DEFAULT_STOP_GRACE.as_millis()).expect("grace fits the wire");
    let policy = StopPolicy::new(grace_ms).expect("the daemon can send the default grace");
    assert_eq!(policy.grace_ms(), grace_ms);
}
