//! Public-API tests for loading, validating, writing, and verifying `service.toml`.

// Rust guideline compliant 2026-10-14

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use pohunek_platform::filesystem::FsError;
use pohunek_platform::shell_env::SearchPath;
use pohunek_platform::supervisor::Namespace;
use pohunek_service_config::{
    ConfigError, ConfigSpec, Deadlines, InputTiming, InventoryProbe, KeepaliveTiming, RequiredHook,
    ServiceConfig, WaitTiming, DEFAULT_CREATE_DRAIN, DEFAULT_EVENT_RETENTION,
    DEFAULT_HOOK_STDERR_TAIL_BYTES, DEFAULT_INPUT_TIMING, DEFAULT_INVENTORY_PROBE,
    DEFAULT_KEEPALIVE_TIMING, DEFAULT_REGISTRY_SHUTDOWN, DEFAULT_WAIT_TIMING,
    DEFAULT_WORKER_JOURNAL_MAX_AGE, MAX_ALLOWLIST_ENTRIES, MAX_CONFIG_BYTES, MAX_DEADLINE,
    MAX_HOOK_STDERR_TAIL_BYTES, MAX_PATTERN_BYTES, MAX_SEARCH_PATH_BYTES,
};
use tempfile::TempDir;

/// The schema example from the crate contract.
const GOLDEN: &str = r#"schema_version = 7
prefix = "/home/u/.local"
active_version = "0.31.6"

[namespace]
uid = 1000
state_root = "/home/u/.local/state/pohunek"
runtime_root = "/run/user/1000/pohunek"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = 30000
daemon_restart_throttle_ms = 5000

[environment]
search_path = ["/opt/homebrew/bin", "/Users/u/.local/bin", "/usr/bin"]
allowlist = ["PATH","HOME","USER","LOGNAME","SHELL","LANG","LC_*","TMPDIR",
  "SSH_AUTH_SOCK","DISPLAY","WAYLAND_DISPLAY","DBUS_SESSION_BUS_ADDRESS","XDG_*"]

[sweep]
grace_ms = 5000

[input]
initial_startup_grace_ms = 5000
submit_delay_ms = 150
write_stall_timeout_ms = 3000

[waits]
turn_default_ms = 300000
turn_max_ms = 1800000

[hooks]
stderr_tail_bytes = 4096

[events]
max_file_bytes = 33554432
max_files = 8
max_age_days = 14

[limits]
open_files = 8192
"#;

/// Every required key and the `namespace` table, as a missing-field diagnostic names them.
const REQUIRED_KEYS: [&str; 30] = [
    "schema_version",
    "prefix",
    "active_version",
    "uid",
    "state_root",
    "runtime_root",
    "worker_connect_ms",
    "worker_initialize_ms",
    "launchctl_command_ms",
    "worker_exit_timeout_ms",
    "daemon_exit_timeout_ms",
    "daemon_restart_throttle_ms",
    "search_path",
    "allowlist",
    "grace_ms",
    "input",
    "initial_startup_grace_ms",
    "submit_delay_ms",
    "write_stall_timeout_ms",
    "waits",
    "turn_default_ms",
    "turn_max_ms",
    "hooks",
    "stderr_tail_bytes",
    "events",
    "max_file_bytes",
    "max_files",
    "max_age_days",
    "open_files",
    "namespace",
];

/// A `0700` config directory inside a canonical temporary root.
struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    config_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = pohunek_test_support::tempdir().expect("create temp dir");
        let root = fs::canonicalize(temp.path()).expect("canonicalize temp dir");
        let config_dir = root.join("config");
        fs::create_dir(&config_dir).expect("create config dir");
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
            .expect("chmod config dir");
        Self {
            _temp: temp,
            root,
            config_dir,
        }
    }

    fn path(&self) -> PathBuf {
        self.config_dir.join("service.toml")
    }

    /// Writes `text` as the config file with `mode` and returns its path.
    fn write(&self, text: &str, mode: u32) -> PathBuf {
        let path = self.path();
        // A previous case may have left a read-only file behind.
        if path.exists() {
            fs::remove_file(&path).expect("remove previous config");
        }
        fs::write(&path, text).expect("write config");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod config");
        path
    }

    fn load(&self, text: &str) -> Result<ServiceConfig, ConfigError> {
        ServiceConfig::load(&self.write(text, 0o600))
    }

    /// Creates a directory below the fixture root and returns its canonical path.
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::create_dir_all(&path).expect("create dir");
        fs::canonicalize(path).expect("canonicalize dir")
    }
}

fn replace(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "fixture lacks {from:?}");
    text.replacen(from, to, 1)
}

fn spec() -> ConfigSpec {
    ConfigSpec {
        prefix: PathBuf::from("/home/u/.local"),
        active_version: "0.31.6".to_owned(),
        uid: 1000,
        state_root: PathBuf::from("/home/u/.local/state/pohunek"),
        runtime_root: PathBuf::from("/run/user/1000/pohunek"),
        deadlines: Deadlines {
            worker_connect: Duration::from_secs(10),
            worker_initialize: Duration::from_secs(45),
            launchctl_command: Duration::from_secs(10),
            worker_exit_timeout: Duration::from_secs(30),
            daemon_exit_timeout: Duration::from_secs(30),
            daemon_restart_throttle: Duration::from_secs(5),
            registry_shutdown: DEFAULT_REGISTRY_SHUTDOWN,
            create_drain: pohunek_service_config::DEFAULT_CREATE_DRAIN,
            driver_shutdown: pohunek_service_config::DEFAULT_DRIVER_SHUTDOWN,
            event_log_flush: pohunek_service_config::DEFAULT_EVENT_LOG_FLUSH,
        },
        environment_allowlist: vec!["PATH".to_owned(), "LC_*".to_owned()],
        search_path: SearchPath::empty(),
        sweep_grace: Duration::from_secs(5),
        input_timing: DEFAULT_INPUT_TIMING,
        wait_timing: DEFAULT_WAIT_TIMING,
        hook_stderr_tail_bytes: DEFAULT_HOOK_STDERR_TAIL_BYTES,
        hook_required: Vec::new(),
        event_retention: DEFAULT_EVENT_RETENTION,
        inventory_probe: DEFAULT_INVENTORY_PROBE,
        worker_journal_max_age: DEFAULT_WORKER_JOURNAL_MAX_AGE,
        open_files: 8192,
    }
}

fn parse_message(result: Result<ServiceConfig, ConfigError>) -> String {
    match result {
        Err(ConfigError::Parse { message, .. }) => message,
        other => panic!("expected a parse error, got {other:?}"),
    }
}

/// Builds the `Parse` error itself so `Display` and `Debug` can be inspected.
fn parse_error(text: &str) -> ConfigError {
    let fixture = Fixture::new();
    match fixture.load(text) {
        Err(error) => error,
        Ok(_) => panic!("expected a parse error"),
    }
}

#[test]
fn golden_example_parses_into_typed_values() {
    let fixture = Fixture::new();
    let config = fixture.load(GOLDEN).expect("golden config loads");

    assert_eq!(config.prefix(), Path::new("/home/u/.local"));
    assert_eq!(config.active_version(), "0.31.6");
    assert_eq!(
        config.version_dir(),
        Path::new("/home/u/.local/libexec/pohunek/0.31.6")
    );
    assert_eq!(
        config.daemon_executable(),
        Path::new("/home/u/.local/libexec/pohunek/0.31.6/pohunekd")
    );
    assert_eq!(
        config.worker_executable(),
        Path::new("/home/u/.local/libexec/pohunek/0.31.6/pohunek-sessiond")
    );
    assert_eq!(config.layout().bin_dir(), Path::new("/home/u/.local/bin"));
    assert_eq!(config.uid(), 1000);
    assert_eq!(
        config.state_root(),
        Path::new("/home/u/.local/state/pohunek")
    );
    assert_eq!(config.runtime_root(), Path::new("/run/user/1000/pohunek"));
    assert_eq!(config.deadlines(), spec().deadlines);
    assert_eq!(
        config.search_path().entries(),
        [
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/Users/u/.local/bin"),
            PathBuf::from("/usr/bin"),
        ]
    );
    assert_eq!(config.environment_allowlist().len(), 13);
    assert_eq!(config.environment_allowlist()[6], "LC_*");
    assert_eq!(config.sweep_grace(), Duration::from_secs(5));
    assert_eq!(config.input_timing(), DEFAULT_INPUT_TIMING);
    assert_eq!(
        config.wait_timing(),
        WaitTiming {
            turn_default: Duration::from_mins(5),
            turn_max: Duration::from_mins(30),
            keepalive: DEFAULT_KEEPALIVE_TIMING,
        }
    );
    assert_eq!(config.wait_timing(), DEFAULT_WAIT_TIMING);
    assert_eq!(
        config.hook_stderr_tail_bytes(),
        DEFAULT_HOOK_STDERR_TAIL_BYTES
    );
    assert_eq!(config.event_retention(), DEFAULT_EVENT_RETENTION);
    assert_eq!(config.open_files(), 8192);
    assert_eq!(
        config.namespace(),
        Namespace::derive(
            1000,
            Path::new("/home/u/.local/state/pohunek"),
            Path::new("/run/user/1000/pohunek"),
        )
    );
}

#[test]
fn every_missing_key_is_named() {
    let fixture = Fixture::new();
    for key in REQUIRED_KEYS {
        let text = if ["namespace", "input", "waits", "hooks", "events"].contains(&key) {
            let (table, next) = match key {
                "namespace" => ("[namespace]", "[deadlines]"),
                "input" => ("[input]", "[waits]"),
                "waits" => ("[waits]", "[hooks]"),
                "hooks" => ("[hooks]", "[events]"),
                _ => ("[events]", "[limits]"),
            };
            let start = GOLDEN.find(table).expect("table");
            let end = GOLDEN.find(next).expect("next table");
            format!("{}{}", &GOLDEN[..start], &GOLDEN[end..])
        } else {
            let start = GOLDEN
                .find(&format!("\n{key} = "))
                .map(|index| index + 1)
                .or_else(|| GOLDEN.starts_with(&format!("{key} = ")).then_some(0))
                .expect("key line");
            let end = if key == "allowlist" {
                GOLDEN.find("\n\n[sweep]").expect("allowlist end") + 1
            } else {
                start + GOLDEN[start..].find('\n').expect("line end") + 1
            };
            format!("{}{}", &GOLDEN[..start], &GOLDEN[end..])
        };
        let message = parse_message(fixture.load(&text));
        assert!(
            message.contains(&format!("missing field `{key}`")),
            "{key}: {message}"
        );
    }
}

#[test]
fn missing_tables_are_named() {
    let fixture = Fixture::new();
    for (table, next) in [
        ("[deadlines]", "[environment]"),
        ("[environment]", "[sweep]"),
        ("[sweep]", "[input]"),
        ("[input]", "[waits]"),
        ("[waits]", "[hooks]"),
        ("[hooks]", "[events]"),
        ("[events]", "[limits]"),
    ] {
        let start = GOLDEN.find(table).expect("table");
        let end = GOLDEN.find(next).expect("next table");
        let text = format!("{}{}", &GOLDEN[..start], &GOLDEN[end..]);
        let name = table.trim_matches(['[', ']']);
        let message = parse_message(fixture.load(&text));
        assert!(
            message.contains(&format!("missing field `{name}`")),
            "{message}"
        );
    }
    let start = GOLDEN.find("[limits]").expect("limits table");
    let message = parse_message(fixture.load(&GOLDEN[..start]));
    assert!(message.contains("missing field `limits`"), "{message}");
}

#[test]
fn mistyped_values_are_parse_errors() {
    let fixture = Fixture::new();
    for text in [
        replace(GOLDEN, "uid = 1000", "uid = -1"),
        replace(GOLDEN, "uid = 1000", "uid = \"1000\""),
        replace(GOLDEN, "grace_ms = 5000", "grace_ms = 5.0"),
        replace(GOLDEN, "open_files = 8192", "open_files = -8192"),
        "not toml at all".to_owned(),
    ] {
        parse_message(fixture.load(&text));
    }
}

/// A property-shaped fake token standing in for what a hand-edited owner
/// private file could carry under an unknown key.
const SENTINEL_TOKEN: &str = "PH-TOKEN-sentinel-7f3a-ea91";

