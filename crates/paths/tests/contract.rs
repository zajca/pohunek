use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use pohunek_paths::{
    validate_socket_path, BasePaths, InvalidPathReason, PathEnv, PathError, Platform, SocketKind,
};
use serde::Deserialize;

// Rust guideline compliant 2026-09-19

const CONTRACT: &str = include_str!("../fixtures/runtime-paths.json");

#[derive(Debug, Deserialize)]
struct Fixture {
    version: u32,
    session_id: String,
    worker_id: String,
    assistant_id: String,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    platform: String,
    effective_uid: u32,
    env: BTreeMap<String, String>,
    expected: Option<Expected>,
    expected_runtime_dir: Option<String>,
    error: Option<ExpectedError>,
}

#[derive(Debug, Deserialize)]
struct Expected {
    runtime_dir: String,
    socket: String,
    lock: String,
    log_dir: String,
    state_dir: String,
    data_dir: String,
    cache_dir: String,
    config_home: String,
    config_dir: String,
    launcher_bin_dir: String,
    sway_config_dir: String,
    assistant_bundle_cache_dir: String,
    assistant_runtime_dir: String,
    worker_runtime_root: String,
    worker_state_root: String,
    worker_socket: String,
    worker_journal: String,
    host_state_dir: String,
    host_identity_path: String,
    host_approval_key_path: String,
    host_governance_path: String,
    host_state_lock_path: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedError {
    variant: String,
    var: String,
    reason: Option<String>,
}

#[test]
fn shared_fixture_matches_rust_contract() {
    let fixture: Fixture = serde_json::from_str(CONTRACT).expect("valid runtime path fixture");
    assert_eq!(fixture.version, 1);

    for case in &fixture.cases {
        let platform = fixture_platform(&case.platform);
        let env = fixture_env(&case.env);
        let result = BasePaths::resolve_for(platform, case.effective_uid, &env);
        match (&case.expected, &case.expected_runtime_dir, &case.error) {
            (Some(expected), None, None) => {
                let paths = result.unwrap_or_else(|error| panic!("{}: {error}", case.name));
                assert_complete_paths(&paths, expected, &fixture);
            }
            (None, Some(expected), None) => assert_eq!(
                result
                    .unwrap_or_else(|error| panic!("{}: {error}", case.name))
                    .runtime_dir,
                PathBuf::from(expected),
                "{}",
                case.name
            ),
            (None, None, Some(expected)) => assert_fixture_error(
                result.expect_err(&format!("{} must fail", case.name)),
                expected,
                &case.name,
            ),
            _ => panic!("{} has an invalid result shape", case.name),
        }
    }
}

#[test]
fn socket_limits_reserve_the_native_terminator() {
    for platform in [Platform::Linux, Platform::MacOs] {
        let limit = platform.socket_path_max_bytes();
        let boundary = PathBuf::from(format!("/{}", "a".repeat(limit - 1)));
        validate_socket_path(&boundary, platform, SocketKind::Auxiliary)
            .expect("boundary-sized socket path");

        let overlong = PathBuf::from(format!("/{}", "a".repeat(limit)));
        assert!(matches!(
            validate_socket_path(&overlong, platform, SocketKind::Auxiliary),
            Err(PathError::SocketPathTooLong {
                kind: SocketKind::Auxiliary,
                actual_bytes,
                max_bytes,
                ..
            }) if actual_bytes == limit + 1 && max_bytes == limit
        ));
    }
}

#[test]
fn auxiliary_socket_paths_must_be_absolute_and_normalized() {
    assert!(matches!(
        validate_socket_path(
            "relative/socket.sock",
            Platform::Linux,
            SocketKind::Auxiliary
        ),
        Err(PathError::SocketPathNotAbsolute {
            kind: SocketKind::Auxiliary,
            ..
        })
    ));
    assert!(matches!(
        validate_socket_path(
            "/tmp/parent/../socket.sock",
            Platform::Linux,
            SocketKind::Auxiliary
        ),
        Err(PathError::SocketPathParentComponent {
            kind: SocketKind::Auxiliary,
            ..
        })
    ));
}

#[test]
fn socket_limits_count_multibyte_unicode_bytes() {
    let path = PathBuf::from(format!("/{}", "é".repeat(52)));
    assert_eq!(path.as_os_str().len(), 105);
    assert!(matches!(
        validate_socket_path(&path, Platform::MacOs, SocketKind::Auxiliary),
        Err(PathError::SocketPathTooLong {
            actual_bytes: 105,
            max_bytes: 103,
            ..
        })
    ));
    validate_socket_path(&path, Platform::Linux, SocketKind::Auxiliary)
        .expect("same encoded path fits Linux");
}

#[test]
fn worker_socket_is_validated_after_safe_id_derivation() {
    let suffix_len = Platform::MacOs.socket_path_max_bytes() - "/pohunek/daemon.sock".len();
    let runtime_base = format!("/{}", "r".repeat(suffix_len - 1));
    let env = PathEnv {
        xdg_runtime_dir: Some(runtime_base.into()),
        xdg_data_home: Some("/data".into()),
        xdg_state_home: Some("/state".into()),
        xdg_cache_home: Some("/cache".into()),
        xdg_config_home: Some("/config".into()),
        home: None,
    };
    let paths = BasePaths::resolve_for(Platform::MacOs, 501, &env).expect("daemon socket fits");

    assert!(matches!(
        paths.worker_socket("s-42"),
        Err(PathError::SocketPathTooLong {
            kind: SocketKind::Worker,
            ..
        })
    ));
    assert_eq!(paths.worker_socket("../unsafe").expect("invalid ID"), None);
}

#[test]
fn daemon_socket_is_validated_during_resolution() {
    let env = PathEnv {
        xdg_runtime_dir: Some(format!("/{}", "r".repeat(100)).into()),
        xdg_data_home: Some("/data".into()),
        xdg_state_home: Some("/state".into()),
        xdg_cache_home: Some("/cache".into()),
        xdg_config_home: Some("/config".into()),
        home: None,
    };

    assert!(matches!(
        BasePaths::resolve_for(Platform::MacOs, 501, &env),
        Err(PathError::SocketPathTooLong {
            kind: SocketKind::Daemon,
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn socket_and_environment_paths_reject_nul_bytes() {
    use std::os::unix::ffi::OsStringExt as _;

    let socket = PathBuf::from(OsString::from_vec(b"/tmp/bad\0socket".to_vec()));
    assert!(matches!(
        validate_socket_path(&socket, Platform::Linux, SocketKind::Auxiliary),
        Err(PathError::SocketPathContainsNul {
            kind: SocketKind::Auxiliary,
            ..
        })
    ));

    let env = PathEnv {
        xdg_runtime_dir: Some(OsString::from_vec(b"/tmp/bad\0runtime".to_vec())),
        xdg_data_home: Some("/data".into()),
        xdg_state_home: Some("/state".into()),
        xdg_cache_home: Some("/cache".into()),
        xdg_config_home: Some("/config".into()),
        home: None,
    };
    assert!(matches!(
        BasePaths::resolve_for(Platform::Linux, 1000, &env),
        Err(PathError::InvalidEnv {
            var,
            reason: InvalidPathReason::ContainsNul,
        }) if var == "XDG_RUNTIME_DIR"
    ));
}

fn fixture_platform(value: &str) -> Platform {
    match value {
        "linux" => Platform::Linux,
        "macos" => Platform::MacOs,
        other => panic!("unknown fixture platform {other}"),
    }
}

fn fixture_env(values: &BTreeMap<String, String>) -> PathEnv {
    let value = |key: &str| values.get(key).map(OsString::from);
    PathEnv {
        xdg_runtime_dir: value("XDG_RUNTIME_DIR"),
        xdg_data_home: value("XDG_DATA_HOME"),
        xdg_state_home: value("XDG_STATE_HOME"),
        xdg_cache_home: value("XDG_CACHE_HOME"),
        xdg_config_home: value("XDG_CONFIG_HOME"),
        home: value("HOME"),
    }
}

fn assert_complete_paths(paths: &BasePaths, expected: &Expected, fixture: &Fixture) {
    assert_path(&paths.runtime_dir, &expected.runtime_dir);
    assert_path(&paths.socket, &expected.socket);
    assert_path(&paths.lock, &expected.lock);
    assert_path(&paths.log_dir, &expected.log_dir);
    assert_path(&paths.state_dir, &expected.state_dir);
    assert_path(&paths.data_dir, &expected.data_dir);
    assert_path(&paths.cache_dir, &expected.cache_dir);
    assert_path(&paths.config_home, &expected.config_home);
    assert_path(&paths.config_dir, &expected.config_dir);
    assert_path(&paths.launcher_bin_dir(), &expected.launcher_bin_dir);
    assert_path(&paths.sway_config_dir(), &expected.sway_config_dir);
    assert_path(
        &paths.assistant_bundle_cache_dir(),
        &expected.assistant_bundle_cache_dir,
    );
    assert_path(
        &paths
            .assistant_runtime_dir(&fixture.assistant_id)
            .expect("fixture assistant ID"),
        &expected.assistant_runtime_dir,
    );
    assert_path(&paths.worker_runtime_root(), &expected.worker_runtime_root);
    assert_path(&paths.worker_state_root(), &expected.worker_state_root);
    assert_path(
        &paths
            .worker_socket(&fixture.session_id)
            .expect("fixture worker socket length")
            .expect("fixture session ID"),
        &expected.worker_socket,
    );
    assert_path(
        &paths
            .worker_journal(&fixture.session_id, &fixture.worker_id)
            .expect("fixture worker IDs"),
        &expected.worker_journal,
    );
    assert_path(&paths.host_state_dir(), &expected.host_state_dir);
    assert_path(&paths.host_identity_path(), &expected.host_identity_path);
    assert_path(
        &paths.host_approval_key_path(),
        &expected.host_approval_key_path,
    );
    assert_path(
        &paths.host_governance_path(),
        &expected.host_governance_path,
    );
    assert_path(
        &paths.host_state_lock_path(),
        &expected.host_state_lock_path,
    );
}

fn assert_path(actual: &Path, expected: &str) {
    assert_eq!(actual, Path::new(expected));
}

fn assert_fixture_error(error: PathError, expected: &ExpectedError, name: &str) {
    match (expected.variant.as_str(), error) {
        ("missing_env", PathError::MissingEnv { var }) => assert_eq!(var, expected.var, "{name}"),
        ("invalid_env", PathError::InvalidEnv { var, reason }) => {
            assert_eq!(var, expected.var, "{name}");
            let expected_reason = match expected.reason.as_deref() {
                Some("empty") => InvalidPathReason::Empty,
                Some("not_absolute") => InvalidPathReason::NotAbsolute,
                Some("parent_component") => InvalidPathReason::ParentComponent,
                other => panic!("{name}: unknown invalid-path reason {other:?}"),
            };
            assert_eq!(reason, expected_reason, "{name}");
        }
        (variant, actual) => panic!("{name}: expected {variant}, got {actual:?}"),
    }
}
