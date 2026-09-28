"""Regression checks for the macOS launchd lifetime acceptance tooling (stdlib only).

The driver only runs on a real Mac, so these tests cover what can be proven on
any host: the driver parses as POSIX sh (and is shellcheck-clean when
shellcheck is installed), and the evidence helper evaluates recorded
observations exactly as `docs/acceptance/README.md` documents.
"""

import base64
import contextlib
import copy
import importlib.util
import io
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ACCEPTANCE = Path(__file__).resolve().parents[1] / "acceptance"
DRIVER = ACCEPTANCE / "macos-launchd-lifetime"
HELPER = ACCEPTANCE / "launchd_lifetime_evidence.py"
SPEC = importlib.util.spec_from_file_location("launchd_lifetime_evidence", HELPER)
evidence = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(evidence)

NAMESPACE = "0123456789ab"
BOOT = "{ sec = 1727000000, usec = 0 }"
REBOOTED = "{ sec = 1727100000, usec = 0 }"


def envelope(payload):
    return json.dumps({"cli_version": "0.31.6", "protocol": {"min": 3, "max": 3}, "ok": payload})


def session(session_id, agent_base, state, runtime_id, pid, reason=None, resume=False, reference=None):
    runtime = {"state": state, "runtime_generation": 1, "runtime_id": runtime_id}
    if reason:
        runtime["loss_reason"] = reason
    entry = {
        "id": session_id,
        "agent": agent_base,
        "agent_base": agent_base,
        "capabilities": {"resume": resume, "fork": False},
        "pid": pid,
        "state": "running" if state == "live" else "stopped",
        "runtime": runtime,
    }
    if reference:
        entry["native_session_id"] = reference
    return entry


def worker(session_id, generation, pid):
    return {
        "service_id": "{}.{}".format(session_id, generation),
        "state": "running",
        "pid": pid,
        "executable": "/Users/u/.local/libexec/pohunek/0.31.6/pohunek-sessiond",
        "arguments": ["--session-id", session_id, "--worker-generation", generation],
        "session_id": session_id,
        "generation": generation,
        "version": "0.31.6",
    }


def service(daemon_pid, workers):
    return {
        "installed": True,
        "config_path": "/Users/u/.config/pohunek/service.toml",
        "namespace": NAMESPACE,
        "prefix": "/Users/u/.local",
        "active_version": "0.31.6",
        "daemon": {"service_id": "daemon", "state": "running", "pid": daemon_pid},
        "workers": workers,
    }


SHELL = "s-shell"
AGENT = "s-claude"


def live_sessions(runtime_suffix=""):
    return [
        session(SHELL, "shell", "live", "r-shell" + runtime_suffix, 501),
        session(AGENT, "claude", "live", "r-claude" + runtime_suffix, 502, resume=True, reference="native-1"),
    ]


def lost_sessions():
    return [
        session(SHELL, "shell", "lost", "r-shell", 501, reason="runtime_lost"),
        session(AGENT, "claude", "lost", "r-claude", 502, reason="runtime_lost", resume=True, reference="native-1"),
    ]


LIVE_WORKERS = [worker(SHELL, "aaaaaaaa", 401), worker(AGENT, "bbbbbbbb", 402)]


