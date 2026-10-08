//! The shared scenario of the release consumer suite: install the package
//! through the CLI, start one session of its runtime on the real upstream
//! against the model stub, drive a turn, stop it and resume it.
//!
//! Everything goes through the release CLI of the layout; the scenario reads
//! only `--json` documents and the processes it started.

// Rust guideline compliant 2026-10-08

use std::path::{Path, PathBuf};

use pohunek_platform::shell_env::resolve_executable_in_path_value;
use pohunek_test_support::env::TestEnv;
use pohunek_test_support::wait::poll_until;
use serde_json::Value;

use crate::release_bounded::{run as run_bounded, PROBE_TIMEOUT};
use crate::release_catalog::PackageFacts;
use crate::release_drivers::{write_profile, Driver, Prepared, Stub};
use crate::release_host::Host;
use crate::release_layout::Layout;

/// Profile every session of the suite launches through.
const PROFILE: &str = "consumer";

/// The prompt the suite submits first. The token makes it recognizable in the
/// conversation history of a later request.
const PROMPT: &str = "say hi first-turn-7f3a";

/// The prompt submitted after the resume; it must not contain any text of
/// [`PROMPT`].
const SECOND_PROMPT: &str = "once more second-turn-9c1d";

/// Token of [`PROMPT`] that identifies the first turn in a request body.
const FIRST_TOKEN: &str = "first-turn-7f3a";

/// Token of [`SECOND_PROMPT`] that identifies the second turn's request.
const SECOND_TOKEN: &str = "second-turn-9c1d";

/// Name of the session the suite starts.
const SESSION_NAME: &str = "consumer";

/// What a run established, for the report.
#[derive(Debug)]
pub(crate) struct Outcome {
    /// The release the daemon's version probe read from the real upstream.
    pub(crate) upstream_version: String,
}

/// Resolves `program` on the `PATH` value `path` with the daemon's own
/// resolver, so the direct version probe and the daemon (which receives the
/// resolved directory in front of the same `PATH`) agree on one executable:
/// relative entries are skipped and a file that is not a trusted executable
/// does not shadow a later valid one.
///
/// # Errors
///
/// Fails with a message naming the program when no entry holds it.
pub(crate) fn locate_upstream(
    program: &str,
    path: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, String> {
    resolve_executable_in_path_value(std::ffi::OsStr::new(program), path)
        .map_err(|error| format!("the upstream `{program}` is not usable on PATH: {error}"))
}

/// `plugin install <archive> --catalog <catalog> --yes`, returning the exit
/// code and the JSON document so callers can assert a refusal.
pub(crate) fn install_with_catalog(host: &Host, archive: &Path, catalog: &Path) -> (i32, Value) {
    host.json(&[
        "plugin",
        "install",
        archive.to_str().expect("utf-8 path"),
        "--catalog",
        catalog.to_str().expect("utf-8 path"),
        "--yes",
    ])
}

/// Installs the package from the official catalog and checks that the
/// registry records it as official, enabled and selected.
pub(crate) fn install_official(host: &Host, archive: &Path, catalog: &Path, facts: &PackageFacts) {
    let (code, installed) = install_with_catalog(host, archive, catalog);
    assert_eq!(code, 0, "official install failed: {installed}");
    assert_eq!(installed["ok"]["status"], "installed", "{installed}");
    let package = &installed["ok"]["package"];
    assert_eq!(package["origin"], "official", "{installed}");
    assert_eq!(package["digest"], facts.digest.as_str(), "{installed}");

    let listed = host.ok(&["plugin", "list"]);
    let packages = listed["packages"].as_array().expect("packages");
    assert_eq!(
        packages.len(),
        1,
        "exactly the package under test: {listed}"
    );
    let listed_package = &packages[0];
    assert_eq!(
        listed_package["package"]["id"],
        facts.package_id.as_str(),
        "{listed}"
    );
    assert_eq!(listed_package["origin"], "official", "{listed}");
    assert_eq!(listed_package["enabled"], true, "{listed}");
    assert_eq!(listed_package["selected"], true, "{listed}");
    assert_eq!(listed_package["digest"], facts.digest.as_str(), "{listed}");

    let inspected = host.ok(&["plugin", "inspect", facts.package_id.as_str()]);
    assert_eq!(
        inspected["package"]["digest"],
        facts.digest.as_str(),
        "{inspected}"
    );
    assert_eq!(inspected["package"]["origin"], "official", "{inspected}");
}

/// The release `program --version` reports, read with the package's own
/// probe grammar in the hermetic environment, independently of the daemon.
pub(crate) fn probe_upstream(
    env: &TestEnv,
    program: &Path,
    facts: &PackageFacts,
    profile_env: &[(String, String)],
) -> String {
    probe_upstream_within(env, program, facts, profile_env, PROBE_TIMEOUT)
}

/// [`probe_upstream`] with an explicit deadline.
pub(crate) fn probe_upstream_within(
    env: &TestEnv,
    program: &Path,
    facts: &PackageFacts,
    profile_env: &[(String, String)],
    timeout: std::time::Duration,
) -> String {
    let mut command = env.command(program);
    command.args(facts.probe.args());
    for (key, value) in profile_env {
        command.env(key, value);
    }
    let output = run_bounded(command, &program.display().to_string(), timeout)
        .unwrap_or_else(|message| panic!("the upstream version probe failed: {message}"));
    assert!(
        output.status.success(),
        "{} {:?} failed: {}",
        program.display(),
        facts.probe.args(),
        String::from_utf8_lossy(&output.stderr)
    );
    let banner = String::from_utf8_lossy(&output.stdout).into_owned();
    let version = facts
        .probe
        .read_output(&banner)
        .unwrap_or_else(|| panic!("the package's probe grammar cannot read the banner {banner:?}"));
    assert!(
        facts.probe.supports(version),
        "upstream {} reports {version}, outside the supported range [{}, {}) of the package",
        facts.program,
        facts.probe.min(),
        facts.probe.below()
    );
    version.to_string()
}

/// The inventory entry `host inspect local` reports for the profile.
fn inventory_entry(host: &Host) -> Value {
    let local = host.ok(&["host", "inspect", "local"]);
    local["runtimes"]
        .as_array()
        .expect("runtimes")
        .iter()
        .find(|runtime| runtime["agent"] == PROFILE)
        .unwrap_or_else(|| panic!("the host lists the profile {PROFILE}: {local}"))
        .clone()
}

/// Checks that the daemon and the worker that serve the session execute the
/// layout's bytes.
fn assert_serving_from_layout(host: &Host, layout: &Layout) {
    layout
        .verify_process(host.daemon_pid, "pohunekd")
        .unwrap_or_else(|error| panic!("{error}"));
    let running = layout
        .pohunek_processes(host.env.root())
        .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        running
            .iter()
            .any(|(pid, name)| *pid == host.daemon_pid && name == "pohunekd"),
        "the daemon is among the pohunek processes of the environment: {running:?}"
    );
    assert!(
        running.iter().any(|(_, name)| name == "pohunek-sessiond"),
        "a live session has a worker running the layout's pohunek-sessiond: {running:?}"
    );
}

