"""Integration scenarios for `scripts/stress-run` (stdlib only).

Every test drives the real script as a subprocess. The command under test is a
small Python child that reports what it observes (affinity, the load workers
next to it, the signals it receives). Process discovery reads `/proc`
directly, and a process is identified by PID plus start time, so a recycled
PID is never mistaken for a survivor. Every scenario ends by asserting that
none of the processes it recorded is still alive (a zombie counts as gone:
its owner is a subreaper outside the test).

Linux-only scenarios are skipped elsewhere with a reason. The macOS refusal
without `--unpinned` runs for real on macOS and is skipped on Linux, where the
platform check cannot be driven without faking the platform.
"""

import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "stress-run"
LINUX = sys.platform.startswith("linux")
# Upper bound for a scenario step that waits on another process.
WAIT_SECONDS = 20.0
# "Gone within 2 s" is the contract for load workers of a killed wrapper.
GONE_SECONDS = 2.0
# Scheduling slack on top of a documented time bound.
MARGIN_SECONDS = 4.0

# The command under test. argv: report path, mode, expected sibling count.
# Modes: `exit:<n>`, `signal:<n>` (kill self), `sleep`, `ignore-term`
# (sleeps through SIGTERM), `trap-term:<n>` (records SIGTERM then exits n),
# `spawn-ignorer` (a grandchild in the group that ignores SIGTERM).
CHILD = r'''
import json, os, signal, sys, time

report, mode, siblings = sys.argv[1], sys.argv[2], int(sys.argv[3])
marker = report + ".term"


def stat(pid):
    raw = open(f"/proc/{pid}/stat").read()
    fields = raw[raw.rfind(")") + 1:].split()
    return int(fields[1]), fields[19]


def record():
    parent = os.getppid()
    deadline = time.monotonic() + 15
    workers = []
    while time.monotonic() < deadline:
        workers = []
        for name in os.listdir("/proc"):
            if not name.isdigit() or int(name) == os.getpid():
                continue
            try:
                ppid, start = stat(name)
                if ppid == parent:
                    workers.append({
                        "pid": int(name),
                        "start": start,
                        "affinity": sorted(os.sched_getaffinity(int(name))),
                    })
            except (OSError, IndexError, ValueError):
                pass
        if len(workers) >= siblings:
            break
        time.sleep(0.05)
    _, start = stat(os.getpid())
    data = {
        "pid": os.getpid(),
        "start": start,
        "pgid": os.getpgid(0),
        "wrapper_pgid": os.getpgid(parent),
        "ppid": parent,
        "affinity": sorted(os.sched_getaffinity(0)),
        "workers": workers,
    }
    with open(report + ".tmp", "w") as stream:
        json.dump(data, stream)
    os.rename(report + ".tmp", report)


if mode.startswith("trap-term:"):
    code = int(mode.split(":")[1])

    def on_term(*_):
        open(marker, "w").write("term")
        sys.exit(code)

    signal.signal(signal.SIGTERM, on_term)
elif mode == "ignore-term":
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
elif mode == "spawn-ignorer":
    import subprocess
    grandchild = subprocess.Popen(
        [sys.executable, "-c",
         "import signal,time\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
         "while True: time.sleep(1)"])
    open(report + ".grandchild", "w").write(str(grandchild.pid))
    signal.signal(signal.SIGTERM, signal.SIG_IGN)

record()
if mode.startswith("exit:"):
    sys.exit(int(mode.split(":")[1]))
if mode.startswith("signal:"):
    os.kill(os.getpid(), int(mode.split(":")[1]))
    time.sleep(30)
while True:
    time.sleep(0.1)
'''

# Reports the command's own cgroup and the memory cap systemd applied to it.
CGROUP_CHILD = r'''
import json, sys
path = open("/proc/self/cgroup").read().strip().split("::", 1)[1]
cap = open(f"/sys/fs/cgroup{path}/memory.max").read().strip()
json.dump({"cgroup": path, "memory_max": cap}, open(sys.argv[1], "w"))
'''