class StateDir:
    """Builds a driver state directory with a passing observation per phase."""

    def __init__(self, root):
        self.root = Path(root)
        (self.root / "run.json").write_text(
            json.dumps(
                {
                    "run_id": "20260924T100000Z-mac",
                    "started_at": "2026-09-24T10:00:00Z",
                    "host": {"product_version": "15.7.9", "arch": "arm64", "hardware_model": "Mac14,2"},
                    "pohunek": {"namespace": NAMESPACE, "active_version": "0.31.6"},
                }
            )
        )

    def write(self, relative, text):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def survive(self, name, **overrides):
        base = "phases/{}/".format(name)
        self.write(base + "sessions.txt", "{}\n{}\n".format(SHELL, AGENT))
        self.write(base + "started_at", "2026-09-24T10:00:00Z")
        self.write(base + "before/sessions.json", envelope(live_sessions()))
        self.write(base + "before/service.json", envelope(service(300, LIVE_WORKERS)))
        self.write(base + "before/boottime.txt", BOOT)
        self.write(base + "after/sessions.json", envelope(overrides.get("after_sessions", live_sessions())))
        self.write(base + "after/service.json", envelope(overrides.get("after_service", service(300, LIVE_WORKERS))))
        self.write(base + "after/boottime.txt", overrides.get("after_boot", BOOT))
        self.write(base + "after/ps.txt", "  401 1 pohunek-sessiond --session-id s-shell --worker-generation aaaaaaaa\n")
        self.write(base + "after/launchctl.txt", "")
        self.write(base + "probe/{}.token".format(SHELL), "123")
        screen = {"visible_lines": ["$ printf 'ok-%s\\n' 123", overrides.get("probe_line", "ok-123"), "$"]}
        self.write(base + "probe/{}.screen.json".format(SHELL), envelope(screen))
        self.write(base + "completed_at", "2026-09-24T10:05:00Z")

    def lost(self, name, **overrides):
        base = "phases/{}/".format(name)
        self.write(base + "sessions.txt", "{}\n{}\n".format(SHELL, AGENT))
        self.write(base + "started_at", "2026-09-24T11:00:00Z")
        self.write(base + "before/sessions.json", envelope(live_sessions()))
        self.write(base + "before/service.json", envelope(service(300, LIVE_WORKERS)))
        self.write(base + "before/boottime.txt", BOOT)
        after_boot = REBOOTED if name == "reboot" else BOOT
        self.write(base + "after/sessions.json", envelope(overrides.get("after_sessions", lost_sessions())))
        self.write(base + "after/service.json", envelope(overrides.get("after_service", service(900, []))))
        self.write(base + "after/boottime.txt", overrides.get("after_boot", after_boot))
        self.write(base + "after/ps.txt", overrides.get("ps", "  900 1 pohunekd --service-config /x\n"))
        labels = "\n".join(
            "{} {}".format(evidence.worker_label(NAMESPACE, sid, gen), overrides.get("label_status", 113))
            for sid, gen in ((SHELL, "aaaaaaaa"), (AGENT, "bbbbbbbb"))
        )
        self.write(base + "after/launchctl.txt", labels + "\n")
        recovered = [
            session(SHELL, "shell", "lost", "r-shell", 501, reason="runtime_lost"),
            session(AGENT, "claude", "live", "r-claude-2", 777, resume=True, reference="native-1"),
        ]
        self.write(base + "recovery/{}.exit".format(AGENT), overrides.get("recovery_exit", "0"))
        self.write(base + "recovery/sessions.json", envelope(recovered))
        self.write(
            base + "recovery/service.json",
            envelope(service(900, [worker(AGENT, overrides.get("new_generation", "cccccccc"), 778)])),
        )
        self.write(base + "completed_at", "2026-09-24T11:10:00Z")

    def complete(self):
        self.survive("screen-lock")
        self.survive("terminal-close")
        self.lost("logout-login")
        self.lost("reboot")


def failed_checks(phase):
    names = [item["name"] for item in phase["checks"] if not item["passed"]]
    for entry in phase["sessions"]:
        names.extend(
            "{}:{}".format(entry["session_id"], item["name"]) for item in entry["checks"] if not item["passed"]
        )
    return names


