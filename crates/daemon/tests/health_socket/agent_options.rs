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
    /// The registry configuration, reused by [`Self::restarted`].
    config: SessionRegistryConfig,
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
        let (shutdown, handle, _) =
            spawn_server_with_config(&socket, "0.0.0", config.clone()).await;
        let control = connect(&socket).await;
        Self {
            socket,
            shutdown,
            handle,
            control,
            agents_dir,
            created: 0,
            config,
        }
    }

    /// Stops the daemon and starts a new one over the same store, profiles,
    /// packages and socket path, as a daemon restart does.
    async fn restarted(self) -> Self {
        let Self {
            socket,
            shutdown,
            handle,
            control,
            agents_dir,
            created,
            config,
        } = self;
        drop(control);
        let _ = shutdown.send(());
        let _ = handle.await;
        // A daemon start reconciles the sessions of the earlier daemon before
        // it serves requests.
        let (registry, worker_home) = worker_backed_registry(&socket, config.clone());
        registry
            .reconcile_workers()
            .await
            .expect("startup reconciles the sessions of the earlier daemon");
        let (shutdown, handle, _) =
            serve_registry(&socket, "0.0.0", None, registry, worker_home).await;
        let control = connect(&socket).await;
        Self {
            socket,
            shutdown,
            handle,
            control,
            agents_dir,
            created,
            config,
        }
    }

    /// Stops session `id` through `session.stop`.
    async fn stop(&mut self, id: &SessionId) {
        let request = Request::make(
            "agent-options-stop",
            method::SESSION_STOP,
            serde_json::to_value(id).expect("serialize session id"),
        );
        ok_payload(exchange(&mut self.control, &request).await);
    }

    /// `session.resume` of `id`, accepting a changed profile when `accept`.
    async fn resume(&mut self, id: &SessionId, accept: bool) -> Response {
        let params = if accept {
            serde_json::json!({ "session_id": id, "accept_profile_change": true })
        } else {
            serde_json::json!({ "session_id": id })
        };
        exchange(
            &mut self.control,
            &Request::make("agent-options-resume", method::SESSION_RESUME, params),
        )
        .await
    }

    /// The session `id` as `session.list` reports it.
    async fn listed(&mut self, id: &SessionId) -> SessionInfo {
        let request = Request::make("agent-options-list", method::SESSION_LIST, Value::Null);
        let sessions: Vec<SessionInfo> =
            serde_json::from_value(ok_payload(exchange(&mut self.control, &request).await))
                .expect("session list");
        sessions
            .into_iter()
            .find(|session| session.id == *id)
            .expect("the session is listed")
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
    let created: SessionInfo =
        serde_json::from_value(ok_payload(response)).expect("the session info");
    assert_eq!(
        created.agent_options,
        selection(&[("depth", "shallow"), ("mode", "bold")]),
        "the session reports every option's choice, defaults included"
    );
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

/// `pairs` (option id, choice id) as a selection map.
fn selection(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(option, choice)| ((*option).to_owned(), (*choice).to_owned()))
        .collect()
}

/// Name of the file the resumable fixture runtime appends one block to per
/// launch, in the session directory.
const LAUNCHES_LOG: &str = "launches.log";

/// The line that ends one launch block of [`LAUNCHES_LOG`].
const LAUNCH_END: &str = "---";

/// A runtime program that appends its argv and `PH_OPT_MODE` to
/// [`LAUNCHES_LOG`] as one block per launch, then stays alive.
const RESUMABLE_PROGRAM: &str = "#!/bin/sh\n\
    { printf '%s\\n' \"$@\"; printf 'MODE=%s\\n' \"$PH_OPT_MODE\"; printf '%s\\n' '---'; } >> launches.log\n\
    exec sleep 30\n";

/// The options of the resumable profile: `mode` (default `calm`) and
/// `depth` (default `shallow`), as `(option table)` lines.
const RESUMABLE_OPTIONS: &str = r#"
[options.mode]
label = "Mode"
default = "calm"
choices.calm = { label = "Calm", args = ["--mode", "calm"], env = { PH_OPT_MODE = "calm-env" } }
choices.bold = { label = "Bold", args = ["--mode", "bold"], env = { PH_OPT_MODE = "bold-env" } }

[options.depth]
label = "Depth"
default = "shallow"
choices.shallow = { label = "Shallow", args = ["--depth=1"] }
choices.deep = { label = "Deep", args = ["--depth=9"] }
"#;

