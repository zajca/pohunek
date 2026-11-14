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
    /// Starts the daemon after `seed` wrote the profiles into the agents
    /// directory; `plugins` enables the package store.
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
        self.created += 1;
        let cwd = self
            .socket
            .dir
            .join(format!("cwd-{agent}-{}", self.created));
        std::fs::create_dir(&cwd).expect("create the session directory");
        let mut params = session_params_in(cwd.clone());
        agent.clone_into(&mut params.agent);
        (
            create_session_with_params(&mut self.control, params).await,
            cwd,
        )
    }

    /// Runs `host.inspect`, whose profile enumeration logs every profile it
    /// skips together with its load error.
    async fn inspect_host(&mut self) {
        let request = Request::make("agent-options-inspect", method::HOST_INSPECT, Value::Null);
        ok_payload(exchange(&mut self.control, &request).await);
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
    rig.inspect_host().await;
    assert_log_names_no_value(&log_dir.path().join("daemon-logs"), cases.len());
    rig.finish().await;
}

/// The archive of a package that serves the `pi` runtime from `/bin/sh` and
/// declares the config home `ACME_HOME` and the launch variable `PI_FLAG`.
fn home_package() -> (Vec<u8>, package::PackageDigest) {
    let descriptor = "schema = 1\nid = \"acme.runtime.pi\"\nversion = \"1.0.0\"\nruntime_api = 1\n\
         requires = [\"launch-env-v1\"]\n\
         [runtime]\nid = \"pi\"\nname = \"Pi fixture\"\nprogram = \"/bin/sh\"\nargs = []\n\
         detect_manifest = \"detect.toml\"\nprompt_arg = true\n\
         [input]\nbracketed_paste = false\nsubmit_delay_ms = 0\ntext_policy = \"unrestricted\"\n\
         [resume]\nsupported = true\nreference_kind = \"id\"\nargs = [\"--session\", \"{reference}\"]\n\
         [native_reference]\nstrategy = \"assigned\"\nlaunch_args = [\"--session-id\", \"{reference}\"]\n\
         [native_reference.existence]\ncheck = \"none\"\n\
         [config_home]\nenv = \"ACME_HOME\"\ndefault = \".acme\"\n\
         [launch_env]\nPI_FLAG = \"1\"\n";
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
    .expect("build the config-home package");
    let digest = package::read_archive(&archive, &package::Limits::DEFAULT)
        .expect("read the config-home package")
        .digest()
        .clone();
    (archive, digest)
}

/// Installs the [`home_package`] and returns its digest.
async fn install_home_package(rig: &mut OptionRig) -> package::PackageDigest {
    let (archive, digest) = home_package();
    let archive_path = rig.socket.dir.join("home-package.tar");
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
    rig.inspect_host().await;
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