/// Checks that a request the upstream sent for the turn tagged `second_token`
/// carried the first prompt and the first reply, i.e. that the upstream
/// restored the conversation instead of opening a fresh one.
///
/// An upstream may send auxiliary requests that mention the prompt (Codex
/// generates a thread title), so one carrying the whole history is enough; a
/// fresh conversation has none.
///
/// # Errors
///
/// Fails with a message when no request carried `second_token` or none of
/// them carried the earlier turn.
pub(crate) fn history_restored(
    stub: &Stub,
    first_token: &str,
    first_reply: &str,
    second_token: &str,
) -> Result<(), String> {
    let requests = stub.bodies_containing(second_token);
    if requests.is_empty() {
        return Err(format!(
            "no model request carried the second prompt {second_token:?}"
        ));
    }
    if requests
        .iter()
        .any(|body| body.contains(first_token) && body.contains(first_reply))
    {
        return Ok(());
    }
    Err(format!(
        "none of the {} requests of the turn after the resume carries the first prompt {first_token:?} and the first reply {first_reply:?}: the conversation was not restored",
        requests.len()
    ))
}

/// Stops the session, resumes it and proves the upstream restored the
/// conversation.
fn stop_resume_and_check_history(
    host: &Host,
    driver: &Driver,
    prepared: &Prepared,
    id: &str,
    reference: &str,
) {
    let reporter_before = host.inspect(id)["active_agent_pid"].as_u64();
    host.stop_session(id);
    host.ok(&["session", "resume", id]);
    host.wait_activity(id, "idle");
    let resumed = host.inspect(id);
    assert_eq!(
        resumed["native_session_id"], reference,
        "the resumed session keeps its binding: {resumed}"
    );
    assert_eq!(resumed["runtime"]["state"], "live", "{resumed}");
    if let Some(line) = driver.ready_line {
        host.wait_screen_line(id, line);
    }

    // The daemon keeps the binding by itself, so only the upstream's own
    // behavior proves the conversation came back: the next turn's request must
    // carry the earlier prompt and reply.
    let finished = prepared.stub.finished();
    host.ok(&["session", "input", id, SECOND_PROMPT]);
    poll_until("the model request of the turn after the resume", || {
        (!prepared.stub.bodies_containing(SECOND_TOKEN).is_empty()).then_some(())
    });
    poll_until("the second turn's reply", || {
        (prepared.stub.finished() > finished).then_some(())
    });
    host.wait_activity(id, "idle");
    history_restored(
        &prepared.stub,
        FIRST_TOKEN,
        &(driver.history_reply)(),
        SECOND_TOKEN,
    )
    .unwrap_or_else(|message| panic!("{message}"));
    if driver.reports_after_resume {
        // The resumed process ran the integration hook again and reported
        // itself.
        let reporter_after = poll_until("the resumed process's hook report", || {
            host.inspect(id)["active_agent_pid"]
                .as_u64()
                .filter(|pid| Some(*pid) != reporter_before)
        });
        assert!(reporter_after > 0, "a reporter pid");
    }
    host.stop_session(id);
}