/// A rig with the resumable fixture runtime installed (an `acme.runtime.pi`
/// package with an assigned native reference and no existence check, so a
/// stopped session resumes) and the profile `work` over it with
/// [`RESUMABLE_OPTIONS`]. Returns the rig and the package digest.
async fn resumable_rig(tag: &str) -> (OptionRig, package::PackageDigest) {
    let mut rig = OptionRig::start(tag, true).await;
    let program = rig.socket.dir.join("resumable-agent");
    std::fs::write(&program, RESUMABLE_PROGRAM).expect("write the fixture runtime");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
        .expect("make the fixture runtime executable");
    let digest = install_package(
        &mut rig,
        pi_package("", &program.display().to_string(), "", ""),
    )
    .await;
    rig.write_profile("work", &resumable_profile(&digest, RESUMABLE_OPTIONS));
    (rig, digest)
}

/// The profile text of `work`: pinned to `digest`, with `options`.
fn resumable_profile(digest: &package::PackageDigest, options: &str) -> String {
    format!("base = \"pi\"\npackage = \"acme.runtime.pi\"\ndigest = \"{digest}\"\n{options}")
}

/// The launch blocks the fixture runtime recorded in `cwd`, once there are
/// `count` of them.
async fn launches(cwd: &Path, count: usize) -> Vec<Vec<String>> {
    let path = cwd.join(LAUNCHES_LOG);
    wait_until("the fixture runtime records its launches", || async {
        let text = std::fs::read_to_string(&path).ok()?;
        let blocks: Vec<Vec<String>> = text
            .split_terminator(&format!("{LAUNCH_END}\n"))
            .map(|block| block.lines().map(str::to_owned).collect())
            .collect();
        (blocks.len() == count).then_some(blocks)
    })
    .await
}

/// Creates a `work` session selecting `mode=bold`, waits for its launch, and
/// stops it. Returns the session and its directory.
async fn stopped_bold_session(rig: &mut OptionRig) -> (SessionInfo, PathBuf) {
    let (response, cwd) = rig.create_with("work", &[("mode", "bold")], None).await;
    let session: SessionInfo =
        serde_json::from_value(ok_payload(response)).expect("the session info");
    assert_eq!(
        session.agent_options,
        selection(&[("depth", "shallow"), ("mode", "bold")])
    );
    let first = launches(&cwd, 1).await;
    assert!(first[0].contains(&"MODE=bold-env".to_owned()), "{first:?}");
    rig.stop(&session.id).await;
    (session, cwd)
}

/// The argv lines of launch `block` (every line before the env line).
fn argv(block: &[String]) -> Vec<&str> {
    block
        .iter()
        .take_while(|line| !line.starts_with("MODE="))
        .map(String::as_str)
        .collect()
}

/// Row 12: relabelling a choice, changing a default and adding a choice
/// leave the revision of a session that recorded its choices unchanged: it
/// resumes without a decision, with the recorded choices (not the new
/// default) and their env.
#[tokio::test]
async fn presentation_and_default_edits_resume_without_a_decision() {
    let (mut rig, digest) = resumable_rig("agent-options-resume-keep").await;
    let (session, cwd) = stopped_bold_session(&mut rig).await;
    let edited = RESUMABLE_OPTIONS
        .replace("label = \"Bold\"", "label = \"Bold and loud\"")
        .replace("default = \"shallow\"", "default = \"deep\"")
        + "choices.wild = { label = \"Wild\", args = [\"--depth=99\"] }\n";
    assert_ne!(edited, RESUMABLE_OPTIONS);
    rig.write_profile("work", &resumable_profile(&digest, &edited));
    ok_payload(rig.resume(&session.id, false).await);
    let blocks = launches(&cwd, 2).await;
    let relaunch = argv(&blocks[1]);
    assert!(
        relaunch
            .windows(3)
            .any(|run| run == ["--depth=1", "--mode", "bold"]),
        "the relaunch keeps the frozen launch args: {relaunch:?}"
    );
    assert!(
        blocks[1].contains(&"MODE=bold-env".to_owned()),
        "{blocks:?}"
    );
    assert_eq!(
        rig.listed(&session.id).await.agent_options,
        selection(&[("depth", "shallow"), ("mode", "bold")]),
        "the recorded choices stand, not the new default"
    );
    rig.finish().await;
}