class DriverScriptTest(unittest.TestCase):
    def test_driver_is_executable_and_parses_as_posix_sh(self):
        self.assertTrue(DRIVER.stat().st_mode & 0o111, "driver must be executable")
        for shell in ("sh", "bash", "dash"):
            if shutil.which(shell) is None:
                continue
            result = subprocess.run([shell, "-n", str(DRIVER)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, "{} -n failed: {}".format(shell, result.stderr))

    def test_driver_is_shellcheck_clean(self):
        if shutil.which("shellcheck") is None:
            self.skipTest("shellcheck is not installed")
        result = subprocess.run(
            ["shellcheck", "-s", "sh", str(DRIVER)], capture_output=True, text=True
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_driver_refuses_to_run_outside_macos(self):
        if Path("/usr/bin/sw_vers").exists():
            self.skipTest("running on macOS")
        with tempfile.TemporaryDirectory() as root:
            result = subprocess.run(
                [str(DRIVER), "--state-dir", root + "/state", "--python", "python3", "start", "--agent", "claude"],
                capture_output=True,
                text=True,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("needs macOS", result.stderr)


class EvidenceTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.state = StateDir(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def test_complete_passing_run(self):
        self.state.complete()
        document = evidence.assemble(self.state.root)
        self.assertEqual(document["schema"], "pohunek.acceptance.launchd-lifetime")
        self.assertEqual(document["schema_version"], 1)
        self.assertEqual([phase["name"] for phase in document["phases"]],
                         ["screen-lock", "terminal-close", "logout-login", "reboot"])
        for phase in document["phases"]:
            self.assertEqual(failed_checks(phase), [], phase["name"])
        self.assertTrue(document["complete"])
        self.assertTrue(document["passed"])
        logout = document["phases"][2]
        agent = [entry for entry in logout["sessions"] if entry["session_id"] == AGENT][0]
        self.assertTrue(agent["recovery_expected"])
        self.assertTrue(agent["recovery_available"])
        self.assertEqual(agent["recovery"]["worker_generation"], "cccccccc")
        shell = [entry for entry in logout["sessions"] if entry["session_id"] == SHELL][0]
        self.assertFalse(shell["recovery_expected"])
        self.assertIsNone(shell["recovery"])

    def test_partial_run_is_incomplete_and_not_passed(self):
        self.state.survive("screen-lock")
        document = evidence.assemble(self.state.root)
        self.assertFalse(document["complete"])
        self.assertFalse(document["passed"])
        self.assertIsNone(document["completed_at"])
        self.assertTrue(document["phases"][0]["passed"])

    def test_survival_fails_when_the_worker_was_replaced(self):
        replaced = [worker(SHELL, "dddddddd", 999), worker(AGENT, "bbbbbbbb", 402)]
        self.state.survive("screen-lock", after_service=service(300, replaced))
        phase = evidence.evaluate_phase(self.state.root, "screen-lock")
        self.assertFalse(phase["passed"])
        self.assertEqual(
            failed_checks(phase),
            ["s-shell:same_worker_generation", "s-shell:same_worker_pid"],
        )

    def test_survival_fails_when_the_pty_does_not_run_the_probe(self):
        self.state.survive("terminal-close", probe_line="$")
        phase = evidence.evaluate_phase(self.state.root, "terminal-close")
        self.assertEqual(failed_checks(phase), ["s-shell:pty_accepts_input_and_prints"])

    def test_survival_accepts_the_probe_in_retained_output(self):
        self.state.survive("terminal-close", probe_line="$")
        data = base64.b64encode(b"$ printf 'ok-%s\\n' 123\r\nok-123\r\n$ ").decode()
        self.state.write(
            "phases/terminal-close/probe/{}.output.json".format(SHELL), envelope({"data_base64": data})
        )
        phase = evidence.evaluate_phase(self.state.root, "terminal-close")
        self.assertTrue(phase["passed"], failed_checks(phase))

    def test_survival_fails_when_the_daemon_restarted(self):
        self.state.survive("screen-lock", after_service=service(301, LIVE_WORKERS))
        phase = evidence.evaluate_phase(self.state.root, "screen-lock")
        self.assertEqual(failed_checks(phase), ["daemon_unchanged"])

    def test_loss_fails_when_a_worker_was_resurrected(self):
        resurrected = service(900, [worker(SHELL, "aaaaaaaa", 401)])
        ps = "  401 1 pohunek-sessiond --session-id s-shell --worker-generation aaaaaaaa\n"
        self.state.lost("logout-login", after_service=resurrected, ps=ps)
        phase = evidence.evaluate_phase(self.state.root, "logout-login")
        self.assertEqual(
            failed_checks(phase),
            ["s-shell:no_worker_job_after", "s-shell:no_worker_process"],
        )

    def test_loss_fails_when_the_old_label_is_still_loaded(self):
        self.state.lost("reboot", label_status=0)
        phase = evidence.evaluate_phase(self.state.root, "reboot")
        self.assertEqual(
            failed_checks(phase),
            ["s-shell:old_generation_label_absent", "s-claude:old_generation_label_absent"],
        )

    def test_loss_requires_the_runtime_lost_reason(self):
        sessions = lost_sessions()
        sessions[0]["runtime"]["loss_reason"] = "runtime_lost_cleanup_unconfirmed"
        self.state.lost("logout-login", after_sessions=sessions)
        phase = evidence.evaluate_phase(self.state.root, "logout-login")
        self.assertEqual(failed_checks(phase), ["s-shell:loss_reason"])

    def test_logout_must_not_look_like_a_reboot(self):
        self.state.lost("logout-login", after_boot=REBOOTED)
        phase = evidence.evaluate_phase(self.state.root, "logout-login")
        self.assertEqual(failed_checks(phase), ["host_not_rebooted"])

    def test_reboot_must_change_the_boot_time(self):
        self.state.lost("reboot", after_boot=BOOT)
        phase = evidence.evaluate_phase(self.state.root, "reboot")
        self.assertEqual(failed_checks(phase), ["host_rebooted"])

    def test_recovery_must_start_a_new_generation(self):
        self.state.lost("logout-login", new_generation="bbbbbbbb")
        phase = evidence.evaluate_phase(self.state.root, "logout-login")
        self.assertEqual(failed_checks(phase), ["s-claude:explicit_recovery_starts_new_generation"])

    def test_failed_resume_fails_the_phase(self):
        self.state.lost("reboot", recovery_exit="1")
        phase = evidence.evaluate_phase(self.state.root, "reboot")
        self.assertEqual(failed_checks(phase), ["s-claude:explicit_recovery_starts_new_generation"])

    def test_loss_phase_needs_a_recoverable_session(self):
        sessions = lost_sessions()
        before = live_sessions()
        for entry in sessions + before:
            entry.pop("native_session_id", None)
        self.state.lost("logout-login", after_sessions=sessions)
        self.state.write("phases/logout-login/before/sessions.json", envelope(before))
        phase = evidence.evaluate_phase(self.state.root, "logout-login")
        self.assertEqual(failed_checks(phase), ["recovery_covered"])

    def test_missing_observation_fails_closed(self):
        self.state.survive("screen-lock")
        (self.state.root / "phases/screen-lock/after/service.json").unlink()
        phase = evidence.evaluate_phase(self.state.root, "screen-lock")
        self.assertFalse(phase["passed"])
        self.assertEqual(failed_checks(phase), ["observations_readable"])


class QueryCommandTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def run_helper(self, *argv):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            status = evidence.main([str(arg) for arg in argv])
        return status, output.getvalue().splitlines()

    def test_labels_name_the_current_generation(self):
        path = self.root / "service.json"
        path.write_text(envelope(service(300, LIVE_WORKERS)))
        status, lines = self.run_helper("labels", path, SHELL, AGENT, "s-unknown")
        self.assertEqual(status, 0)
        self.assertEqual(
            lines,
            [
                "io.github.zajca.pohunek.0123456789ab.worker.s-shell.aaaaaaaa",
                "io.github.zajca.pohunek.0123456789ab.worker.s-claude.bbbbbbbb",
            ],
        )

    def test_service_field_prints_booleans_in_json_form(self):
        path = self.root / "service.json"
        path.write_text(envelope(service(300, [])))
        self.assertEqual(self.run_helper("service-field", path, "installed"), (0, ["true"]))
        self.assertEqual(self.run_helper("service-field", path, "daemon.state"), (0, ["running"]))
        self.assertEqual(self.run_helper("service-field", path, "daemon.missing"), (0, [""]))

    def test_recovery_queries_follow_capabilities_and_references(self):
        path = self.root / "sessions.json"
        waiting = copy.deepcopy(live_sessions())
        waiting[1].pop("native_session_id")
        path.write_text(envelope(waiting))
        self.assertEqual(self.run_helper("missing-references", path, SHELL, AGENT), (0, [AGENT]))
        self.assertEqual(self.run_helper("shell-sessions", path, SHELL, AGENT), (0, [SHELL]))
        path.write_text(envelope(lost_sessions()))
        self.assertEqual(self.run_helper("recoverable", path, SHELL, AGENT), (0, [AGENT]))
        self.assertEqual(
            self.run_helper("states", path, SHELL, "s-unknown"),
            (0, ["s-shell lost", "s-unknown absent"]),
        )

    def test_session_id_reads_new_and_resume_results(self):
        created = self.root / "created.json"
        created.write_text(envelope({"id": "s-new"}))
        self.assertEqual(self.run_helper("session-id", created), (0, ["s-new"]))
        resumed = self.root / "resumed.json"
        resumed.write_text(envelope({"session": {"id": "s-resumed"}}))
        self.assertEqual(self.run_helper("session-id", resumed), (0, ["s-resumed"]))

    def test_malformed_input_is_an_error(self):
        path = self.root / "broken.json"
        path.write_text("{")
        errors = io.StringIO()
        with contextlib.redirect_stderr(errors):
            status, _ = self.run_helper("states", path, SHELL)
        self.assertEqual(status, 2)
        self.assertIn("is not JSON", errors.getvalue())


if __name__ == "__main__":
    unittest.main()