/// Drives the whole scenario of `driver` against the installed package.
///
/// # Panics
///
/// Panics at the first assertion that does not hold.
pub(crate) fn run_scenario(
    host: &Host,
    facts: &PackageFacts,
    driver: &Driver,
    prepared: &Prepared,
    upstream: &Path,
) -> Outcome {
    let probed = probe_upstream(&host.env, upstream, facts, &prepared.profile_env);
    write_profile(
        &host.env,
        PROFILE,
        driver.runtime,
        facts.package_id.as_str(),
        facts.digest.as_str(),
        &prepared.profile_env,
    );
    if driver.hooks {
        let installed = host.ok(&[
            "integration",
            "install",
            "--agent",
            driver.runtime,
            "--profile",
            PROFILE,
        ]);
        assert!(
            installed["failed"].as_array().is_none_or(Vec::is_empty),
            "{installed}"
        );
    }

    let entry = inventory_entry(host);
    assert_eq!(entry["available"], true, "{entry}");
    assert_eq!(
        entry["supported"], true,
        "the daemon's version probe refuses the upstream: {entry}"
    );
    assert_eq!(
        entry["version"].as_str(),
        Some(probed.as_str()),
        "the daemon read another version than the direct probe: {entry}"
    );

    let cwd = host.env.cwd().to_path_buf();
    let cwd = cwd.to_str().expect("utf-8 cwd");
    let mut launch = vec![
        "session",
        "new",
        "--agent",
        PROFILE,
        "--cwd",
        cwd,
        "--name",
        SESSION_NAME,
    ];
    if let Some(columns) = driver.columns {
        launch.extend(["--cols", columns]);
    }
    let launched = host.ok(&launch);
    let id = launched["id"].as_str().expect("session id").to_owned();
    assert_eq!(launched["agent_base"], driver.runtime, "{launched}");
    assert_eq!(launched["capabilities"]["resume"], true, "{launched}");

    host.wait_activity(&id, "idle");
    if let Some(line) = driver.ready_line {
        host.wait_screen_line(&id, line);
    }
    assert_serving_from_layout(host, &host.layout);

    // Input: the daemon writes the text, waits the descriptor's submit delay
    // and writes the submit key. The stub holds the reply until it is released.
    let prompt = driver
        .hold_marker
        .map_or_else(|| PROMPT.to_owned(), |marker| format!("{marker} {PROMPT}"));
    host.ok(&["session", "input", &id, &prompt]);
    host.wait_activity(&id, "working");
    poll_until("the stub to receive the turn's request", || {
        (prepared.stub.started() >= 1).then_some(())
    });
    prepared.stub.open_gate();
    host.wait_screen_line(&id, &(driver.reply)());
    host.wait_activity(&id, "idle");

    // The conversation id the daemon will resume with.
    let reference = poll_until("the session's native reference", || {
        let record = host.inspect(&id);
        let reported = !driver.hooks || record["active_agent_session_id"].is_string();
        record["native_session_id"]
            .as_str()
            .filter(|_| reported)
            .map(str::to_owned)
    });

    stop_resume_and_check_history(host, driver, prepared, &id, &reference);

    assert_ne!(
        prepared.stub.saw_foreign_credential(),
        Some(true),
        "a request to the stub carried an unexpected credential"
    );
    Outcome {
        upstream_version: probed,
    }
}
