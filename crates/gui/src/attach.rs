//! Terminal attach/resume plumbing and window-size unit conversions.

// Rust guideline compliant 2026-10-01

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use iced::Task;
use pohunek_gui_core::{
    render_attach_argv, render_attach_command, AttachTemplateError, AttachTemplateValues, HostId,
};
use protocol::{SessionId, SessionInfo};
use thiserror::Error;

use crate::bin_resolver::{BinError, BinResolver};
use crate::command::resume_session_task;
use crate::config::{AttachCommandMode, AttachSelection, LaunchSettings};
use crate::message::Message;
use crate::runtime;
use crate::terminal::{
    attach_arguments, endpoint_environment, spawn_observed, ObserveError, TerminalError,
    TerminalLauncher, SYSTEM_OPEN,
};
use crate::PohunekApp;

/// The shell that runs a rendered `attach_command` in shell mode. It is an
/// absolute path because a Finder or launchd start has no useful `PATH`; every
/// supported host ships `/bin/sh`.
const SHELL_PROGRAM: &str = "/bin/sh";

/// Everything one attach action needs, owned so it can run on a blocking thread.
#[derive(Debug, Clone)]
pub(crate) struct AttachPlan {
    pub(crate) selection: AttachSelection,
    pub(crate) resolver: Arc<BinResolver>,
    pub(crate) launch: LaunchSettings,
    pub(crate) values: AttachTemplateValues,
}

/// Where the stock-terminal launcher finds its collaborators.
#[derive(Debug, Clone)]
pub(crate) struct TerminalEnvironment {
    /// The `open` executable.
    pub(crate) opener: PathBuf,
    /// Script directory; `None` selects the pohunek runtime directory.
    pub(crate) script_dir: Option<PathBuf>,
}

impl TerminalEnvironment {
    pub(crate) fn system() -> Self {
        Self {
            opener: PathBuf::from(SYSTEM_OPEN),
            script_dir: None,
        }
    }
}

