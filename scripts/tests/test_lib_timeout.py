"""Behavior of the portable `pohunek_run_with_timeout` helper in lib.sh (stdlib only)."""

from pathlib import Path
import functools
import os
import shlex
import signal
import subprocess
import sys
import tempfile
import threading
import time
from typing import NamedTuple, Optional
import unittest
from unittest import mock

LIB = Path(__file__).resolve().parents[1] / "lib.sh"
# Bound on the wall clock of each scenario; far above every expected duration.
SCENARIO_TIMEOUT_SECONDS = 20
# Bound on how long a process killed at the deadline may take to disappear
# from `ps`, far below the lifetime it would have had without the kill.
REAP_BOUND_SECONDS = 2
REAP_POLL_SECONDS = 0.05
# Bound on how long a recorded pid may take to appear in its file: the shell
# creates the file before it writes the pid, and may be descheduled in between.
RECORD_BOUND_SECONDS = 5
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


def run(body, stdin=None, timeout=SCENARIO_TIMEOUT_SECONDS):
    """Runs `body` after sourcing lib.sh in a shell of its own process group.

    A scenario still running after `timeout` seconds is killed together with
    every process it started, and `subprocess.TimeoutExpired` is raised.
    """
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
        stdout, stderr = shell.communicate(stdin, timeout=timeout)
    except subprocess.TimeoutExpired:
        kill_scenario(shell.pid)
        try:
            shell.communicate(timeout=REAP_BOUND_SECONDS)
        except subprocess.TimeoutExpired:
            # A process outside the shell's tree holds the pipes open.
            shell.kill()
        raise
    if shell.returncode != 0:
        raise subprocess.CalledProcessError(shell.returncode, script, stdout, stderr)
    return Run(stdout, time.monotonic() - started, shell.pid)


def recorded(pid_file, *argv):
    """Returns a shell command line running `argv` after recording its pid."""
    return shlex.join(["sh", "-c", RECORD_PID_THEN_EXEC, "_", str(pid_file), *argv])


class Process(NamedTuple):
    parent: int
    group: int
    session: Optional[int]  # None where `ps` has no session id column


@functools.cache
def ps_lists_sessions():
    """Returns whether `ps` can print a session id (procps does, BSD `ps` does not).

    BSD `ps` only offers `sess`, a kernel pointer that is no usable id.
    """
    probe = subprocess.run(
        ["ps", "-o", "sid=", "-p", str(os.getpid())], capture_output=True, text=True
    )
    return probe.returncode == 0 and probe.stdout.strip().isdigit()


def live_processes():
    """Returns {pid: Process} of every process that is not a zombie.

    A process killed by the helper may stay a zombie until its reaper runs;
    it no longer executes anything and does not count as alive.
    """
    sessions = ps_lists_sessions()
    columns = "pid=,ppid=,pgid=,sid=,stat=" if sessions else "pid=,ppid=,pgid=,stat="
    listing = subprocess.run(
        ["ps", "-A", "-o", columns], capture_output=True, text=True, check=True
    ).stdout
    alive = {}
    for line in listing.splitlines():
        fields = line.split()
        state = fields[-1]
        if not state.startswith("Z"):
            pid, parent, group = (int(field) for field in fields[:3])
            session = int(fields[3]) if sessions else None
            alive[pid] = Process(parent, group, session)
    return alive


def scenario_processes(root):
    """Returns the live processes that belong to the scenario shell `root`.

    The shell is a session and process group leader, so a member is any
    process of that session or group, or a descendant of a member. The helper
    puts the tested command into a group of its own, and a background child
    outlives the shell, which reparents it: the session id (where `ps` has
    one) and the group keep naming those, the parent links catch the rest.
    A process that left the group, was orphaned and sits in a `ps` without
    session ids is out of reach.
    """
    table = live_processes()
    members = {
        pid
        for pid, process in table.items()
        if pid == root or process.group == root or process.session == root
    }
    grown = True
    while grown:
        grown = False
        for pid, process in table.items():
            if pid not in members and process.parent in members:
                members.add(pid)
                grown = True
    return {pid: table[pid] for pid in members}


def kill_scenario(root):
    """Kills every process of the scenario shell `root`, which may have exited.

    Repeats until none is left, as a process may fork between the listing and
    the kill. The groups of the members are killed too.
    """
    deadline = time.monotonic() + REAP_BOUND_SECONDS
    own_group = os.getpgrp()
    members = scenario_processes(root)
    while members and time.monotonic() <= deadline:
        for group in {process.group for process in members.values()} - {own_group}:
            kill_group(group)
        for pid in members:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(REAP_POLL_SECONDS)
        members = scenario_processes(root)


def kill_group(group):
    """Kills every member of process group `group`, if any is left."""
    try:
        os.killpg(group, signal.SIGKILL)
    except ProcessLookupError:
        pass


def survivors(pids=(), group=None):
    """Returns the live processes among `pids` or in process group `group`.

    Only processes the test started are named, so an unrelated process can
    neither be reported nor hide a leftover.
    """
    return sorted(
        pid
        for pid, process in live_processes().items()
        if pid in pids or process.group == group
    )


def survivors_after_reap(pids=(), group=None):
    """Returns the survivors left once the reap bound has elapsed."""
    deadline = time.monotonic() + REAP_BOUND_SECONDS
    left = survivors(pids, group)
    while left and time.monotonic() <= deadline:
        time.sleep(REAP_POLL_SECONDS)
        left = survivors(pids, group)
    return left


def command_line(pid):
    """Returns the complete command line of process `pid`.

    `-ww` lifts the width limit, which otherwise follows `COLUMNS` (procps)
    or the terminal (BSD `ps`) and cuts the arguments short.
    """
    return subprocess.run(
        ["ps", "-ww", "-p", str(pid), "-o", "args="],
        capture_output=True,
        text=True,
        check=True,
    ).stdout