/// A parse diagnostic must never quote file contents: the parser `Display`
/// renders the source line, so a corrupt file could print a holdover value
/// such as this sentinel into journald or an error envelope. Only the key
/// name and the position survive.
#[test]
fn parse_diagnostics_never_quote_file_contents() {
    let unknown_key = replace(
        GOLDEN,
        "[environment]\n",
        &format!("[environment]\noperator_token = \"{SENTINEL_TOKEN}\"\n"),
    );
    let broken_integer = replace(
        GOLDEN,
        "uid = 1000",
        &format!("uid = {SENTINEL_TOKEN} # broken"),
    );
    let unterminated_string = replace(
        GOLDEN,
        "prefix = \"/home/u/.local\"",
        &format!("prefix = \"{SENTINEL_TOKEN}"),
    );
    let string_in_integer = replace(GOLDEN, "uid = 1000", &format!("uid = \"{SENTINEL_TOKEN}\""));
    for text in [
        &unknown_key,
        &broken_integer,
        &unterminated_string,
        &string_in_integer,
    ] {
        let error = parse_error(text);
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(
            !display.contains(SENTINEL_TOKEN) && !debug.contains(SENTINEL_TOKEN),
            "parse diagnostic quotes the file: display={display} debug={debug}"
        );
        assert!(
            display.contains("line ") && display.contains("column "),
            "diagnostic lost the position: {display}"
        );
    }
    // Deserialization diagnostics for unknown fields name the rejected key;
    // type and syntax diagnostics name only the position (and u32 etc.).
    let display = parse_error(&unknown_key).to_string();
    assert!(
        display.contains("operator_token"),
        "diagnostic lost the key: {display}"
    );
}

/// Semantic validation must not echo a rejected value either: a value that
/// parses but fails validation, such as a secret pasted into a version, path,
/// or allowlist entry, is described by key, position, reason, and length.
#[test]
fn validation_errors_never_quote_rejected_values() {
    let allowlist = |entries: &str| {
        let start = GOLDEN.find("allowlist = ").expect("allowlist");
        let end = GOLDEN.find("\n\n[sweep]").expect("allowlist end");
        format!(
            "{}allowlist = [{entries}]{}",
            &GOLDEN[..start],
            &GOLDEN[end..]
        )
    };
    let secret = SENTINEL_TOKEN;
    let long_secret = format!("{secret}{}", "_".repeat(MAX_PATTERN_BYTES));
    let cases = [
        (
            "active_version",
            replace(
                GOLDEN,
                "active_version = \"0.31.6\"",
                &format!("active_version = \"v {secret}\""),
            ),
        ),
        (
            "prefix",
            replace(
                GOLDEN,
                "prefix = \"/home/u/.local\"",
                &format!("prefix = \"relative/{secret}\""),
            ),
        ),
        (
            "namespace.state_root",
            replace(
                GOLDEN,
                "state_root = \"/home/u/.local/state/pohunek\"",
                &format!("state_root = \"/home/{secret}/../x\""),
            ),
        ),
        (
            "namespace.runtime_root",
            replace(
                GOLDEN,
                "runtime_root = \"/run/user/1000/pohunek\"",
                &format!("runtime_root = \"/run/{secret}/\""),
            ),
        ),
        (
            "environment.allowlist",
            allowlist(&format!("\"PATH\", \"{secret}\"")),
        ),
        (
            "environment.allowlist",
            allowlist(&format!("\"PATH\", \"{long_secret}\"")),
        ),
        (
            "environment.allowlist",
            allowlist("\"PH_TOKEN_sentinel_7f3a\", \"PATH\", \"PH_TOKEN_sentinel_7f3a\""),
        ),
    ];
    let fixture = Fixture::new();
    for (key, text) in &cases {
        let Err(error) = fixture.load(text) else {
            panic!("{key}: invalid value was accepted");
        };
        assert!(
            !matches!(error, ConfigError::Parse { .. }),
            "{key}: expected a semantic error, got {error:?}"
        );
        let display = error.to_string();
        let debug = format!("{error:?}");
        for rendered in [&display, &debug] {
            assert!(
                !rendered.contains(secret)
                    && !rendered.contains("sentinel")
                    && !rendered.contains("PH_TOKEN"),
                "{key}: the error quotes the rejected value: {rendered}"
            );
        }
        assert!(display.contains(key), "{key}: key missing from {display}");
    }
}

#[test]
fn other_schema_versions_are_rejected_before_their_keys() {
    let fixture = Fixture::new();
    let newer = replace(
        GOLDEN,
        "schema_version = 7",
        "schema_version = 8\nfuture_key = true",
    );
    assert!(matches!(
        fixture.load(&newer),
        Err(ConfigError::UnsupportedSchema { found: 8 })
    ));
    let zero = replace(GOLDEN, "schema_version = 7", "schema_version = 0");
    assert!(matches!(
        fixture.load(&zero),
        Err(ConfigError::UnsupportedSchema { found: 0 })
    ));
}

/// Frozen previous-release shape, independent of the current writer.
const SCHEMA_TWO_GOLDEN: &str = r#"schema_version = 2
prefix = "/home/u/.local"
active_version = "0.33.1"

[namespace]
uid = 1000
state_root = "/home/u/.local/state/pohunek"
runtime_root = "/run/user/1000/pohunek"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = 30000
daemon_restart_throttle_ms = 5000

[environment]
search_path = ["/opt/homebrew/bin", "/Users/u/.local/bin", "/usr/bin"]
allowlist = ["PATH","HOME","USER","LOGNAME","SHELL","LANG","LC_*","TMPDIR",
  "SSH_AUTH_SOCK","DISPLAY","WAYLAND_DISPLAY","DBUS_SESSION_BUS_ADDRESS","XDG_*"]

[sweep]
grace_ms = 5000

[limits]
open_files = 8192
"#;

fn schema_two_text() -> String {
    SCHEMA_TWO_GOLDEN.to_owned()
}

#[test]
fn upgrade_source_explicitly_migrates_schema_two_to_required_input_keys() {
    let fixture = Fixture::new();
    let previous = schema_two_text();
    let path = fixture.write(&previous, 0o600);
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::UnsupportedSchema { found: 2 })
    ));
    let source = ServiceConfig::load_upgrade_source(&path).expect("schema 2 upgrade source");
    assert_eq!(source.source_schema_version, 2);
    assert_eq!(source.config.input_timing(), DEFAULT_INPUT_TIMING);
    assert_eq!(source.config.wait_timing(), DEFAULT_WAIT_TIMING);
    assert_eq!(
        source.config.hook_stderr_tail_bytes(),
        DEFAULT_HOOK_STDERR_TAIL_BYTES
    );
    assert_eq!(source.config.event_retention(), DEFAULT_EVENT_RETENTION);
    assert_eq!(source.original_bytes(), previous.as_bytes());
    let backup = fixture.root.join("upgrade-backup/service.toml");
    source.write_original(&backup).expect("save private backup");
    assert_eq!(fs::read(&backup).expect("read backup"), previous.as_bytes());
    assert_eq!(
        fs::metadata(&backup)
            .expect("backup metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    source.config.write(&path).expect("write current schema");
    let current = ServiceConfig::load_upgrade_source(&path).expect("current schema upgrade source");
    assert_eq!(current.source_schema_version, 7);
    assert_eq!(current.config, source.config);
    let migrated = fs::read_to_string(&path).expect("read migrated config");
    for line in [
        "schema_version = 7",
        "submit_delay_ms = 150",
        "write_stall_timeout_ms = 3000",
        "[hooks]",
        "stderr_tail_bytes = 4096",
        "turn_default_ms = 300000",
        "turn_max_ms = 1800000",
    ] {
        assert!(migrated.contains(line), "{line} missing from {migrated}");
    }
    assert_eq!(
        ServiceConfig::load(&path).expect("migrated file loads"),
        source.config
    );
    let saved = ServiceConfig::load_upgrade_source(&backup).expect("verified backup");
    assert_eq!(saved.source_schema_version, 2);
    saved
        .write_original(&path)
        .expect("rollback exact schema 2");
    assert_eq!(fs::read(&path).expect("read rollback"), previous.as_bytes());
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::UnsupportedSchema { found: 2 })
    ));
}

/// Frozen schema-4 shape exactly as the previous writer emitted it: `[input]`
/// has no stall bound. Its `[input]` and `[waits]` values differ from the
/// defaults, so a migration that reset them would show.
const SCHEMA_FOUR_GOLDEN: &str = r#"# Pohunek service configuration, written by `pohunek service install`.
# Every key is required; unknown keys are rejected.

schema_version = 4
prefix = "/home/u/.local"
active_version = "0.34.2"

[namespace]
uid = 1000
state_root = "/home/u/.local/state/pohunek"
runtime_root = "/run/user/1000/pohunek"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = 30000
daemon_restart_throttle_ms = 5000

[environment]
search_path = ["/usr/bin"]
allowlist = ["PATH", "HOME", "LANG", "LC_*", "XDG_*"]

[sweep]
grace_ms = 5000

[input]
initial_startup_grace_ms = 9000
submit_delay_ms = 275

[waits]
turn_default_ms = 120000
turn_max_ms = 2700000

[limits]
open_files = 8192
"#;

#[test]
fn upgrade_source_migrates_schema_four_keeping_its_values_and_adding_the_stall_default() {
    let fixture = Fixture::new();
    let previous = SCHEMA_FOUR_GOLDEN.to_owned();
    let path = fixture.write(&previous, 0o600);
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::UnsupportedSchema { found: 4 })
    ));
    let source = ServiceConfig::load_upgrade_source(&path).expect("schema 4 upgrade source");
    assert_eq!(source.source_schema_version, 4);
    assert_eq!(source.config.active_version(), "0.34.2");
    assert_eq!(
        source.config.input_timing(),
        InputTiming {
            initial_startup_grace: Duration::from_secs(9),
            submit_delay: Duration::from_millis(275),
            write_stall_timeout: DEFAULT_INPUT_TIMING.write_stall_timeout,
        }
    );
    assert_eq!(
        source.config.wait_timing(),
        WaitTiming {
            turn_default: Duration::from_mins(2),
            turn_max: Duration::from_mins(45),
            keepalive: DEFAULT_KEEPALIVE_TIMING,
        }
    );
    assert_eq!(
        source.config.hook_stderr_tail_bytes(),
        DEFAULT_HOOK_STDERR_TAIL_BYTES
    );
    assert_eq!(source.config.event_retention(), DEFAULT_EVENT_RETENTION);
    assert_eq!(source.original_bytes(), previous.as_bytes());

    let backup = fixture.root.join("upgrade-backup/service.toml");
    source.write_original(&backup).expect("save private backup");
    assert_eq!(fs::read(&backup).expect("read backup"), previous.as_bytes());

    source.config.write(&path).expect("write current schema");
    let migrated = fs::read_to_string(&path).expect("read migrated config");
    for line in [
        "schema_version = 7",
        "initial_startup_grace_ms = 9000",
        "submit_delay_ms = 275",
        "write_stall_timeout_ms = 3000",
        "[hooks]",
        "stderr_tail_bytes = 4096",
        "turn_default_ms = 120000",
        "turn_max_ms = 2700000",
    ] {
        assert!(migrated.contains(line), "{line} missing from {migrated}");
    }
    assert_eq!(
        ServiceConfig::load(&path).expect("migrated file loads"),
        source.config
    );

    let saved = ServiceConfig::load_upgrade_source(&backup).expect("verified backup");
    assert_eq!(saved.source_schema_version, 4);
    saved
        .write_original(&path)
        .expect("rollback exact schema 4");
    assert_eq!(fs::read(&path).expect("read rollback"), previous.as_bytes());
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::UnsupportedSchema { found: 4 })
    ));
}

/// Frozen schema-5 shape exactly as the previous writer emitted it: no
/// `[hooks]` table. Its `[input]` and `[waits]` values differ from the
/// defaults, so a migration that reset them would show.
const SCHEMA_FIVE_GOLDEN: &str = r#"# Pohunek service configuration, written by `pohunek service install`.
# Every key is required; unknown keys are rejected.

schema_version = 5
prefix = "/home/u/.local"
active_version = "0.34.2"

[namespace]
uid = 1000
state_root = "/home/u/.local/state/pohunek"
runtime_root = "/run/user/1000/pohunek"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = 30000
daemon_restart_throttle_ms = 5000

[environment]
search_path = ["/usr/bin"]
allowlist = ["PATH", "HOME", "LANG", "LC_*", "XDG_*"]

[sweep]
grace_ms = 5000

[input]
initial_startup_grace_ms = 9000
submit_delay_ms = 275
write_stall_timeout_ms = 4500

[waits]
turn_default_ms = 120000
turn_max_ms = 2700000

[limits]
open_files = 8192
"#;

