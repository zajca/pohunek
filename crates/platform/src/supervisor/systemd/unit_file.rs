//! Renders the persistent systemd unit files of one installation.
//!
//! Files are built from typed sections whose values pass through one escaping
//! function per value grammar (`systemd.syntax(7)`, `systemd.service(5)`), so a
//! hostile path or argument can never add a directive, a command, a specifier,
//! or an environment expansion.

use std::path::Path;
use std::time::Duration;

use super::super::{Error, JobDefinition, RestartPolicy};

// Rust guideline compliant 2026-09-24

/// Description of the daemon unit shown by `systemctl --user status`.
const DAEMON_DESCRIPTION: &str = "Pohunek host control plane";
/// Description of the slice grouping worker units.
const SESSIONS_DESCRIPTION: &str = "Pohunek durable session workers";
/// Target that starts the daemon at login once the unit is enabled.
const DAEMON_WANTED_BY: &str = "default.target";

/// One `[Section]` with its ordered `Key=value` entries.
#[derive(Debug)]
struct Section {
    name: &'static str,
    entries: Vec<(&'static str, String)>,
}

impl Section {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            entries: Vec::new(),
        }
    }

    fn entry(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.entries.push((key, value.into()));
        self
    }
}

fn render(sections: &[Section]) -> String {
    let mut output = String::new();
    for (index, section) in sections.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        output.push('[');
        output.push_str(section.name);
        output.push_str("]\n");
        for (key, value) in &section.entries {
            output.push_str(key);
            output.push('=');
            output.push_str(value);
            output.push('\n');
        }
    }
    output
}

/// Renders the persistent daemon unit for `definition`.
///
/// The unit is `Type=notify` with `NotifyAccess=main`, restarts according to
/// the definition's policy, keeps the whole control group in `KillMode`, and
/// is wanted by `default.target` so enabling it starts the daemon at login.
/// Log paths in the definition are ignored because systemd keeps the journal.
///
/// # Errors
///
/// Returns [`Error::InvalidDefinition`] when a value cannot be expressed in a
/// unit file, such as a control character or a working directory ending in
/// whitespace or a backslash.
pub fn render_daemon_unit(definition: &JobDefinition) -> Result<String, Error> {
    let mut command = Vec::with_capacity(definition.arguments().len() + 1);
    command.push(exec_word(path_str(definition.executable())?)?);
    for argument in definition.arguments() {
        command.push(exec_word(argument)?);
    }
    let mut service = Section::new("Service")
        .entry("Type", "notify")
        .entry("NotifyAccess", "main")
        .entry("ExecStart", command.join(" "))
        .entry(
            "WorkingDirectory",
            plain_path(definition.working_directory())?,
        );
    for (key, value) in definition.environment() {
        service = service.entry("Environment", environment_assignment(key, value)?);
    }
    service = match definition.restart() {
        RestartPolicy::Never => service.entry("Restart", "no"),
        RestartPolicy::OnFailure { throttle } => service
            .entry("Restart", "on-failure")
            .entry("RestartSec", time_span(throttle)),
    };
    service = service
        .entry("TimeoutStartSec", time_span(definition.start_timeout()))
        .entry("TimeoutStopSec", time_span(definition.exit_timeout()))
        .entry("KillMode", "control-group")
        .entry("LimitNOFILE", definition.open_files().to_string());
    Ok(render(&[
        Section::new("Unit").entry("Description", DAEMON_DESCRIPTION),
        service,
        Section::new("Install").entry("WantedBy", DAEMON_WANTED_BY),
    ]))
}

/// Renders the slice grouping this installation's worker units.
///
/// Memory and task accounting stay enabled so `systemd-cgtop` and the slice's
/// cgroup show what all session workers consume together. CPU accounting is
/// on by default for the unified cgroup hierarchy on every supported systemd,
/// and newer releases (261 observed) warn that `CPUAccounting=` is ignored, so
/// the slice does not set it.
#[must_use]
pub fn render_sessions_slice() -> String {
    render(&[
        Section::new("Unit").entry("Description", SESSIONS_DESCRIPTION),
        Section::new("Slice")
            .entry("MemoryAccounting", "yes")
            .entry("TasksAccounting", "yes"),
    ])
}

fn invalid(detail: String) -> Error {
    Error::InvalidDefinition { detail }
}

fn path_str(path: &Path) -> Result<&str, Error> {
    path.to_str()
        .ok_or_else(|| invalid("unit file paths must be valid UTF-8".to_owned()))
}

