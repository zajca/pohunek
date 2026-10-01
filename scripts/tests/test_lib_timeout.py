"""Behavior of the portable `pohunek_run_with_timeout` helper in lib.sh (stdlib only)."""

from pathlib import Path
import os
import shlex
import signal
import subprocess
import sys
import tempfile
import time
from typing import NamedTuple
import unittest

LIB = Path(__file__).resolve().parents[1] / "lib.sh"
# Bound on the wall clock of each scenario; far above every expected duration.
SCENARIO_TIMEOUT_SECONDS = 20
# Bound on how long a process killed at the deadline may take to disappear
# from `ps`, far below the lifetime it would have had without the kill.
REAP_BOUND_SECONDS = 2
REAP_POLL_SECONDS = 0.05
# Command-line fragments of the long sleeps the scenarios start. The module
# runs next to a decoy process carrying all of them, so the reap assertions
# must identify processes by what the test started, never by command line.
DECOY_MARKERS = (
    "sleep 17.25",
    "sleep 31.5",
    "time.sleep(32.5)",
    "time.sleep(33.5)",
    "time.sleep(34.5)",
    "sleep 35.5",
    "sleep 36.5",
)
DECOY_LIFETIME_SECONDS = 600
# Appends the pid of the shell to the file in `$1`, then becomes the command
# in `"$@"`: `exec` keeps the pid, so the file names the command itself.
RECORD_PID_THEN_EXEC = 'echo $$ >>"$1"; shift; exec "$@"'

decoy = None


def setUpModule():
    global decoy
    decoy = subprocess.Popen(
        [
            sys.executable,
            "-c",
            f"import time; time.sleep({DECOY_LIFETIME_SECONDS})",
            *DECOY_MARKERS,
        ]
    )


def tearDownModule():
    decoy.kill()
    decoy.wait()


class Run(NamedTuple):
    output: str
    elapsed: float
    group: int  # process group of the scenario's shell and its plain children


