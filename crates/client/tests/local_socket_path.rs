//! `local_socket_path_for` resolves the daemon socket the way the shared path
//! contract says and refuses a runtime directory or socket that is not
//! private to the caller.
//!
//! Resolution failures are driven by the shared `runtime-paths.json` fixture.
//! Filesystem checks run on a private temporary runtime directory, never on the
//! fixture's absolute paths.

// Rust guideline compliant 2026-11-26

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use pohunek_client::{
    local_socket_path_for, Client, ClientError, ClientOptions, OriginSource, PathEnv, PathError,
    Platform, UnsafeEndpointReason,
};
use serde::Deserialize;
use tokio::net::UnixListener;

const CONTRACT: &str = include_str!("../../paths/fixtures/runtime-paths.json");

/// Directory the daemon creates below `XDG_RUNTIME_DIR`.
const APP_DIR: &str = "pohunek";

/// Socket file name inside the runtime directory.
const SOCKET_NAME: &str = "daemon.sock";

/// Mode of the runtime directory the daemon creates.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Mode of the socket the daemon binds.
const PRIVATE_SOCKET_MODE: u32 = 0o600;

/// Mode that grants group and other access.
const OPEN_MODE: u32 = 0o777;

#[derive(Debug, Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    platform: String,
    effective_uid: u32,
    env: BTreeMap<String, String>,
    error: Option<FixtureError>,
}

#[derive(Debug, Deserialize)]
struct FixtureError {
    var: String,
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

fn runtime_env(runtime_root: &Path) -> PathEnv {
    PathEnv {
        xdg_runtime_dir: Some(runtime_root.as_os_str().to_owned()),
        ..PathEnv::default()
    }
}

fn effective_uid(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("inspect the test root")
        .uid()
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("set the mode");
}

/// A private runtime root holding the daemon's runtime directory and socket.
struct Runtime {
    root: PathBuf,
    uid: u32,
    _guard: Box<dyn std::any::Any>,
}

impl Runtime {
    fn new() -> Self {
        let guard = pohunek_test_support::tempdir_with_prefix("ph-sock-")
            .expect("create the private runtime root");
        let root = guard.path().to_path_buf();
        let uid = effective_uid(&root);
        Self {
            root,
            uid,
            _guard: Box::new(guard),
        }
    }

    fn dir(&self) -> PathBuf {
        self.root.join(APP_DIR)
    }

    fn socket(&self) -> PathBuf {
        self.dir().join(SOCKET_NAME)
    }

    fn resolve(&self) -> Result<PathBuf, ClientError> {
        local_socket_path_for(Platform::Linux, self.uid, &runtime_env(&self.root))
    }

    /// Creates the private runtime directory the daemon makes.
    fn create_dir(&self) {
        std::fs::create_dir(self.dir()).expect("create the runtime directory");
        set_mode(&self.dir(), PRIVATE_DIR_MODE);
    }

