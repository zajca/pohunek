//! Profile options (`#2147`): a profile declares closed `[options.<id>]`
//! enumerations whose selected choices add launch args and env.
//!
//! Every scenario launches through the public control socket. The profiles
//! run `/bin/sh` (never `$SHELL`), and the script writes the argv it
//! received and the option env it saw into `launch.out` in the session's
//! working directory, so the launch the choices composed is observed at the
//! child process.

// Rust guideline compliant 2026-11-14

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt as _;

use super::*;

/// Name of the file the launch script writes in the session directory.
const LAUNCH_OUT: &str = "launch.out";

/// Profile head shared by the option profiles: a `/bin/sh` program whose
/// script records `"$@"` (the profile's trailing `sh` is `$0`, so every
/// choice arg lands in `"$@"`) and the two option variables, then sleeps.
/// The file is renamed into place so a reader never sees a partial write.
const RECORDING_HEAD: &str = r#"base = "shell"
program = "/bin/sh"
args = ['-c', 'printf "%s\n" "$@" > launch.tmp; printf "MODE=%s\nDEPTH=%s\n" "$PH_OPT_MODE" "$PH_OPT_DEPTH" >> launch.tmp; mv launch.tmp launch.out; exec sleep 30', 'sh']
"#;

/// Two options declared in reverse id order: `mode` before `depth`. The
/// composed argv takes the selected choices in option-id order, so `depth`
/// comes first.
const TWO_OPTIONS: &str = r#"
[options.mode]
label = "Mode"
default = "calm"
choices.calm = { label = "Calm", args = ["--mode", "calm"], env = { PH_OPT_MODE = "calm-env" } }
choices.bold = { label = "Bold", args = ["--mode", "bold"], env = { PH_OPT_MODE = "bold-env" } }

[options.depth]
label = "Depth"
default = "shallow"
choices.shallow = { label = "Shallow", args = ["--depth=1"] }
choices.deep = { label = "Deep", args = ["--depth=9"], env = { PH_OPT_DEPTH = "deep-env" } }
"#;

/// A marker carried by every hostile value and label text; no error message
/// and no daemon log line may contain it.
const MARKER: &str = "MarkerQ7x2";

/// A daemon over an isolated agents directory with a private host state, so
/// profile sessions can freeze their revision.
struct OptionRig {
    socket: TestSocket,
    shutdown: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
    control: Framed<UnixStream, LinesCodec>,
    agents_dir: PathBuf,
    /// Sessions created so far; numbers each session's directory.
    created: usize,
}

impl OptionRig {
    /// Starts a daemon over an empty agents directory; `plugins` enables the
    /// package store.
    async fn start(tag: &str, plugins: bool) -> Self {
        let socket = temp_socket(tag);
        let agents_dir = socket.dir.join("agents");
        std::fs::create_dir(&agents_dir).expect("create isolated agent profiles");
        let config = SessionRegistryConfig {
            store_path: Some(socket.dir.join("metadata.jsonl")),
            agents_dir: Some(agents_dir.clone()),
            host_state_dir: Some(socket.dir.join("host-state")),
            plugins_dir: plugins.then(|| socket.dir.join("plugins")),
            ..support::hermetic_registry_config()
        };
        let (shutdown, handle, _) = spawn_server_with_config(&socket, "0.0.0", config).await;
        let control = connect(&socket).await;
        Self {
            socket,
            shutdown,
            handle,
            control,
            agents_dir,
            created: 0,
        }
    }

    async fn finish(self) {
        drop(self.control);
        let _ = self.shutdown.send(());
        let _ = self.handle.await;
    }

    fn write_profile(&self, name: &str, text: &str) {
        std::fs::write(self.agents_dir.join(format!("{name}.toml")), text)
            .expect("write the agent profile");
    }

    /// `session.new` of `agent` in a fresh directory of its own.
    async fn create(&mut self, agent: &str) -> (Response, PathBuf) {
        self.create_with(agent, &[], None).await
    }

    /// `session.new` of `agent` selecting `options` (option id, choice id),
    /// keyed with `key` when one is given, in a fresh directory of its own.
    async fn create_with(
        &mut self,
        agent: &str,
        options: &[(&str, &str)],
        key: Option<&str>,
    ) -> (Response, PathBuf) {
        self.created += 1;
        let cwd = self
            .socket
            .dir
            .join(format!("cwd-{agent}-{}", self.created));
        std::fs::create_dir(&cwd).expect("create the session directory");
        let params = selection_params(agent, &cwd, options, key);
        (
            create_session_with_params(&mut self.control, params).await,
            cwd,
        )
    }

