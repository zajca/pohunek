//! A fake agent whose own process is the session's PTY root and reports native
//! identities to the daemon over the public control socket.
//!
//! The agent is a Python image behind [`python_agent_launcher`], so the process
//! the worker spawned keeps its pid through the `exec`. It reads one JSON command
//! per line from a FIFO that the test opens, and for every `send` command it
//! writes the command's request line to the daemon socket the command names,
//! either from its own process (`root`) or from a forked
//! child that inherits the same environment (`descendant`). Each answer is
//! appended to a results file as `{"step", "response"}`. An `exec` command
//! replaces the image in place with the given argv.

#![expect(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

// Rust guideline compliant 2026-10-13

use std::io::Write as _;
use std::path::{Path, PathBuf};

use pohunek_test_support::wait::wait_until;
use protocol::{Request, Response};
use serde_json::{json, Value};

use super::official_runtimes;

/// Which process of the agent sends a step's request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Via {
    /// The PTY root itself.
    Root,
    /// A child the PTY root forks; it inherits the root's socket path and
    /// instance environment, but its kernel peer is not the root.
    Descendant,
}

impl Via {
    const fn wire_name(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Descendant => "descendant",
        }
    }
}

/// Handle to one installed reporting agent: its gate and its results file.
#[derive(Debug)]
pub(crate) struct RootReporter {
    gate: std::fs::File,
    results: PathBuf,
}

impl RootReporter {
    /// Installs `bin/<program>` as a reporting agent that answers the version
    /// probe of the official `runtime` package, and
    /// prepares its gate and results files under `dir`.
    ///
    /// The agent starts reading the gate as soon as the session launches, so a
    /// test writes its steps after `session.new` returns and the pid is known.
    pub(crate) fn install(bin: &Path, program: &str, runtime: &str, dir: &Path) -> Self {
        Self::install_with_banner(bin, program, runtime, dir, "")
    }

    /// [`RootReporter::install`] for an agent that writes `banner` to its
    /// terminal before it reads the gate, such as the screen a runtime's input
    /// readiness waits for.
    pub(crate) fn install_with_banner(
        bin: &Path,
        program: &str,
        runtime: &str,
        dir: &Path,
        banner: &str,
    ) -> Self {
        let gate_path = dir.join(format!("{program}.gate"));
        let results = dir.join(format!("{program}.results"));
        nix::unistd::mkfifo(&gate_path, nix::sys::stat::Mode::S_IRWXU).expect("create the gate");
        // Opened read-write so neither side blocks on the other: the test never
        // waits for the agent to open the gate, and the agent's read waits for a
        // line rather than for end of file.
        let gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gate_path)
            .expect("open the gate");
        let script = dir.join(format!("{program}-root.py"));
        std::fs::write(&script, reporter_script(&gate_path, &results, banner))
            .expect("write the reporting agent");
        let launcher = bin.join(program);
        std::fs::write(
            &launcher,
            official_runtimes::python_agent_launcher(runtime, &script),
        )
        .expect("write the agent launcher");
        std::fs::set_permissions(
            &launcher,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .expect("make the agent launcher executable");
        Self { gate, results }
    }

    /// Has the agent send `request` from `via` to the daemon listening on
    /// `socket`, and returns the daemon's answer.
    ///
    /// The socket path travels in the command: a session's child is not given
    /// the daemon socket path, and the shipped hooks learn it from their
    /// managed configuration.
    pub(crate) async fn send(
        &mut self,
        step: &str,
        via: Via,
        socket: &Path,
        request: &Request,
    ) -> Response {
        let line = serde_json::to_string(request).expect("serialize the report request");
        self.command(&json!({
            "action": "send",
            "step": step,
            "via": via.wire_name(),
            "socket": socket.display().to_string(),
            "line": line,
        }));
        self.answer(step).await
    }

    /// Replaces the agent image with `argv` once every earlier command ran.
    pub(crate) fn exec(&mut self, argv: &[&str]) {
        self.command(&json!({ "action": "exec", "argv": argv }));
    }

    fn command(&mut self, command: &Value) {
        let mut text = command.to_string();
        text.push('\n');
        self.gate
            .write_all(text.as_bytes())
            .expect("write a command to the gate");
    }

    async fn answer(&self, step: &str) -> Response {
        let results = self.results.clone();
        let step = step.to_owned();
        wait_until("the reporting agent to answer the step", || async {
            let recorded = std::fs::read_to_string(&results).unwrap_or_default();
            if let Ok(failure) = std::fs::read_to_string(format!("{}.failure", results.display())) {
                panic!("the reporting agent failed: {failure}");
            }
            recorded.lines().find_map(|line| {
                let mut entry: Value = serde_json::from_str(line).expect("result line is JSON");
                (entry["step"] == step.as_str()).then(|| {
                    serde_json::from_value::<Response>(entry["response"].take())
                        .expect("the recorded answer is a protocol response")
                })
            })
        })
        .await
    }
}

/// The Python program the agent runs; it only speaks the protocol above.
fn reporter_script(gate: &Path, results: &Path, banner: &str) -> String {
    let banner = serde_json::to_string(banner).expect("banner literal");
    let gate = serde_json::to_string(&gate.display().to_string()).expect("gate path literal");
    let results = serde_json::to_string(&results.display().to_string()).expect("results literal");
    format!(
        r#"import json
import os
import socket
import sys
import traceback

gate_path = {gate}
results_path = {results}
failure_path = results_path + ".failure"

def record_failure(kind, value, trace):
    # The test reads this file, so the agent never dies without a reason.
    with open(failure_path, "w", encoding="utf-8") as handle:
        handle.write("".join(traceback.format_exception(kind, value, trace)))

sys.excepthook = record_failure

sys.stdout.buffer.write({banner}.encode("utf-8"))
sys.stdout.buffer.flush()

def send(socket_path, line):
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(socket_path)
    client.sendall((line + "\n").encode())
    received = b""
    while b"\n" not in received:
        chunk = client.recv(4096)
        if not chunk:
            raise RuntimeError("the daemon closed the connection before answering")
        received += chunk
    client.close()
    return json.loads(received.split(b"\n", 1)[0])

def record(step, response):
    with open(results_path, "a", encoding="utf-8") as handle:
        handle.write(json.dumps({{"step": step, "response": response}}) + "\n")

def send_from_child(step, socket_path, line):
    child = os.fork()
    if child == 0:
        try:
            record(step, send(socket_path, line))
        except BaseException:
            record_failure(*sys.exc_info())
            os._exit(1)
        os._exit(0)
    _, status = os.waitpid(child, 0)
    if os.waitstatus_to_exitcode(status) != 0:
        raise RuntimeError(f"descendant step {{step}} failed")

with open(gate_path, encoding="utf-8") as gate:
    for raw in gate:
        command = json.loads(raw)
        if command["action"] == "exec":
            os.execv(command["argv"][0], command["argv"])
        if command["via"] == "descendant":
            send_from_child(command["step"], command["socket"], command["line"])
        else:
            record(command["step"], send(command["socket"], command["line"]))
"#
    )
}