#[test]
fn upgrade_source_migrates_schema_five_keeping_its_values_and_adding_the_hooks_default() {
    let fixture = Fixture::new();
    let previous = SCHEMA_FIVE_GOLDEN.to_owned();
    let path = fixture.write(&previous, 0o600);
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::UnsupportedSchema { found: 5 })
    ));
    let source = ServiceConfig::load_upgrade_source(&path).expect("schema 5 upgrade source");
    assert_eq!(source.source_schema_version, 5);
    assert_eq!(source.config.active_version(), "0.34.2");
    assert_eq!(
        source.config.input_timing(),
        InputTiming {
            initial_startup_grace: Duration::from_secs(9),
            submit_delay: Duration::from_millis(275),
            write_stall_timeout: Duration::from_millis(4500),
        }
    );
    assert_eq!(
        source.config.wait_timing(),
        WaitTiming {
            turn_default: Duration::from_mins(2),
            turn_max: Duration::from_mins(45),
            keepalive: DEFAULT_KEEPALIVE_TIMING,
        }
    );
    assert_eq!(
        source.config.hook_stderr_tail_bytes(),
        DEFAULT_HOOK_STDERR_TAIL_BYTES
    );
    assert_eq!(source.config.event_retention(), DEFAULT_EVENT_RETENTION);
    assert_eq!(source.original_bytes(), previous.as_bytes());

    let backup = fixture.root.join("upgrade-backup/service.toml");
    source.write_original(&backup).expect("save private backup");
    assert_eq!(fs::read(&backup).expect("read backup"), previous.as_bytes());

    source.config.write(&path).expect("write current schema");
    let migrated = fs::read_to_string(&path).expect("read migrated config");
    for line in [
        "schema_version = 7",
        "initial_startup_grace_ms = 9000",
        "submit_delay_ms = 275",
        "write_stall_timeout_ms = 4500",
        "turn_default_ms = 120000",
        "turn_max_ms = 2700000",
        "[hooks]",
        "stderr_tail_bytes = 4096",
        "[events]",
        "max_file_bytes = 33554432",
        "max_files = 8",
        "max_age_days = 14",
    ] {
        assert!(migrated.contains(line), "{line} missing from {migrated}");
    }
    assert_eq!(
        ServiceConfig::load(&path).expect("migrated file loads"),
        source.config
    );

    let saved = ServiceConfig::load_upgrade_source(&backup).expect("verified backup");
    assert_eq!(saved.source_schema_version, 5);
    saved
        .write_original(&path)
        .expect("rollback exact schema 5");
    assert_eq!(fs::read(&path).expect("read rollback"), previous.as_bytes());
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::UnsupportedSchema { found: 5 })
    ));
}

/// Frozen schema-6 shape exactly as the previous writer emitted it: no
/// `[events]` table. Its `[hooks]` value differs from the default, so a
/// migration that reset it would show.
const SCHEMA_SIX_GOLDEN: &str = r#"# Pohunek service configuration, written by `pohunek service install`.
# Every key is required; unknown keys are rejected.

schema_version = 6
prefix = "/home/u/.local"
active_version = "0.35.2"

[namespace]
uid = 1000
state_root = "/home/u/.local/state/pohunek"
runtime_root = "/run/user/1000/pohunek"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = 30000
daemon_restart_throttle_ms = 5000

[environment]
search_path = ["/usr/bin"]
allowlist = ["PATH", "HOME", "LANG", "LC_*", "XDG_*"]

[sweep]
grace_ms = 5000

[input]
initial_startup_grace_ms = 9000
submit_delay_ms = 275
write_stall_timeout_ms = 4500

[waits]
turn_default_ms = 120000
turn_max_ms = 2700000

[hooks]
stderr_tail_bytes = 2048

[limits]
open_files = 8192
"#;

#[test]
fn upgrade_source_migrates_schema_six_keeping_its_values_and_adding_the_events_default() {
    let fixture = Fixture::new();
    let previous = SCHEMA_SIX_GOLDEN.to_owned();
    let path = fixture.write(&previous, 0o600);
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::UnsupportedSchema { found: 6 })
    ));
    let source = ServiceConfig::load_upgrade_source(&path).expect("schema 6 upgrade source");
    assert_eq!(source.source_schema_version, 6);
    assert_eq!(source.config.hook_stderr_tail_bytes(), 2048);
    assert_eq!(source.config.event_retention(), DEFAULT_EVENT_RETENTION);
    assert_eq!(source.original_bytes(), previous.as_bytes());

    source.config.write(&path).expect("write current schema");
    let migrated = fs::read_to_string(&path).expect("read migrated config");
    for line in [
        "schema_version = 7",
        "stderr_tail_bytes = 2048",
        "[events]",
        "max_file_bytes = 33554432",
        "max_files = 8",
        "max_age_days = 14",
    ] {
        assert!(migrated.contains(line), "{line} missing from {migrated}");
    }
    assert_eq!(
        ServiceConfig::load(&path).expect("migrated file loads"),
        source.config
    );
}

#[test]
fn a_schema_7_file_without_events_names_the_missing_table() {
    let fixture = Fixture::new();
    let start = GOLDEN.find("[events]").expect("events table");
    let end = GOLDEN.find("[limits]").expect("limits table");
    let text = format!("{}{}", &GOLDEN[..start], &GOLDEN[end..]);
    let message = parse_message(fixture.load(&text));
    assert!(message.contains("missing field `events`"), "{message}");
}

#[test]
fn event_retention_outside_its_bounds_is_rejected_by_key() {
    let fixture = Fixture::new();
    for (line, replacement, key) in [
        (
            "max_file_bytes = 33554432",
            "max_file_bytes = 1048575",
            "events.max_file_bytes",
        ),
        (
            "max_file_bytes = 33554432",
            "max_file_bytes = 1073741825",
            "events.max_file_bytes",
        ),
        ("max_files = 8", "max_files = 1", "events.max_files"),
        ("max_files = 8", "max_files = 65", "events.max_files"),
        (
            "max_age_days = 14",
            "max_age_days = 0",
            "events.max_age_days",
        ),
        (
            "max_age_days = 14",
            "max_age_days = 3651",
            "events.max_age_days",
        ),
        (
            "max_age_days = 14",
            "max_age_days = 300000000000000",
            "events.max_age_days",
        ),
    ] {
        let text = replace(GOLDEN, line, replacement);
        match fixture.load(&text) {
            Err(ConfigError::OutOfRange { key: found, .. }) => assert_eq!(found, key),
            other => panic!("{replacement}: {other:?}"),
        }
    }
    for replacement in [
        "max_file_bytes = 1048576",
        "max_file_bytes = 1073741824",
        "max_files = 2",
        "max_files = 64",
        "max_age_days = 1",
        "max_age_days = 3650",
    ] {
        let line = replacement.split(" =").next().expect("key");
        let original = GOLDEN
            .lines()
            .find(|candidate| candidate.starts_with(&format!("{line} =")))
            .expect("golden line");
        fixture
            .load(&replace(GOLDEN, original, replacement))
            .unwrap_or_else(|error| panic!("{replacement}: {error}"));
    }
}

/// The `GOLDEN` of the v0.35.2 tag, byte for byte: the schema-6 file the
/// latest released writer emits.
const RELEASED_0_35_2_GOLDEN: &str = r#"schema_version = 6
prefix = "/home/u/.local"
active_version = "0.31.6"

[namespace]
uid = 1000
state_root = "/home/u/.local/state/pohunek"
runtime_root = "/run/user/1000/pohunek"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = 30000
daemon_restart_throttle_ms = 5000

[environment]
search_path = ["/opt/homebrew/bin", "/Users/u/.local/bin", "/usr/bin"]
allowlist = ["PATH","HOME","USER","LOGNAME","SHELL","LANG","LC_*","TMPDIR",
  "SSH_AUTH_SOCK","DISPLAY","WAYLAND_DISPLAY","DBUS_SESSION_BUS_ADDRESS","XDG_*"]

[sweep]
grace_ms = 5000

[input]
initial_startup_grace_ms = 5000
submit_delay_ms = 150
write_stall_timeout_ms = 3000

[waits]
turn_default_ms = 300000
turn_max_ms = 1800000

[hooks]
stderr_tail_bytes = 4096

[limits]
open_files = 8192
"#;

#[test]
fn a_file_written_by_release_0_35_2_upgrades_to_schema_seven() {
    let fixture = Fixture::new();
    let path = fixture.write(RELEASED_0_35_2_GOLDEN, 0o600);
    let source = ServiceConfig::load_upgrade_source(&path).expect("released schema-6 source");
    assert_eq!(source.source_schema_version, 6);
    assert_eq!(source.config.event_retention(), DEFAULT_EVENT_RETENTION);
    assert_eq!(source.original_bytes(), RELEASED_0_35_2_GOLDEN.as_bytes());
    source.config.write(&path).expect("write schema 7");
    ServiceConfig::load(&path).expect("the migrated file loads");
}

#[test]
fn a_schema_six_file_with_a_hand_added_key_names_it_in_the_refusal() {
    let fixture = Fixture::new();
    let path = fixture.write(
        &replace(
            RELEASED_0_35_2_GOLDEN,
            "[sweep]",
            "[sweep]\nfuture_sweep = 1",
        ),
        0o600,
    );
    let message = match ServiceConfig::load_upgrade_source(&path) {
        Err(ConfigError::Parse { message, .. }) => message,
        other => panic!("expected a parse refusal, got {other:?}"),
    };
    assert!(message.contains("future_sweep"), "{message}");
    assert!(
        message.contains("a schema 6 file is read in its exact shape"),
        "{message}"
    );
}

/// Frozen schema-3 shape, independent of the current writer. Its `[input]`
/// values differ from the defaults, so a migration that reset them would show.
const SCHEMA_THREE_GOLDEN: &str = r#"schema_version = 3
prefix = "/home/u/.local"
active_version = "0.40.0"

[namespace]
uid = 1000
state_root = "/home/u/.local/state/pohunek"
runtime_root = "/run/user/1000/pohunek"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = 30000
daemon_restart_throttle_ms = 5000

[environment]
search_path = ["/usr/bin"]
allowlist = ["PATH", "HOME", "LANG", "LC_*", "XDG_*"]

[sweep]
grace_ms = 5000

[input]
initial_startup_grace_ms = 9000
submit_delay_ms = 275

[limits]
open_files = 8192
"#;

#[test]
fn upgrade_source_migrates_schema_three_keeping_input_and_adding_default_waits() {
    let fixture = Fixture::new();
    let previous = SCHEMA_THREE_GOLDEN.to_owned();
    let path = fixture.write(&previous, 0o600);
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::UnsupportedSchema { found: 3 })
    ));
    let source = ServiceConfig::load_upgrade_source(&path).expect("schema 3 upgrade source");
    assert_eq!(source.source_schema_version, 3);
    assert_eq!(source.config.active_version(), "0.40.0");
    assert_eq!(
        source.config.input_timing().initial_startup_grace,
        Duration::from_secs(9)
    );
    assert_eq!(
        source.config.input_timing().submit_delay,
        Duration::from_millis(275)
    );
    assert_eq!(
        source.config.input_timing().write_stall_timeout,
        DEFAULT_INPUT_TIMING.write_stall_timeout
    );
    assert_eq!(
        source.config.wait_timing(),
        WaitTiming {
            turn_default: Duration::from_mins(5),
            turn_max: Duration::from_mins(30),
            keepalive: DEFAULT_KEEPALIVE_TIMING,
        }
    );
    assert_eq!(
        source.config.hook_stderr_tail_bytes(),
        DEFAULT_HOOK_STDERR_TAIL_BYTES
    );
    assert_eq!(source.config.event_retention(), DEFAULT_EVENT_RETENTION);
    assert_eq!(source.original_bytes(), previous.as_bytes());

    let backup = fixture.root.join("upgrade-backup/service.toml");
    source.write_original(&backup).expect("save private backup");
    assert_eq!(fs::read(&backup).expect("read backup"), previous.as_bytes());

    source.config.write(&path).expect("write current schema");
    let migrated = fs::read_to_string(&path).expect("read migrated config");
    for line in [
        "schema_version = 7",
        "initial_startup_grace_ms = 9000",
        "submit_delay_ms = 275",
        "write_stall_timeout_ms = 3000",
        "[hooks]",
        "stderr_tail_bytes = 4096",
        "[waits]",
        "turn_default_ms = 300000",
        "turn_max_ms = 1800000",
    ] {
        assert!(migrated.contains(line), "{line} missing from {migrated}");
    }
    assert_eq!(
        ServiceConfig::load(&path).expect("migrated file loads"),
        source.config
    );

    let saved = ServiceConfig::load_upgrade_source(&backup).expect("verified backup");
    assert_eq!(saved.source_schema_version, 3);
    saved
        .write_original(&path)
        .expect("rollback exact schema 3");
    assert_eq!(fs::read(&path).expect("read rollback"), previous.as_bytes());
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::UnsupportedSchema { found: 3 })
    ));
}