    /// Runs `host.inspect` and returns its runtime inventory. Its profile
    /// enumeration logs every profile it skips together with its load error.
    async fn inspect_host(&mut self) -> Vec<protocol::AgentRuntime> {
        let request = Request::make("agent-options-inspect", method::HOST_INSPECT, Value::Null);
        let capabilities: protocol::HostCapabilities =
            serde_json::from_value(ok_payload(exchange(&mut self.control, &request).await))
                .expect("host capabilities");
        assert!(
            capabilities.session_new_agent_options_supported,
            "the daemon advertises agent options"
        );
        capabilities.runtimes
    }

    /// The ids of every session the daemon lists.
    async fn session_ids(&mut self) -> Vec<SessionId> {
        let request = Request::make("agent-options-list", method::SESSION_LIST, Value::Null);
        let sessions: Vec<SessionInfo> =
            serde_json::from_value(ok_payload(exchange(&mut self.control, &request).await))
                .expect("session list");
        sessions.into_iter().map(|session| session.id).collect()
    }

    /// The refusal of `session.new` of `agent` selecting `options`, after
    /// checking that it created nothing: no session is listed and the
    /// launch script never ran.
    async fn refused(&mut self, agent: &str, options: &[(&str, &str)]) -> protocol::ProtocolError {
        let before = self.session_ids().await;
        let (response, cwd) = self.create_with(agent, options, None).await;
        let error = err_payload(response);
        assert_eq!(
            self.session_ids().await,
            before,
            "{agent}: nothing is created"
        );
        assert!(
            !cwd.join(LAUNCH_OUT).exists(),
            "{agent}: the agent never launched"
        );
        error
    }

    /// The `invalid_profile` detail `session.new` of `agent` fails with.
    async fn load_error(&mut self, agent: &str) -> String {
        let (response, _cwd) = self.create(agent).await;
        let error = err_payload(response);
        assert_eq!(error.code, "invalid_profile", "{agent}: {error:?}");
        error
            .msg
            .strip_prefix(&format!("invalid agent profile '{agent}': "))
            .unwrap_or_else(|| panic!("{agent}: the invalid_profile prefix: {error:?}"))
            .to_owned()
    }
}

/// `session.new` params of `agent` in `cwd` selecting `options`, keyed
/// with `key` when one is given.
fn selection_params(
    agent: &str,
    cwd: &Path,
    options: &[(&str, &str)],
    key: Option<&str>,
) -> SessionNewParams {
    let mut params = session_params_in(cwd.to_path_buf());
    agent.clone_into(&mut params.agent);
    params.client_request_id = key.map(str::to_owned);
    params.agent_options = options
        .iter()
        .map(|(option, choice)| ((*option).to_owned(), (*choice).to_owned()))
        .collect();
    params
}

/// What the launch script recorded in `cwd`, once it has written it.
async fn recorded_launch(cwd: &Path) -> String {
    let path = cwd.join(LAUNCH_OUT);
    wait_until("the launch script records its argv and env", || async {
        std::fs::read_to_string(&path).ok()
    })
    .await
}

/// The text of every daemon log file under `dir`.
fn log_text(dir: &Path) -> String {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
        .collect()
}

/// Message of the warning `host.inspect` logs, with the load error, for
/// each profile its enumeration skips.
const SKIPPED_PROFILE_LOG: &str = "skipping unresolvable agent profile during enumeration";

/// Asserts that the daemon log under `dir` carries the skipped-profile
/// warning, with the load errors, `skipped` times, and never `MARKER`.
fn assert_log_names_no_value(dir: &Path, skipped: usize) {
    let text = log_text(dir);
    assert_eq!(
        text.matches(SKIPPED_PROFILE_LOG).count(),
        skipped,
        "host.inspect logs each refused profile once"
    );
    assert!(
        text.contains("invalid_profile"),
        "the skipped-profile warnings carry the load errors"
    );
    assert!(!text.contains(MARKER), "the daemon log echoes no value");
}

/// Row 2: a profile with options launches with every default when
/// `session.new` selects nothing. The default choices' args follow the
/// profile args in option-id order, and their env reaches the child; both
/// `mode` choices set `PH_OPT_MODE`, which alternatives of one option may.
#[tokio::test]
async fn an_options_profile_launches_with_its_defaults() {
    let mut rig = OptionRig::start("agent-options-defaults", false).await;
    rig.write_profile("opts", &format!("{RECORDING_HEAD}{TWO_OPTIONS}"));
    let (response, cwd) = rig.create("opts").await;
    let session: SessionInfo =
        serde_json::from_value(ok_payload(response)).expect("the options profile launches");
    assert_eq!(session.agent, "opts");
    assert_eq!(
        recorded_launch(&cwd).await,
        "--depth=1\n--mode\ncalm\nMODE=calm-env\nDEPTH=\n",
        "the default choices compose in option-id order"
    );
    rig.finish().await;
}