fn reject_control(field: &str, value: &str) -> Result<(), Error> {
    if value.chars().any(char::is_control) {
        return Err(invalid(format!(
            "{field} contains a control character, which a unit file cannot carry"
        )));
    }
    Ok(())
}

/// Quotes one `ExecStart=` word.
///
/// systemd expands specifiers over the whole line, then splits and C-unescapes
/// quoted words, then substitutes `$VAR` per word. Escaping `\` and `"` keeps
/// the word intact, `%%` survives specifier expansion as `%`, and `$$`
/// survives environment substitution as `$`. Quoting every word also keeps a
/// lone `;` from separating commands.
fn exec_word(value: &str) -> Result<String, Error> {
    reject_control("ExecStart argument", value)?;
    let mut word = String::with_capacity(value.len() + 2);
    word.push('"');
    for character in value.chars() {
        match character {
            '\\' => word.push_str("\\\\"),
            '"' => word.push_str("\\\""),
            '%' => word.push_str("%%"),
            '$' => word.push_str("$$"),
            other => word.push(other),
        }
    }
    word.push('"');
    Ok(word)
}

/// Quotes one `Environment=` assignment.
///
/// `Environment=` expands specifiers and C-unescapes quoted words but never
/// substitutes `$`, so only `\`, `"`, and `%` need escaping.
fn environment_assignment(key: &str, value: &str) -> Result<String, Error> {
    reject_control("Environment value", value)?;
    let mut assignment = String::with_capacity(key.len() + value.len() + 3);
    assignment.push('"');
    for character in key.chars().chain(std::iter::once('=')).chain(value.chars()) {
        match character {
            '\\' => assignment.push_str("\\\\"),
            '"' => assignment.push_str("\\\""),
            '%' => assignment.push_str("%%"),
            other => assignment.push(other),
        }
    }
    assignment.push('"');
    Ok(assignment)
}

/// Renders an unquoted path value such as `WorkingDirectory=`.
///
/// These settings expand specifiers but never unquote, and the parser trims
/// surrounding whitespace and treats a trailing `\` as a line continuation, so
/// such paths are rejected instead of silently changed.
fn plain_path(path: &Path) -> Result<String, Error> {
    let value = path_str(path)?;
    reject_control("WorkingDirectory", value)?;
    if value.ends_with(char::is_whitespace) || value.ends_with('\\') {
        return Err(invalid(
            "WorkingDirectory must not end with whitespace or a backslash".to_owned(),
        ));
    }
    Ok(value.replace('%', "%%"))
}

