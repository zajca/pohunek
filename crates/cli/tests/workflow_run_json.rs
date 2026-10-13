//! End-to-end contracts of `pohunek workflow run --json` and of the verbatim
//! argument tail of `workflow run`: the action's JSON document reaches the
//! envelope byte for byte, every other ending is one typed error envelope
//! that holds no output byte, the exit statuses match text mode, and every
//! argument after the action name reaches the action unchanged.
//!
//! Every scenario runs the real `pohunek` binary in a hermetic environment.
//! The plugin is built in the test with `package::build_archive` and
//! installed with `pohunek workflow install --yes`. Plugin programs use
//! absolute paths and shell builtins only, because the runner gives them no
//! `PATH`.

// Rust guideline compliant 2026-10-13

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

#[cfg(target_os = "linux")]
use nix::unistd::Pid;
use package::{build_archive, read_archive, ArchiveEntry, Limits, PackageDigest};
use pohunek_test_support::bin_exe;
use pohunek_test_support::env::TestEnv;
#[cfg(target_os = "linux")]
use pohunek_test_support::wait::poll_until;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;

/// Id of the fixture plugin.
const PLUGIN_ID: &str = "acme.json";

/// Version of the fixture plugin.
const PLUGIN_VERSION: &str = "1.0.0";

/// A document whose bytes a parse-and-reserialize round trip changes: keys
/// out of order, a float spelled with a fraction, and an integer beyond
/// `i64`.
const DOCUMENT: &str = r#"{"z":1,"a":1.0,"n":12345678901234567890}"#;

/// Deepest array and object nesting a `--json` document may have, as
/// documented: `serde_json`'s default parser accepts 127 levels, and the
/// envelope holds the document one level down.
const MAX_DOCUMENT_DEPTH: usize = 126;

/// Printed on stdout by actions whose output must never be shown.
const OUTPUT_SENTINEL: &str = "output-sentinel-7a31";

/// Output cap of the `flood` action.
const FLOOD_CAP: u64 = 4096;

/// Exit status of a run stopped because an output stream passed its cap.
const OUTPUT_LIMIT_EXIT: i32 = 125;

/// Exit status of a run stopped at its deadline.
const TIMEOUT_EXIT: i32 = 124;

/// Exit status `failing` exits with.
const FAILING_EXIT: i32 = 3;

/// Every action the fixture plugin declares.
///
/// `emit` prints its first argument without a newline. `doc` prints
/// [`DOCUMENT`] with whitespace around it and a line on stderr. `argv`
/// records its arguments NUL-terminated in the state directory and prints
/// their count as a JSON document. `context` prints the context JSON.
/// `none` accepts no arguments and marks that it ran. `failing` prints a
/// valid document, then exits [`FAILING_EXIT`]; `killed` ends itself with
/// `SIGTERM`. `flood` writes past its cap; `slow` outlives its deadline;
/// `escape` leaves a process outside its group writing to stdout.
fn manifest() -> String {
    format!(
        r#"schema = 1

[plugin]
id = "{PLUGIN_ID}"
version = "{PLUGIN_VERSION}"
description = "Workflow run --json scenarios"

[actions.emit]
argv = ["bin/emit"]
args = "passthrough"

[actions.doc]
argv = ["bin/doc"]

[actions.argv]
argv = ["bin/argv"]
args = "passthrough"

[actions.context]
argv = ["bin/context"]

[actions.none]
argv = ["bin/none"]

[actions.failing]
argv = ["bin/failing"]

[actions.killed]
argv = ["bin/killed"]

[actions.flood]
argv = ["bin/flood"]
max_output_bytes = {FLOOD_CAP}

[actions.slow]
argv = ["bin/slow"]
timeout_ms = 500

[actions.escape]
argv = ["bin/escape"]
"#
    )
}

const EMIT: &str = "#!/bin/sh\nprintf '%s' \"$1\"\n";