/// Reports why an attach action could not start a terminal.
#[derive(Debug, Error)]
pub(crate) enum AttachError {
    #[error(transparent)]
    Bin(#[from] BinError),
    #[error("resolved `pohunek_bin` is not valid UTF-8, which attach_command templates require")]
    NonUtf8Bin,
    #[error("invalid attach_command: {0}")]
    Template(#[from] AttachTemplateError),
    #[error(transparent)]
    Observe(#[from] ObserveError),
    #[error("cannot locate the attach script directory: {0}")]
    ScriptDirectory(#[source] pohunek_paths::PathError),
    #[error(transparent)]
    Terminal(#[from] TerminalError),
}

/// Resolves `pohunek_bin` and the launcher program, then starts the terminal.
///
/// Returns a warning to show next to the success message, when there is one.
/// Blocks on executable resolution, on the observation window of a template
/// launch, and on the bounded `open` call of a stock terminal, so callers run
/// it off the UI thread.
pub(crate) fn run_attach(
    plan: &AttachPlan,
    terminal: &TerminalEnvironment,
) -> Result<Option<String>, AttachError> {
    let bin = plan.resolver.resolve()?;
    match &plan.selection {
        AttachSelection::Command { template, mode } => {
            let values = AttachTemplateValues {
                bin: bin.to_str().ok_or(AttachError::NonUtf8Bin)?.to_owned(),
                host: plan.values.host.clone(),
                id: plan.values.id.clone(),
            };
            let observe = plan.launch.attach_observe;
            match mode {
                AttachCommandMode::Shell => {
                    let command = render_attach_command(template, &values)?;
                    spawn_observed(
                        SHELL_PROGRAM.as_ref(),
                        &[OsString::from("-c"), OsString::from(command)],
                        observe,
                    )?;
                }
                AttachCommandMode::Argv => {
                    let argv: Vec<OsString> = render_attach_argv(template, &values)?
                        .into_iter()
                        .map(OsString::from)
                        .collect();
                    let (word, arguments) = argv
                        .split_first()
                        .expect("render_attach_argv returns at least the program");
                    // A bare launcher name (`kitty`) must not depend on the
                    // GUI's own `PATH`, which a Finder launch keeps minimal.
                    let program = plan.resolver.resolve_name(word)?;
                    spawn_observed(program.as_os_str(), arguments, observe)?;
                }
            }
            Ok(None)
        }
        AttachSelection::Terminal(kind) => {
            let script_dir = match &terminal.script_dir {
                Some(dir) => dir.clone(),
                None => {
                    TerminalLauncher::default_script_dir().map_err(AttachError::ScriptDirectory)?
                }
            };
            let launcher = TerminalLauncher::new(
                *kind,
                terminal.opener.clone(),
                script_dir,
                plan.launch.open_timeout,
                plan.launch.attach_script_max_age,
            )
            .with_environment(endpoint_environment(|name| std::env::var_os(name)));
            let mut argv = vec![bin.into_os_string()];
            argv.extend(attach_arguments(&plan.values.host, &plan.values.id));
            let report = launcher.launch(&argv)?;
            Ok((!report.warnings.is_empty()).then(|| report.warnings.join("; ")))
        }
    }
}

/// Build the task that opens a session in a terminal.
///
/// Live sessions start the configured terminal immediately. Terminal
/// sessions first ask the daemon to relaunch from native resume metadata; the
/// command-completion path then calls this again and attaches to the live PTY.
pub(crate) fn attach_task(
    app: &PohunekApp,
    host_id: &HostId,
    session_id: &SessionId,
) -> Result<Task<Message>, String> {
    let session = app
        .workspace
        .hosts
        .get(host_id)
        .and_then(|host| host.sessions.get(&session_id.0));
    if let Some(session) = session.filter(|session| session_is_in_resumable_state(session)) {
        if !session.capabilities.resume {
            return Err("session does not support resume".to_owned());
        }
        if !session_has_native_resume_reference(session) {
            return Err("session does not have native resume metadata".to_owned());
        }
        return resume_session_task(app, host_id, session_id);
    }

    let plan = app.attach_plan(host_id, session_id)?;
    Ok(Task::perform(
        runtime::perform_blocking_or(
            move || {
                run_attach(&plan, &TerminalEnvironment::system()).map_err(|err| err.to_string())
            },
            Err,
        ),
        Message::AttachSpawned,
    ))
}

fn session_has_native_resume_reference(session: &SessionInfo) -> bool {
    session
        .native_session_id
        .as_deref()
        .is_some_and(|value| !value.is_empty())
        || session
            .native_session_path
            .as_deref()
            .is_some_and(|value| !value.is_empty())
}

fn session_is_in_resumable_state(session: &SessionInfo) -> bool {
    session.state.is_terminal()
        || session
            .runtime
            .as_ref()
            .is_some_and(|runtime| runtime.state == protocol::RuntimeState::Lost)
}

pub(crate) fn window_dimension_to_f32(value: u32) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Iced reports positive window pixel sizes as f32; UI state persists integer pixels"
)]
pub(crate) fn window_dimension_to_u32(value: f32) -> u32 {
    value.round().clamp(1.0, f32::from(u16::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    use super::*;
    use crate::terminal::AttachTerminal;

    const TEST_HOST: &str = "h $(echo INJECTED) 'q'";
    const TEST_ID: &str = "-i\"d; echo INJECTED";

    fn script(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write script");
        crate::test_support::make_executable(path);
    }

    /// Creates a recorder that writes its NUL-terminated argv into a FIFO, so a
    /// test can block on the detached child's output instead of polling.
    fn fifo_recorder(dir: &Path) -> (PathBuf, PathBuf) {
        let fifo = dir.join("argv.fifo");
        let status = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
        assert!(status.success());
        let recorder = dir.join("recorder");
        script(
            &recorder,
            &format!("printf '%s\\0' \"$@\" > '{}'", fifo.display()),
        );
        (recorder, fifo)
    }

    fn read_argv(path: &Path) -> Vec<Vec<u8>> {
        let bytes = fs::read(path).expect("read argv");
        let mut words: Vec<Vec<u8>> = bytes.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect();
        assert_eq!(words.pop(), Some(Vec::new()));
        words
    }

    fn expected_attach_words(prefix: &[&str]) -> Vec<Vec<u8>> {
        prefix
            .iter()
            .map(|word| word.as_bytes().to_vec())
            .chain([TEST_HOST.as_bytes().to_vec(), b"attach".to_vec()])
            .chain([TEST_ID.as_bytes().to_vec()])
            .collect()
    }

    fn plan(selection: AttachSelection, bin: &Path) -> AttachPlan {
        let program = bin.to_str().expect("utf8 path").to_owned();
        AttachPlan {
            selection,
            resolver: Arc::new(BinResolver::with_discovery(&program, || {
                Err(BinError::SearchPath("discovery must not run".to_owned()))
            })),
            launch: LaunchSettings {
                open_timeout: Duration::from_secs(30),
                login_shell_timeout: Duration::from_secs(1),
                login_shell_max_output_bytes: 1024,
                notification_timeout: Duration::from_secs(1),
                attach_observe: Duration::from_millis(100),
                attach_script_max_age: Duration::from_secs(3600),
            },
            values: AttachTemplateValues {
                bin: program,
                host: TEST_HOST.to_owned(),
                id: TEST_ID.to_owned(),
            },
        }
    }

    fn command(template: &str, mode: AttachCommandMode) -> AttachSelection {
        AttachSelection::Command {
            template: template.to_owned(),
            mode,
        }
    }

    fn unused_terminal() -> TerminalEnvironment {
        TerminalEnvironment {
            opener: PathBuf::from("/nonexistent/open"),
            script_dir: None,
        }
    }

    #[test]
    fn argv_mode_passes_values_as_single_arguments_without_a_shell() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let (recorder, fifo) = fifo_recorder(dir.path());
        let plan = plan(
            command("{bin} --flag {host} attach {id}", AttachCommandMode::Argv),
            &recorder,
        );

        run_attach(&plan, &unused_terminal()).expect("attach");

        assert_eq!(read_argv(&fifo), expected_attach_words(&["--flag"]));
    }

    #[test]
    fn shell_mode_passes_values_as_single_arguments() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let (recorder, fifo) = fifo_recorder(dir.path());
        let plan = plan(
            command("{bin} --flag {host} attach {id}", AttachCommandMode::Shell),
            &recorder,
        );

        run_attach(&plan, &unused_terminal()).expect("attach");

        assert_eq!(read_argv(&fifo), expected_attach_words(&["--flag"]));
    }

    /// Gives a plan an observation window long enough to see an early exit.
    fn observing(mut plan: AttachPlan) -> AttachPlan {
        plan.launch.attach_observe = Duration::from_secs(30);
        plan
    }

    #[test]
    fn argv_mode_reports_a_missing_program_with_its_name() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let (recorder, _fifo) = fifo_recorder(dir.path());
        let plan = plan(
            command("/nonexistent/launcher {bin}", AttachCommandMode::Argv),
            &recorder,
        );

        let error = run_attach(&plan, &unused_terminal()).expect_err("missing program");

        assert!(matches!(error, AttachError::Bin(_)), "{error}");
        assert!(error.to_string().contains("/nonexistent/launcher"));
    }

    #[test]
    fn argv_mode_resolves_a_bare_launcher_through_the_search_policy() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let (recorder, fifo) = fifo_recorder(dir.path());
        let bin_dir = crate::test_support::fixture();
        // A launcher that exists only in the discovered search path, not on the
        // GUI's own PATH, like kitty under a Finder launch.
        let launcher = bin_dir.path().join("fake-terminal");
        fs::copy(&recorder, &launcher).expect("copy launcher");
        let search =
            pohunek_platform::shell_env::SearchPath::new(vec![bin_dir.path().to_path_buf()])
                .expect("search path");
        let mut plan = plan(
            command(
                "fake-terminal --flag {host} attach {id}",
                AttachCommandMode::Argv,
            ),
            &recorder,
        );
        plan.resolver = Arc::new(BinResolver::with_discovery(
            recorder.to_str().expect("utf8"),
            move || Ok(search.clone()),
        ));

        run_attach(&plan, &unused_terminal()).expect("attach");

        assert_eq!(read_argv(&fifo), expected_attach_words(&["--flag"]));
    }

    #[test]
    fn argv_mode_reports_a_bare_launcher_that_cannot_be_found() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let (recorder, _fifo) = fifo_recorder(dir.path());
        let empty = crate::test_support::fixture();
        let search = pohunek_platform::shell_env::SearchPath::new(vec![empty.path().to_path_buf()])
            .expect("search path");
        let mut plan = plan(
            command("no-such-terminal {bin}", AttachCommandMode::Argv),
            &recorder,
        );
        plan.resolver = Arc::new(BinResolver::with_discovery(
            recorder.to_str().expect("utf8"),
            move || Ok(search.clone()),
        ));

        let error = run_attach(&plan, &unused_terminal()).expect_err("missing launcher");

        assert!(matches!(error, AttachError::Bin(_)), "{error}");
        assert!(error.to_string().contains("no-such-terminal"));
    }