/// Renders a time span in the largest unit that keeps it exact.
///
/// systemd resolves time spans to microseconds; a sub-microsecond remainder is
/// rounded up so a validated non-zero span never renders as `0`, which systemd
/// reads as "no timeout".
fn time_span(value: Duration) -> String {
    if value.subsec_nanos() == 0 {
        format!("{}s", value.as_secs())
    } else if value.subsec_nanos().is_multiple_of(1_000_000) {
        format!("{}ms", value.as_millis())
    } else {
        format!("{}us", value.as_nanos().div_ceil(1_000))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::supervisor::JobSpec;

    fn definition(arguments: &[&str], working_directory: &str) -> Result<JobDefinition, Error> {
        JobDefinition::new(JobSpec {
            executable: PathBuf::from("/home/u/.local/libexec/pohunek/1.2.3/pohunekd"),
            arguments: arguments.iter().map(|&value| value.to_owned()).collect(),
            environment: BTreeMap::from([
                ("HOME".to_owned(), "/home/u".to_owned()),
                (
                    "XDG_STATE_HOME".to_owned(),
                    "/home/u/state \"q\" 100% \\x".to_owned(),
                ),
            ]),
            working_directory: PathBuf::from(working_directory),
            logs: None,
            start_timeout: Duration::from_secs(45),
            exit_timeout: Duration::from_millis(1_500),
            restart: RestartPolicy::OnFailure {
                throttle: Duration::from_secs(5),
            },
            open_files: 8_192,
        })
    }

    #[test]
    fn daemon_unit_matches_the_golden_file() {
        let definition = definition(
            &["--service-config", "/home/u/.config/pohunek/service.toml"],
            "/home/u",
        )
        .expect("valid definition");
        assert_eq!(
            render_daemon_unit(&definition).expect("renderable unit"),
            "[Unit]\n\
             Description=Pohunek host control plane\n\
             \n\
             [Service]\n\
             Type=notify\n\
             NotifyAccess=main\n\
             ExecStart=\"/home/u/.local/libexec/pohunek/1.2.3/pohunekd\" \"--service-config\" \"/home/u/.config/pohunek/service.toml\"\n\
             WorkingDirectory=/home/u\n\
             Environment=\"HOME=/home/u\"\n\
             Environment=\"XDG_STATE_HOME=/home/u/state \\\"q\\\" 100%% \\\\x\"\n\
             Restart=on-failure\n\
             RestartSec=5s\n\
             TimeoutStartSec=45s\n\
             TimeoutStopSec=1500ms\n\
             KillMode=control-group\n\
             LimitNOFILE=8192\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        );
    }

    #[test]
    fn a_never_restarting_daemon_has_no_restart_delay() {
        let mut spec = JobSpec {
            executable: PathBuf::from("/opt/pohunekd"),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            working_directory: PathBuf::from("/"),
            logs: None,
            start_timeout: Duration::from_micros(1_500_001),
            exit_timeout: Duration::from_secs(30),
            restart: RestartPolicy::Never,
            open_files: 256,
        };
        let rendered = render_daemon_unit(&JobDefinition::new(spec.clone()).expect("valid"))
            .expect("renderable unit");
        assert!(rendered.contains("\nRestart=no\n"));
        assert!(!rendered.contains("RestartSec"));
        assert!(rendered.contains("\nTimeoutStartSec=1500001us\n"));
        assert_eq!(time_span(Duration::from_nanos(1)), "1us");
        spec.executable = PathBuf::from("/opt/100%/$HOME/pohunekd");
        let rendered =
            render_daemon_unit(&JobDefinition::new(spec).expect("valid")).expect("renderable");
        assert!(rendered.contains("\nExecStart=\"/opt/100%%/$$HOME/pohunekd\"\n"));
    }

    #[test]
    fn exec_words_escape_every_special_character() {
        for (input, expected) in [
            ("plain", "\"plain\""),
            ("with space", "\"with space\""),
            ("a\"b", "\"a\\\"b\""),
            ("back\\slash", "\"back\\\\slash\""),
            ("100%", "\"100%%\""),
            ("%h", "\"%%h\""),
            ("$HOME", "\"$$HOME\""),
            ("${HOME}", "\"$${HOME}\""),
            (";", "\";\""),
            ("'single'", "\"'single'\""),
            ("\\n", "\"\\\\n\""),
            ("", "\"\""),
            ("žluťoučký", "\"žluťoučký\""),
        ] {
            assert_eq!(exec_word(input).expect("expressible word"), expected);
        }
    }

    #[test]
    fn control_characters_are_rejected_everywhere() {
        for hostile in [
            "new\nline",
            "carriage\rreturn",
            "tab\there",
            "bell\u{7}",
            "c1\u{85}",
        ] {
            assert!(matches!(
                exec_word(hostile),
                Err(Error::InvalidDefinition { .. })
            ));
            assert!(matches!(
                environment_assignment("HOME", hostile),
                Err(Error::InvalidDefinition { .. })
            ));
        }
        for hostile in ["/home/u\nExecStartPre=/bin/sh", "/home/u ", "/home/u\\"] {
            assert!(matches!(
                definition(&[], hostile).and_then(|definition| render_daemon_unit(&definition)),
                Err(Error::InvalidDefinition { .. })
            ));
        }
        assert!(matches!(
            definition(&["--flag\nExecStartPre=/bin/sh"], "/home/u")
                .and_then(|definition| render_daemon_unit(&definition)),
            Err(Error::InvalidDefinition { .. })
        ));
    }

    #[test]
    fn environment_assignments_keep_dollar_signs_literal() {
        assert_eq!(
            environment_assignment("HOME", "/h/$USER/%i").expect("expressible"),
            "\"HOME=/h/$USER/%%i\""
        );
    }

    #[test]
    fn working_directories_escape_specifiers() {
        assert_eq!(
            plain_path(Path::new("/srv/100% done")).expect("expressible"),
            "/srv/100%% done"
        );
    }

    #[test]
    fn sessions_slice_matches_the_golden_file() {
        assert_eq!(
            render_sessions_slice(),
            "[Unit]\n\
             Description=Pohunek durable session workers\n\
             \n\
             [Slice]\n\
             MemoryAccounting=yes\n\
             TasksAccounting=yes\n"
        );
    }
}