fn doc() -> String {
    format!("#!/bin/sh\nprintf '  %s\\n\\n' '{DOCUMENT}'\nprintf 'doc-stderr\\n' >&2\n")
}

const ARGV: &str = r#"#!/bin/sh
for arg in "$@"; do printf '%s\0' "$arg"; done > "$POHUNEK_PLUGIN_STATE_DIR/argv"
printf '{"count":%s}\n' "$#"
"#;

const CONTEXT: &str = "#!/bin/sh\nprintf '%s' \"$POHUNEK_PLUGIN_CONTEXT\"\n";

const NONE: &str = "#!/bin/sh\n: > \"$POHUNEK_PLUGIN_STATE_DIR/spawned\"\nprintf '{}'\n";

fn failing() -> String {
    format!(
        "#!/bin/sh\nprintf '%s\\n' '{{\"partial\":\"{OUTPUT_SENTINEL}\"}}'\n\
         printf 'failing-stderr\\n' >&2\nexit {FAILING_EXIT}\n"
    )
}

const KILLED: &str = "#!/bin/sh\nprintf '{}'\nkill -TERM $$\n";

/// Writes filler past the cap, then the sentinel, ignoring `SIGPIPE`.
fn flood() -> String {
    format!(
        "#!/bin/sh\ntrap '' PIPE\ni=0\nwhile [ \"$i\" -lt 128 ]; do \
         printf '%s\\n' 'filler-filler-filler-filler-filler-filler-filler-filler' \
         2>/dev/null; i=$((i + 1)); done\nprintf '%s\\n' '{OUTPUT_SENTINEL}' 2>/dev/null\n"
    )
}

/// Prints a valid document, then outlives the action's 500 ms deadline.
// timing-allowed: #325 a run that outlives its deadline is the subject of the timeout scenario.
const SLOW: &str = "#!/bin/sh\nprintf '{}'\n/bin/sleep 30\n";

/// Prints a valid document, then leaves behind a process in a session of its
/// own that keeps stdout open and writes a byte five times a second, and
/// exits 0 once that process has left the group.
// timing-allowed: #325 a steadily writing escaped process is the subject of the cut scenario.
const ESCAPE: &str = r#"#!/bin/sh
printf '{}'
/usr/bin/setsid /bin/sh -c ': > "$POHUNEK_PLUGIN_STATE_DIR/escaped"; while :; do printf x; /bin/sleep 0.2; done' &
printf '%s\n' "$!" > "$POHUNEK_PLUGIN_STATE_DIR/escapee.pid"
while [ ! -e "$POHUNEK_PLUGIN_STATE_DIR/escaped" ]; do :; done
"#;

fn entry(path: &str, contents: &str, executable: bool) -> ArchiveEntry {
    ArchiveEntry {
        path: path.to_owned(),
        contents: contents.as_bytes().to_vec(),
        executable,
    }
}

fn plugin_files() -> Vec<ArchiveEntry> {
    vec![
        entry("workflow.toml", &manifest(), false),
        entry("bin/emit", EMIT, true),
        entry("bin/doc", &doc(), true),
        entry("bin/argv", ARGV, true),
        entry("bin/context", CONTEXT, true),
        entry("bin/none", NONE, true),
        entry("bin/failing", &failing(), true),
        entry("bin/killed", KILLED, true),
        entry("bin/flood", &flood(), true),
        entry("bin/slow", SLOW, true),
        entry("bin/escape", ESCAPE, true),
    ]
}

/// The process envelope, with `ok` kept as its own bytes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    cli_version: String,
    protocol: Value,
    ok: Option<Box<RawValue>>,
    err: Option<Value>,
}

/// One hermetic environment with the fixture plugin installed.
struct Fixture {
    env: TestEnv,
}

