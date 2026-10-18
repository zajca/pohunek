//! Per-runtime drivers of the release consumer suite.
//!
//! A driver prepares a hermetic configuration of one real upstream agent that
//! talks only to a loopback model stub, and names the few facts the shared
//! scenario cannot derive: how the agent signals that it is ready, how a
//! reply is held until the suite releases it, and which integration, if any,
//! reports the native reference. The table is explicit and keyed by runtime id;
//! a runtime without an entry fails the suite, so a new official package
//! cannot ship without a driver.

// Rust guideline compliant 2026-10-12

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use pohunek_test_support::env::TestEnv;

use super::hermes_provider::{self, Exchanges, HermesProvider};
use super::messages_stub::{self, MessagesStub};
use super::model_stub::{self, ModelStub};
use super::responses_stub::{self, ResponsesStub};

/// Mode of a profile file and of the profile directory.
const PROFILE_MODE: u32 = 0o600;
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Placeholder API key of the loopback Pi provider; the stub never checks it.
const PI_STUB_KEY: &str = "stub";

/// Name of the provider the Pi configuration points at the stub.
const PI_STUB_PROVIDER: &str = "stub";

/// Codex feature switches that keep it from contacting anything but the stub:
/// plugin marketplace sync (a `git fetch`), connectors and in-app updates.
const CODEX_OFFLINE_FEATURES: &str =
    "plugins = false\nremote_plugin = false\nplugin_sharing = false\napps = false\nin_app_updates = false";

/// Length of the API-key suffix Claude records as approved: the key's last 20
/// characters.
const CLAUDE_APPROVED_KEY_SUFFIX_LEN: usize = 20;

/// Terminal width Claude is launched at; its screens are specified at this
/// width.
const CLAUDE_COLUMNS: &str = "100";

/// The whole reply of the loopback Hermes provider. Plain ASCII without
/// characters JSON escapes, so it reads the same on screen and inside a later
/// request body.
const HERMES_REPLY: &str = "HERMES_CONSUMER_TURN_OK";

/// Text of the line the pinned Hermes 0.20.0 prints below its banner once its
/// prompt accepts input, both at launch and after `--resume` (there below the
/// restored conversation).
const HERMES_READY_LINE: &str = "Welcome to Hermes Agent!";

/// The operator plugin's hook runs again in the resumed Hermes process and
/// reports its pid once that process initializes its agent for the next turn.
const HERMES_REPORTS_AFTER_RESUME: bool = true;

/// Hermes traffic of one run: the launched process and the resumed one, one
/// turn each. The pinned release probes the discovery paths once per process,
/// when it initializes its agent for the first turn, and sends no `CONNECT`
/// probe with the home [`hermes_provider::write_home`] writes.
const HERMES_EXCHANGES: Exchanges = Exchanges {
    processes: 2,
    turns: 2,
};

/// The loopback model endpoint of one run.
pub(crate) enum Stub {
    Pi(ModelStub),
    Codex(ResponsesStub),
    Claude(MessagesStub),
    Hermes(HermesProvider),
}

impl Stub {
    /// Lets every held reply, and every later one, complete.
    pub(crate) fn open_gate(&self) {
        match self {
            Self::Pi(stub) => stub.open_gate(),
            Self::Codex(stub) => stub.open_gate(),
            Self::Claude(stub) => stub.open_gate(),
            Self::Hermes(stub) => stub.open_gate(),
        }
    }

    /// Model requests whose response has begun.
    pub(crate) fn started(&self) -> usize {
        match self {
            Self::Pi(stub) => stub.started(),
            Self::Codex(stub) => stub.started(),
            Self::Claude(stub) => stub.started(),
            Self::Hermes(stub) => stub.started(),
        }
    }

    /// Model requests whose response completed.
    pub(crate) fn finished(&self) -> usize {
        match self {
            Self::Pi(stub) => stub.finished(),
            Self::Codex(stub) => stub.finished(),
            Self::Claude(stub) => stub.finished(),
            Self::Hermes(stub) => stub.finished(),
        }
    }

    /// The recorded request bodies that carry `needle`, oldest first.
    pub(crate) fn bodies_containing(&self, needle: &str) -> Vec<String> {
        match self {
            Self::Pi(stub) => stub.bodies_containing(needle),
            Self::Codex(stub) => stub.bodies_containing(needle),
            Self::Claude(stub) => stub.bodies_containing(needle),
            Self::Hermes(stub) => stub.bodies_containing(needle),
        }
    }