/// The stored `profile_revision` hex of `id` in the store at `path`.
fn stored_revision_hex(path: &Path, id: &SessionId) -> String {
    Store::new(path.to_path_buf())
        .load_sessions()
        .expect("read the durable records")
        .into_iter()
        .find(|record| record.session_id == id.0)
        .expect("the session has a durable record")
        .recovery
        .expect("a profile session has a recovery record")
        .profile_revision
        .expect("a profile session freezes its profile revision")
        .to_string()
}

/// The revision a launch freezes covers the selected choices, not the whole
/// `[options]` table: relabelling an option or a choice and adding a choice
/// keep it, and so does editing a choice the launch did not select, while
/// editing the selected choice's args moves it.
#[tokio::test]
async fn the_revision_covers_the_selected_choices_only() {
    let mut rig = OptionRig::start("agent-options-revision", false).await;
    let store = rig.socket.dir.join("metadata.jsonl");
    let revision_of = async |rig: &mut OptionRig, text: &str| {
        rig.write_profile("opts", text);
        let (response, _cwd) = rig.create("opts").await;
        let session: SessionInfo =
            serde_json::from_value(ok_payload(response)).expect("the options profile launches");
        stored_revision_hex(&store, &session.id)
    };
    let base = format!("{RECORDING_HEAD}{TWO_OPTIONS}");
    let frozen = revision_of(&mut rig, &base).await;
    let kept = [
        (
            "an option label",
            base.replace("label = \"Mode\"", "label = \"Temper\""),
        ),
        (
            "a choice label",
            base.replace("label = \"Calm\"", "label = \"Quiet\""),
        ),
        (
            "an added choice",
            format!("{base}choices.wild = {{ label = \"Wild\", args = [\"--depth=99\"] }}\n"),
        ),
        (
            "an unselected choice",
            base.replace("[\"--mode\", \"bold\"]", "[\"--mode\", \"loud\"]"),
        ),
    ];
    for (edit, text) in kept {
        assert_ne!(text, base, "{edit}: the edit applies");
        assert_eq!(
            revision_of(&mut rig, &text).await,
            frozen,
            "{edit} keeps the revision"
        );
    }
    let moved = base.replace("[\"--mode\", \"calm\"]", "[\"--mode\", \"calm\", \"-v\"]");
    assert_ne!(moved, base, "the selected choice edit applies");
    assert_ne!(
        revision_of(&mut rig, &moved).await,
        frozen,
        "editing the selected choice moves the revision"
    );
    rig.finish().await;
}

/// One option with the given choice table lines, default `calm`.
fn one_option(choices: &str) -> String {
    format!("{RECORDING_HEAD}\n[options.mode]\nlabel = \"Mode\"\ndefault = \"calm\"\n{choices}")
}

