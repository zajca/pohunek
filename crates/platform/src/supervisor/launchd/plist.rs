//! Renders and reads back launchd job definitions as XML property lists.
//!
//! Serialization and escaping belong to the `plist` crate; this module only
//! chooses keys. One renderer covers both job kinds, keyed on the definition's
//! [`RestartPolicy`]:
//!
//! - [`RestartPolicy::Never`] (session workers): `RunAtLoad` and no
//!   `KeepAlive`, so a worker runs once per bootstrap and is never restarted
//!   or resurrected.
//! - [`RestartPolicy::OnFailure`] (the daemon agent): `KeepAlive` with
//!   `SuccessfulExit=false` and `ThrottleInterval`, the launchd equivalent of
//!   systemd `Restart=on-failure` with `RestartSec`.
//!
//! `ProcessType` is omitted, which selects launchd's Standard class: the
//! Background class throttles interactive agents and Interactive is reserved
//! for UI-critical jobs. The definition's start timeout has no launchd key;
//! readiness is proven through the worker's private socket instead.

use std::time::Duration;

use ::plist::{Dictionary, Integer, Value};

use super::super::{DefinitionFacts, Error, JobDefinition, RestartPolicy};

// Rust guideline compliant 2026-09-24

/// Largest definition file read back from disk.
///
/// A valid definition holds at most 64 arguments of 4 KiB plus a few bounded
/// paths; even if every byte needed the longest XML escape (`&quot;`, six
/// bytes) it stays below 2 MiB. Larger files are corrupt or foreign.
#[cfg_attr(
    not(target_os = "macos"),
    expect(
        dead_code,
        reason = "only the macOS backend reads definitions from disk"
    )
)]
pub(crate) const MAX_DEFINITION_BYTES: usize = 2 * 1024 * 1024;

/// Fields read back from a stored definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stored {
    /// The `Label` key.
    pub(crate) label: String,
    /// Executable and arguments from `ProgramArguments`.
    pub(crate) facts: DefinitionFacts,
    /// The `ExitTimeOut` key.
    pub(crate) exit_timeout: Duration,
}

