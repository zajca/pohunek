//! Decides which cgroup placements attest that a process did not descend from a
//! worker.
//!
//! The comparison is pure: it reads only the two paths, so the scenarios use
//! the shapes systemd gives a user manager, a sessions slice and worker units.
#![cfg(target_os = "linux")]

// Rust guideline compliant 2026-11-02

use pohunek_platform::process::ControlGroupPath;

const UID: u32 = 1000;
const NAMESPACE: &str = "7dfe7c984626";
const OTHER_NAMESPACE: &str = "0123456789ab";
const MANAGER: &str = "/user.slice/user-1000.slice/user@1000.service";

fn sessions_slice(namespace: &str) -> String {
    format!("{MANAGER}/pohunek.slice/pohunek-{namespace}.slice/pohunek-{namespace}-sessions.slice")
}

fn worker_unit(namespace: &str, session: &str, generation: &str) -> String {
    format!("pohunek-{namespace}-worker-{session}-{generation}.service")
}

fn worker() -> ControlGroupPath {
    path(&format!(
        "{}/{}",
        sessions_slice(NAMESPACE),
        worker_unit(NAMESPACE, "s-01M6CAD3DJMDPMBMTN6BPV077S", "fl2xghzt")
    ))
}

fn path(value: &str) -> ControlGroupPath {
    ControlGroupPath::parse(value).expect("a valid control group")
}

fn foreign(candidate: &str) -> bool {
    path(candidate).outside_worker_scope_of(&worker(), UID)
}

#[test]
fn a_placement_where_no_descendant_lands_by_accident_is_foreign() {
    assert!(foreign("/user.slice/user-1000.slice/session-2.scope"));
    assert!(foreign("/system.slice/docker-abc.scope"));
    assert!(foreign(&format!("{MANAGER}/app.slice/run-p1-i1.scope")));
    assert!(foreign(&format!(
        "{MANAGER}/app.slice/gcr-ssh-agent.service"
    )));
    assert!(foreign(&format!(
        "{MANAGER}/pohunek.slice/pohunek-{NAMESPACE}.slice/other.service"
    )));
    assert!(foreign(&format!(
        "{}/{}",
        sessions_slice(NAMESPACE),
        worker_unit(NAMESPACE, "s-01M6DZVAR9F0A0SB1CVEBFW83Y", "fl2xghzt")
    )));
    assert!(foreign(&format!(
        "{}/{}/child.scope",
        sessions_slice(NAMESPACE),
        worker_unit(NAMESPACE, "s-01M6DZVAR9F0A0SB1CVEBFW83Y", "abcdefgh")
    )));
}

#[test]
fn a_placement_a_descendant_of_the_worker_can_reach_by_accident_is_unproven() {
    let slice = sessions_slice(NAMESPACE);
    let own = worker_unit(NAMESPACE, "s-01M6CAD3DJMDPMBMTN6BPV077S", "fl2xghzt");
    assert!(!foreign(&format!("{slice}/{own}")));
    assert!(!foreign(&format!("{slice}/{own}/child.scope")));
    assert!(!foreign(&format!(
        "{slice}/{}",
        worker_unit(NAMESPACE, "s-01M6CAD3DJMDPMBMTN6BPV077S", "grjy46wf")
    )));
    assert!(!foreign(&slice));
    assert!(!foreign(&format!("{slice}/tmux-spawn-1234.scope")));
    assert!(!foreign(&format!(
        "{slice}/{}",
        worker_unit(OTHER_NAMESPACE, "s-01M6DZVAR9F0A0SB1CVEBFW83Y", "fl2xghzt")
    )));
}

#[test]
fn a_look_alike_name_is_not_matched_as_a_worker_unit() {
    let slice = sessions_slice(NAMESPACE);
    assert!(!foreign(&format!(
        "{slice}/{}2",
        worker_unit(NAMESPACE, "s-01M6DZVAR9F0A0SB1CVEBFW83Y", "fl2xghzt")
    )));
    assert!(!foreign(&format!(
        "{slice}/pohunek-{NAMESPACE}-worker-.service"
    )));
}

#[test]
fn a_worker_path_that_is_not_a_unit_of_a_sessions_slice_proves_nothing_inside_the_manager() {
    let candidate = path(&format!("{MANAGER}/app.slice/run-p1-i1.scope"));
    let outside = path("/user.slice/user-1000.slice/session-2.scope");
    let unit = worker_unit(NAMESPACE, "s-01M6CAD3DJMDPMBMTN6BPV077S", "fl2xghzt");
    let shapes = [
        // No user manager segment.
        format!("/system.slice/{unit}"),
        // Not a worker unit leaf.
        format!("{}/other.service", sessions_slice(NAMESPACE)),
        // Parent is not the namespace's sessions slice.
        format!("{MANAGER}/app.slice/{unit}"),
        // Sessions slice of another namespace.
        format!("{}/{unit}", sessions_slice(OTHER_NAMESPACE)),
        // The manager itself is the parent.
        format!("{MANAGER}/{unit}"),
    ];
    for shape in shapes {
        let worker = path(&shape);
        assert!(
            !candidate.outside_worker_scope_of(&worker, UID),
            "inside the manager, worker {shape}"
        );
        assert_eq!(
            outside.outside_worker_scope_of(&worker, UID),
            shape.contains("/user@1000.service"),
            "outside the manager, worker {shape}"
        );
    }
}