/// Row 3: structural load rules. Each case is its own profile; the refusal
/// is `invalid_profile` naming the option or choice id, never an invalid id
/// or label text, in the error and in the warning `host.inspect` logs for
/// each skipped profile.
#[tokio::test]
async fn option_structure_is_validated_on_load() {
    let log_dir = pohunek_test_support::tempdir().expect("log directory");
    let _capture = pohunek_daemon::logging::capture(&log_dir.path().join("daemon-logs"))
        .expect("the daemon log capture starts");
    let mut rig = OptionRig::start("agent-options-structure", false).await;
    let seventeen = (0..17).fold(String::new(), |mut text, index| {
        writeln!(
            text,
            "\n[options.o{index:02}]\nlabel = \"O\"\ndefault = \"a\"\nchoices.a = {{ label = \"A\" }}"
        )
        .expect("write to a string");
        text
    });
    let cases: Vec<(&str, String, String)> = vec![
        (
            "default-missing",
            format!(
                "{RECORDING_HEAD}\n[options.mode]\nlabel = \"Mode\"\ndefault = \"loud{MARKER}\"\nchoices.calm = {{ label = \"Calm\" }}\n"
            ),
            "option 'mode': default is not one of its choices".to_owned(),
        ),
        (
            "no-choices",
            format!("{RECORDING_HEAD}\n[options.mode]\nlabel = \"Mode\"\ndefault = \"calm\"\nchoices = {{}}\n"),
            "option 'mode' declares 0 choices; 1 to 32 are allowed".to_owned(),
        ),
        (
            "too-many-options",
            format!("{RECORDING_HEAD}{seventeen}"),
            "the profile declares 17 options; at most 16 are allowed".to_owned(),
        ),
        (
            "bad-option-id",
            format!(
                "{RECORDING_HEAD}\n[options.\"bad {MARKER}\"]\nlabel = \"Mode\"\ndefault = \"calm\"\nchoices.calm = {{ label = \"Calm\" }}\n"
            ),
            "an option id is not a valid name (letters, digits, '.', '_' and '-', at most 64 bytes)"
                .to_owned(),
        ),
        (
            "bad-choice-id",
            one_option(&format!(
                "choices.calm = {{ label = \"Calm\" }}\nchoices.\"-{MARKER}\" = {{ label = \"Odd\" }}\n"
            )),
            "option 'mode': a choice id is not a valid name (letters, digits, '.', '_' and '-', at most 64 bytes)"
                .to_owned(),
        ),
        (
            "option-label-bidi",
            format!(
                "{RECORDING_HEAD}\n[options.mode]\nlabel = \"Mo\u{202e}de {MARKER}\"\ndefault = \"calm\"\nchoices.calm = {{ label = \"Calm\" }}\n"
            ),
            "option 'mode': label must be 1 to 64 bytes without control, separator or format characters"
                .to_owned(),
        ),
        (
            "choice-label-long",
            one_option(&format!(
                "choices.calm = {{ label = \"{MARKER}{}\" }}\n",
                "x".repeat(64)
            )),
            "option 'mode' choice 'calm': label must be 1 to 64 bytes without control, separator or format characters"
                .to_owned(),
        ),
        (
            "min-runtime",
            one_option(&format!(
                "choices.calm = {{ label = \"Calm\", min_runtime = \"soon{MARKER}\" }}\n"
            )),
            "option 'mode' choice 'calm': min_runtime is not a version".to_owned(),
        ),
    ];
    for (name, text, _detail) in &cases {
        rig.write_profile(name, text);
    }
    for (name, _text, detail) in &cases {
        let refused = rig.load_error(name).await;
        assert_eq!(&refused, detail, "{name}");
        assert!(
            !refused.contains(MARKER),
            "{name}: echoes no text: {refused}"
        );
    }
    let _runtimes = rig.inspect_host().await;
    assert_log_names_no_value(&log_dir.path().join("daemon-logs"), cases.len());
    rig.finish().await;
}

/// The archive of a package that serves the `pi` runtime from `/bin/sh` and
/// declares the config home `ACME_HOME` and the launch variable `PI_FLAG`.
fn home_package() -> (Vec<u8>, package::PackageDigest) {
    pi_package(
        "requires = [\"launch-env-v1\"]\n",
        "/bin/sh",
        "",
        "[config_home]\nenv = \"ACME_HOME\"\ndefault = \".acme\"\n\
         [launch_env]\nPI_FLAG = \"1\"\n",
    )
}

/// The archive of a package that serves the `pi` runtime from `program`
/// and reads its version with `program --version`, which prints
/// `fixture <version>`, accepting `[1.0.0, 3.0.0)`.
fn probed_package(program: &Path) -> (Vec<u8>, package::PackageDigest) {
    pi_package(
        "",
        &program.display().to_string(),
        "version_probe = { parser = \"semver-line-v1\", args = [\"--version\"], min = \"1.0.0\", below = \"3.0.0\", line = \"fixture {version}\" }\n",
        "",
    )
}

/// The archive of an `acme.runtime.pi` package launching `program`, with
/// `requires` after the header, `runtime` lines in its `[runtime]` table and
/// `tables` after the descriptor.
fn pi_package(
    requires: &str,
    program: &str,
    runtime: &str,
    tables: &str,
) -> (Vec<u8>, package::PackageDigest) {
    let descriptor = format!(
        "schema = 1\nid = \"acme.runtime.pi\"\nversion = \"1.0.0\"\nruntime_api = 1\n{requires}\
         [runtime]\nid = \"pi\"\nname = \"Pi fixture\"\nprogram = \"{program}\"\nargs = []\n\
         detect_manifest = \"detect.toml\"\nprompt_arg = true\n{runtime}\
         [input]\nbracketed_paste = false\nsubmit_delay_ms = 0\ntext_policy = \"unrestricted\"\n\
         [resume]\nsupported = true\nreference_kind = \"id\"\nargs = [\"--session\", \"{{reference}}\"]\n\
         [native_reference]\nstrategy = \"assigned\"\nlaunch_args = [\"--session-id\", \"{{reference}}\"]\n\
         [native_reference.existence]\ncheck = \"none\"\n{tables}"
    );
    let archive = package::build_archive(
        &[
            package::ArchiveEntry {
                path: "runtime.toml".to_owned(),
                contents: descriptor.as_bytes().to_vec(),
                executable: false,
            },
            package::ArchiveEntry {
                path: "detect.toml".to_owned(),
                contents: b"[[rules]]\nid = \"ready\"\nstate = \"idle\"\npriority = 100\nregion = \"whole_recent\"\nany = [{ contains = \"ready\" }]\n".to_vec(),
                executable: false,
            },
        ],
        &package::Limits::DEFAULT,
    )
    .expect("build the fixture package");
    let digest = package::read_archive(&archive, &package::Limits::DEFAULT)
        .expect("read the fixture package")
        .digest()
        .clone();
    (archive, digest)
}