    /// Whether a request carried a credential the hermetic setup never
    /// configures; `None` when the stub does not record it.
    pub(crate) fn saw_foreign_credential(&self) -> Option<bool> {
        match self {
            Self::Pi(_) => None,
            Self::Codex(stub) => Some(stub.saw_credential()),
            Self::Claude(stub) => Some(stub.saw_credential()),
            Self::Hermes(stub) => Some(stub.saw_foreign_credential()),
        }
    }

    /// Checks the whole traffic of the run against the turns `prompts`, in
    /// order, for a stub that pins it; the other stubs accept any traffic.
    pub(crate) fn assert_exchanges(&self, prompts: &[&str]) {
        if let Self::Hermes(stub) = self {
            stub.assert_exchanges(prompts);
        }
    }

    /// Panics with the stub's diagnostics once it refused an exchange; a stub
    /// that answers every request never does.
    pub(crate) fn assert_healthy(&self) {
        if let Self::Hermes(stub) = self {
            stub.assert_healthy();
        }
    }
}

/// What a driver prepared for one run.
pub(crate) struct Prepared {
    pub(crate) stub: Stub,
    /// Entries of the profile's `[env]` table.
    pub(crate) profile_env: Vec<(String, String)>,
    /// The runtime home the driver wrote, for an integration that is
    /// installed into a home named by path.
    pub(crate) runtime_home: Option<PathBuf>,
}

/// How the runtime reports its conversation id to the daemon.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Integration {
    /// No hook; the daemon assigns the reference at launch.
    None,
    /// `integration install --agent <runtime> --profile <profile>` registers
    /// a hook in the config home the profile's environment selects.
    ProfileHook,
    /// `integration install --agent hermes` installs the operator plugin from
    /// the installed package into [`Prepared::runtime_home`]; the plugin's
    /// hook reports the conversation id.
    HermesPlugin,
}

/// Facts about one runtime the shared scenario needs.
pub(crate) struct Driver {
    pub(crate) runtime: &'static str,
    /// Terminal width of the session, when the runtime's screens need one.
    pub(crate) columns: Option<&'static str>,
    /// Text that must be on screen before the first input is typed.
    pub(crate) ready_line: Option<&'static str>,
    /// Prefix of the prompt that makes the stub hold its reply.
    pub(crate) hold_marker: Option<&'static str>,
    /// How the runtime reports its conversation id.
    pub(crate) integration: Integration,
    /// Whether the resumed process reports itself through the hook again.
    /// Codex 0.160.0 registers no new reporter after a resume.
    pub(crate) reports_after_resume: bool,
    /// Writes the agent configuration below `env` and starts the stub.
    pub(crate) prepare: fn(&TestEnv) -> Prepared,
    /// Text of the stub's reply as it appears on screen.
    pub(crate) reply: fn() -> String,
    /// The reply as it appears in the raw request body of a later turn, where
    /// the agent sends the conversation so far.
    pub(crate) history_reply: fn() -> String,
}

/// The drivers, one per official runtime package.
pub(crate) const DRIVERS: [Driver; 4] = [
    Driver {
        runtime: "pi",
        columns: None,
        ready_line: None,
        hold_marker: None,
        integration: Integration::None,
        reports_after_resume: false,
        prepare: prepare_pi,
        reply: || format!("{}{}", model_stub::FIRST_CHUNK, model_stub::SECOND_CHUNK),
        history_reply: || format!("{}{}", model_stub::FIRST_CHUNK, model_stub::SECOND_CHUNK),
    },
    Driver {
        runtime: "codex",
        columns: None,
        ready_line: None,
        hold_marker: Some(responses_stub::HOLD_MARKER),
        integration: Integration::ProfileHook,
        reports_after_resume: false,
        prepare: prepare_codex,
        reply: || {
            format!(
                "{}{}",
                responses_stub::FIRST_CHUNK,
                responses_stub::SECOND_CHUNK
            )
        },
        history_reply: || {
            format!(
                "{}{}",
                responses_stub::FIRST_CHUNK,
                responses_stub::SECOND_CHUNK
            )
        },
    },
    Driver {
        runtime: "claude",
        columns: Some(CLAUDE_COLUMNS),
        ready_line: Some("for shortcuts"),
        hold_marker: Some(messages_stub::HOLD_MARKER),
        integration: Integration::ProfileHook,
        reports_after_resume: true,
        prepare: prepare_claude,
        reply: || messages_stub::SECOND_CHUNK.to_owned(),
        // The request body is JSON, so the paragraph break is escaped.
        history_reply: || {
            format!(
                "{}{}{}",
                messages_stub::FIRST_CHUNK,
                messages_stub::CHUNK_BREAK.escape_default(),
                messages_stub::SECOND_CHUNK
            )
        },
    },
    Driver {
        runtime: "hermes",
        columns: None,
        ready_line: Some(HERMES_READY_LINE),
        hold_marker: None,
        integration: Integration::HermesPlugin,
        reports_after_resume: HERMES_REPORTS_AFTER_RESUME,
        prepare: prepare_hermes,
        reply: || HERMES_REPLY.to_owned(),
        history_reply: || HERMES_REPLY.to_owned(),
    },
];