#[test]
fn upgrade_source_rejects_invalid_previous_shape_and_unsupported_schema() {
    let fixture = Fixture::new();
    let previous = schema_two_text();
    let missing = replace(&previous, "grace_ms = 5000\n", "");
    let path = fixture.write(&missing, 0o600);
    assert!(matches!(
        ServiceConfig::load_upgrade_source(&path),
        Err(ConfigError::Parse { .. })
    ));
    let unexpected = format!("{previous}\n[unexpected]\nvalue = 1\n");
    fixture.write(&unexpected, 0o600);
    assert!(matches!(
        ServiceConfig::load_upgrade_source(&path),
        Err(ConfigError::Parse { .. })
    ));
    fixture.write(
        &replace(&previous, "schema_version = 2", "schema_version = 1"),
        0o600,
    );
    assert!(matches!(
        ServiceConfig::load_upgrade_source(&path),
        Err(ConfigError::UnsupportedSchema { found: 1 })
    ));
    // Schema 3 is parsed in its exact shape: it neither lacks `[input]` nor
    // carries the `[waits]` table of the current schema.
    let schema_three = SCHEMA_THREE_GOLDEN;
    let start = schema_three.find("[input]").expect("input table");
    let end = schema_three.find("[limits]").expect("limits table");
    for text in [
        format!("{}{}", &schema_three[..start], &schema_three[end..]),
        replace(
            schema_three,
            "[limits]",
            "[waits]\nturn_default_ms = 300000\nturn_max_ms = 1800000\n\n[limits]",
        ),
    ] {
        fixture.write(&text, 0o600);
        match ServiceConfig::load_upgrade_source(&path) {
            Err(ConfigError::Parse { .. }) => {}
            other => panic!("schema 3 of the wrong shape: {other:?}"),
        }
    }
    fixture.write(
        &replace(GOLDEN, "schema_version = 7", "schema_version = 8"),
        0o600,
    );
    assert!(matches!(
        ServiceConfig::load_upgrade_source(&path),
        Err(ConfigError::UnsupportedSchema { found: 8 })
    ));
}

#[test]
fn durations_are_bounded() {
    let fixture = Fixture::new();
    let max = MAX_DEADLINE.as_millis();
    for (line, key) in [
        ("worker_connect_ms = 10000", "deadlines.worker_connect_ms"),
        (
            "worker_initialize_ms = 45000",
            "deadlines.worker_initialize_ms",
        ),
        (
            "launchctl_command_ms = 10000",
            "deadlines.launchctl_command_ms",
        ),
        (
            "worker_exit_timeout_ms = 30000",
            "deadlines.worker_exit_timeout_ms",
        ),
        (
            "daemon_exit_timeout_ms = 30000",
            "deadlines.daemon_exit_timeout_ms",
        ),
        (
            "daemon_restart_throttle_ms = 5000",
            "deadlines.daemon_restart_throttle_ms",
        ),
        ("grace_ms = 5000", "sweep.grace_ms"),
        (
            "initial_startup_grace_ms = 5000",
            "input.initial_startup_grace_ms",
        ),
        ("submit_delay_ms = 150", "input.submit_delay_ms"),
        (
            "write_stall_timeout_ms = 3000",
            "input.write_stall_timeout_ms",
        ),
    ] {
        let name = line.split(" = ").next().expect("key");
        for value in [0, max + 1] {
            let text = replace(GOLDEN, line, &format!("{name} = {value}"));
            match fixture.load(&text) {
                Err(ConfigError::OutOfRange {
                    key: rejected,
                    min: 1,
                    ..
                }) => assert_eq!(rejected, key),
                other => panic!("{key} = {value}: {other:?}"),
            }
        }
        let text = replace(GOLDEN, line, &format!("{name} = {max}"));
        fixture.load(&text).expect("maximum deadline is accepted");
    }
}

/// The `[waits]` bounds and order rule, from the crate contract: each value
/// within 5000..=3600000 ms, and the default at most the ceiling.
#[test]
fn wait_timings_are_bounded_and_ordered() {
    let fixture = Fixture::new();
    let waits = |default_ms: u64, max_ms: u64| {
        replace(
            &replace(
                GOLDEN,
                "turn_default_ms = 300000",
                &format!("turn_default_ms = {default_ms}"),
            ),
            "turn_max_ms = 1800000",
            &format!("turn_max_ms = {max_ms}"),
        )
    };
    for (text, key, value) in [
        (waits(0, 1_800_000), "waits.turn_default_ms", 0),
        (waits(4_999, 1_800_000), "waits.turn_default_ms", 4_999),
        (waits(300_000, 3_600_001), "waits.turn_max_ms", 3_600_001),
        (waits(300_000, 0), "waits.turn_max_ms", 0),
        (
            waits(3_600_001, 3_600_001),
            "waits.turn_default_ms",
            3_600_001,
        ),
    ] {
        match fixture.load(&text) {
            Err(ConfigError::OutOfRange {
                key: rejected,
                value: rejected_value,
                min: 5_000,
                max: 3_600_000,
            }) => assert_eq!((rejected, rejected_value), (key, value)),
            other => panic!("{key} = {value}: {other:?}"),
        }
    }
    for (default_ms, max_ms) in [(5_000, 5_000), (5_000, 3_600_000), (3_600_000, 3_600_000)] {
        let loaded = fixture
            .load(&waits(default_ms, max_ms))
            .expect("bounds are inclusive");
        assert_eq!(
            loaded.wait_timing(),
            WaitTiming {
                turn_default: Duration::from_millis(default_ms),
                turn_max: Duration::from_millis(max_ms),
                keepalive: DEFAULT_KEEPALIVE_TIMING,
            }
        );
    }

    let error = fixture
        .load(&waits(1_800_001, 1_800_000))
        .expect_err("a default above the ceiling is refused");
    assert!(
        matches!(
            error,
            ConfigError::WaitDefaultAboveMax {
                default_ms: 1_800_001,
                max_ms: 1_800_000,
            }
        ),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("waits.turn_default_ms") && message.contains("waits.turn_max_ms"),
        "{message}"
    );
}

#[test]
fn custom_wait_timing_is_loaded_and_preserved_on_write() {
    let fixture = Fixture::new();
    let custom = replace(
        &replace(
            GOLDEN,
            "turn_default_ms = 300000",
            "turn_default_ms = 120000",
        ),
        "turn_max_ms = 1800000",
        "turn_max_ms = 2700000",
    );
    let loaded = fixture.load(&custom).expect("load custom wait timing");
    assert_eq!(
        loaded.wait_timing(),
        WaitTiming {
            turn_default: Duration::from_secs(120),
            turn_max: Duration::from_mins(45),
            keepalive: DEFAULT_KEEPALIVE_TIMING,
        }
    );
    loaded
        .write(&fixture.path())
        .expect("write custom wait timing");
    let written = fs::read_to_string(fixture.path()).expect("read written config");
    assert!(written.contains("turn_default_ms = 120000"), "{written}");
    assert!(written.contains("turn_max_ms = 2700000"), "{written}");
    assert_eq!(
        ServiceConfig::load(&fixture.path()).expect("reload written config"),
        loaded
    );
}

#[test]
fn custom_host_input_timing_is_loaded_and_preserved_on_write() {
    let fixture = Fixture::new();
    let custom = replace(
        &replace(
            GOLDEN,
            "initial_startup_grace_ms = 5000",
            "initial_startup_grace_ms = 11000",
        ),
        "submit_delay_ms = 150",
        "submit_delay_ms = 275",
    );
    let loaded = fixture.load(&custom).expect("load custom host timing");
    assert_eq!(
        loaded.input_timing().initial_startup_grace,
        Duration::from_secs(11)
    );
    assert_eq!(
        loaded.input_timing().submit_delay,
        Duration::from_millis(275)
    );
    loaded
        .write(&fixture.path())
        .expect("write custom host timing");
    let written = fs::read_to_string(fixture.path()).expect("read written config");
    assert!(written.contains("initial_startup_grace_ms = 11000"));
    assert!(written.contains("submit_delay_ms = 275"));
}

#[test]
fn open_files_is_bounded() {
    let fixture = Fixture::new();
    for value in [0, 255, 1_048_577] {
        let text = replace(
            GOLDEN,
            "open_files = 8192",
            &format!("open_files = {value}"),
        );
        assert!(
            matches!(
                fixture.load(&text),
                Err(ConfigError::OutOfRange {
                    key: "limits.open_files",
                    min: 256,
                    max: 1_048_576,
                    ..
                })
            ),
            "{value}"
        );
    }
    for value in [256, 1_048_576] {
        let text = replace(
            GOLDEN,
            "open_files = 8192",
            &format!("open_files = {value}"),
        );
        fixture.load(&text).expect("bound is inclusive");
    }
}

#[test]
fn hook_stderr_tail_is_bounded_and_zero_disables_the_capture() {
    let fixture = Fixture::new();
    let with = |value: u32| {
        replace(
            GOLDEN,
            "stderr_tail_bytes = 4096",
            &format!("stderr_tail_bytes = {value}"),
        )
    };
    assert!(
        matches!(
            fixture.load(&with(MAX_HOOK_STDERR_TAIL_BYTES + 1)),
            Err(ConfigError::OutOfRange {
                key: "hooks.stderr_tail_bytes",
                min: 0,
                max: 32_768,
                ..
            })
        ),
        "above the ceiling"
    );
    for value in [0, MAX_HOOK_STDERR_TAIL_BYTES] {
        let loaded = fixture.load(&with(value)).expect("bound is inclusive");
        assert_eq!(loaded.hook_stderr_tail_bytes(), value);
        loaded.write(&fixture.path()).expect("write hooks value");
        let written = fs::read_to_string(fixture.path()).expect("read written config");
        assert!(
            written.contains(&format!("stderr_tail_bytes = {value}")),
            "{written}"
        );
    }
}

/// `GOLDEN` with `required = <entries>` added to its `[hooks]` table.
fn with_required(entries: &str) -> String {
    replace(
        GOLDEN,
        "stderr_tail_bytes = 4096",
        &format!("stderr_tail_bytes = 4096\nrequired = {entries}"),
    )
}

#[test]
fn required_post_create_round_trips_and_is_not_an_unknown_key() {
    let fixture = Fixture::new();
    let loaded = fixture
        .load(&with_required("[\"post-create\"]"))
        .expect("a required post-create loads");
    assert_eq!(loaded.hook_required(), [RequiredHook::PostCreate]);
    assert!(loaded.unknown_keys().is_empty(), "required is a known key");
    loaded.write(&fixture.path()).expect("write the config");
    let written = fs::read_to_string(fixture.path()).expect("read the written config");
    assert!(
        written.contains("required = [\"post-create\"]"),
        "{written}"
    );
    let reloaded = ServiceConfig::load(&fixture.path()).expect("reload the written config");
    assert_eq!(reloaded.hook_required(), [RequiredHook::PostCreate]);
}

#[test]
fn an_absent_or_empty_required_list_requires_nothing_and_is_not_written() {
    let fixture = Fixture::new();
    for text in [GOLDEN.to_owned(), with_required("[]")] {
        let loaded = fixture.load(&text).expect("no required hook loads");
        assert!(loaded.hook_required().is_empty());
        loaded.write(&fixture.path()).expect("write the config");
        let written = fs::read_to_string(fixture.path()).expect("read the written config");
        assert!(
            !written.contains("required ="),
            "a previous release must still read the file: {written}"
        );
    }
}

#[test]
fn an_invalid_required_list_names_its_key_and_entry_without_echoing_the_value() {
    let fixture = Fixture::new();
    let unknown = fixture.load(&with_required("[\"after-create\"]"));
    let Err(ConfigError::InvalidHookEvent { index: 0 }) = unknown else {
        panic!("an unknown event is refused: {unknown:?}");
    };
    let message = unknown.expect_err("refused").to_string();
    assert!(message.contains("hooks.required entry 0"), "{message}");
    assert!(!message.contains("after-create"), "{message}");

    for event in ["pre-create", "pre-remove", "post-remove", "session-start"] {
        let result = fixture.load(&with_required(&format!("[\"{event}\"]")));
        assert!(
            matches!(
                result,
                Err(ConfigError::UnsupportedRequiredHook { index: 0, event: named }) if named == event
            ),
            "{event}: {result:?}"
        );
    }

    let duplicate = fixture.load(&with_required("[\"post-create\", \"post-create\"]"));
    assert!(
        matches!(
            duplicate,
            Err(ConfigError::DuplicateRequiredHook { index: 1, first: 0 })
        ),
        "{duplicate:?}"
    );

    let mistyped = fixture.load(&with_required("\"post-create\""));
    assert!(
        matches!(mistyped, Err(ConfigError::Parse { .. })),
        "{mistyped:?}"
    );

    let non_string = fixture.load(&with_required("[7]"));
    assert!(
        matches!(non_string, Err(ConfigError::Parse { .. })),
        "{non_string:?}"
    );
}

#[test]
fn paths_must_be_absolute_and_normalized() {
    let fixture = Fixture::new();
    for (line, key) in [
        ("prefix = \"/home/u/.local\"", "prefix"),
        (
            "state_root = \"/home/u/.local/state/pohunek\"",
            "namespace.state_root",
        ),
        (
            "runtime_root = \"/run/user/1000/pohunek\"",
            "namespace.runtime_root",
        ),
    ] {
        let name = line.split(" = ").next().expect("key");
        for bad in [
            "relative/dir",
            "",
            "/a/../b",
            "/a/./b",
            "/a//b",
            "/a/b/",
            "./a",
            "/a/\\u0000b",
        ] {
            let text = replace(GOLDEN, line, &format!("{name} = \"{bad}\""));
            match fixture.load(&text) {
                Err(ConfigError::InvalidPath { key: rejected, .. }) => {
                    assert_eq!(rejected, key, "{bad}");
                }
                other => panic!("{key} = {bad:?}: {other:?}"),
            }
        }
        let long = format!("/{}", "a".repeat(4096));
        let text = replace(GOLDEN, line, &format!("{name} = \"{long}\""));
        assert!(matches!(
            fixture.load(&text),
            Err(ConfigError::InvalidPath { .. })
        ));
    }
}

