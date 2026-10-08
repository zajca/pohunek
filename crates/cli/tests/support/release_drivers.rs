//! Per-runtime drivers of the release consumer suite.
//!
//! A driver prepares a hermetic configuration of one real upstream agent that
//! talks only to a loopback model stub, and names the few facts the shared
//! scenario cannot derive: how the agent signals that it is ready, how a
//! reply is held until the suite releases it, and whether the native
//! reference comes from a hook. The table is explicit and keyed by runtime id;
//! a runtime without an entry fails the suite, so a new official package
//! cannot ship without a driver.

// Rust guideline compliant 2026-10-08

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use pohunek_test_support::env::TestEnv;

use crate::messages_stub::{self, MessagesStub};
use crate::model_stub::{self, ModelStub};
use crate::responses_stub::{self, ResponsesStub};

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

/// The loopback model endpoint of one run.
pub(crate) enum Stub {
    Pi(ModelStub),
    Codex(ResponsesStub),
    Claude(MessagesStub),
}

impl Stub {
    /// Lets every held reply, and every later one, complete.
    pub(crate) fn open_gate(&self) {
        match self {
            Self::Pi(stub) => stub.open_gate(),
            Self::Codex(stub) => stub.open_gate(),
            Self::Claude(stub) => stub.open_gate(),
        }
    }

    /// Model requests whose response has begun.
    pub(crate) fn started(&self) -> usize {
        match self {
            Self::Pi(stub) => stub.started(),
            Self::Codex(stub) => stub.started(),
            Self::Claude(stub) => stub.started(),
        }
    }

    /// Model requests whose response completed.
    pub(crate) fn finished(&self) -> usize {
        match self {
            Self::Pi(stub) => stub.finished(),
            Self::Codex(stub) => stub.finished(),
            Self::Claude(stub) => stub.finished(),
        }
    }

    /// The recorded request bodies that carry `needle`, oldest first.
    pub(crate) fn bodies_containing(&self, needle: &str) -> Vec<String> {
        match self {
            Self::Pi(stub) => stub.bodies_containing(needle),
            Self::Codex(stub) => stub.bodies_containing(needle),
            Self::Claude(stub) => stub.bodies_containing(needle),
        }
    }

    /// Whether a request carried a credential the hermetic setup never
    /// configures; `None` when the stub does not record it.
    pub(crate) fn saw_foreign_credential(&self) -> Option<bool> {
        match self {
            Self::Pi(_) => None,
            Self::Codex(stub) => Some(stub.saw_credential()),
            Self::Claude(stub) => Some(stub.saw_credential()),
        }
    }
}

/// What a driver prepared for one run.
pub(crate) struct Prepared {
    pub(crate) stub: Stub,
    /// Entries of the profile's `[env]` table.
    pub(crate) profile_env: Vec<(String, String)>,
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
    /// Whether the runtime reports its conversation id through a hook that
    /// `integration install` registers.
    pub(crate) hooks: bool,
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
pub(crate) const DRIVERS: [Driver; 3] = [
    Driver {
        runtime: "pi",
        columns: None,
        ready_line: None,
        hold_marker: None,
        hooks: false,
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
        hooks: true,
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
        hooks: true,
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
    }
}