/// The driver of `runtime`.
///
/// # Errors
///
/// Fails with a message naming the table to extend when no driver exists.
pub(crate) fn driver_for(runtime: &str) -> Result<&'static Driver, String> {
    DRIVERS
        .iter()
        .find(|driver| driver.runtime == runtime)
        .ok_or_else(|| {
            let known: Vec<&str> = DRIVERS.iter().map(|driver| driver.runtime).collect();
            format!(
                "no consumer driver exists for runtime {runtime:?}; add an entry to DRIVERS in \
                 crates/cli/tests/support/release_drivers.rs (known drivers: {known:?})"
            )
        })
}

/// Writes the profile `name` pinned to the installed package.
pub(crate) fn write_profile(
    env: &TestEnv,
    name: &str,
    runtime: &str,
    package_id: &str,
    digest: &str,
    profile_env: &[(String, String)],
) {
    // The daemon requires every directory above the profiles to be private.
    let agents = env.config_home().join("pohunek/agents");
    fs::create_dir_all(&agents).expect("create the agents directory");
    for dir in [
        agents.parent().expect("pohunek config dir"),
        agents.as_path(),
    ] {
        fs::set_permissions(dir, fs::Permissions::from_mode(PRIVATE_DIR_MODE))
            .expect("private config directory");
    }
    let mut text = format!(
        "base = \"{runtime}\"\npackage = \"{package_id}\"\ndigest = \"{digest}\"\n\n[env]\n"
    );
    for (key, value) in profile_env {
        writeln!(text, "{key} = \"{value}\"").expect("write to a string");
    }
    let path = agents.join(format!("{name}.toml"));
    fs::write(&path, text).expect("write the profile");
    fs::set_permissions(&path, fs::Permissions::from_mode(PROFILE_MODE)).expect("profile mode");
}

fn path_text(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_owned()
}

fn prepare_pi(env: &TestEnv) -> Prepared {
    let stub = ModelStub::start();
    let agent_dir = env.home().join(".pi/agent");
    fs::create_dir_all(&agent_dir).expect("create the Pi agent directory");
    fs::write(
        agent_dir.join("models.json"),
        serde_json::json!({
            "providers": {
                PI_STUB_PROVIDER: {
                    "baseUrl": stub.base_url(),
                    "api": "openai-completions",
                    "apiKey": PI_STUB_KEY,
                    "models": [{ "id": model_stub::MODEL_ID }],
                }
            }
        })
        .to_string(),
    )
    .expect("write models.json");
    fs::write(
        agent_dir.join("settings.json"),
        serde_json::json!({
            "defaultProvider": PI_STUB_PROVIDER,
            "defaultModel": model_stub::MODEL_ID,
        })
        .to_string(),
    )
    .expect("write settings.json");
    Prepared {
        stub: Stub::Pi(stub),
        runtime_home: None,
        profile_env: [
            ("PI_OFFLINE", "1"),
            ("PI_SKIP_VERSION_CHECK", "1"),
            ("PI_TELEMETRY", "0"),
        ]
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .to_vec(),
    }
}