impl Fixture {
    fn new() -> Self {
        let env = TestEnv::new().expect("create the hermetic test environment");
        let bytes = build_archive(&plugin_files(), &Limits::DEFAULT).expect("the archive builds");
        let digest: PackageDigest = read_archive(&bytes, &Limits::DEFAULT)
            .expect("the archive reads")
            .digest()
            .clone();
        let archive = env.root().join("json.tar.zst");
        fs::write(&archive, bytes).expect("write the archive");
        let fixture = Self { env };
        let output = fixture
            .command(&[
                "workflow",
                "install",
                archive.to_str().expect("UTF-8 path"),
                "--sha256",
                digest.as_str(),
                "--yes",
                "--json",
            ])
            .output()
            .expect("run the install");
        assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
        fixture
    }

    /// `pohunek <arguments>` in the hermetic environment.
    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = self.env.command(bin_exe("pohunek"));
        command.args(arguments);
        command
    }

    /// `pohunek workflow run --json acme.json <action> [<args>]`.
    fn json(&self, action: &str, args: &[&str]) -> Output {
        let mut arguments = vec!["workflow", "run", "--json", PLUGIN_ID, action];
        arguments.extend_from_slice(args);
        self.command(&arguments)
            .output()
            .expect("run the pohunek binary")
    }

    fn state_dir(&self) -> PathBuf {
        self.env
            .state_home()
            .join("pohunek/workflow-plugin-state")
            .join(PLUGIN_ID)
    }

    /// The text of every run log file, active and rotated.
    fn run_log_text(&self) -> String {
        let Ok(entries) = fs::read_dir(self.state_dir().join("logs")) else {
            return String::new();
        };
        entries
            .map(|entry| entry.expect("read a log entry").path())
            .filter(|path| {
                let family = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("runs.jsonl"));
                family && path.extension() != Some(std::ffi::OsStr::new("lock"))
            })
            .map(|path| fs::read_to_string(path).expect("read a run log file"))
            .collect()
    }

    /// The newest record of the active run log.
    fn last_record(&self) -> Value {
        let text = fs::read_to_string(self.state_dir().join("logs/runs.jsonl"))
            .expect("the run log exists");
        let line = text.lines().last().expect("a run record");
        serde_json::from_str(line).expect("a run record is one JSON line")
    }

    /// Reads a PID file the plugin of a running action writes, waiting until
    /// it is complete.
    #[cfg(target_os = "linux")]
    fn pid(&self, name: &str) -> Pid {
        let path = self.state_dir().join(name);
        poll_until(&format!("the plugin to write {name}"), || {
            let text = fs::read_to_string(&path).ok()?;
            text.strip_suffix('\n')?.parse().ok().map(Pid::from_raw)
        })
    }
}

fn describe(output: &Output) -> String {
    format!(
        "status {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Parses stdout as exactly one envelope of the standard shape.
fn envelope(output: &Output) -> Envelope {
    let document: Envelope = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not one envelope ({error}):\n{}",
            describe(output)
        )
    });
    assert!(!document.cli_version.is_empty(), "{}", describe(output));
    assert!(document.protocol.is_object(), "{}", describe(output));
    document
}

/// Asserts stdout is one error envelope with `code` and returns its `err`.
fn error_envelope(output: &Output, code: &str) -> Value {
    let document = envelope(output);
    assert!(
        document.ok.is_none(),
        "an error carries no ok: {}",
        describe(output)
    );
    let err = document.err.expect("an error envelope");
    assert_eq!(err["code"], code, "{}", describe(output));
    err
}