/// Installs the [`home_package`] and returns its digest.
async fn install_home_package(rig: &mut OptionRig) -> package::PackageDigest {
    install_package(rig, home_package()).await
}

/// Installs the package `archive` with the `digest`, enabled and selected,
/// and returns the digest.
async fn install_package(
    rig: &mut OptionRig,
    (archive, digest): (Vec<u8>, package::PackageDigest),
) -> package::PackageDigest {
    let archive_path = rig.socket.dir.join("package.tar");
    std::fs::write(&archive_path, archive).expect("write the package archive");
    let installed = exchange(
        &mut rig.control,
        &Request::make(
            "agent-options-install",
            method::PACKAGE_INSTALL,
            serde_json::json!({
                "archive_path": archive_path,
                "trust": { "kind": "explicit_digest", "digest": digest },
                "enable": true,
                "select": true,
                "dry_run": false
            }),
        ),
    )
    .await;
    ok_payload(installed);
    digest
}

/// Row 4: choice env and args rules. Each case is its own profile; the
/// refusal names the option, choice, env key or arg index and never the
/// value, in the error and in the warning `host.inspect` logs for each
/// skipped profile.
#[tokio::test]
async fn choice_env_and_args_are_validated_on_load() {
    let log_dir = pohunek_test_support::tempdir().expect("log directory");
    let _capture = pohunek_daemon::logging::capture(&log_dir.path().join("daemon-logs"))
        .expect("the daemon log capture starts");
    let mut rig = OptionRig::start("agent-options-env", true).await;
    let digest = install_home_package(&mut rig).await;
    let cases = env_cases(&digest);
    for (name, text, _detail) in &cases {
        rig.write_profile(name, text);
    }
    for (name, _text, detail) in &cases {
        let refused = rig.load_error(name).await;
        assert_eq!(refused, *detail, "{name}");
        assert!(
            !refused.contains(MARKER),
            "{name}: echoes no value: {refused}"
        );
    }
    let _runtimes = rig.inspect_host().await;
    assert_log_names_no_value(&log_dir.path().join("daemon-logs"), cases.len());
    rig.finish().await;
}