fn prepare_codex(env: &TestEnv) -> Prepared {
    let stub = ResponsesStub::start();
    let home = env.root().join("codex-home");
    fs::create_dir_all(&home).expect("create the Codex home");
    // Approval and sandbox policy are top-level keys and must precede the
    // provider table.
    fs::write(
        home.join("config.toml"),
        format!(
            "model = \"{model}\"\nmodel_provider = \"stub\"\napproval_policy = \"on-request\"\nsandbox_mode = \"read-only\"\ncheck_for_update_on_startup = false\n\n[analytics]\nenabled = false\n\n[features]\n{features}\n\n[model_providers.stub]\nname = \"stub\"\nbase_url = \"{base}\"\nwire_api = \"responses\"\nrequires_openai_auth = false\n",
            model = responses_stub::MODEL_ID,
            base = stub.base_url(),
            features = CODEX_OFFLINE_FEATURES,
        ),
    )
    .expect("write config.toml");
    Prepared {
        stub: Stub::Codex(stub),
        profile_env: vec![("CODEX_HOME".to_owned(), path_text(&home))],
        runtime_home: None,
    }
}

fn prepare_claude(env: &TestEnv) -> Prepared {
    let stub = MessagesStub::start();
    let home = env.root().join("claude-home");
    fs::create_dir_all(&home).expect("create the Claude home");
    // Onboarding done, the dummy key approved and the project folder trusted:
    // Claude opens its prompt without a dialog.
    let key = messages_stub::API_KEY;
    let suffix = &key[key.len() - CLAUDE_APPROVED_KEY_SUFFIX_LEN..];
    let cwd = path_text(env.cwd());
    let config = serde_json::json!({
        "hasCompletedOnboarding": true,
        "customApiKeyResponses": { "approved": [suffix], "rejected": [] },
        "projects": { cwd: { "hasTrustDialogAccepted": true } },
    });
    fs::write(
        home.join(".claude.json"),
        serde_json::to_vec_pretty(&config).expect("config JSON"),
    )
    .expect("write .claude.json");
    fs::write(
        home.join("settings.json"),
        serde_json::to_vec_pretty(&serde_json::json!({ "theme": "dark" })).expect("settings JSON"),
    )
    .expect("write settings.json");
    // The switches stop updates, telemetry, error reporting, plugin-marketplace
    // registration and the other non-essential traffic.
    let profile_env = [
        ("CLAUDE_CONFIG_DIR", path_text(&home)),
        ("ANTHROPIC_BASE_URL", stub.base_url()),
        ("ANTHROPIC_API_KEY", messages_stub::API_KEY.to_owned()),
        ("ANTHROPIC_MODEL", messages_stub::MODEL_ID.to_owned()),
        ("DISABLE_AUTOUPDATER", "1".to_owned()),
        ("DISABLE_UPDATES", "1".to_owned()),
        ("DISABLE_TELEMETRY", "1".to_owned()),
        ("DISABLE_ERROR_REPORTING", "1".to_owned()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".to_owned()),
        (
            "CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL",
            "1".to_owned(),
        ),
    ]
    .map(|(key, value)| (key.to_owned(), value))
    .to_vec();
    Prepared {
        stub: Stub::Claude(stub),
        profile_env,
        runtime_home: None,
    }
}

fn prepare_hermes(env: &TestEnv) -> Prepared {
    // Every completion is held until the suite opens the gate, so the turn's
    // working state is observable without a prompt marker.
    let stub = HermesProvider::held(HERMES_REPLY, HERMES_EXCHANGES);
    let home = env.root().join("hermes-home");
    hermes_provider::write_home(&home, &stub);
    // Bytecode writes would change the verified stage the upstream runs from;
    // lazy installs would reach the network; the unreachable session bus keeps
    // Hermes off the host's desktop services.
    let mut profile_env: Vec<(String, String)> = [
        ("HERMES_HOME", path_text(&home)),
        ("PYTHONDONTWRITEBYTECODE", "1".to_owned()),
        ("HERMES_DISABLE_LAZY_INSTALLS", "1".to_owned()),
        (
            "DBUS_SESSION_BUS_ADDRESS",
            format!(
                "unix:path={}",
                path_text(&env.root().join("no-session-bus"))
            ),
        ),
    ]
    .map(|(key, value)| (key.to_owned(), value))
    .to_vec();
    profile_env.extend(stub.proxy_env());
    Prepared {
        stub: Stub::Hermes(stub),
        profile_env,
        runtime_home: Some(home),
    }
}