def run(body, stdin=None):
    """Runs `body` after sourcing lib.sh in a shell of its own process group."""
    script = f'. "{LIB}"\nstatus=0\n{body}\nprintf "status=%s\\n" "$status"\n'
    started = time.monotonic()
    shell = subprocess.Popen(
        ["sh", "-c", script],
        stdin=subprocess.DEVNULL if stdin is None else subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    try:
        stdout, stderr = shell.communicate(stdin, timeout=SCENARIO_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        os.killpg(shell.pid, signal.SIGKILL)
        shell.communicate()
        raise
    if shell.returncode != 0:
        raise subprocess.CalledProcessError(shell.returncode, script, stdout, stderr)
    return Run(stdout, time.monotonic() - started, shell.pid)


def recorded(pid_file, *argv):
    """Returns a shell command line running `argv` after recording its pid."""
    return shlex.join(["sh", "-c", RECORD_PID_THEN_EXEC, "_", str(pid_file), *argv])


def live_pids():
    """Returns {pid: process group} of every process that is not a zombie.

    A process killed by the helper may stay a zombie until its reaper runs;
    it no longer executes anything and does not count as alive.
    """
    listing = subprocess.run(
        ["ps", "-A", "-o", "pid=,pgid=,stat="],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    alive = {}
    for line in listing.splitlines():
        pid, group, state = line.split()
        if not state.startswith("Z"):
            alive[int(pid)] = int(group)
    return alive


def survivors(pids=(), group=None):
    """Returns the live processes among `pids` or in process group `group`.

    Only processes the test started are named, so an unrelated process can
    neither be reported nor hide a leftover.
    """
    return sorted(
        pid
        for pid, pid_group in live_pids().items()
        if pid in pids or pid_group == group
    )


def survivors_after_reap(pids=(), group=None):
    """Returns the survivors left once the reap bound has elapsed."""
    deadline = time.monotonic() + REAP_BOUND_SECONDS
    left = survivors(pids, group)
    while left and time.monotonic() <= deadline:
        time.sleep(REAP_POLL_SECONDS)
        left = survivors(pids, group)
    return left


class ScenarioTestCase(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.scratch = Path(scratch.name)

    def pid_file(self, name):
        return self.scratch / name

    def recorded_pids(self, name):
        """Returns the pids the scenario recorded in the file `name`."""
        pids = [int(pid) for pid in self.pid_file(name).read_text().split()]
        self.assertTrue(pids, "the scenario recorded no pid")
        return pids


class RunWithTimeoutTests(ScenarioTestCase):
    def test_early_exit_keeps_status_and_cancels_the_watchdog(self):
        # The watchdog's sleeper is a plain child of the scenario's shell, so
        # it stays in the shell's process group after the helper returns.
        output, elapsed, group = run(
            'pohunek_run_with_timeout 17.25 sh -c "exit 3" || status=$?'
        )
        # dash reports signalled jobs ("Terminated"); nothing may leak into stdout.
        self.assertEqual(output, "status=3\n")
        self.assertLess(elapsed, 5)
        self.assertEqual(
            survivors_after_reap(group=group), [], "watchdog sleeper was left behind"
        )

    def test_fast_commands_never_print_job_reports(self):
        # A command that exits before the watchdog installs its trap used to
        # make dash print "Terminated" for the cancelled watchdog.
        body = "\n".join(
            "pohunek_run_with_timeout 5 true || status=$?" for _ in range(20)
        )
        output, _, _ = run(body)
        self.assertEqual(output, "status=0\n")

    def test_success_returns_zero_and_passes_stdin_and_stdout(self):
        output, _, _ = run(
            "pohunek_run_with_timeout 10 cat || status=$?", stdin="hello\n"
        )
        self.assertEqual(output, "hello\nstatus=0\n")

    def test_deadline_returns_124_and_reaps_the_command(self):
        pid_file = self.pid_file("command.pid")
        command = recorded(pid_file, "sleep", "31.5")
        output, elapsed, _ = run(f"pohunek_run_with_timeout 1 {command} || status=$?")
        self.assertEqual(output, "status=124\n")
        self.assertLess(elapsed, 6)
        self.assertEqual(
            survivors(self.recorded_pids("command.pid")),
            [],
            "timed-out command was left behind",
        )

    def test_success_reaped_after_the_deadline_is_not_a_timeout(self):
        # The command stops the calling shell and exits 0 at once, so it stays
        # an unreaped zombie until a helper resumes the shell. The watchdog
        # fires in between and its TERM "succeeds" against the zombie; the
        # shell must still report the command's own success. The resumer runs
        # outside the command's process group, which the deadline kills.
        resume_after = 1.5  # past the 1 s deadline, inside the 1 s TERM grace
        output, elapsed, _ = run(
            f'(sleep {resume_after}; kill -CONT "$$") >/dev/null 2>&1 &\n'
            "pohunek_run_with_timeout 1 sh -c 'kill -STOP \"$1\"; exit 0' _ \"$$\""
            " || status=$?"
        )
        self.assertEqual(output, "status=0\n")
        self.assertLess(elapsed, 6)

    def test_a_command_failing_after_term_reports_the_timeout(self):
        # A command that handles TERM and exits non-zero was still stopped by
        # the deadline, so the helper reports the timeout, not its status.
        exit_on_term = (
            "import signal, sys, time; "
            "signal.signal(signal.SIGTERM, lambda *_: sys.exit(7)); "
            "time.sleep(33.5)"
        )
        command = recorded(self.pid_file("command.pid"), "python3", "-c", exit_on_term)
        output, elapsed, _ = run(f"pohunek_run_with_timeout 1 {command} || status=$?")
        self.assertEqual(output, "status=124\n")
        self.assertLess(elapsed, 6)
        self.assertEqual(
            survivors(self.recorded_pids("command.pid")),
            [],
            "timed-out command survived",
        )

    def test_a_command_ignoring_term_is_killed_after_the_grace(self):
        ignore_term = (
            "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); "
            "time.sleep(32.5)"
        )
        command = recorded(self.pid_file("command.pid"), "python3", "-c", ignore_term)
        output, elapsed, _ = run(f"pohunek_run_with_timeout 1 {command} || status=$?")
        self.assertIn("status=124", output)
        self.assertLess(elapsed, 8)
        self.assertEqual(
            survivors(self.recorded_pids("command.pid")),
            [],
            "TERM-ignoring command survived",
        )

    def test_a_descendant_ignoring_term_is_killed_after_the_grace(self):
        # The command's own shell dies on TERM, but the child it spawned
        # ignores TERM; the escalation must still reach it through the group.
        # The child's output goes to /dev/null so a survivor cannot hold the
        # captured pipe open.
        ignore_term = (
            "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); "
            "time.sleep(34.5)"
        )
        spawn = 'python3 -c "$2" >/dev/null 2>&1 & echo $! >>"$1"; wait'
        command = shlex.join(
            ["sh", "-c", spawn, "_", str(self.pid_file("descendants.pid")), ignore_term]
        )
        output, elapsed, _ = run(f"pohunek_run_with_timeout 1 {command} || status=$?")
        self.assertEqual(output, "status=124\n")
        self.assertLess(elapsed, 8)
        self.assertEqual(
            survivors_after_reap(self.recorded_pids("descendants.pid")),
            [],
            "TERM-ignoring descendant survived the timeout",
        )

    def test_no_descendant_survives_the_timeout(self):
        # Descendants in the command's group receive the deadline's TERM even
        # though only their parent is the helper's child.
        spawn = (
            'sleep 35.5 >/dev/null 2>&1 & echo $! >>"$1"; '
            'sleep 36.5 >/dev/null 2>&1 & echo $! >>"$1"; wait'
        )
        command = shlex.join(
            ["sh", "-c", spawn, "_", str(self.pid_file("descendants.pid"))]
        )
        output, elapsed, _ = run(f"pohunek_run_with_timeout 1 {command} || status=$?")
        self.assertEqual(output, "status=124\n")
        self.assertLess(elapsed, 8)
        pids = self.recorded_pids("descendants.pid")
        self.assertEqual(len(pids), 2)
        self.assertEqual(
            survivors_after_reap(pids), [], "descendant survived the timeout"
        )

    def test_a_missing_command_keeps_the_shell_status(self):
        output, _, _ = run(
            "pohunek_run_with_timeout 5 pohunek-no-such-command || status=$?"
        )
        self.assertEqual(output, "status=127\n")


class ReapDetectionTests(ScenarioTestCase):
    """The reap assertions are bound to the processes the scenario started."""

    def test_the_decoy_carries_every_command_line_the_scenarios_use(self):
        args = subprocess.run(
            ["ps", "-p", str(decoy.pid), "-o", "args="],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
        for marker in DECOY_MARKERS:
            self.assertIn(marker, args)

    def test_a_leftover_in_the_scenario_group_is_reported(self):
        _, _, group = run("sleep 31.5 >/dev/null 2>&1 &")
        left = survivors(group=group)
        # Nothing but the scenario's own leftover is in the group, whatever the
        # decoy or other processes on the host look like.
        self.assertEqual(len(left), 1)
        os.killpg(group, signal.SIGKILL)
        self.assertEqual(survivors_after_reap(group=group), [])

    def test_a_recorded_command_left_running_is_reported(self):
        pid_file = self.pid_file("command.pid")
        command = recorded(pid_file, "sleep", "31.5")
        run(f"{command} >/dev/null 2>&1 &")
        deadline = time.monotonic() + REAP_BOUND_SECONDS
        while not pid_file.exists() and time.monotonic() < deadline:
            time.sleep(REAP_POLL_SECONDS)
        pids = self.recorded_pids("command.pid")
        self.assertEqual(survivors(pids), pids)
        for pid in pids:
            os.kill(pid, signal.SIGKILL)
        self.assertEqual(survivors_after_reap(pids), [])


if __name__ == "__main__":
    unittest.main()