#[test]
fn install_versions_must_be_one_safe_component() {
    let fixture = Fixture::new();
    let too_long = "1".repeat(65);
    for bad in [
        "",
        ".",
        "..",
        "../x",
        "a/b",
        "0.31 6",
        "v:1",
        too_long.as_str(),
    ] {
        let text = replace(
            GOLDEN,
            "active_version = \"0.31.6\"",
            &format!("active_version = \"{bad}\""),
        );
        match fixture.load(&text) {
            Err(ConfigError::InvalidVersion { length, .. }) => assert_eq!(length, bad.len()),
            other => panic!("{bad:?}: {other:?}"),
        }
    }
    let text = replace(
        GOLDEN,
        "active_version = \"0.31.6\"",
        "active_version = \"0.32.0-rc.1+build.7\"",
    );
    fixture
        .load(&text)
        .expect("semver-like version is accepted");
}

#[test]
fn reserved_uid_is_rejected() {
    let fixture = Fixture::new();
    let text = replace(GOLDEN, "uid = 1000", "uid = 4294967295");
    assert!(matches!(
        fixture.load(&text),
        Err(ConfigError::InvalidUid { uid: u32::MAX })
    ));
}

#[test]
fn allowlist_patterns_are_names_or_trailing_prefixes() {
    let fixture = Fixture::new();
    let with_allowlist = |entries: &[&str]| {
        let rendered: Vec<String> = entries.iter().map(|entry| format!("{entry:?}")).collect();
        let start = GOLDEN.find("allowlist = ").expect("allowlist");
        let end = GOLDEN.find("\n\n[sweep]").expect("allowlist end");
        format!(
            "{}allowlist = [{}]{}",
            &GOLDEN[..start],
            rendered.join(", "),
            &GOLDEN[end..]
        )
    };

    let long_pattern = "A".repeat(MAX_PATTERN_BYTES + 1);
    for bad in [
        "*",
        "",
        "1PATH",
        "PA-TH",
        "PA*TH",
        "PATH**",
        "PÄTH",
        " PATH",
        "PATH=1",
        long_pattern.as_str(),
    ] {
        match fixture.load(&with_allowlist(&["HOME", bad])) {
            Err(ConfigError::InvalidPattern { index, length, .. }) => {
                assert_eq!((index, length), (1, bad.len()), "{bad:?}");
            }
            other => panic!("{bad:?}: {other:?}"),
        }
    }

    assert!(matches!(
        fixture.load(&with_allowlist(&[])),
        Err(ConfigError::AllowlistSize { count: 0, .. })
    ));
    let many: Vec<String> = (0..=MAX_ALLOWLIST_ENTRIES)
        .map(|index| format!("VAR_{index}"))
        .collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert!(matches!(
        fixture.load(&with_allowlist(&many)),
        Err(ConfigError::AllowlistSize { count, .. }) if count == MAX_ALLOWLIST_ENTRIES + 1
    ));
    assert!(matches!(
        fixture.load(&with_allowlist(&["PATH", "HOME", "PATH"])),
        Err(ConfigError::DuplicatePattern { index: 2, first: 0 })
    ));

    let longest = "A".repeat(MAX_PATTERN_BYTES - 1) + "*";
    let config = fixture
        .load(&with_allowlist(&[
            "_",
            "__CFBundleIdentifier",
            "LC_*",
            "X*",
            &longest,
        ]))
        .expect("valid patterns are accepted");
    assert_eq!(config.environment_allowlist()[4], longest);
    let maximum = &many[..MAX_ALLOWLIST_ENTRIES];
    fixture
        .load(&with_allowlist(maximum))
        .expect("the maximum entry count is accepted");
}

#[test]
fn group_or_world_accessible_files_are_rejected() {
    let fixture = Fixture::new();
    for mode in [0o644, 0o640, 0o604, 0o660, 0o400, 0o700] {
        let path = fixture.write(GOLDEN, mode);
        assert!(
            matches!(
                ServiceConfig::load(&path),
                Err(ConfigError::Untrusted { .. })
            ),
            "{mode:o}"
        );
    }
}

#[test]
fn readable_owner_directories_are_accepted() {
    let fixture = Fixture::new();
    let path = fixture.write(GOLDEN, 0o600);
    for mode in [0o700, 0o750, 0o755] {
        fs::set_permissions(&fixture.config_dir, fs::Permissions::from_mode(mode))
            .expect("chmod config dir");
        ServiceConfig::load(&path).unwrap_or_else(|error| panic!("{mode:o}: {error}"));
    }
}

#[test]
fn writable_by_others_directories_are_rejected() {
    let fixture = Fixture::new();
    let path = fixture.write(GOLDEN, 0o600);
    for mode in [0o775, 0o777, 0o757, 0o1777] {
        fs::set_permissions(&fixture.config_dir, fs::Permissions::from_mode(mode))
            .expect("chmod config dir");
        let result = ServiceConfig::load(&path);
        assert!(
            matches!(result, Err(ConfigError::Untrusted { .. })),
            "{mode:o}: {result:?}"
        );
    }
}

/// Returns whether the tests run as root, which owns the system directories.
fn running_as_root() -> bool {
    let probe = pohunek_test_support::tempdir().expect("create temp dir");
    fs::metadata(probe.path()).expect("stat temp dir").uid() == 0
}

#[test]
fn foreign_owned_directories_are_rejected() {
    // `/usr` is a real, root-owned, not world-writable directory on Linux and
    // Darwin, so it is foreign-owned for any unprivileged user. A test run as
    // root cannot produce a foreign owner without `chown`, so it skips.
    if running_as_root() {
        return;
    }
    let path = Path::new("/usr/service.toml");
    let result = ServiceConfig::load(path);
    assert!(
        matches!(
            result,
            Err(ConfigError::Untrusted {
                source: FsError::UnsafeOwner { .. },
                ..
            })
        ),
        "{result:?}"
    );
    let config = ServiceConfig::new(spec()).expect("valid spec");
    let result = config.write(path);
    assert!(
        matches!(
            result,
            Err(ConfigError::Untrusted {
                source: FsError::UnsafeOwner { .. },
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn symlinks_are_rejected() {
    let fixture = Fixture::new();
    let target = fixture.config_dir.join("real.toml");
    fs::write(&target, GOLDEN).expect("write target");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("chmod target");
    symlink(&target, fixture.path()).expect("create symlink");
    assert!(matches!(
        ServiceConfig::load(&fixture.path()),
        Err(ConfigError::Untrusted { .. })
    ));

    let linked_dir = fixture.root.join("linked");
    symlink(&fixture.config_dir, &linked_dir).expect("create dir symlink");
    fs::remove_file(fixture.path()).expect("remove file symlink");
    fixture.write(GOLDEN, 0o600);
    // The no-follow directory open fails with an OS error (ENOTDIR on Linux,
    // ELOOP on Darwin) rather than a typed trust violation; either way the
    // symlinked directory is never traversed.
    let result = ServiceConfig::load(&linked_dir.join("service.toml"));
    assert!(
        matches!(
            result,
            Err(ConfigError::Io { .. } | ConfigError::Untrusted { .. })
        ),
        "{result:?}"
    );
}

#[test]
fn hard_links_are_rejected() {
    let fixture = Fixture::new();
    let path = fixture.write(GOLDEN, 0o600);
    fs::hard_link(&path, fixture.config_dir.join("alias.toml")).expect("create hard link");
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::Untrusted { .. })
    ));
}

#[test]
fn oversized_files_are_rejected() {
    let fixture = Fixture::new();
    let padding = format!("# {}\n", "x".repeat(MAX_CONFIG_BYTES));
    assert!(matches!(
        fixture.load(&format!("{padding}{GOLDEN}")),
        Err(ConfigError::TooLarge { max_bytes, .. }) if max_bytes == MAX_CONFIG_BYTES
    ));
}

#[test]
fn non_utf8_files_are_parse_errors() {
    let fixture = Fixture::new();
    let path = fixture.path();
    let mut bytes = GOLDEN.as_bytes().to_vec();
    bytes.extend_from_slice(b"# \xff\n");
    fs::write(&path, bytes).expect("write config");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod config");
    assert!(matches!(
        ServiceConfig::load(&path),
        Err(ConfigError::Parse { .. })
    ));
}

#[test]
fn config_paths_must_be_absolute_and_present() {
    let fixture = Fixture::new();
    for bad in ["service.toml", "/", "/a/../service.toml", "/a/b/"] {
        assert!(
            matches!(
                ServiceConfig::load(Path::new(bad)),
                Err(ConfigError::ConfigPath { .. })
            ),
            "{bad}"
        );
    }
    match ServiceConfig::load(&fixture.path()) {
        Err(ConfigError::Io { source, .. }) => {
            assert_eq!(source.io_kind(), Some(std::io::ErrorKind::NotFound));
        }
        other => panic!("missing file: {other:?}"),
    }
}

#[test]
fn write_creates_a_private_file_in_a_private_directory() {
    let fixture = Fixture::new();
    let config = ServiceConfig::new(spec()).expect("valid spec");
    let path = fixture
        .root
        .join("fresh")
        .join("pohunek")
        .join("service.toml");
    config.write(&path).expect("write config");

    let file_mode = fs::metadata(&path).expect("stat file").permissions().mode() & 0o7777;
    let dir_mode = fs::metadata(path.parent().expect("parent"))
        .expect("stat dir")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(file_mode, 0o600);
    assert_eq!(dir_mode, 0o700);
    assert_eq!(ServiceConfig::load(&path).expect("reload"), config);

    let mut upgraded = config.to_spec();
    upgraded.active_version = "0.32.0".to_owned();
    let upgraded = ServiceConfig::new(upgraded).expect("valid upgrade");
    upgraded.write(&path).expect("replace config");
    assert_eq!(ServiceConfig::load(&path).expect("reload"), upgraded);
    let leftovers: Vec<_> = fs::read_dir(path.parent().expect("parent"))
        .expect("list dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(leftovers, ["service.toml"]);
}

#[test]
fn write_keeps_an_existing_readable_directory_mode() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.config_dir, fs::Permissions::from_mode(0o755))
        .expect("chmod config dir");
    let config = ServiceConfig::new(spec()).expect("valid spec");
    config.write(&fixture.path()).expect("write config");
    let dir_mode = fs::metadata(&fixture.config_dir)
        .expect("stat dir")
        .permissions()
        .mode()
        & 0o7777;
    let file_mode = fs::metadata(fixture.path())
        .expect("stat file")
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(dir_mode, 0o755);
    assert_eq!(file_mode, 0o600);
    assert_eq!(
        ServiceConfig::load(&fixture.path()).expect("reload"),
        config
    );
}

#[test]
fn write_refuses_a_directory_writable_by_others() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.config_dir, fs::Permissions::from_mode(0o775))
        .expect("chmod config dir");
    let config = ServiceConfig::new(spec()).expect("valid spec");
    assert!(matches!(
        config.write(&fixture.path()),
        Err(ConfigError::Untrusted { .. })
    ));
    assert!(!fixture.path().exists());
}

#[test]
fn namespace_uses_the_recorded_canonical_roots() {
    let fixture = Fixture::new();
    let state = fixture.dir("state/pohunek");
    let runtime = fixture.dir("runtime/pohunek");
    let mut value = spec();
    value.state_root.clone_from(&state);
    value.runtime_root.clone_from(&runtime);
    let config = ServiceConfig::new(value).expect("valid spec");

    assert_eq!(
        config.namespace(),
        Namespace::derive(
            1000,
            &fs::canonicalize(&state).expect("canonical state"),
            &fs::canonicalize(&runtime).expect("canonical runtime"),
        )
    );
    config
        .verify_installation(1000, &state, &runtime)
        .expect("same installation verifies");

    // An equivalent spelling through a symlink canonicalizes to the same root.
    let alias = fixture.root.join("state-alias");
    symlink(&state, &alias).expect("create alias");
    config
        .verify_installation(1000, &alias, &runtime)
        .expect("aliased root verifies");
}

#[test]
fn verify_installation_rejects_another_installation() {
    let fixture = Fixture::new();
    let state = fixture.dir("state/pohunek");
    let runtime = fixture.dir("runtime/pohunek");
    let other = fixture.dir("other/pohunek");
    let mut value = spec();
    value.state_root.clone_from(&state);
    value.runtime_root.clone_from(&runtime);
    let config = ServiceConfig::new(value).expect("valid spec");

    assert!(matches!(
        config.verify_installation(1001, &state, &runtime),
        Err(ConfigError::NamespaceMismatch {
            key: "namespace.uid",
            ..
        })
    ));
    match config.verify_installation(1000, &other, &runtime) {
        Err(ConfigError::NamespaceMismatch {
            key: "namespace.state_root",
            recorded,
            actual,
        }) => {
            assert_eq!(recorded, state.display().to_string());
            assert_eq!(actual, other.display().to_string());
        }
        result => panic!("state mismatch: {result:?}"),
    }
    assert!(matches!(
        config.verify_installation(1000, &state, &other),
        Err(ConfigError::NamespaceMismatch {
            key: "namespace.runtime_root",
            ..
        })
    ));
    assert!(matches!(
        config.verify_installation(1000, &state, &fixture.root.join("missing")),
        Err(ConfigError::Canonicalize {
            key: "namespace.runtime_root",
            ..
        })
    ));
}