def load_module():
    loader = importlib.machinery.SourceFileLoader("stress_run", str(SCRIPT))
    spec = importlib.util.spec_from_loader("stress_run", loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def proc_stat(pid):
    """`(state, start time)` of `pid` from `/proc`, or None when it is gone."""
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    fields = raw[raw.rfind(")") + 1:].split()
    return fields[0], fields[19]


def alive(pid, start):
    """True while `pid` still has the recorded start time and is not a zombie."""
    current = proc_stat(pid)
    return current is not None and current[1] == start and current[0] != "Z"


def children_of(pid):
    result = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            raw = (entry / "stat").read_text()
        except OSError:
            continue
        fields = raw[raw.rfind(")") + 1:].split()
        if int(fields[1]) == pid:
            result.append(int(entry.name))
    return result


def wait_for(path, timeout=WAIT_SECONDS):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return json.loads(path.read_text())
        time.sleep(0.02)
    raise AssertionError(f"{path.name} was not written within {timeout}s")


def wait_gone(records, timeout):
    """Wait until none of `records` (pid, start) is alive; True when so."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not any(alive(pid, start) for pid, start in records):
            return True
        time.sleep(0.02)
    return not any(alive(pid, start) for pid, start in records)


class StressRunCase(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="stress-run-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.recorded = []
        self.wrappers = []
        self.addCleanup(self.reap)
        self.child = self.tmp / "child.py"
        self.child.write_text(CHILD)

    def reap(self):
        """Stop whatever a failed scenario left, by verified PID and start time."""
        for pid, start in self.recorded:
            if alive(pid, start):
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        for wrapper in self.wrappers:
            if wrapper.poll() is None:
                wrapper.kill()
            wrapper.wait()
            for stream in (wrapper.stdout, wrapper.stderr):
                stream.close()

    def start(self, options, mode, siblings, extra_env=None, report="report.json"):
        """Start the wrapper around the reporting child; returns (process, report)."""
        report_path = self.tmp / report
        env = dict(os.environ)
        env.update(extra_env or {})
        wrapper = subprocess.Popen(
            [sys.executable, str(SCRIPT), *options, "--", sys.executable,
             str(self.child), str(report_path), mode, str(siblings)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env,
        )
        self.wrappers.append(wrapper)
        return wrapper, report_path

    def record(self, data):
        self.recorded.append((data["pid"], data["start"]))
        for worker in data["workers"]:
            self.recorded.append((worker["pid"], worker["start"]))

    def finish(self, wrapper, timeout=WAIT_SECONDS):
        out, err = wrapper.communicate(timeout=timeout)
        return wrapper.returncode, out, err

    def assert_nothing_alive(self):
        self.assertTrue(
            wait_gone(self.recorded, GONE_SECONDS),
            f"survivors: {[r for r in self.recorded if alive(*r)]}",
        )


@unittest.skipUnless(LINUX, "needs /proc and CPU affinity")
class LinuxScenarios(StressRunCase):
    def test_pinning_and_load_worker_count(self):
        # Scenario 1: command and workers share one CPU set of the requested size.
        wrapper, report = self.start(
            ["--deadline", "30s", "--cpus", "2", "--load-per-cpu", "2"], "exit:0", 4)
        data = wait_for(report)
        self.record(data)
        code, _, err = self.finish(wrapper)
        self.assertEqual(code, 0, err)
        expected = sorted(os.sched_getaffinity(0))[-2:]
        self.assertEqual(data["affinity"], expected)
        self.assertEqual(len(data["workers"]), 4)
        for worker in data["workers"]:
            self.assertEqual(worker["affinity"], expected)
        self.assertEqual(data["pgid"], data["pid"], "command leads its own group")
        self.assertNotEqual(data["pgid"], data["wrapper_pgid"])
        self.assertIn("load_workers=4", err)
        self.assert_nothing_alive()

    def test_one_cpu_two_workers(self):
        wrapper, report = self.start(
            ["--deadline", "30s", "--cpus", "1", "--load-per-cpu", "2"], "exit:0", 2)
        data = wait_for(report)
        self.record(data)
        self.assertEqual(len(data["workers"]), 2)
        self.assertEqual(self.finish(wrapper)[0], 0)
        self.assert_nothing_alive()

    def test_unpinned_keeps_the_inherited_mask(self):
        wrapper, report = self.start(
            ["--deadline", "30s", "--cpus", "1", "--load-per-cpu", "1", "--unpinned"],
            "exit:0", 1)
        data = wait_for(report)
        self.record(data)
        code, _, err = self.finish(wrapper)
        self.assertEqual(code, 0, err)
        self.assertEqual(data["affinity"], sorted(os.sched_getaffinity(0)))
        self.assertIn("unpinned", err)
        self.assert_nothing_alive()

    def test_exit_status_propagates(self):
        # Scenario 2: 0, a non-zero code, and death by signal (128 + n).
        for mode, expected in (("exit:0", 0), ("exit:7", 7),
                               (f"signal:{int(signal.SIGKILL)}", 128 + int(signal.SIGKILL)),
                               (f"signal:{int(signal.SIGTERM)}", 128 + int(signal.SIGTERM))):
            with self.subTest(mode=mode):
                report_name = f"report-{mode.replace(':', '-')}.json"
                wrapper, report = self.start(
                    ["--deadline", "30s", "--cpus", "1", "--load-per-cpu", "1"],
                    mode, 1, report=report_name)
                data = wait_for(report)
                self.record(data)
                code, _, err = self.finish(wrapper)
                self.assertEqual(code, expected, err)
                self.assertIn(f"status={expected}", err)
        self.assert_nothing_alive()

    def test_deadline_terminates_the_command(self):
        # Scenario 3: a sleeping command is stopped with SIGTERM at the deadline.
        wrapper, report = self.start(
            ["--deadline", "2s", "--cpus", "1", "--load-per-cpu", "2"], "sleep", 2)
        data = wait_for(report)
        self.record(data)
        started = time.monotonic()
        code, _, err = self.finish(wrapper)
        self.assertEqual(code, 124, err)
        self.assertIn("deadline 2s", err)
        self.assertLess(time.monotonic() - started, 2 + MARGIN_SECONDS)
        self.assert_nothing_alive()

    def test_deadline_kills_a_command_that_ignores_sigterm(self):
        # The group gets SIGKILL after the grace period, grandchildren included.
        grace = load_module().KILL_GRACE_SECONDS
        wrapper, report = self.start(
            ["--deadline", "2s", "--cpus", "1", "--load-per-cpu", "1"],
            "spawn-ignorer", 1)
        data = wait_for(report)
        self.record(data)
        grandchild = int(Path(str(report) + ".grandchild").read_text())
        grandchild_stat = proc_stat(grandchild)
        self.assertIsNotNone(grandchild_stat)
        self.recorded.append((grandchild, grandchild_stat[1]))
        started = time.monotonic()
        code, _, err = self.finish(wrapper)
        self.assertEqual(code, 124, err)
        self.assertLess(time.monotonic() - started, 2 + grace + MARGIN_SECONDS)
        self.assertGreaterEqual(time.monotonic() - started, grace - 1)
        self.assert_nothing_alive()

    def test_sigkill_of_the_wrapper_leaves_no_load(self):
        # Scenario 4, the regression for the detached `nohup stress` incident:
        # workers die at once and the command receives SIGTERM.
        wrapper, report = self.start(
            ["--deadline", "5m", "--cpus", "1", "--load-per-cpu", "2"], "trap-term:0", 2)
        data = wait_for(report)
        self.record(data)
        self.assertEqual(len(data["workers"]), 2)
        wrapper.kill()
        wrapper.wait()
        self.assertTrue(
            wait_gone(self.recorded, GONE_SECONDS),
            f"survivors: {[r for r in self.recorded if alive(*r)]}",
        )
        self.assertTrue(Path(str(report) + ".term").exists(),
                        "the command did not receive SIGTERM")

    def test_sigterm_to_the_wrapper_reaches_the_command_group(self):
        # Scenario 5: forwarded to the command's group, load stops, status reported.
        wrapper, report = self.start(
            ["--deadline", "5m", "--cpus", "1", "--load-per-cpu", "2"], "trap-term:7", 2)
        data = wait_for(report)
        self.record(data)
        wrapper.send_signal(signal.SIGTERM)
        code, _, err = self.finish(wrapper)
        self.assertEqual(code, 7, err)
        self.assertIn("status=7", err)
        self.assertTrue(Path(str(report) + ".term").exists())
        self.assert_nothing_alive()

    def test_sigint_to_the_wrapper_is_forwarded_as_sigterm(self):
        wrapper, report = self.start(
            ["--deadline", "5m", "--cpus", "1", "--load-per-cpu", "1"], "trap-term:9", 1)
        data = wait_for(report)
        self.record(data)
        wrapper.send_signal(signal.SIGINT)
        code, _, err = self.finish(wrapper)
        self.assertEqual(code, 9, err)
        self.assert_nothing_alive()

    def run_rejected(self, options, extra_env=None):
        marker = self.tmp / "started"
        env = dict(os.environ)
        env.update(extra_env or {})
        argv = [sys.executable, str(SCRIPT), *options, "--", sys.executable, "-c",
                f"open({str(marker)!r}, 'w').close()"]
        result = subprocess.run(
            argv, capture_output=True, text=True, env=env, timeout=WAIT_SECONDS)
        self.assertFalse(marker.exists(), "the command was started")
        self.assertEqual(result.returncode, 2, result.stderr)
        return result.stderr

    def test_missing_deadline_starts_nothing(self):
        # Scenario 6.
        self.assertIn("--deadline", self.run_rejected(["--cpus", "1"]))

    def test_cpus_out_of_range_start_nothing(self):
        self.assertIn("--cpus", self.run_rejected(["--deadline", "5s", "--cpus", "0"]))
        too_many = len(os.sched_getaffinity(0)) + 1
        self.assertIn("exceeds", self.run_rejected(
            ["--deadline", "5s", "--cpus", str(too_many)]))

    def test_load_per_cpu_below_one_starts_nothing(self):
        self.assertIn("--load-per-cpu", self.run_rejected(
            ["--deadline", "5s", "--load-per-cpu", "0"]))

    def test_malformed_deadline_starts_nothing(self):
        for text in ("0s", "10", "abc", "5x", "1h-"):
            with self.subTest(text=text):
                self.assertIn("--deadline", self.run_rejected(["--deadline", text]))

    def test_missing_command_starts_nothing(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--deadline", "5s"],
            capture_output=True, text=True, timeout=WAIT_SECONDS)
        self.assertEqual(result.returncode, 2)
        self.assertIn("command", result.stderr)
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--deadline", "5s", "--"],
            capture_output=True, text=True, timeout=WAIT_SECONDS)
        self.assertEqual(result.returncode, 2)

    def test_memory_max_without_a_user_manager_starts_nothing(self):
        isolated = self.tmp / "no-manager"
        isolated.mkdir()
        stderr = self.run_rejected(
            ["--deadline", "5s", "--memory-max", "256M"],
            extra_env={
                "XDG_RUNTIME_DIR": str(isolated),
                "DBUS_SESSION_BUS_ADDRESS": f"unix:path={isolated}/none",
            })
        self.assertIn("--memory-max", stderr)

    def test_an_abbreviated_option_is_rejected(self):
        # `--memory` must not reach the --memory-max re-exec filter.
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--deadline", "5s", "--memory", "4G", "--", "true"],
            capture_output=True, text=True, timeout=WAIT_SECONDS)
        self.assertEqual(result.returncode, 2)

    def test_memory_max_rejects_a_malformed_size(self):
        self.assertIn("--memory-max", self.run_rejected(
            ["--deadline", "5s", "--memory-max", "lots"]))

    def test_an_unstartable_command_reports_and_stops_the_load(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--deadline", "30s", "--cpus", "1",
             "--load-per-cpu", "2", "--", str(self.tmp / "does-not-exist")],
            capture_output=True, text=True, timeout=WAIT_SECONDS)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("cannot start", result.stderr)


def user_manager_available():
    systemd_run = shutil.which("systemd-run")
    if not LINUX or systemd_run is None:
        return False
    probe = subprocess.run(
        [systemd_run, "--user", "--scope", "--collect", "--quiet", "--", "true"],
        capture_output=True, stdin=subprocess.DEVNULL)
    return probe.returncode == 0


@unittest.skipUnless(user_manager_available(), "no reachable systemd user manager (e.g. CI)")
class MemoryMaxScenario(StressRunCase):
    def test_command_runs_in_a_capped_transient_scope(self):
        # Scenario 8: the cap is read from the command's own cgroup files and
        # the scope is gone once the wrapper has exited.
        report = self.tmp / "cgroup.json"
        child = self.tmp / "cgroup_child.py"
        child.write_text(CGROUP_CHILD)
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--deadline", "30s", "--cpus", "1",
             "--load-per-cpu", "1", "--memory-max", "256M", "--", sys.executable,
             str(child), str(report)],
            capture_output=True, text=True, timeout=WAIT_SECONDS)
        self.assertEqual(result.returncode, 0, result.stderr)
        data = json.loads(report.read_text())
        self.assertEqual(data["memory_max"], str(256 * 1024 * 1024))
        self.assertTrue(data["cgroup"].endswith(".scope"), data["cgroup"])
        deadline = time.monotonic() + GONE_SECONDS
        while Path(f"/sys/fs/cgroup{data['cgroup']}").exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertFalse(Path(f"/sys/fs/cgroup{data['cgroup']}").exists())


@unittest.skipUnless(sys.platform == "darwin", "the refusal is exercised on macOS only")
class MacosRefusal(StressRunCase):
    def test_refuses_without_unpinned_and_runs_with_it(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--deadline", "30s", "--", "true"],
            capture_output=True, text=True, timeout=WAIT_SECONDS)
        self.assertEqual(result.returncode, 2)
        self.assertIn("--unpinned", result.stderr)
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--deadline", "30s", "--cpus", "1",
             "--load-per-cpu", "1", "--unpinned", "--", "true"],
            capture_output=True, text=True, timeout=WAIT_SECONDS)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("host-wide", result.stderr)


class HelpScenario(unittest.TestCase):
    def test_help_documents_every_flag_and_the_recipe(self):
        result = subprocess.run(
            [sys.executable, str(SCRIPT), "--help"], capture_output=True, text=True,
            timeout=WAIT_SECONDS)
        self.assertEqual(result.returncode, 0)
        for token in ("--deadline", "--cpus", "--load-per-cpu", "--unpinned",
                      "--memory-max", "--no-run", "124"):
            self.assertIn(token, result.stdout)


if __name__ == "__main__":
    unittest.main()