class ScenarioTestCase(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.scratch = Path(scratch.name)

    def pid_file(self, name):
        return self.scratch / name

    def recorded_pids(self, name, count=1):
        """Returns the `count` pids the scenario recorded in the file `name`.

        The recorder creates the file before it writes the pid, so the file is
        read until it holds `count` complete lines.
        """
        path = self.pid_file(name)
        deadline = time.monotonic() + RECORD_BOUND_SECONDS
        while True:
            text = path.read_text() if path.exists() else ""
            lines = text.split("\n")[:-1]  # the last piece is an unfinished line
            if len(lines) >= count or time.monotonic() > deadline:
                break
            time.sleep(REAP_POLL_SECONDS)
        self.assertEqual(len(lines), count, "the scenario recorded too few pids")
        return [int(line) for line in lines]


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
        pids = self.recorded_pids("descendants.pid", count=2)
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
        # A narrow terminal width must not cut the listing short.
        with mock.patch.dict(os.environ, {"COLUMNS": "80"}):
            args = command_line(decoy.pid)
        for marker in DECOY_MARKERS:
            self.assertIn(marker, args)

    def test_a_leftover_in_the_scenario_group_is_reported(self):
        _, _, group = run("sleep 31.5 >/dev/null 2>&1 &")
        self.addCleanup(kill_group, group)
        left = survivors(group=group)
        # Nothing but the scenario's own leftover is in the group, whatever the
        # decoy or other processes on the host look like.
        self.assertEqual(len(left), 1)
        kill_group(group)
        self.assertEqual(survivors_after_reap(group=group), [])

    def test_a_recorded_command_left_running_is_reported(self):
        command = recorded(self.pid_file("command.pid"), "sleep", "31.5")
        _, _, group = run(f"{command} >/dev/null 2>&1 &")
        self.addCleanup(kill_group, group)
        pids = self.recorded_pids("command.pid")
        self.assertEqual(survivors(pids), pids)
        for pid in pids:
            os.kill(pid, signal.SIGKILL)
        self.assertEqual(survivors_after_reap(pids), [])

    def test_a_pid_is_read_only_once_its_line_is_complete(self):
        # The recorder creates the file, then waits for the gate before it
        # writes the pid, so the file exists but is empty until the test opens
        # the gate.
        gate = self.scratch / "gate"
        os.mkfifo(gate)
        gated = (
            ': >>"$1"; read -r _ <"$2"; echo $$ >>"$1"; shift 2; exec "$@"'
        )
        command = shlex.join(
            ["sh", "-c", gated, "_", str(self.pid_file("command.pid")), str(gate)]
            + ["sleep", "31.5"]
        )
        _, _, group = run(f"{command} >/dev/null 2>&1 &")
        self.addCleanup(kill_group, group)
        pid_file = self.pid_file("command.pid")
        deadline = time.monotonic() + RECORD_BOUND_SECONDS
        while not pid_file.exists() and time.monotonic() < deadline:
            time.sleep(REAP_POLL_SECONDS)
        self.assertEqual(pid_file.read_text(), "")

        def open_gate():
            with open(gate, "w") as opened:
                opened.write("go\n")

        # A daemon thread: opening the FIFO blocks until the recorder reads it.
        opener = threading.Thread(target=open_gate, daemon=True)
        opener.start()
        self.addCleanup(opener.join, RECORD_BOUND_SECONDS)
        pids = self.recorded_pids("command.pid")
        self.assertEqual(survivors(pids), pids)


class ScenarioTimeoutTests(ScenarioTestCase):
    """A scenario that outlives its timeout is killed with all it started."""

    def test_a_background_child_of_an_exited_shell_is_killed(self):
        # The shell exits at once; the child keeps the captured pipes open and
        # is reparented, so no parent link leads from the shell to it.
        command = recorded(self.pid_file("command.pid"), "sleep", "45")
        started = time.monotonic()
        with self.assertRaises(subprocess.TimeoutExpired):
            run(f"{command} &", timeout=2)
        self.assertLess(time.monotonic() - started, SCENARIO_TIMEOUT_SECONDS)
        self.assertEqual(
            survivors_after_reap(self.recorded_pids("command.pid")),
            [],
            "a background child of the exited shell survived the timeout",
        )

    @unittest.skipUnless(ps_lists_sessions(), "ps has no session id column")
    def test_an_orphan_in_a_group_of_its_own_is_killed(self):
        # Neither the shell's group nor a parent link names the orphan; only
        # the session it never left does.
        own_group = "import os, time; os.setpgid(0, 0); time.sleep(45)"
        command = recorded(self.pid_file("command.pid"), "python3", "-c", own_group)
        with self.assertRaises(subprocess.TimeoutExpired):
            run(f"{command} &", timeout=2)
        self.assertEqual(
            survivors_after_reap(self.recorded_pids("command.pid")),
            [],
            "an orphan in a group of its own survived the timeout",
        )

    def test_the_command_of_a_timed_out_scenario_is_killed_at_once(self):
        # The helper runs the command in a process group of its own and the
        # command holds the captured pipes open for as long as it lives.
        command = recorded(self.pid_file("command.pid"), "sleep", "45")
        started = time.monotonic()
        with self.assertRaises(subprocess.TimeoutExpired):
            run(f"pohunek_run_with_timeout 60 {command}", timeout=2)
        self.assertLess(time.monotonic() - started, SCENARIO_TIMEOUT_SECONDS)
        self.assertEqual(
            survivors_after_reap(self.recorded_pids("command.pid")),
            [],
            "the timed-out scenario left its command behind",
        )


if __name__ == "__main__":
    unittest.main()