#[test]
fn search_path_round_trips_through_write_and_load() {
    let fixture = Fixture::new();
    let search = SearchPath::new(vec![
        PathBuf::from("/opt/my tools/bin"),
        PathBuf::from("/Users/j\u{e9}/it's \"here\""),
        PathBuf::from("/usr/bin"),
    ])
    .expect("search path");
    let config = ServiceConfig::new(ConfigSpec {
        search_path: search.clone(),
        ..spec()
    })
    .expect("valid spec");
    assert_eq!(config.to_spec().search_path, search);
    config.write(&fixture.path()).expect("write config");
    let loaded = ServiceConfig::load(&fixture.path()).expect("load config");
    assert_eq!(loaded.search_path(), &search);
    assert_eq!(loaded, config);
    assert_eq!(loaded.to_toml(), config.to_toml());
}

#[test]
fn an_empty_search_path_is_recorded_explicitly() {
    let fixture = Fixture::new();
    let config = ServiceConfig::new(spec()).expect("valid spec");
    assert!(config.search_path().is_empty());
    assert!(config.to_toml().contains("search_path = []"));
    config.write(&fixture.path()).expect("write config");
    assert!(ServiceConfig::load(&fixture.path())
        .expect("load config")
        .search_path()
        .is_empty());
}

#[test]
fn invalid_search_paths_are_rejected_without_quoting_the_value() {
    let fixture = Fixture::new();
    let secret = SENTINEL_TOKEN;
    let long = format!("/{}", "x".repeat(MAX_SEARCH_PATH_BYTES));
    for entries in [
        format!("\"{secret}\""),
        "\"\"".to_owned(),
        "\"/usr/bin\", \".\"".to_owned(),
        "\"/usr/../bin\"".to_owned(),
        "\"//usr/bin\"".to_owned(),
        "\"/usr//bin\"".to_owned(),
        "\"/usr/bin/\"".to_owned(),
        "\"/usr/./bin\"".to_owned(),
        "\"/a:/b\"".to_owned(),
        "\"/a\\u001bb\"".to_owned(),
        "\"/usr/bin\", \"/usr/bin\"".to_owned(),
        format!("\"{long}\""),
    ] {
        let text = replace(
            GOLDEN,
            "search_path = [\"/opt/homebrew/bin\", \"/Users/u/.local/bin\", \"/usr/bin\"]",
            &format!("search_path = [{entries}]"),
        );
        match fixture.load(&text) {
            Err(error @ ConfigError::InvalidSearchPath { .. }) => {
                let rendered = format!("{error} {error:?}");
                assert!(!rendered.contains(secret), "{rendered}");
                assert!(!rendered.contains("/a:/b"), "{rendered}");
            }
            other => panic!("{entries}: expected InvalidSearchPath, got {other:?}"),
        }
    }
}