    /// Binds a socket with the mode the daemon gives it.
    fn bind_socket(&self) -> UnixListener {
        let listener = UnixListener::bind(self.socket()).expect("bind the daemon socket");
        set_mode(&self.socket(), PRIVATE_SOCKET_MODE);
        listener
    }
}

fn unsafe_reason(error: &ClientError) -> UnsafeEndpointReason {
    match error {
        ClientError::LocalSocketUnsafe {
            source: PathError::UnsafeSocketEndpoint { reason, .. },
        } => *reason,
        other => panic!("expected LocalSocketUnsafe, got {other:?}"),
    }
}

#[test]
fn every_runtime_directory_failure_of_the_shared_fixture_is_a_typed_error() {
    let fixture: Fixture = serde_json::from_str(CONTRACT).expect("valid runtime path fixture");
    let mut checked = 0;
    for case in &fixture.cases {
        // The socket resolver reads only the runtime directory variable, so the
        // fixture's data, state and config failures do not concern it.
        let Some(error) = case.error.as_ref().filter(|e| e.var == "XDG_RUNTIME_DIR") else {
            continue;
        };
        let platform = match case.platform.as_str() {
            "linux" => Platform::Linux,
            "macos" => Platform::MacOs,
            other => panic!("{}: unknown platform {other}", case.name),
        };
        let result = local_socket_path_for(platform, case.effective_uid, &fixture_env(&case.env));
        assert!(
            matches!(result, Err(ClientError::LocalSocketUnresolvable { .. })),
            "{} ({}): {result:?}",
            case.name,
            error.var
        );
        checked += 1;
    }
    assert!(checked >= 2, "the fixture holds runtime directory failures");
}

#[test]
fn an_overlong_or_relative_runtime_root_is_a_typed_error() {
    let overlong = PathEnv {
        xdg_runtime_dir: Some(OsString::from(format!("/{}", "a".repeat(200)))),
        ..PathEnv::default()
    };
    assert!(matches!(
        local_socket_path_for(Platform::Linux, 1000, &overlong),
        Err(ClientError::LocalSocketPathInvalid {
            source: PathError::SocketPathTooLong { .. }
        })
    ));

    let relative = PathEnv {
        xdg_runtime_dir: Some(OsString::from("run/user/1000")),
        ..PathEnv::default()
    };
    assert!(matches!(
        local_socket_path_for(Platform::Linux, 1000, &relative),
        Err(ClientError::LocalSocketUnresolvable { .. })
    ));
}

#[tokio::test]
async fn a_private_runtime_directory_and_socket_resolve_and_connect() {
    let runtime = Runtime::new();
    runtime.create_dir();
    let _listener = runtime.bind_socket();

    let path = runtime.resolve().expect("a private socket resolves");
    assert_eq!(path, runtime.socket());

    Client::connect_local_with_options(
        &path,
        ClientOptions::default().with_origin_source(OriginSource::Omitted),
    )
    .await
    .expect("the resolved path connects");
}

#[test]
fn an_absent_runtime_directory_resolves_to_the_socket_path() {
    let runtime = Runtime::new();
    assert_eq!(runtime.resolve().expect("no daemon yet"), runtime.socket());
}

#[test]
fn a_symlinked_runtime_directory_is_refused() {
    let runtime = Runtime::new();
    let elsewhere = runtime.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("create the link target");
    set_mode(&elsewhere, PRIVATE_DIR_MODE);
    std::os::unix::fs::symlink(&elsewhere, runtime.dir()).expect("link the runtime directory");

    let error = runtime
        .resolve()
        .expect_err("a symlinked directory is unsafe");
    assert_eq!(unsafe_reason(&error), UnsafeEndpointReason::Symlink);
}

#[test]
fn a_runtime_directory_open_to_group_or_other_is_refused() {
    let runtime = Runtime::new();
    runtime.create_dir();
    set_mode(&runtime.dir(), OPEN_MODE);

    let error = runtime.resolve().expect_err("an open directory is unsafe");
    assert!(matches!(
        unsafe_reason(&error),
        UnsafeEndpointReason::GroupOrOtherAccess { .. }
    ));
}

#[test]
fn a_runtime_directory_of_another_user_is_refused() {
    let runtime = Runtime::new();
    runtime.create_dir();

    let result = local_socket_path_for(
        Platform::Linux,
        runtime.uid + 1,
        &runtime_env(&runtime.root),
    );
    let error = result.expect_err("a foreign owner is unsafe");
    assert_eq!(
        unsafe_reason(&error),
        UnsafeEndpointReason::ForeignOwner {
            owner: runtime.uid,
            expected: runtime.uid + 1
        }
    );
}

#[tokio::test]
async fn a_socket_that_is_open_linked_or_not_a_socket_is_refused() {
    let runtime = Runtime::new();
    runtime.create_dir();
    let _listener = runtime.bind_socket();

    set_mode(&runtime.socket(), OPEN_MODE);
    let error = runtime.resolve().expect_err("an open socket is unsafe");
    assert!(matches!(
        unsafe_reason(&error),
        UnsafeEndpointReason::GroupOrOtherAccess { .. }
    ));

    std::fs::remove_file(runtime.socket()).expect("remove the socket");
    std::fs::write(runtime.socket(), b"not a socket").expect("write a regular file");
    set_mode(&runtime.socket(), PRIVATE_SOCKET_MODE);
    let error = runtime.resolve().expect_err("a regular file is unsafe");
    assert_eq!(unsafe_reason(&error), UnsafeEndpointReason::NotSocket);

    std::fs::remove_file(runtime.socket()).expect("remove the file");
    std::os::unix::fs::symlink(&runtime.root, runtime.socket()).expect("link the socket");
    let error = runtime.resolve().expect_err("a linked socket is unsafe");
    assert_eq!(unsafe_reason(&error), UnsafeEndpointReason::Symlink);
}
