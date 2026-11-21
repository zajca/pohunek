//! Which daemon unit settings keep a daemon from following the service
//! manager's `PATH`: any setting that can decide the job's `PATH` does.

// Rust guideline compliant 2026-11-21

use pohunek_platform::supervisor::UnitEnvironment;

fn loaded() -> UnitEnvironment {
    UnitEnvironment {
        load_state: "loaded".to_owned(),
        ..UnitEnvironment::default()
    }
}

#[test]
fn only_a_unit_that_cannot_decide_its_path_follows_the_manager() {
    let inheriting = loaded();
    assert!(!inheriting.may_set_search_path());

    let unrelated = UnitEnvironment {
        environment: vec!["PATHEXT=/x".to_owned(), "CLAUDE_CONFIG_DIR=/c".to_owned()],
        pass_environment: vec!["HOME".to_owned()],
        unset_environment: vec!["OTHER".to_owned()],
        ..loaded()
    };
    assert!(!unrelated.may_set_search_path());

    let deciding: [(&str, UnitEnvironment); 6] = [
        (
            "Environment=PATH=",
            UnitEnvironment {
                environment: vec!["PATH=/opt/bin".to_owned()],
                ..loaded()
            },
        ),
        (
            "ExecSearchPath=",
            UnitEnvironment {
                exec_search_path: vec!["/opt/bin".to_owned()],
                ..loaded()
            },
        ),
        (
            "EnvironmentFile=",
            UnitEnvironment {
                environment_files: vec![("/etc/pohunek.env".to_owned(), true)],
                ..loaded()
            },
        ),
        (
            "PassEnvironment=PATH",
            UnitEnvironment {
                pass_environment: vec!["PATH".to_owned()],
                ..loaded()
            },
        ),
        (
            "UnsetEnvironment=PATH",
            UnitEnvironment {
                unset_environment: vec!["PATH".to_owned()],
                ..loaded()
            },
        ),
        (
            "properties stale until a daemon-reload",
            UnitEnvironment {
                need_daemon_reload: true,
                ..loaded()
            },
        ),
    ];
    for (setting, unit) in deciding {
        assert!(unit.may_set_search_path(), "{setting}");
    }
}