#[test]
fn the_actions_document_is_the_ok_payload_byte_for_byte() {
    let fixture = Fixture::new();

    let output = fixture.json("doc", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let document = envelope(&output);
    assert!(document.err.is_none(), "{}", describe(&output));
    let ok = document.ok.expect("a success envelope");
    assert_eq!(ok.get(), DOCUMENT, "{}", describe(&output));
    // Stderr streams to the terminal's stderr, not into the envelope.
    assert_eq!(String::from_utf8_lossy(&output.stderr), "doc-stderr\n");

    let record = fixture.last_record();
    assert_eq!(record["outcome"], "ok", "{record}");
    assert!(record.get("error_code").is_none(), "{record}");
    // Two spaces, the document, then two newlines.
    let printed = DOCUMENT.len() + 4;
    assert_eq!(record["stdout_bytes"], printed, "{record}");
    assert_eq!(record["stderr_bytes"], "doc-stderr\n".len(), "{record}");
}

#[test]
fn the_context_tells_the_action_its_output_is_json() {
    let fixture = Fixture::new();

    let output = fixture.json("context", &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let ok = envelope(&output).ok.expect("a success envelope");
    let context: Value = serde_json::from_str(ok.get()).expect("the context is JSON");
    assert_eq!(context["output"], "json", "{context}");
    assert_eq!(context["invocation"]["name"], "context", "{context}");

    let text = fixture
        .command(&["workflow", "run", PLUGIN_ID, "context"])
        .output()
        .expect("run in text mode");
    assert_eq!(text.status.code(), Some(0), "{}", describe(&text));
    let context: Value = serde_json::from_slice(&text.stdout).expect("the context is JSON");
    assert_eq!(context["output"], "text", "{context}");
}

/// `depth` nested arrays.
fn nested(depth: usize) -> String {
    format!("{}{}", "[".repeat(depth), "]".repeat(depth))
}

#[test]
fn a_document_nested_deeper_than_the_envelope_can_carry_is_refused() {
    let fixture = Fixture::new();

    // At the limit the envelope stays parseable by a default serde_json
    // consumer, and the document arrives unchanged.
    let deepest = nested(MAX_DOCUMENT_DEPTH);
    let output = fixture.json("emit", &[&deepest]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let ok = envelope(&output).ok.expect("a success envelope");
    assert_eq!(ok.get(), deepest);
    serde_json::from_slice::<Value>(&output.stdout)
        .unwrap_or_else(|error| panic!("a default consumer cannot parse the envelope: {error}"));

    // Brackets inside strings, after escaped quotes and backslashes, are not
    // nesting.
    let brackets = "[".repeat(2 * MAX_DOCUMENT_DEPTH);
    let quoted = format!(r#"["\"{brackets}","\\",{{"k{brackets}":"{brackets}"}}]"#);
    let output = fixture.json("emit", &[&quoted]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(
        envelope(&output).ok.expect("a success envelope").get(),
        quoted
    );

    for depth in [MAX_DOCUMENT_DEPTH + 1, 1000] {
        let output = fixture.json("emit", &[&nested(depth)]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "depth {depth}: {}",
            describe(&output)
        );
        let err = error_envelope(&output, "workflow_output_invalid");
        let msg = err["msg"].as_str().expect("msg is a string");
        assert!(msg.ends_with("it is not JSON"), "{msg}");
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("[["),
            "output bytes were printed: {}",
            describe(&output)
        );
        let record = fixture.last_record();
        assert_eq!(record["outcome"], "output_invalid", "{record}");
    }
}

#[test]
fn stdout_that_is_not_exactly_one_document_is_a_content_free_error() {
    let fixture = Fixture::new();
    let not_json = format!("{OUTPUT_SENTINEL} is not json");
    let cases = [
        (not_json.as_str(), "it is not JSON"),
        ("", "it is empty"),
        (" \n\t", "it is empty"),
        (
            r#"{"a":1}{"b":2}"#,
            "it has more data after its first JSON document",
        ),
        ("{} x", "it has more data after its first JSON document"),
        (r#"{"a":"#, "it ends inside its JSON document"),
    ];
    for (printed, reason) in cases {
        let output = fixture.json("emit", &[printed]);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{printed:?}: {}",
            describe(&output)
        );
        let err = error_envelope(&output, "workflow_output_invalid");
        assert_eq!(err["class"], "runtime", "{err}");
        let msg = err["msg"].as_str().expect("msg is a string");
        assert!(msg.ends_with(reason), "{printed:?}: {msg}");
        let record = fixture.last_record();
        assert_eq!(record["outcome"], "output_invalid", "{record}");
        assert_eq!(record["error_code"], "workflow_output_invalid", "{record}");
        assert_eq!(record["stdout_bytes"], printed.len(), "{record}");
    }

    let sinks = [
        String::from_utf8_lossy(&fixture.json("emit", &[&not_json]).stdout).into_owned(),
        String::from_utf8_lossy(&fixture.json("emit", &[&not_json]).stderr).into_owned(),
        fixture.run_log_text(),
    ];
    for sink in &sinks {
        assert!(
            !sink.contains(OUTPUT_SENTINEL),
            "output bytes leaked: {sink}"
        );
    }
}

#[test]
fn every_argument_after_the_action_reaches_it_verbatim() {
    let fixture = Fixture::new();
    let args = [
        "--json",
        "--",
        "-h",
        "--help",
        "--host",
        "elsewhere",
        "",
        "a b",
        "ž ✓ 漢字",
        "--",
    ];
    let expected: Vec<u8> = args.iter().flat_map(|arg| arg.bytes().chain([0])).collect();
    let recorded = fixture.state_dir().join("argv");

    // Text mode: nothing after the action is read by pohunek.
    let mut arguments = vec!["workflow", "run", PLUGIN_ID, "argv"];
    arguments.extend_from_slice(&args);
    let output = fixture
        .command(&arguments)
        .output()
        .expect("run in text mode");
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{{\"count\":{}}}\n", args.len())
    );
    assert_eq!(fs::read(&recorded).expect("argv recorded"), expected);
    fs::remove_file(&recorded).expect("reset the recording");

    // JSON mode: the same tail, with pohunek's own `--json` before PLUGIN.
    let output = fixture.json("argv", &args);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let ok = envelope(&output).ok.expect("a success envelope");
    assert_eq!(ok.get(), format!("{{\"count\":{}}}", args.len()));
    assert_eq!(fs::read(&recorded).expect("argv recorded"), expected);
    assert_eq!(fixture.last_record()["arg_count"], args.len());
}

#[test]
fn extra_arguments_to_an_action_that_refuses_them_start_nothing() {
    let fixture = Fixture::new();

    let output = fixture.json("none", &["--", "extra"]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let err = error_envelope(&output, "workflow_action_args_refused");
    assert_eq!(err["class"], "configuration", "{err}");
    assert!(
        !fixture.state_dir().join("spawned").exists(),
        "a refused action ran"
    );
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "refused", "{record}");
    assert_eq!(record["arg_count"], 2, "{record}");
}

#[test]
fn refusals_before_a_run_are_envelopes_with_their_own_codes() {
    let fixture = Fixture::new();

    let output = fixture
        .command(&["workflow", "run", "--json", "acme.absent", "doc"])
        .output()
        .expect("run an unknown plugin");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    error_envelope(&output, "workflow_plugin_unknown");

    let output = fixture.json("missing", &["--json"]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    error_envelope(&output, "workflow_action_unknown");
    assert_eq!(fixture.last_record()["outcome"], "refused");

    let output = fixture
        .command(&[
            "--host",
            "elsewhere",
            "workflow",
            "run",
            "--json",
            PLUGIN_ID,
            "doc",
        ])
        .output()
        .expect("run against a remote host");
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    error_envelope(&output, "workflow_local_only");

    let disabled = fixture
        .command(&["workflow", "disable", PLUGIN_ID])
        .output()
        .expect("disable");
    assert_eq!(disabled.status.code(), Some(0), "{}", describe(&disabled));
    let output = fixture.json("doc", &[]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    error_envelope(&output, "workflow_plugin_disabled");
    assert_eq!(
        fixture.last_record()["error_code"],
        "workflow_plugin_disabled"
    );
}

#[test]
fn a_failed_action_reports_its_status_and_none_of_its_output() {
    let fixture = Fixture::new();

    let output = fixture.json("failing", &[]);
    assert_eq!(
        output.status.code(),
        Some(FAILING_EXIT),
        "{}",
        describe(&output)
    );
    let err = error_envelope(&output, "workflow_action_failed");
    assert_eq!(err["exit_code"], FAILING_EXIT, "{err}");
    assert!(err.get("signal").is_none(), "{err}");
    assert_eq!(err["class"], "runtime", "{err}");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains(OUTPUT_SENTINEL),
        "the failed action's stdout was printed: {}",
        describe(&output)
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "failing-stderr\n");
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "exit_nonzero", "{record}");
    assert_eq!(record["error_code"], "workflow_action_failed", "{record}");
    assert_eq!(record["exit_code"], FAILING_EXIT, "{record}");

    let output = fixture.json("killed", &[]);
    let terminated = 128 + libc::SIGTERM;
    assert_eq!(
        output.status.code(),
        Some(terminated),
        "{}",
        describe(&output)
    );
    let err = error_envelope(&output, "workflow_action_failed");
    assert_eq!(err["exit_code"], terminated, "{err}");
    assert_eq!(err["signal"], libc::SIGTERM, "{err}");
    assert_eq!(fixture.last_record()["signal"], libc::SIGTERM);
}

#[test]
fn a_run_the_cli_stops_is_an_envelope_with_the_text_mode_status() {
    let fixture = Fixture::new();

    let output = fixture.json("flood", &[]);
    assert_eq!(
        output.status.code(),
        Some(OUTPUT_LIMIT_EXIT),
        "{}",
        describe(&output)
    );
    error_envelope(&output, "workflow_output_limit");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("filler"), "a partial capture was printed");
    assert!(
        !stdout.contains(OUTPUT_SENTINEL),
        "bytes past the cap were printed"
    );
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "output_limit", "{record}");
    assert_eq!(record["stdout_bytes"], FLOOD_CAP, "{record}");

    let output = fixture.json("slow", &[]);
    assert_eq!(
        output.status.code(),
        Some(TIMEOUT_EXIT),
        "{}",
        describe(&output)
    );
    error_envelope(&output, "workflow_action_timeout");
    assert_eq!(fixture.last_record()["outcome"], "timeout");
}

/// Kills the escaped process of the `escape` action when the scenario ends.
#[cfg(target_os = "linux")]
struct Escapee(Pid);

#[cfg(target_os = "linux")]
impl Drop for Escapee {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(self.0, nix::sys::signal::Signal::SIGKILL);
    }
}

/// Linux only: the scenario needs `/usr/bin/setsid` to leave the group, and
/// macOS ships no `setsid` program at a fixed path.
#[cfg(target_os = "linux")]
#[test]
fn a_capture_cut_by_an_escaped_writer_is_an_error_not_a_partial_document() {
    let fixture = Fixture::new();
    let mut cli = fixture
        .command(&["workflow", "run", "--json", PLUGIN_ID, "escape"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start the CLI");
    let _escapee = Escapee(fixture.pid("escapee.pid"));
    poll_until("the CLI to cut the escaped writer", || {
        cli.try_wait().expect("poll the CLI").map(drop)
    });
    let output = cli.wait_with_output().expect("collect the CLI output");
    // The action printed `{}` and exited 0; the bytes the escaped process
    // added make the capture unusable, so no `ok` is printed.
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let err = error_envelope(&output, "workflow_output_incomplete");
    assert_eq!(err["class"], "runtime", "{err}");
    let record = fixture.last_record();
    assert_eq!(record["outcome"], "output_incomplete", "{record}");
    assert_eq!(
        record["error_code"], "workflow_output_incomplete",
        "{record}"
    );
}
