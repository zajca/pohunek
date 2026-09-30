//! Terminal attach/resume plumbing and window-size unit conversions.

// Rust guideline compliant 2026-09-30

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::Arc;

use iced::Task;
use pohunek_gui_core::{
    render_attach_argv, spawn_attach_command, AttachCommandSpawner, AttachSpawnError,
    AttachTemplateError, AttachTemplateValues, HostId,
};
use protocol::{SessionId, SessionInfo};
use thiserror::Error;

use crate::bin_resolver::{BinError, BinResolver};
use crate::command::resume_session_task;
use crate::config::{AttachCommandMode, AttachSelection, LaunchSettings};
use crate::message::Message;
use crate::runtime;
use crate::terminal::{
    attach_arguments, spawn_detached, TerminalError, TerminalLauncher, SYSTEM_OPEN,
};
use crate::PohunekApp;

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
    #[error("failed to spawn attach program `{}`: {source}", program.to_string_lossy())]
    Spawn {
        program: OsString,
        source: std::io::Error,
    },
    #[error("{0}")]
    Shell(String),
    #[error("cannot locate the attach script directory: {0}")]
    ScriptDirectory(#[source] pohunek_paths::PathError),
    #[error(transparent)]
    Terminal(#[from] TerminalError),
}

/// Spawns rendered attach commands through `sh -c`, detached from the GUI.
#[derive(Debug, Default)]
struct ShellAttachSpawner;

impl AttachCommandSpawner for ShellAttachSpawner {
    fn spawn(&mut self, command: &str) -> Result<(), String> {
        spawn_detached(
            OsStr::new("sh"),
            &[OsString::from("-c"), OsString::from(command)],
        )
        .map(|_| ())
        .map_err(|err| format!("failed to spawn attach command `{command}`: {err}"))
    }
}

/// Resolves `pohunek_bin` and starts the configured terminal.
///
/// Blocks on executable resolution and, for a stock terminal, on the bounded
/// `open` call, so callers run it off the UI thread.
pub(crate) fn run_attach(
    plan: &AttachPlan,
    terminal: &TerminalEnvironment,
) -> Result<(), AttachError> {
    let bin = plan.resolver.resolve()?;
    match &plan.selection {
        AttachSelection::Command { template, mode } => {
            let values = AttachTemplateValues {
                bin: bin.to_str().ok_or(AttachError::NonUtf8Bin)?.to_owned(),
                host: plan.values.host.clone(),
                id: plan.values.id.clone(),
            };
            match mode {
                AttachCommandMode::Shell => {
                    spawn_attach_command(&mut ShellAttachSpawner, template, &values)
                        .map(|_| ())
                        .map_err(|error| match error {
                            AttachSpawnError::Template(error) => AttachError::Template(error),
                            AttachSpawnError::Spawn(message) => AttachError::Shell(message),
                            _ => AttachError::Shell(error.to_string()),
                        })
                }
                AttachCommandMode::Argv => {
                    let argv: Vec<OsString> = render_attach_argv(template, &values)?
                        .into_iter()
                        .map(OsString::from)
                        .collect();
                    let (program, arguments) = argv
                        .split_first()
                        .expect("render_attach_argv returns at least the program");
                    spawn_detached(program, arguments)
                        .map(|_| ())
                        .map_err(|source| AttachError::Spawn {
                            program: program.clone(),
                            source,
                        })
                }
            }
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
            );
            let mut argv = vec![bin.into_os_string()];
            argv.extend(attach_arguments(&plan.values.host, &plan.values.id));
            launcher.launch(&argv)?;
            Ok(())
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
        runtime::perform_blocking(move || {
            run_attach(&plan, &TerminalEnvironment::system()).map_err(|err| err.to_string())
        }),
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
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod script");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
        let (recorder, fifo) = fifo_recorder(dir.path());
        let plan = plan(
            command("{bin} --flag {host} attach {id}", AttachCommandMode::Shell),
            &recorder,
        );

        run_attach(&plan, &unused_terminal()).expect("attach");

        assert_eq!(read_argv(&fifo), expected_attach_words(&["--flag"]));
    }

    #[test]
    fn argv_mode_reports_a_missing_program_with_its_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (recorder, _fifo) = fifo_recorder(dir.path());
        let plan = plan(
            command("/nonexistent/launcher {bin}", AttachCommandMode::Argv),
            &recorder,
        );

        let error = run_attach(&plan, &unused_terminal()).expect_err("missing program");

        assert!(matches!(error, AttachError::Spawn { .. }), "{error}");
        assert!(error.to_string().contains("/nonexistent/launcher"));
    }

    #[test]
    fn argv_mode_rejects_an_unterminated_template() {
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
        let dir = tempfile::tempdir().expect("tempdir");
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
}