/// Row 13: editing the selected choice's body is a profile change: resume
/// refuses with `agent_profile_changed`, and with acceptance relaunches as a
/// plain profile does under acceptance: the frozen args, and the env of the
/// choice as the profile now declares it.
#[tokio::test]
async fn editing_the_selected_choice_needs_a_decision() {
    let (mut rig, digest) = resumable_rig("agent-options-resume-edit").await;
    let (session, cwd) = stopped_bold_session(&mut rig).await;
    let edited = RESUMABLE_OPTIONS.replace(
        "args = [\"--mode\", \"bold\"], env = { PH_OPT_MODE = \"bold-env\" }",
        "args = [\"--mode\", \"bold\", \"-v\"], env = { PH_OPT_MODE = \"bold-env-2\" }",
    );
    assert_ne!(edited, RESUMABLE_OPTIONS);
    rig.write_profile("work", &resumable_profile(&digest, &edited));
    let refused = err_payload(rig.resume(&session.id, false).await);
    assert_eq!(refused.code, "agent_profile_changed", "{refused:?}");
    ok_payload(rig.resume(&session.id, true).await);
    let blocks = launches(&cwd, 2).await;
    let relaunch = argv(&blocks[1]);
    assert!(
        relaunch
            .windows(3)
            .any(|run| run == ["--depth=1", "--mode", "bold"])
            && !relaunch.contains(&"-v"),
        "the accepted relaunch keeps the frozen args: {relaunch:?}"
    );
    assert!(
        blocks[1].contains(&"MODE=bold-env-2".to_owned()),
        "the env comes from the current choice body: {blocks:?}"
    );
    rig.finish().await;
}

/// Row 14: editing a choice the session did not select leaves its revision
/// unchanged; it resumes without a decision.
#[tokio::test]
async fn editing_an_unselected_choice_resumes_without_a_decision() {
    let (mut rig, digest) = resumable_rig("agent-options-resume-other").await;
    let (session, cwd) = stopped_bold_session(&mut rig).await;
    let edited = RESUMABLE_OPTIONS.replace("[\"--mode\", \"calm\"]", "[\"--mode\", \"quiet\"]");
    assert_ne!(edited, RESUMABLE_OPTIONS);
    rig.write_profile("work", &resumable_profile(&digest, &edited));
    ok_payload(rig.resume(&session.id, false).await);
    let blocks = launches(&cwd, 2).await;
    assert!(
        blocks[1].contains(&"MODE=bold-env".to_owned()),
        "{blocks:?}"
    );
    rig.finish().await;
}

/// Row 15: a recorded choice the profile no longer declares is refused with
/// `agent_option_choice_missing`, also when the owner accepts the changed
/// profile: no other choice stands in for it. The upgrade preflight judges
/// the session with the same code.
#[tokio::test]
async fn a_removed_choice_refuses_resume_even_when_accepted() {
    let (mut rig, digest) = resumable_rig("agent-options-resume-missing").await;
    let (session, cwd) = stopped_bold_session(&mut rig).await;
    let edited = RESUMABLE_OPTIONS.replace(
        "choices.bold = { label = \"Bold\", args = [\"--mode\", \"bold\"], env = { PH_OPT_MODE = \"bold-env\" } }\n",
        "",
    );
    assert_ne!(edited, RESUMABLE_OPTIONS);
    rig.write_profile("work", &resumable_profile(&digest, &edited));
    for accept in [false, true] {
        let refused = err_payload(rig.resume(&session.id, accept).await);
        assert_eq!(
            refused.code, "agent_option_choice_missing",
            "accept={accept}: {refused:?}"
        );
        assert_eq!(
            refused.msg,
            "the agent profile no longer offers choice 'bold' of option 'mode'"
        );
    }
    assert_eq!(launches(&cwd, 1).await.len(), 1, "nothing relaunched");
    // The upgrade preflight reaches the refusal resume returns.
    let judged = preflight_verdict(&rig, &session.id);
    assert_eq!(judged.code, "agent_option_choice_missing", "{judged:?}");
    rig.finish().await;
}