/// The row 4 cases as `(profile name, profile text, load error detail)`;
/// `digest` pins the [`home_package`] the config-home and launch-env cases
/// build on.
fn env_cases(digest: &package::PackageDigest) -> Vec<(&'static str, String, &'static str)> {
    let on_package = |choices: &str| {
        format!(
            "base = \"pi\"\npackage = \"acme.runtime.pi\"\ndigest = \"{digest}\"\n\
             [options.mode]\nlabel = \"Mode\"\ndefault = \"calm\"\n{choices}"
        )
    };
    let token = format!("sk-ant-{MARKER}{}", "a".repeat(40));
    vec![
        (
            "secret-slot",
            format!(
                "secret_env = [\"PH_SLOT\"]\n{}",
                one_option(&format!(
                    "choices.calm = {{ label = \"Calm\", env = {{ PH_SLOT = \"{MARKER}\" }} }}\n"
                ))
            ),
            "option 'mode' choice 'calm': env key 'PH_SLOT' is a secret_env slot of the profile",
        ),
        (
            "reserved",
            one_option(&format!(
                "choices.calm = {{ label = \"Calm\", env = {{ POHUNEK_X = \"{MARKER}\" }} }}\n"
            )),
            "option 'mode' choice 'calm': env key 'POHUNEK_X' cannot name a reserved POHUNEK_ variable",
        ),
        (
            "config-home",
            on_package(&format!(
                "choices.calm = {{ label = \"Calm\", env = {{ ACME_HOME = \"/{MARKER}\" }} }}\n"
            )),
            "option 'mode' choice 'calm': env key 'ACME_HOME' is the runtime's config home variable",
        ),
        (
            "launch-env",
            on_package(&format!(
                "choices.calm = {{ label = \"Calm\", env = {{ PI_FLAG = \"{MARKER}\" }} }}\n"
            )),
            "option 'mode' choice 'calm': env key 'PI_FLAG' is set by the runtime's [launch_env]",
        ),
        (
            "path",
            one_option(&format!(
                "choices.calm = {{ label = \"Calm\", env = {{ PATH = \"/{MARKER}\" }} }}\n"
            )),
            "option 'mode' choice 'calm': env key 'PATH' is set by the launch itself",
        ),
        (
            "loader",
            one_option(&format!(
                "choices.calm = {{ label = \"Calm\", env = {{ LD_PRELOAD = \"/{MARKER}.so\" }} }}\n"
            )),
            "option 'mode' choice 'calm': env key 'LD_PRELOAD' is a dynamic-loader variable",
        ),
        (
            "profile-env",
            format!(
                "{}\n[env]\nPH_FIXED = \"plain\"\n",
                one_option(&format!(
                    "choices.calm = {{ label = \"Calm\", env = {{ PH_FIXED = \"{MARKER}\" }} }}\n"
                ))
            ),
            "option 'mode' choice 'calm': env key 'PH_FIXED' is also a key of [env]",
        ),
        (
            "two-options",
            format!(
                "{RECORDING_HEAD}\n[options.alpha]\nlabel = \"Alpha\"\ndefault = \"one\"\n\
                 choices.one = {{ label = \"One\", env = {{ PH_SHARED = \"a\" }} }}\n\
                 [options.beta]\nlabel = \"Beta\"\ndefault = \"two\"\n\
                 choices.two = {{ label = \"Two\", env = {{ PH_SHARED = \"{MARKER}\" }} }}\n"
            ),
            "option 'beta' choice 'two': env key 'PH_SHARED' is also set by option 'alpha'",
        ),
        (
            "credential-name",
            one_option(&format!(
                "choices.calm = {{ label = \"Calm\", env = {{ API_TOKEN = \"{MARKER}\" }} }}\n"
            )),
            "option 'mode' choice 'calm': env key 'API_TOKEN' names a credential",
        ),
        (
            "credential-value",
            one_option(&format!(
                "choices.calm = {{ label = \"Calm\", env = {{ PH_OPT_MODE = \"{token}\" }} }}\n"
            )),
            "option 'mode' choice 'calm': the value of env key 'PH_OPT_MODE' has the shape of a vendor token",
        ),
        (
            "credential-arg",
            one_option(&format!(
                "choices.calm = {{ label = \"Calm\", args = [\"--quiet\", \"--token={MARKER}abc\"] }}\n"
            )),
            "option 'mode' choice 'calm': args[1] carries a credential",
        ),
    ]
}

/// A profile over the shell with the [`TWO_OPTIONS`], named `opts`, and a
/// profile without options named `plain`.
async fn selection_rig(tag: &str) -> OptionRig {
    let rig = OptionRig::start(tag, false).await;
    rig.write_profile("opts", &format!("{RECORDING_HEAD}{TWO_OPTIONS}"));
    rig.write_profile("plain", RECORDING_HEAD);
    rig
}

/// Row 5: a selection replaces the default of the option it names; the
/// option it does not name keeps its default.
#[tokio::test]
async fn a_selected_choice_replaces_the_default_of_its_option() {
    let mut rig = selection_rig("agent-options-select").await;
    let (response, cwd) = rig.create_with("opts", &[("mode", "bold")], None).await;
    ok_payload(response);
    assert_eq!(
        recorded_launch(&cwd).await,
        "--depth=1\n--mode\nbold\nMODE=bold-env\nDEPTH=\n",
        "the selected choice and the other option's default compose"
    );
    let (response, cwd) = rig
        .create_with("opts", &[("depth", "deep"), ("mode", "bold")], None)
        .await;
    ok_payload(response);
    assert_eq!(
        recorded_launch(&cwd).await,
        "--depth=9\n--mode\nbold\nMODE=bold-env\nDEPTH=deep-env\n",
        "every selected choice composes in option-id order"
    );
    rig.finish().await;
}

/// Rows 6-8: a selection naming an undeclared option or choice, or any
/// selection for a bare runtime or a profile without options, is refused
/// before anything is created, naming the ids only.
#[tokio::test]
async fn an_undeclared_selection_is_refused_before_anything_is_created() {
    let mut rig = selection_rig("agent-options-refused").await;
    let unknown = rig.refused("opts", &[("colour", "red")]).await;
    assert_eq!(unknown.code, "agent_option_unknown", "{unknown:?}");
    assert_eq!(unknown.msg, "the agent profile declares no option 'colour'");
    let choice = rig.refused("opts", &[("mode", "gpt")]).await;
    assert_eq!(choice.code, "agent_option_choice_unknown", "{choice:?}");
    assert_eq!(
        choice.msg,
        "option 'mode' of the agent profile has no choice 'gpt'"
    );
    for agent in ["shell", "plain"] {
        let undeclared = rig.refused(agent, &[("mode", "bold")]).await;
        assert_eq!(
            undeclared.code, "agent_options_not_declared",
            "{agent}: {undeclared:?}"
        );
        assert_eq!(
            undeclared.msg,
            format!("agent '{agent}' declares no options")
        );
    }
    rig.finish().await;
}