/// A stored definition that cannot be used.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ParseError {
    /// The bytes are not an XML property list.
    #[error("definition is not an XML property list: {0}")]
    Malformed(#[source] ::plist::Error),
    /// A required key is absent or has the wrong type.
    #[error("definition key `{0}` is missing or has the wrong type")]
    Key(&'static str),
    /// A key holds a value the backend never writes.
    #[error("definition key `{0}` has an invalid value")]
    Value(&'static str),
}

/// Renders the XML property list for `label`.
///
/// # Errors
///
/// Returns [`Error::InvalidDefinition`] when a string holds a character that
/// XML 1.0 cannot carry unchanged: C0 controls other than tab and line feed,
/// carriage return (parsers normalize it to line feed), and the noncharacters
/// U+FFFE and U+FFFF.
pub(crate) fn render(label: &str, definition: &JobDefinition) -> Result<Vec<u8>, Error> {
    let mut job = Dictionary::new();
    job.insert("Label".to_owned(), string("Label", label)?);
    let mut program = Vec::with_capacity(definition.arguments().len() + 1);
    program.push(path("ProgramArguments", definition.executable())?);
    for argument in definition.arguments() {
        program.push(string("ProgramArguments", argument)?);
    }
    job.insert("ProgramArguments".to_owned(), Value::Array(program));
    job.insert("RunAtLoad".to_owned(), Value::Boolean(true));
    if let RestartPolicy::OnFailure { throttle } = definition.restart() {
        let mut keep_alive = Dictionary::new();
        keep_alive.insert("SuccessfulExit".to_owned(), Value::Boolean(false));
        job.insert("KeepAlive".to_owned(), Value::Dictionary(keep_alive));
        job.insert("ThrottleInterval".to_owned(), seconds(throttle));
    }
    let mut environment = Dictionary::new();
    for (key, value) in definition.environment() {
        environment.insert(key.clone(), string("EnvironmentVariables", value)?);
    }
    job.insert(
        "EnvironmentVariables".to_owned(),
        Value::Dictionary(environment),
    );
    job.insert(
        "WorkingDirectory".to_owned(),
        path("WorkingDirectory", definition.working_directory())?,
    );
    if let Some(logs) = definition.logs() {
        job.insert(
            "StandardOutPath".to_owned(),
            path("StandardOutPath", &logs.stdout)?,
        );
        job.insert(
            "StandardErrorPath".to_owned(),
            path("StandardErrorPath", &logs.stderr)?,
        );
    }
    job.insert("ExitTimeOut".to_owned(), seconds(definition.exit_timeout()));
    job.insert("AbandonProcessGroup".to_owned(), Value::Boolean(false));
    job.insert(
        "SoftResourceLimits".to_owned(),
        open_files(definition.open_files()),
    );
    job.insert(
        "HardResourceLimits".to_owned(),
        open_files(definition.open_files()),
    );
    let mut bytes = Vec::new();
    Value::Dictionary(job)
        .to_writer_xml(&mut bytes)
        .map_err(|source| Error::InvalidDefinition {
            detail: format!("definition could not be serialized: {source}"),
        })?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Reads the label, program, and exit timeout back from a stored definition.
///
/// Only the XML format the backend writes is accepted.
pub(crate) fn parse(bytes: &[u8]) -> Result<Stored, ParseError> {
    let value = Value::from_reader_xml(bytes).map_err(ParseError::Malformed)?;
    let job = value.as_dictionary().ok_or(ParseError::Key("<root>"))?;
    let label = job
        .get("Label")
        .and_then(Value::as_string)
        .ok_or(ParseError::Key("Label"))?
        .to_owned();
    let program = job
        .get("ProgramArguments")
        .and_then(Value::as_array)
        .ok_or(ParseError::Key("ProgramArguments"))?;
    let mut program = program
        .iter()
        .map(|value| value.as_string().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or(ParseError::Key("ProgramArguments"))?
        .into_iter();
    let executable = program
        .next()
        .map(std::path::PathBuf::from)
        .filter(|executable| executable.is_absolute())
        .ok_or(ParseError::Value("ProgramArguments"))?;
    let exit_timeout = job
        .get("ExitTimeOut")
        .and_then(Value::as_unsigned_integer)
        .ok_or(ParseError::Key("ExitTimeOut"))?;
    if exit_timeout == 0 {
        return Err(ParseError::Value("ExitTimeOut"));
    }
    Ok(Stored {
        label,
        facts: DefinitionFacts {
            executable,
            arguments: program.collect(),
        },
        exit_timeout: Duration::from_secs(exit_timeout),
    })
}

/// Returns whether XML 1.0 carries `value` through a write and read unchanged.
fn xml_safe(value: &str) -> bool {
    value.chars().all(|character| {
        matches!(character, '\t' | '\n')
            || (character >= ' ' && !matches!(character, '\u{fffe}' | '\u{ffff}'))
    })
}

fn string(key: &str, value: &str) -> Result<Value, Error> {
    if xml_safe(value) {
        Ok(Value::String(value.to_owned()))
    } else {
        Err(Error::InvalidDefinition {
            detail: format!("`{key}` holds a character a property list cannot carry"),
        })
    }
}

fn path(key: &str, value: &std::path::Path) -> Result<Value, Error> {
    // `JobDefinition` guarantees UTF-8 paths.
    let value = value.to_str().ok_or_else(|| Error::InvalidDefinition {
        detail: format!("`{key}` is not valid UTF-8"),
    })?;
    string(key, value)
}

/// Converts a positive duration to whole seconds, rounding up so a sub-second
/// bound never becomes zero, which launchd reads as "use the default".
fn seconds(duration: Duration) -> Value {
    let whole = duration.as_secs() + u64::from(duration.subsec_nanos() > 0);
    Value::Integer(Integer::from(whole))
}

fn open_files(limit: u64) -> Value {
    let mut limits = Dictionary::new();
    limits.insert(
        "NumberOfFiles".to_owned(),
        Value::Integer(Integer::from(limit)),
    );
    Value::Dictionary(limits)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::super::super::{JobLogs, JobSpec};
    use super::*;

    const LABEL: &str =
        "io.github.zajca.pohunek.0123456789ab.worker.s-01KYAPVPFVHD56Z69B9CX3XWN2.abcd2345";

    fn worker_spec() -> JobSpec {
        JobSpec {
            executable: PathBuf::from("/opt/pohunek/libexec/pohunek/1.0.0/pohunek-sessiond"),
            arguments: [
                "--session-id",
                "s-01KYAPVPFVHD56Z69B9CX3XWN2",
                "--worker-generation",
                "abcd2345",
                "--service-config",
                "/Users/u/.config/pohunek/service.toml",
            ]
            .map(str::to_owned)
            .to_vec(),
            environment: BTreeMap::from([
                ("HOME".to_owned(), "/Users/u".to_owned()),
                (
                    "XDG_STATE_HOME".to_owned(),
                    "/Users/u/.local/state".to_owned(),
                ),
            ]),
            working_directory: PathBuf::from("/Users/u"),
            logs: Some(JobLogs {
                stdout: PathBuf::from("/Users/u/.local/state/pohunek/logs/launchd/w.out.log"),
                stderr: PathBuf::from("/Users/u/.local/state/pohunek/logs/launchd/w.err.log"),
            }),
            start_timeout: Duration::from_secs(45),
            exit_timeout: Duration::from_secs(30),
            restart: RestartPolicy::Never,
            open_files: 8_192,
        }
    }

    fn rendered(spec: JobSpec) -> String {
        let definition = JobDefinition::new(spec).expect("valid definition");
        String::from_utf8(render(LABEL, &definition).expect("definition renders"))
            .expect("XML is UTF-8")
    }

    #[test]
    fn worker_definition_matches_the_golden_plist() {
        let expected = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>io.github.zajca.pohunek.0123456789ab.worker.s-01KYAPVPFVHD56Z69B9CX3XWN2.abcd2345</string>
	<key>ProgramArguments</key>
	<array>
		<string>/opt/pohunek/libexec/pohunek/1.0.0/pohunek-sessiond</string>
		<string>--session-id</string>
		<string>s-01KYAPVPFVHD56Z69B9CX3XWN2</string>
		<string>--worker-generation</string>
		<string>abcd2345</string>
		<string>--service-config</string>
		<string>/Users/u/.config/pohunek/service.toml</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>EnvironmentVariables</key>
	<dict>
		<key>HOME</key>
		<string>/Users/u</string>
		<key>XDG_STATE_HOME</key>
		<string>/Users/u/.local/state</string>
	</dict>
	<key>WorkingDirectory</key>
	<string>/Users/u</string>
	<key>StandardOutPath</key>
	<string>/Users/u/.local/state/pohunek/logs/launchd/w.out.log</string>
	<key>StandardErrorPath</key>
	<string>/Users/u/.local/state/pohunek/logs/launchd/w.err.log</string>
	<key>ExitTimeOut</key>
	<integer>30</integer>
	<key>AbandonProcessGroup</key>
	<false/>
	<key>SoftResourceLimits</key>
	<dict>
		<key>NumberOfFiles</key>
		<integer>8192</integer>
	</dict>
	<key>HardResourceLimits</key>
	<dict>
		<key>NumberOfFiles</key>
		<integer>8192</integer>
	</dict>
</dict>
</plist>
"#;
        assert_eq!(rendered(worker_spec()), expected);
    }

    #[test]
    fn daemon_definition_restarts_only_after_failure() {
        let mut spec = worker_spec();
        spec.restart = RestartPolicy::OnFailure {
            throttle: Duration::from_millis(4_500),
        };
        spec.exit_timeout = Duration::from_millis(1);
        let xml = rendered(spec);
        let expected = "\t<key>RunAtLoad</key>
\t<true/>
\t<key>KeepAlive</key>
\t<dict>
\t\t<key>SuccessfulExit</key>
\t\t<false/>
\t</dict>
\t<key>ThrottleInterval</key>
\t<integer>5</integer>
";
        assert!(xml.contains(expected), "{xml}");
        assert!(xml.contains("<key>ExitTimeOut</key>\n\t<integer>1</integer>"));
        assert!(!xml.contains("ProcessType"));
    }

    #[test]
    fn workers_never_keep_alive() {
        let xml = rendered(worker_spec());
        assert!(!xml.contains("KeepAlive"));
        assert!(!xml.contains("ThrottleInterval"));
        assert!(!xml.contains("ProcessType"));
    }

    #[test]
    fn hostile_values_are_escaped_by_the_plist_crate() {
        let hostile = [
            "</string><key>KeepAlive</key><true/><string>",
            "a & b < c > d \" e ' f",
            "]]><!-- -->",
            "&amp; stays literal",
            "  leading and trailing  ",
            "multi\nline\twith tab",
            "unicode: \u{17e}lu\u{165}ou\u{10d}k\u{fd} k\u{16f}\u{148} \u{1f980} \u{202e}rtl",
            "\u{7f} delete",
        ];
        let mut spec = worker_spec();
        spec.arguments
            .extend(hostile.iter().map(|value| (*value).to_owned()));
        let definition = JobDefinition::new(spec).expect("valid definition");
        let bytes = render(LABEL, &definition).expect("definition renders");
        let xml = String::from_utf8(bytes.clone()).expect("XML is UTF-8");
        assert!(xml.contains("&lt;/string&gt;&lt;key&gt;KeepAlive&lt;/key&gt;"));
        assert!(xml.contains("a &amp; b &lt; c &gt; d"));
        assert!(xml.contains("&amp;amp; stays literal"));
        assert!(!xml.contains("<key>KeepAlive</key>"));

        let stored = parse(&bytes).expect("rendered definition parses");
        assert_eq!(stored.label, LABEL);
        assert_eq!(stored.facts, definition.facts());
        assert_eq!(stored.exit_timeout, Duration::from_secs(30));
    }

    #[test]
    fn characters_xml_cannot_carry_are_rejected() {
        for value in [
            "bell\u{7}",
            "escape\u{1b}[31m",
            "carriage\rreturn",
            "\u{fffe}",
            "\u{ffff}",
        ] {
            let mut spec = worker_spec();
            spec.arguments.push(value.to_owned());
            let definition = JobDefinition::new(spec).expect("valid definition");
            assert!(
                matches!(
                    render(LABEL, &definition),
                    Err(Error::InvalidDefinition { .. })
                ),
                "rendered {value:?}"
            );
        }
    }

    #[test]
    fn parsing_rejects_foreign_and_incomplete_definitions() {
        let binary = {
            let mut bytes = Vec::new();
            Value::Dictionary(Dictionary::new())
                .to_writer_binary(&mut bytes)
                .expect("binary plist renders");
            bytes
        };
        assert!(matches!(parse(&binary), Err(ParseError::Malformed(_))));
        assert!(matches!(
            parse(b"not a plist"),
            Err(ParseError::Malformed(_))
        ));

        let xml = |body: &str| {
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">{body}</plist>"
            )
        };
        let cases = [
            (xml("<array/>"), "<root>"),
            (xml("<dict></dict>"), "Label"),
            (
                xml("<dict><key>Label</key><string>x</string></dict>"),
                "ProgramArguments",
            ),
            (
                xml("<dict><key>Label</key><string>x</string>\
                     <key>ProgramArguments</key><array><integer>1</integer></array></dict>"),
                "ProgramArguments",
            ),
            (
                xml("<dict><key>Label</key><string>x</string>\
                     <key>ProgramArguments</key><array><string>/bin/sh</string></array></dict>"),
                "ExitTimeOut",
            ),
        ];
        for (document, key) in cases {
            assert!(
                matches!(parse(document.as_bytes()), Err(ParseError::Key(found)) if found == key),
                "{document}"
            );
        }
        let relative = xml("<dict><key>Label</key><string>x</string>\
             <key>ProgramArguments</key><array><string>sh</string></array>\
             <key>ExitTimeOut</key><integer>5</integer></dict>");
        assert!(matches!(
            parse(relative.as_bytes()),
            Err(ParseError::Value("ProgramArguments"))
        ));
        let zero = xml("<dict><key>Label</key><string>x</string>\
             <key>ProgramArguments</key><array><string>/bin/sh</string></array>\
             <key>ExitTimeOut</key><integer>0</integer></dict>");
        assert!(matches!(
            parse(zero.as_bytes()),
            Err(ParseError::Value("ExitTimeOut"))
        ));
    }

    #[test]
    fn external_entities_are_never_resolved() {
        let document = br#"<?xml version="1.0"?>
<!DOCTYPE plist [<!ENTITY secret SYSTEM "file:///etc/passwd">]>
<plist version="1.0"><dict><key>Label</key><string>&secret;</string>
<key>ProgramArguments</key><array><string>/bin/sh</string></array>
<key>ExitTimeOut</key><integer>5</integer></dict></plist>"#;
        // The reader drops the unknown entity instead of fetching it; the empty
        // label then never equals a file name, so the definition is rejected.
        let stored = parse(document).expect("undeclared entities are dropped");
        assert_eq!(stored.label, "");
    }
}