/// A file written before `environment.search_path` existed is refused up front
/// with an actionable message instead of a missing-field parse error.
#[test]
fn a_schema_one_file_is_rejected_with_an_actionable_error() {
    let fixture = Fixture::new();
    let without_search_path = GOLDEN
        .replacen("schema_version = 7", "schema_version = 1", 1)
        .lines()
        .filter(|line| !line.starts_with("search_path = "))
        .collect::<Vec<_>>()
        .join("\n");
    let error = fixture
        .load(&without_search_path)
        .expect_err("schema 1 is unsupported");
    assert!(
        matches!(error, ConfigError::UnsupportedSchema { found: 1 }),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(message.contains("pohunek service uninstall"), "{message}");
    assert!(message.contains("pohunek service install"), "{message}");
    assert!(!message.contains("missing field"), "{message}");
}

// ---------------------------------------------------------------------------
// Tolerant loading (#1401): unknown keys warn, the security tables stay strict.
// ---------------------------------------------------------------------------

/// Paths of the unknown keys a config reports, in report order.
fn unknown_paths(config: &ServiceConfig) -> Vec<&str> {
    config
        .unknown_keys()
        .iter()
        .map(|key| key.path.as_str())
        .collect()
}

/// 1-based line of the first line of `text` that starts with `needle`.
fn line_of(text: &str, needle: &str) -> usize {
    text.lines()
        .position(|line| line.starts_with(needle))
        .unwrap_or_else(|| panic!("fixture lacks a line starting with {needle:?}"))
        + 1
}

/// Value that stands in for a secret a hand-edited file could hold.
const UNKNOWN_SECRET: &str = "PH-UNKNOWN-sentinel-51c2-b7d0";

#[test]
fn unknown_keys_in_tolerant_tables_load_and_are_reported() {
    let fixture = Fixture::new();
    let text = [
        ("prefix =", "future_top = \"x\"\nprefix ="),
        (
            "worker_connect_ms",
            "future_deadline = 1\nworker_connect_ms",
        ),
        ("grace_ms = 5000", "grace_ms = 5000\nfuture_sweep = 1"),
        ("submit_delay_ms", "future_input = 1\nsubmit_delay_ms"),
        (
            "turn_max_ms = 1800000",
            "turn_max_ms = 1800000\nfuture_wait = 1",
        ),
        (
            "stderr_tail_bytes = 4096",
            "stderr_tail_bytes = 4096\nfuture_hook = 1",
        ),
        ("open_files = 8192", "open_files = 8192\nfuture_limit = 1"),
    ]
    .iter()
    .fold(GOLDEN.to_owned(), |text, (from, to)| {
        replace(&text, from, to)
    });

    let config = fixture
        .load(&text)
        .expect("tolerant tables accept unknown keys");

    let mut expected = vec![
        ("future_top", line_of(&text, "future_top")),
        (
            "deadlines.future_deadline",
            line_of(&text, "future_deadline"),
        ),
        ("sweep.future_sweep", line_of(&text, "future_sweep")),
        ("input.future_input", line_of(&text, "future_input")),
        ("waits.future_wait", line_of(&text, "future_wait")),
        ("hooks.future_hook", line_of(&text, "future_hook")),
        ("limits.future_limit", line_of(&text, "future_limit")),
    ];
    let mut reported: Vec<(&str, usize)> = config
        .unknown_keys()
        .iter()
        .map(|key| (key.path.as_str(), key.line))
        .collect();
    expected.sort_unstable();
    reported.sort_unstable();
    assert_eq!(reported, expected);

    // Known values are untouched by the unknown keys around them.
    let baseline = fixture.load(GOLDEN).expect("golden loads");
    assert!(baseline.unknown_keys().is_empty());
    assert_eq!(config.to_spec(), baseline.to_spec());
}

#[test]
fn an_unknown_section_loads_and_is_reported() {
    let fixture = Fixture::new();
    let text = format!(
        "{GOLDEN}\n[future]\nflag = true\n\n[future.nested]\ndeep = 1\n\n[[future_list]]\nitem = 1\n"
    );
    let config = fixture.load(&text).expect("an unknown section loads");

    let mut paths = unknown_paths(&config);
    paths.sort_unstable();
    assert_eq!(
        paths,
        [
            "future",
            "future.flag",
            "future.nested",
            "future.nested.deep",
            "future_list"
        ]
    );
    let section = config
        .unknown_keys()
        .iter()
        .find(|key| key.path == "future.nested.deep")
        .expect("nested key");
    assert_eq!(section.line, line_of(&text, "deep"));
}

#[test]
fn unknown_keys_in_security_tables_are_refused() {
    let fixture = Fixture::new();
    for text in [
        replace(GOLDEN, "uid = 1000", "uid = 1000\nsurprise = 1"),
        replace(
            GOLDEN,
            "[environment]\n",
            "[environment]\nsurprise = [\"A\"]\n",
        ),
        format!("{GOLDEN}\n[namespace.sub]\nvalue = 1\n"),
    ] {
        let message = parse_message(fixture.load(&text));
        assert!(
            message.contains("unknown field `surprise`") || message.contains("`sub`"),
            "{message}"
        );
        assert!(message.contains("line "), "{message}");
    }
}

#[test]
fn a_typo_of_a_required_key_fails_with_a_suggestion() {
    let fixture = Fixture::new();
    let text = replace(
        GOLDEN,
        "stderr_tail_bytes = 4096",
        "stderr_tail_byte = 4096",
    );
    let message = parse_message(fixture.load(&text));
    assert!(
        message.contains("missing field `stderr_tail_bytes`"),
        "{message}"
    );
    assert!(
        message.contains("did you mean `hooks.stderr_tail_bytes`"),
        "{message}"
    );
    assert!(
        message.contains("`hooks.stderr_tail_byte` is not a key"),
        "{message}"
    );

    // A top-level typo of a required table is hinted the same way.
    let table = replace(GOLDEN, "[limits]", "[limit]");
    let message = parse_message(fixture.load(&table));
    assert!(message.contains("missing field `limits`"), "{message}");
    assert!(message.contains("did you mean `limits`"), "{message}");

    // A key that resembles nothing gets no hint.
    let far = replace(GOLDEN, "stderr_tail_bytes = 4096", "unrelated = 4096");
    let message = parse_message(fixture.load(&far));
    assert!(
        message.contains("missing field `stderr_tail_bytes`"),
        "{message}"
    );
    assert!(!message.contains("did you mean"), "{message}");
}

#[test]
fn a_near_miss_unknown_key_warns_with_a_suggestion() {
    let fixture = Fixture::new();
    let text = replace(
        GOLDEN,
        "open_files = 8192",
        "open_files = 8192\nopen_file = 1",
    );
    let config = fixture.load(&text).expect("a near miss only warns");

    let key = config
        .unknown_keys()
        .iter()
        .find(|key| key.path == "limits.open_file")
        .expect("reported");
    assert_eq!(key.suggestion.as_deref(), Some("limits.open_files"));
    assert!(
        key.to_string().contains("did you mean `limits.open_files`"),
        "{key}"
    );
    assert_eq!(config.open_files(), 8192);
}

#[test]
fn unknown_keys_never_mask_validation_errors() {
    let fixture = Fixture::new();
    let text = replace(
        &replace(
            GOLDEN,
            "open_files = 8192",
            "open_files = 1\nfuture_limit = 1",
        ),
        "prefix =",
        "future_top = 1\nprefix =",
    );
    match fixture.load(&text) {
        Err(ConfigError::OutOfRange { key, .. }) => assert_eq!(key, "limits.open_files"),
        other => panic!("expected the open_files range error, got {other:?}"),
    }
}

#[test]
fn the_schema_version_rule_is_unchanged_for_tolerant_files() {
    let fixture = Fixture::new();
    let newer = replace(
        GOLDEN,
        "schema_version = 7",
        "schema_version = 8\nfuture_key = true",
    );
    assert!(matches!(
        fixture.load(&newer),
        Err(ConfigError::UnsupportedSchema { found: 8 })
    ));
    let missing = GOLDEN.replacen("schema_version = 7\n", "future_key = 1\n", 1);
    let message = parse_message(fixture.load(&missing));
    assert!(
        message.contains("missing field `schema_version`"),
        "{message}"
    );
}

#[test]
fn previous_schema_sources_stay_strict() {
    let fixture = Fixture::new();
    let path = fixture.path();
    for (version, golden, shared_table_line) in [
        (2, SCHEMA_TWO_GOLDEN, "grace_ms = 5000"),
        (3, SCHEMA_THREE_GOLDEN, "grace_ms = 5000"),
        (4, SCHEMA_FOUR_GOLDEN, "grace_ms = 5000"),
        (5, SCHEMA_FIVE_GOLDEN, "grace_ms = 5000"),
        (6, SCHEMA_SIX_GOLDEN, "grace_ms = 5000"),
    ] {
        fixture.write(golden, 0o600);
        ServiceConfig::load_upgrade_source(&path)
            .unwrap_or_else(|error| panic!("schema {version} golden loads: {error}"));
        for text in [
            replace(golden, "prefix =", "future_top = 1\nprefix ="),
            replace(
                golden,
                shared_table_line,
                &format!("{shared_table_line}\nfuture_sweep = 1"),
            ),
            format!("{golden}\n[future]\nvalue = 1\n"),
        ] {
            fixture.write(&text, 0o600);
            match ServiceConfig::load_upgrade_source(&path) {
                Err(ConfigError::Parse { .. }) => {}
                other => panic!("schema {version} with an unknown key: {other:?}"),
            }
            match ServiceConfig::load_upgrade_source_bytes(&path, text.as_bytes()) {
                Err(ConfigError::Parse { .. }) => {}
                other => panic!("schema {version} bytes with an unknown key: {other:?}"),
            }
        }
    }
}

/// Header every rendering starts with.
const SPEC_HEADER: &str =
    "# Pohunek service configuration, written by `pohunek service install`.\n\
# Every key is required. Unknown keys are kept and reported as warnings,\n\
# except inside [namespace] and [environment], where they are rejected.\n\n";

/// Body of today's rendering of `spec()`, frozen: a file with only schema-7
/// keys must stay byte-identical below the header, so a strict previous
/// release reads every file this release writes.
const SPEC_BODY: &str = r#"schema_version = 7
prefix = "/home/u/.local"
active_version = "0.31.6"

[namespace]
uid = 1000
state_root = "/home/u/.local/state/pohunek"
runtime_root = "/run/user/1000/pohunek"

[deadlines]
worker_connect_ms = 10000
worker_initialize_ms = 45000
launchctl_command_ms = 10000
worker_exit_timeout_ms = 30000
daemon_exit_timeout_ms = 30000
daemon_restart_throttle_ms = 5000

[environment]
search_path = []
allowlist = ["PATH", "LC_*"]

[sweep]
grace_ms = 5000

[input]
initial_startup_grace_ms = 5000
submit_delay_ms = 150
write_stall_timeout_ms = 3000

[waits]
turn_default_ms = 300000
turn_max_ms = 1800000

[hooks]
stderr_tail_bytes = 4096

[events]
max_file_bytes = 33554432
max_files = 8
max_age_days = 14

[limits]
open_files = 8192
"#;

#[test]
fn a_schema_7_file_renders_unchanged() {
    let config = ServiceConfig::new(spec()).expect("valid spec");
    let rendering = format!("{SPEC_HEADER}{SPEC_BODY}");
    assert_eq!(config.to_toml(), rendering);

    let fixture = Fixture::new();
    let reloaded = fixture.load(&rendering).expect("rendering loads");
    assert!(reloaded.unknown_keys().is_empty());
    assert_eq!(reloaded.to_toml(), rendering);
}

/// Parses rendered output into a plain table for structural comparison.
fn table_of(text: &str) -> toml::Table {
    toml::from_str(text).expect("rendered output is TOML")
}

#[test]
fn rendering_preserves_unknown_keys() {
    let fixture = Fixture::new();
    let text = format!(
        "{}\n[future]\nflag = true\nname = \"{UNKNOWN_SECRET}\"\n\n[future.nested]\ndeep = [1, 2]\n",
        replace(
            &replace(GOLDEN, "prefix =", "future_top = \"top\"\nprefix ="),
            "open_files = 8192",
            "open_files = 8192\nfuture_limit = 7\n\n[limits.extra]\ninner = \"v\"",
        ),
    );
    let config = fixture.load(&text).expect("loads");
    let rendered = config.to_toml();

    let original = table_of(&text);
    let again = table_of(&rendered);
    assert_eq!(again, original, "rendered:\n{rendered}");

    // Typed values win, are written once, and the reload reports the same keys.
    assert_eq!(rendered.matches("open_files =").count(), 1, "{rendered}");
    let reloaded = fixture.load(&rendered).expect("rendered file loads");
    assert_eq!(unknown_paths(&reloaded), unknown_paths(&config));
    assert_eq!(reloaded.to_toml(), rendered, "rendering is stable");

    // A changed typed value is written from the typed value, not from the file.
    let edited = config.with_active_version("0.31.7").expect("new version");
    assert_eq!(edited.active_version(), "0.31.7");
    assert_eq!(unknown_paths(&edited), unknown_paths(&config));
    let edited_table = table_of(&edited.to_toml());
    assert_eq!(edited_table["future_top"].as_str(), Some("top"));
    assert_eq!(edited_table["active_version"].as_str(), Some("0.31.7"));
}

/// Layouts whose unknown subtrees interleave with known tables and with each
/// other, so a rewrite has to keep ancestors, dotted keys and order apart.
fn interleaved_layouts() -> Vec<String> {
    let secret = UNKNOWN_SECRET;
    let before_limits = |extra: &str| replace(GOLDEN, "[limits]", &format!("{extra}\n[limits]"));
    vec![
        // Unknown tables around a known table whose dotted unknown key has a
        // later sub-table.
        replace(
            &before_limits("[future]\nk = 1\n"),
            "open_files = 8192",
            &format!(
                "open_files = 8192\nextra.foo = \"{secret}\"\n\n[limits.extra.bar]\nb = \"{secret}\"\n\n[future.tail]\nt = 3"
            ),
        ),
        // An unknown array of tables and an unknown table inside a known one.
        replace(
            &before_limits(&format!("[[plugin]]\nname = \"{secret}\"\n\n[[plugin]]\nname = \"two\"\n")),
            "grace_ms = 5000",
            &format!("grace_ms = 5000\n\n[sweep.later]\nv = \"{secret}\"\n\n[sweep.later.deep]\nw = 1"),
        ),
        // Unknown inline values and an unknown table under the root only.
        replace(
            &replace(GOLDEN, "prefix =", &format!("top = {{ a = \"{secret}\", b = [1, 2] }}\nprefix =")),
            "[limits]",
            "[zz]\nlast = 1\n\n[limits]",
        ),
    ]
}

#[test]
fn rendering_keeps_interleaved_unknown_subtrees_valid_and_stable() {
    let fixture = Fixture::new();
    for text in interleaved_layouts() {
        let config = fixture.load(&text).expect("loads");
        let rendered = config.to_toml();
        assert_eq!(table_of(&rendered), table_of(&text), "{rendered}");
        let reloaded = fixture.load(&rendered).expect("rendered file loads");
        let sorted = |config: &ServiceConfig| {
            let mut paths = unknown_paths(config);
            paths.sort_unstable();
            paths.into_iter().map(str::to_owned).collect::<Vec<_>>()
        };
        assert_eq!(sorted(&reloaded), sorted(&config));
        assert_eq!(reloaded.to_toml(), rendered, "rendering is stable");
        let upgraded = config.with_active_version("0.31.7").expect("new version");
        fixture
            .load(&upgraded.to_toml())
            .expect("an upgraded rendering loads");
    }
}

#[test]
fn many_unknown_keys_stay_within_the_file_bound() {
    let fixture = Fixture::new();
    let mut text = format!("{GOLDEN}\n[future]\n");
    let mut count = 0usize;
    while text.len() + 16 < MAX_CONFIG_BYTES {
        writeln!(text, "k{count} = 1").expect("writing to String is infallible");
        count += 1;
    }
    assert!(text.len() <= MAX_CONFIG_BYTES);
    let config = fixture.load(&text).expect("loads within the bound");
    assert_eq!(config.unknown_keys().len(), count + 1);
    assert_eq!(
        table_of(&config.to_toml())["future"]
            .as_table()
            .expect("kept table")
            .len(),
        count
    );
}

#[test]
fn unknown_key_values_are_never_echoed_by_the_library() {
    let fixture = Fixture::new();
    let text = replace(
        GOLDEN,
        "open_files = 8192",
        &format!("open_files = 8192\nfuture_secret = \"{UNKNOWN_SECRET}\""),
    );
    let config = fixture.load(&text).expect("loads");
    let surfaces = format!(
        "{:?} {:?}",
        config,
        config
            .unknown_keys()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    );
    assert!(!surfaces.contains(UNKNOWN_SECRET), "{surfaces}");
    assert!(surfaces.contains("limits.future_secret"), "{surfaces}");
}

#[test]
fn a_rewrite_that_outgrows_the_load_bound_is_refused_and_leaves_the_file() {
    let fixture = Fixture::new();
    // A compact array just under the bound; the rendering spreads it out.
    let big = format!(
        "big = [{}]\n",
        vec!["0"; (MAX_CONFIG_BYTES - GOLDEN.len() - 64) / 2].join(",")
    );
    let text = format!("{GOLDEN}\n[future]\n{big}");
    assert!(text.len() <= MAX_CONFIG_BYTES);
    let path = fixture.write(&text, 0o600);
    let config = ServiceConfig::load(&path).expect("loads within the bound");

    assert!(
        config.to_toml().len() > MAX_CONFIG_BYTES,
        "the fixture must outgrow the bound"
    );
    let result = config.write(&path);
    assert!(
        matches!(result, Err(ConfigError::TooLarge { .. })),
        "{result:?}"
    );
    assert_eq!(fs::read_to_string(&path).expect("read"), text);
}

/// Replaces the `[waits]` table of the golden file with `extra` keyed lines
/// appended after the turn keys.
fn with_keepalive_keys(extra: &str) -> String {
    replace(
        GOLDEN,
        "turn_max_ms = 1800000",
        &format!("turn_max_ms = 1800000\n{extra}"),
    )
}

/// The `[waits]` keepalive bounds, from the crate contract: idle and interval
/// 1000..=600000 ms in whole seconds, count 1..=10.
#[test]
fn keepalive_values_outside_their_bounds_are_refused_by_key() {
    let fixture = Fixture::new();
    for (line, key, value, min, max) in [
        (
            "keepalive_idle_ms = 0",
            "waits.keepalive_idle_ms",
            0,
            1_000,
            600_000,
        ),
        (
            "keepalive_idle_ms = 600000\nkeepalive_interval_ms = 601000",
            "waits.keepalive_interval_ms",
            601_000,
            1_000,
            600_000,
        ),
        (
            "keepalive_idle_ms = 601000",
            "waits.keepalive_idle_ms",
            601_000,
            1_000,
            600_000,
        ),
        (
            "keepalive_interval_ms = 0",
            "waits.keepalive_interval_ms",
            0,
            1_000,
            600_000,
        ),
        ("keepalive_count = 0", "waits.keepalive_count", 0, 1, 10),
        ("keepalive_count = 11", "waits.keepalive_count", 11, 1, 10),
    ] {
        match fixture.load(&with_keepalive_keys(line)) {
            Err(ConfigError::OutOfRange {
                key: rejected,
                value: rejected_value,
                min: rejected_min,
                max: rejected_max,
            }) => assert_eq!(
                (rejected, rejected_value, rejected_min, rejected_max),
                (key, value, min, max),
                "{line}"
            ),
            other => panic!("{line}: {other:?}"),
        }
    }
    for line in [
        "keepalive_idle_ms = 1000\nkeepalive_interval_ms = 1000\nkeepalive_count = 1",
        "keepalive_idle_ms = 600000\nkeepalive_interval_ms = 600000\nkeepalive_count = 10",
    ] {
        fixture
            .load(&with_keepalive_keys(line))
            .unwrap_or_else(|error| panic!("{line}: bounds are inclusive: {error:?}"));
    }
}

/// The kernel options take whole seconds, so a sub-second value is refused
/// rather than rounded.
#[test]
fn keepalive_times_must_be_whole_seconds() {
    let fixture = Fixture::new();
    for (line, key) in [
        ("keepalive_idle_ms = 30500", "waits.keepalive_idle_ms"),
        (
            "keepalive_interval_ms = 10001",
            "waits.keepalive_interval_ms",
        ),
    ] {
        match fixture.load(&with_keepalive_keys(line)) {
            Err(ConfigError::WholeSeconds { key: rejected }) => assert_eq!(rejected, key),
            other => panic!("{line}: {other:?}"),
        }
    }
}

/// A file without the optional keys means the documented defaults, and a
/// rewrite does not add them, so a release that predates them still reads it.
#[test]
fn a_file_without_keepalive_keys_loads_the_defaults_and_rewrites_unchanged() {
    let fixture = Fixture::new();
    let loaded = fixture.load(GOLDEN).expect("load the golden file");
    assert_eq!(loaded.wait_timing().keepalive, DEFAULT_KEEPALIVE_TIMING);
    assert_eq!(
        DEFAULT_KEEPALIVE_TIMING.slot_lifetime_bound(),
        Duration::from_secs(60)
    );
    let rendered = loaded.to_toml();
    assert!(!rendered.contains("keepalive"), "{rendered}");
    let reloaded = fixture.load(&rendered).expect("reload the rendered file");
    assert_eq!(reloaded, loaded);
}

/// Non-default keepalive keys are loaded, written back, and known keys.
#[test]
fn non_default_keepalive_keys_round_trip_and_are_not_unknown() {
    let fixture = Fixture::new();
    let text = with_keepalive_keys(
        "keepalive_idle_ms = 45000\nkeepalive_interval_ms = 15000\nkeepalive_count = 5",
    );
    let loaded = fixture.load(&text).expect("load custom keepalive keys");
    assert_eq!(
        loaded.wait_timing().keepalive,
        KeepaliveTiming {
            idle: Duration::from_secs(45),
            interval: Duration::from_secs(15),
            count: 5,
        }
    );
    assert!(
        loaded.unknown_keys().is_empty(),
        "{:?}",
        loaded.unknown_keys()
    );
    let rendered = loaded.to_toml();
    for line in [
        "keepalive_idle_ms = 45000",
        "keepalive_interval_ms = 15000",
        "keepalive_count = 5",
    ] {
        assert!(rendered.contains(line), "{line} in {rendered}");
    }
    let reloaded = fixture.load(&rendered).expect("reload the rendered file");
    assert_eq!(reloaded, loaded);

    let only_count = fixture
        .load(&with_keepalive_keys("keepalive_count = 5"))
        .expect("a single key is enough");
    let rendered = only_count.to_toml();
    assert!(rendered.contains("keepalive_count = 5"), "{rendered}");
    assert!(!rendered.contains("keepalive_idle_ms"), "{rendered}");
}

/// Appends an `[inventory]` table with `lines` to the golden file, before `[limits]`.
fn with_inventory(lines: &str) -> String {
    replace(
        GOLDEN,
        "[limits]",
        &format!("[inventory]\n{lines}\n\n[limits]"),
    )
}

/// A file without `[inventory]` means the documented defaults, and a rewrite
/// does not add the table, so a release that predates it still reads the file.
#[test]
fn a_file_without_inventory_keys_loads_the_defaults_and_rewrites_unchanged() {
    let fixture = Fixture::new();
    let loaded = fixture.load(GOLDEN).expect("load the golden file");
    assert_eq!(loaded.inventory_probe(), DEFAULT_INVENTORY_PROBE);
    let rendered = loaded.to_toml();
    assert!(!rendered.contains("inventory"), "{rendered}");
    assert_eq!(fixture.load(&rendered).expect("reload"), loaded);
}

/// Non-default inventory keys are loaded, written back, and known keys; a
/// single key leaves the others at their defaults and out of the rewrite.
#[test]
fn non_default_inventory_keys_round_trip_and_are_not_unknown() {
    let fixture = Fixture::new();
    let text = with_inventory(
        "probe_cache_ttl_ms = 60000\nprobe_failure_ttl_ms = 2000\nprobe_cache_max_entries = 8\nprobe_parallelism = 4",
    );
    let loaded = fixture.load(&text).expect("load custom inventory keys");
    assert_eq!(
        loaded.inventory_probe(),
        InventoryProbe {
            cache_ttl: Duration::from_secs(60),
            failure_ttl: Duration::from_secs(2),
            cache_max_entries: 8,
            parallelism: 4,
        }
    );
    assert!(
        loaded.unknown_keys().is_empty(),
        "{:?}",
        loaded.unknown_keys()
    );
    let rendered = loaded.to_toml();
    assert!(rendered.contains("[inventory]"), "{rendered}");
    assert_eq!(fixture.load(&rendered).expect("reload"), loaded);

    let only_parallelism = fixture
        .load(&with_inventory("probe_parallelism = 4"))
        .expect("a single key is enough");
    let rendered = only_parallelism.to_toml();
    assert!(rendered.contains("probe_parallelism = 4"), "{rendered}");
    assert!(!rendered.contains("probe_cache_ttl_ms"), "{rendered}");
}

/// Each inventory key is bounded, and the bounds are inclusive.
#[test]
fn inventory_values_outside_their_bounds_are_refused_by_key() {
    let fixture = Fixture::new();
    for (line, key, value, min, max) in [
        (
            "probe_cache_ttl_ms = 0",
            "inventory.probe_cache_ttl_ms",
            0,
            1,
            600_000,
        ),
        (
            "probe_failure_ttl_ms = 600001",
            "inventory.probe_failure_ttl_ms",
            600_001,
            1,
            600_000,
        ),
        (
            "probe_cache_max_entries = 0",
            "inventory.probe_cache_max_entries",
            0,
            1,
            4096,
        ),
        (
            "probe_cache_max_entries = 4097",
            "inventory.probe_cache_max_entries",
            4097,
            1,
            4096,
        ),
        (
            "probe_parallelism = 0",
            "inventory.probe_parallelism",
            0,
            1,
            256,
        ),
        (
            "probe_parallelism = 257",
            "inventory.probe_parallelism",
            257,
            1,
            256,
        ),
    ] {
        match fixture.load(&with_inventory(line)) {
            Err(ConfigError::OutOfRange {
                key: rejected,
                value: rejected_value,
                min: rejected_min,
                max: rejected_max,
            }) => assert_eq!(
                (rejected, rejected_value, rejected_min, rejected_max),
                (key, value, min, max),
                "{line}"
            ),
            other => panic!("{line}: {other:?}"),
        }
    }
    fixture
        .load(&with_inventory(
            "probe_cache_ttl_ms = 1\nprobe_failure_ttl_ms = 600000\nprobe_cache_max_entries = 4096\nprobe_parallelism = 1",
        ))
        .expect("bounds are inclusive");
}

/// Replaces the `[deadlines]` table of the golden file with `extra` keyed
/// lines appended after the restart throttle.
fn with_registry_shutdown(extra: &str) -> String {
    replace(
        GOLDEN,
        "daemon_restart_throttle_ms = 5000",
        &format!("daemon_restart_throttle_ms = 5000\n{extra}"),
    )
}

/// `deadlines.registry_shutdown_ms` is bounded like the other deadlines.
#[test]
fn registry_shutdown_outside_its_bounds_is_refused_by_key() {
    let fixture = Fixture::new();
    for (line, value) in [
        ("registry_shutdown_ms = 0", 0),
        ("registry_shutdown_ms = 600001", 600_001),
    ] {
        match fixture.load(&with_registry_shutdown(line)) {
            Err(ConfigError::OutOfRange {
                key,
                value: rejected,
                min,
                max,
            }) => assert_eq!(
                (key, rejected, min, max),
                ("deadlines.registry_shutdown_ms", value, 1, 600_000),
                "{line}"
            ),
            other => panic!("{line}: {other:?}"),
        }
    }
}

/// A non-default `registry_shutdown_ms` is loaded, written back, and a known key.
#[test]
fn non_default_registry_shutdown_round_trips_and_is_not_unknown() {
    let fixture = Fixture::new();
    let loaded = fixture
        .load(&with_registry_shutdown("registry_shutdown_ms = 7000"))
        .expect("load a custom registry shutdown bound");
    assert_eq!(loaded.deadlines().registry_shutdown, Duration::from_secs(7));
    assert_ne!(DEFAULT_REGISTRY_SHUTDOWN, Duration::from_secs(7));
    assert!(
        loaded.unknown_keys().is_empty(),
        "{:?}",
        loaded.unknown_keys()
    );
    let rendered = loaded.to_toml();
    assert!(
        rendered.contains("registry_shutdown_ms = 7000"),
        "{rendered}"
    );
    let reloaded = fixture.load(&rendered).expect("reload the rendered file");
    assert_eq!(reloaded, loaded);

    let default = fixture.load(GOLDEN).expect("load the golden file");
    assert_eq!(
        default.deadlines().registry_shutdown,
        DEFAULT_REGISTRY_SHUTDOWN
    );
    assert!(!default.to_toml().contains("registry_shutdown"));
}

/// The shutdown phase keys are bounded like the other deadlines, optional, and
/// omitted by the writer while they equal their defaults.
#[test]
fn shutdown_phase_keys_round_trip_and_reject_out_of_range() {
    let fixture = Fixture::new();
    for key in [
        "deadlines.create_drain_ms",
        "deadlines.driver_shutdown_ms",
        "deadlines.event_log_flush_ms",
    ] {
        let line = key.trim_start_matches("deadlines.");
        for value in [0_u64, 600_001] {
            match fixture.load(&with_registry_shutdown(&format!("{line} = {value}"))) {
                Err(ConfigError::OutOfRange {
                    key: rejected,
                    value: rejected_value,
                    min,
                    max,
                }) => assert_eq!(
                    (rejected, rejected_value, min, max),
                    (key, value, 1, 600_000)
                ),
                other => panic!("{key} = {value}: {other:?}"),
            }
        }
    }
    let loaded = fixture
        .load(&with_registry_shutdown(
            "create_drain_ms = 3000\ndriver_shutdown_ms = 2000\nevent_log_flush_ms = 1000",
        ))
        .expect("load custom phase bounds");
    let deadlines = loaded.deadlines();
    assert_eq!(deadlines.create_drain, Duration::from_secs(3));
    assert_eq!(deadlines.driver_shutdown, Duration::from_secs(2));
    assert_eq!(deadlines.event_log_flush, Duration::from_secs(1));
    assert!(loaded.unknown_keys().is_empty());
    let rendered = loaded.to_toml();
    assert_eq!(fixture.load(&rendered).expect("reload"), loaded);

    let default = fixture.load(GOLDEN).expect("load the golden file");
    let rendered = default.to_toml();
    for key in ["create_drain", "driver_shutdown", "event_log_flush"] {
        assert!(!rendered.contains(key), "{key} written at its default");
    }
    assert_eq!(default.deadlines().create_drain, DEFAULT_CREATE_DRAIN);
}

/// Phase bounds that do not fit inside `daemon_exit_timeout_ms` load, are
/// shortened to fit, and report the shortfall; the defaults fit untouched.
#[test]
fn phase_bounds_that_exceed_exit_timeout_are_fitted_and_reported() {
    let fixture = Fixture::new();
    let default = fixture.load(GOLDEN).expect("load the golden file");
    let fitted = default.deadlines().fitted_shutdown();
    assert_eq!(fitted.shortfall, None);
    assert_eq!(fitted.create_drain, DEFAULT_CREATE_DRAIN);
    assert_eq!(fitted.registry_shutdown, DEFAULT_REGISTRY_SHUTDOWN);

    let lowered = replace(
        GOLDEN,
        "daemon_exit_timeout_ms = 30000",
        "daemon_exit_timeout_ms = 20000",
    );
    let loaded = fixture
        .load(&lowered)
        .expect("a lowered exit timeout loads");
    let fitted = loaded.deadlines().fitted_shutdown();
    let shortfall = fitted.shortfall.expect("the defaults need 27000 ms");
    assert_eq!(shortfall.required, Duration::from_secs(27));
    assert_eq!(shortfall.exit_timeout, Duration::from_secs(20));
    let total = fitted.create_drain
        + fitted.driver_shutdown
        + fitted.registry_shutdown
        + fitted.event_log_flush;
    assert!(
        total + Duration::from_secs(2) <= Duration::from_secs(20),
        "{total:?}"
    );
    assert!(
        fitted.create_drain > fitted.registry_shutdown,
        "proportions are kept"
    );
    let message = shortfall.to_string();
    assert!(
        message.contains("daemon_exit_timeout_ms = 20000"),
        "{message}"
    );
    assert!(message.contains("27000"), "{message}");

    let huge = fixture
        .load(&with_registry_shutdown("registry_shutdown_ms = 600000"))
        .expect("an oversized phase bound loads");
    let fitted = huge.deadlines().fitted_shutdown();
    assert!(fitted.shortfall.is_some());
    assert!(fitted.registry_shutdown < Duration::from_secs(28));
}

/// Appends a `[worker_journals]` table with `lines` to the golden file, before `[limits]`.
fn with_worker_journals(lines: &str) -> String {
    replace(
        GOLDEN,
        "[limits]",
        &format!("[worker_journals]\n{lines}\n\n[limits]"),
    )
}

/// A file without `[worker_journals]` means the documented default, and a
/// rewrite does not add the table, so a release that predates it still reads
/// the file.
#[test]
fn a_file_without_worker_journals_loads_the_default_and_rewrites_unchanged() {
    let fixture = Fixture::new();
    let loaded = fixture.load(GOLDEN).expect("load the golden file");
    assert_eq!(
        loaded.worker_journal_max_age(),
        DEFAULT_WORKER_JOURNAL_MAX_AGE
    );
    let rendered = loaded.to_toml();
    assert!(!rendered.contains("worker_journals"), "{rendered}");
    assert_eq!(fixture.load(&rendered).expect("reload"), loaded);
}

/// A non-default age is loaded, known, and written back.
#[test]
fn a_custom_worker_journal_age_round_trips_and_is_not_unknown() {
    let fixture = Fixture::new();
    let loaded = fixture
        .load(&with_worker_journals("max_age_days = 30"))
        .expect("load a custom age");
    assert_eq!(loaded.worker_journal_max_age(), Duration::from_hours(720));
    assert!(
        loaded.unknown_keys().is_empty(),
        "{:?}",
        loaded.unknown_keys()
    );
    let rendered = loaded.to_toml();
    assert!(rendered.contains("max_age_days = 30"), "{rendered}");
    assert_eq!(fixture.load(&rendered).expect("reload"), loaded);
}

/// The age is bounded in whole days, and the bounds are inclusive.
#[test]
fn worker_journal_ages_outside_their_bounds_are_refused_by_key() {
    let fixture = Fixture::new();
    for days in [0_u64, 366] {
        match fixture.load(&with_worker_journals(&format!("max_age_days = {days}"))) {
            Err(ConfigError::OutOfRange {
                key,
                value,
                min,
                max,
            }) => assert_eq!(
                (key, value, min, max),
                ("worker_journals.max_age_days", days, 1, 365)
            ),
            other => panic!("{days} days must be refused, got {other:?}"),
        }
    }
    for days in [1_u64, 365] {
        fixture
            .load(&with_worker_journals(&format!("max_age_days = {days}")))
            .expect("the bounds are inclusive");
    }
}