/// A selection whose shape no profile can declare is a `bad_request`
/// before the agent is resolved, and the message echoes no id.
#[tokio::test]
async fn a_malformed_selection_is_a_bad_request() {
    let mut rig = selection_rig("agent-options-shape").await;
    let too_many: Vec<(String, String)> = (0..=protocol::MAX_PROFILE_OPTIONS)
        .map(|index| (format!("o{index}"), "a".to_owned()))
        .collect();
    let too_many: Vec<(&str, &str)> = too_many
        .iter()
        .map(|(option, choice)| (option.as_str(), choice.as_str()))
        .collect();
    let long = "x".repeat(protocol::MAX_PROFILE_OPTION_ID_BYTES + 1);
    let cases: [(&str, Vec<(&str, &str)>); 4] = [
        ("too many entries", too_many),
        (
            "an option id outside the charset",
            vec![("mode bold", "bold")],
        ),
        ("a choice id with a leading '-'", vec![("mode", "-bold")]),
        ("an overlong choice id", vec![("mode", long.as_str())]),
    ];
    for (case, options) in cases {
        let error = rig.refused("opts", &options).await;
        assert_eq!(error.code, "bad_request", "{case}: {error:?}");
        assert!(
            !error.msg.contains("mode bold") && !error.msg.contains("-bold"),
            "{case}: echoes no id: {error:?}"
        );
    }
    rig.finish().await;
}

/// The choices `host.inspect` reports for option `option` of profile
/// `agent`, by choice id.
fn inspected_choices(
    runtimes: &[protocol::AgentRuntime],
    agent: &str,
    option: &str,
) -> BTreeMap<String, protocol::AgentOptionChoice> {
    runtimes
        .iter()
        .find(|entry| entry.agent == agent)
        .unwrap_or_else(|| panic!("{agent} is inventoried"))
        .options
        .iter()
        .find(|declared| declared.id == option)
        .unwrap_or_else(|| panic!("{agent} reports option {option}"))
        .choices
        .iter()
        .map(|choice| (choice.id.clone(), choice.clone()))
        .collect()
}

/// Row 9: a floored choice on a runtime that reports no version (a profile
/// over the shell) is unavailable in `host.inspect` and refused by
/// `session.new`; the unfloored choice launches.
#[tokio::test]
async fn a_floored_choice_without_a_runtime_version_is_unavailable() {
    let mut rig = OptionRig::start("agent-options-unknown-version", false).await;
    rig.write_profile(
        "floored",
        &one_option(
            "choices.calm = { label = \"Calm\" }\n\
             choices.next = { label = \"Next\", args = [\"--next\"], min_runtime = \"2.2.0\" }\n",
        ),
    );
    let runtimes = rig.inspect_host().await;
    let entry = runtimes
        .iter()
        .find(|entry| entry.agent == "floored")
        .expect("the profile is inventoried");
    assert_eq!(entry.options.len(), 1);
    assert_eq!(entry.options[0].label, "Mode");
    assert_eq!(entry.options[0].default, "calm");
    let choices = inspected_choices(&runtimes, "floored", "mode");
    assert_eq!(
        choices["next"],
        protocol::AgentOptionChoice {
            id: "next".to_owned(),
            label: "Next".to_owned(),
            available: false,
            reason: Some(protocol::AgentOptionUnavailable::RuntimeVersionUnknown),
            min_runtime: Some("2.2.0".to_owned()),
        }
    );
    assert_eq!(
        choices["calm"],
        protocol::AgentOptionChoice {
            id: "calm".to_owned(),
            label: "Calm".to_owned(),
            available: true,
            reason: None,
            min_runtime: None,
        }
    );
    assert!(
        runtimes
            .iter()
            .find(|entry| entry.agent == "shell")
            .expect("the bare shell is inventoried")
            .options
            .is_empty(),
        "a bare runtime declares no options"
    );
    let refused = rig.refused("floored", &[("mode", "next")]).await;
    assert_eq!(
        refused.code, "agent_option_choice_unavailable",
        "{refused:?}"
    );
    assert_eq!(
        refused.msg,
        "choice 'next' of option 'mode' is unavailable: the runtime reports no version to compare with the choice's min_runtime"
    );
    let (response, cwd) = rig.create("floored").await;
    ok_payload(response);
    // `printf "%s\n"` without arguments prints one empty line.
    assert_eq!(recorded_launch(&cwd).await, "\nMODE=\nDEPTH=\n");
    rig.finish().await;
}