    #[test]
    fn an_argv_launcher_that_exits_at_once_is_reported() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let (recorder, _fifo) = fifo_recorder(dir.path());
        let plan = observing(plan(
            command("/usr/bin/false {bin}", AttachCommandMode::Argv),
            &recorder,
        ));

        let error = run_attach(&plan, &unused_terminal()).expect_err("dead launcher");

        assert!(
            matches!(
                error,
                AttachError::Observe(ObserveError::ExitedEarly { .. })
            ),
            "{error}"
        );
    }

    #[test]
    fn a_dead_shell_template_is_reported_with_its_exit_status() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let (recorder, _fifo) = fifo_recorder(dir.path());
        let plan = observing(plan(
            command("/nonexistent/terminal {bin}", AttachCommandMode::Shell),
            &recorder,
        ));

        let error = run_attach(&plan, &unused_terminal()).expect_err("dead template");

        assert!(error.to_string().contains("127"), "{error}");
    }

    #[test]
    fn argv_mode_rejects_an_unterminated_template() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let (recorder, _fifo) = fifo_recorder(dir.path());
        let plan = plan(command("{bin} 'oops", AttachCommandMode::Argv), &recorder);

        assert!(matches!(
            run_attach(&plan, &unused_terminal()),
            Err(AttachError::Template(
                AttachTemplateError::UnterminatedQuote
            ))
        ));
    }

    #[test]
    fn an_unresolvable_pohunek_bin_fails_before_anything_is_spawned() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let marker = dir.path().join("spawned");
        let plan = plan(
            command(
                &format!("touch '{}'", marker.display()),
                AttachCommandMode::Shell,
            ),
            &dir.path().join("no-such-pohunek"),
        );

        let error = run_attach(&plan, &unused_terminal()).expect_err("unresolvable");

        assert!(matches!(error, AttachError::Bin(_)), "{error}");
        assert!(error.to_string().contains("no-such-pohunek"));
        assert!(!marker.exists());
    }

    fn terminal_environment(dir: &Path, opener_body: &str) -> TerminalEnvironment {
        let opener = dir.join("fake-open");
        let record = dir.join("open-argv");
        script(
            &opener,
            &format!(
                "printf '%s\\0' \"$@\" > '{}'\n{opener_body}",
                record.display()
            ),
        );
        TerminalEnvironment {
            opener,
            script_dir: Some(dir.join("gui-attach")),
        }
    }

    #[test]
    fn terminal_mode_runs_the_resolved_bin_with_attach_arguments_through_open() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let output = dir.path().join("argv.out");
        let recorder = dir.path().join("recorder");
        script(
            &recorder,
            &format!("printf '%s\\0' \"$@\" > '{}'", output.display()),
        );
        let environment = terminal_environment(dir.path(), "/bin/sh \"$3\"");
        let plan = plan(
            AttachSelection::Terminal(AttachTerminal::TerminalApp),
            &recorder,
        );

        run_attach(&plan, &environment).expect("attach");

        assert_eq!(
            read_argv(&output),
            vec![
                format!("--host={TEST_HOST}").into_bytes(),
                b"attach".to_vec(),
                b"--".to_vec(),
                TEST_ID.as_bytes().to_vec(),
            ]
        );
        let open_args = read_argv(&dir.path().join("open-argv"));
        assert_eq!(&open_args[..2], [b"-a".to_vec(), b"Terminal".to_vec()]);
    }

    #[test]
    fn terminal_mode_surfaces_an_opener_failure_as_a_typed_error() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let recorder = dir.path().join("recorder");
        script(&recorder, "exit 0");
        let environment = terminal_environment(dir.path(), "exit 9");
        let plan = plan(
            AttachSelection::Terminal(AttachTerminal::TerminalApp),
            &recorder,
        );

        let error = run_attach(&plan, &environment).expect_err("opener failure");

        assert!(
            matches!(
                error,
                AttachError::Terminal(TerminalError::OpenerFailed { .. })
            ),
            "{error}"
        );
        assert!(error.to_string().contains("exited unsuccessfully"));
    }

    #[test]
    fn terminal_mode_rejects_a_nul_byte_in_the_session_id() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let recorder = dir.path().join("recorder");
        script(&recorder, "exit 0");
        let environment = terminal_environment(dir.path(), "exit 0");
        let mut plan = plan(
            AttachSelection::Terminal(AttachTerminal::TerminalApp),
            &recorder,
        );
        plan.values.id = "s\0-1".to_owned();

        let error = run_attach(&plan, &environment).expect_err("nul");

        assert!(
            matches!(
                error,
                AttachError::Terminal(TerminalError::Script(crate::terminal::ScriptError::NulByte))
            ),
            "{error}"
        );
    }

    #[test]
    fn terminal_mode_surfaces_a_stale_script_cleanup_warning_without_failing() {
        let _watchdog = crate::test_support::watchdog();
        let dir = crate::test_support::fixture();
        let recorder = dir.path().join("recorder");
        script(&recorder, "exit 0");
        let environment = terminal_environment(dir.path(), "exit 0");
        let script_dir = dir.path().join("gui-attach");
        fs::create_dir(&script_dir).expect("mkdir");
        fs::set_permissions(&script_dir, fs::Permissions::from_mode(0o700)).expect("chmod");
        // A script with a loose mode cannot be validated, so it cannot be removed.
        let loose = script_dir.join("attach-loose.command");
        script(&loose, "exit 0");
        let plan = plan(
            AttachSelection::Terminal(AttachTerminal::TerminalApp),
            &recorder,
        );

        let warning = run_attach(&plan, &environment)
            .expect("the launch succeeds")
            .expect("a cleanup warning");

        assert!(warning.contains("attach-loose.command"), "{warning}");
    }
}