/// Rows 16-17: the recorded choices survive a daemon restart, and the
/// recovery after the restart re-derives the env of the recorded choice.
#[tokio::test]
async fn recorded_choices_survive_a_restart_and_drive_the_relaunch() {
    let (mut rig, _digest) = resumable_rig("agent-options-restart").await;
    let (session, cwd) = stopped_bold_session(&mut rig).await;
    let mut rig = rig.restarted().await;
    assert_eq!(
        rig.listed(&session.id).await.agent_options,
        selection(&[("depth", "shallow"), ("mode", "bold")]),
        "the choices are persisted"
    );
    ok_payload(rig.resume(&session.id, false).await);
    let blocks = launches(&cwd, 2).await;
    assert!(
        blocks[1].contains(&"MODE=bold-env".to_owned()),
        "the relaunch carries the recorded choice's env: {blocks:?}"
    );
    rig.finish().await;
}

/// The upgrade preflight's verdict on the stopped session `id` of `rig`'s
/// store, profiles and packages.
fn preflight_verdict(
    rig: &OptionRig,
    id: &SessionId,
) -> pohunek_service_config::preflight::SessionVerdict {
    let report = pohunek_daemon::session::upgrade_preflight::run_with(
        &pohunek_daemon::session::upgrade_preflight::PreflightInputs {
            store_path: rig.socket.dir.join("metadata.jsonl"),
            worker_state_root: rig.socket.dir.join("no-workers"),
            plugins_dir: rig.socket.dir.join("plugins"),
        },
        &pohunek_daemon::session::upgrade_preflight::PreflightOptions {
            probe_budget: pohunek_service_config::preflight::PROBE_BUDGET,
            probe_clock: pohunek_daemon::capabilities::ProbeClock::default(),
            entry: std::time::Instant::now(),
            release: None,
            profiles_dir: Some(rig.agents_dir.clone()),
            host_state_dir: Some(rig.socket.dir.join("host-state")),
            launch: Ok(pohunek_daemon::runtime::LaunchEnvironment::subprocess_default()),
        },
        &pohunek_daemon::procwatch::HostInspector::new(),
    );
    report
        .stopped_sessions
        .into_iter()
        .find(|verdict| verdict.session_id == id.0)
        .expect("the stopped session is judged")
}

/// Row 19: the upgrade preflight judges a stopped session with recorded
/// choices and an unchanged profile as resume does: no profile change.
#[tokio::test]
async fn the_upgrade_preflight_judges_recorded_choices_as_resume_does() {
    let (mut rig, _digest) = resumable_rig("agent-options-preflight").await;
    let (session, _cwd) = stopped_bold_session(&mut rig).await;
    let judged = preflight_verdict(&rig, &session.id);
    assert_eq!(
        judged.verdict,
        pohunek_service_config::preflight::Verdict::Adoptable,
        "{judged:?}"
    );
    assert_eq!(judged.code, "adoptable", "{judged:?}");
    ok_payload(rig.resume(&session.id, false).await);
    rig.finish().await;
}

/// Row 14b: a choice whose args are the runtime's `trusted_checkout_args`
/// never adds them to the launch of an untrusted checkout, as for profile
/// args: only the trust predicate adds them.
#[tokio::test]
async fn trusted_checkout_args_of_a_choice_never_reach_an_untrusted_launch() {
    let mut rig = OptionRig::start("agent-options-trust", true).await;
    let program = rig.socket.dir.join("resumable-agent");
    std::fs::write(&program, RESUMABLE_PROGRAM).expect("write the fixture runtime");
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
        .expect("make the fixture runtime executable");
    let digest = install_package(
        &mut rig,
        pi_package(
            "requires = [\"checkout-trust-args-v1\"]\n",
            &program.display().to_string(),
            "trusted_checkout_args = [\"--trust-workspace\"]\n\
             trusted_checkout_paths = [\".agents\"]\n",
            "",
        ),
    )
    .await;
    rig.write_profile(
        "work",
        &resumable_profile(
            &digest,
            "[options.trust]\nlabel = \"Trust\"\ndefault = \"yes\"\n\
             choices.yes = { label = \"Yes\", args = [\"--marker\", \"--trust-workspace\"] }\n",
        ),
    );
    let (response, cwd) = rig.create("work").await;
    ok_payload(response);
    let blocks = launches(&cwd, 1).await;
    let launched = argv(&blocks[0]);
    assert!(launched.contains(&"--marker"), "{launched:?}");
    assert!(
        !launched.contains(&"--trust-workspace"),
        "an untrusted checkout never gets the trust args: {launched:?}"
    );
    rig.finish().await;
}