/// Row 10: on a package runtime whose probe reads `1.5.0`, a choice floored
/// above it is unavailable (`runtime_too_old`) and refused, and a choice
/// floored at or below it is available and launches.
#[tokio::test]
async fn a_floor_compares_with_the_probed_runtime_version() {
    let mut rig = OptionRig::start("agent-options-probed", true).await;
    let program = rig.socket.dir.join("pi-fixture");
    std::fs::write(
        &program,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'fixture 1.5.0\\n'; exit 0; fi\n\
         printf '%s\\n' \"$@\" > launch.tmp; mv launch.tmp launch.out; exec sleep 30\n",
    )
    .expect("write the fixture runtime");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
        .expect("make the fixture runtime executable");
    let digest = install_package(&mut rig, probed_package(&program)).await;
    rig.write_profile(
        "tiers",
        &format!(
            "base = \"pi\"\npackage = \"acme.runtime.pi\"\ndigest = \"{digest}\"\n\
             [options.tier]\nlabel = \"Tier\"\ndefault = \"base\"\n\
             choices.base = {{ label = \"Base\" }}\n\
             choices.newer = {{ label = \"Newer\", args = [\"--newer\"], min_runtime = \"1.6.0\" }}\n\
             choices.older = {{ label = \"Older\", args = [\"--older\"], min_runtime = \"1.4.0\" }}\n\
             choices.exact = {{ label = \"Exact\", args = [\"--at-floor\"], min_runtime = \"1.5.0\" }}\n"
        ),
    );
    let runtimes = rig.inspect_host().await;
    let entry = runtimes
        .iter()
        .find(|entry| entry.agent == "tiers")
        .expect("the profile is inventoried");
    assert_eq!(entry.version.as_deref(), Some("1.5.0"), "{entry:?}");
    let choices = inspected_choices(&runtimes, "tiers", "tier");
    assert!(!choices["newer"].available);
    assert_eq!(
        choices["newer"].reason,
        Some(protocol::AgentOptionUnavailable::RuntimeTooOld)
    );
    assert_eq!(choices["newer"].min_runtime.as_deref(), Some("1.6.0"));
    for available in ["base", "older", "exact"] {
        assert!(choices[available].available, "{available}");
        assert_eq!(choices[available].reason, None, "{available}");
    }
    let refused = rig.refused("tiers", &[("tier", "newer")]).await;
    assert_eq!(
        refused.code, "agent_option_choice_unavailable",
        "{refused:?}"
    );
    assert_eq!(
        refused.msg,
        "choice 'newer' of option 'tier' is unavailable: the runtime is older than the choice's min_runtime"
    );
    for (choice, flag) in [("older", "--older"), ("exact", "--at-floor")] {
        let (response, cwd) = rig.create_with("tiers", &[("tier", choice)], None).await;
        ok_payload(response);
        assert!(
            recorded_launch(&cwd).await.lines().any(|line| line == flag),
            "{choice}: the floored choice launches"
        );
    }
    rig.finish().await;
}

/// Row 11: the selection is part of the `client_request_id` fingerprint.
/// The same key with the same selection replays; with another selection, or
/// with none, it conflicts. (A key stored before selections existed keeps
/// replaying: `session_request::a_frozen_record_replays_...`.)
#[tokio::test]
async fn a_keyed_create_fingerprints_its_selection() {
    let mut rig = selection_rig("agent-options-keyed").await;
    let cwd = rig.socket.dir.join("keyed");
    std::fs::create_dir(&cwd).expect("create the keyed session directory");
    let send = async |rig: &mut OptionRig, options: &[(&str, &str)]| {
        let params = selection_params("opts", &cwd, options, Some("opts-key"));
        create_session_with_params(&mut rig.control, params).await
    };
    let first: protocol::SessionNewResult =
        serde_json::from_value(ok_payload(send(&mut rig, &[("mode", "bold")]).await))
            .expect("the first create");
    assert_ne!(
        first.replayed,
        Some(true),
        "the first create is not a replay"
    );
    let again: protocol::SessionNewResult =
        serde_json::from_value(ok_payload(send(&mut rig, &[("mode", "bold")]).await))
            .expect("the repeat replays");
    assert_eq!(again.replayed, Some(true), "the repeat replays");
    assert_eq!(again.session.id, first.session.id);
    for other in [
        &[("mode", "calm")][..],
        &[][..],
        &[("depth", "deep"), ("mode", "bold")][..],
    ] {
        let error = err_payload(send(&mut rig, other).await);
        assert_eq!(
            error.code, "session_request_conflict",
            "{other:?}: {error:?}"
        );
    }
    rig.finish().await;
}
