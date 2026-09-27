"""Behavior of the portable `pohunek_run_with_timeout` helper in lib.sh (stdlib only)."""

from pathlib import Path
import subprocess
import time
import unittest

LIB = Path(__file__).resolve().parents[1] / "lib.sh"
# Bound on the wall clock of each scenario; far above every expected duration.
SCENARIO_TIMEOUT_SECONDS = 20


def run(body, stdin=None):
    """Runs `body` after sourcing lib.sh and returns (stdout, elapsed seconds)."""
    script = f'. "{LIB}"\nstatus=0\n{body}\nprintf "status=%s\\n" "$status"\n'
    started = time.monotonic()
    completed = subprocess.run(
        ["sh", "-c", script],
        input=stdin,
        capture_output=True,
        text=True,
        timeout=SCENARIO_TIMEOUT_SECONDS,
        check=True,
    )
    return completed.stdout, time.monotonic() - started


def running(marker):
    """Returns whether any process command line contains `marker`."""
    listing = subprocess.run(
        ["ps", "-A", "-o", "args="], capture_output=True, text=True, check=True
    ).stdout
    return any(marker in line and "ps -A" not in line for line in listing.splitlines())


class RunWithTimeoutTests(unittest.TestCase):
    def test_early_exit_keeps_status_and_cancels_the_watchdog(self):
        # A distinctive deadline makes the watchdog's sleeper findable in `ps`.
        output, elapsed = run('pohunek_run_with_timeout 17.25 sh -c "exit 3" || status=$?')
        # dash reports signalled jobs ("Terminated"); nothing may leak into stdout.
        self.assertEqual(output, "status=3\n")
        self.assertLess(elapsed, 5)
        self.assertFalse(running("sleep 17.25"), "watchdog sleeper was left behind")

    def test_fast_commands_never_print_job_reports(self):
        # A command that exits before the watchdog installs its trap used to
        # make dash print "Terminated" for the cancelled watchdog.
        body = "\n".join(
            "pohunek_run_with_timeout 5 true || status=$?" for _ in range(20)
        )
        output, _ = run(body)
        self.assertEqual(output, "status=0\n")

    def test_success_returns_zero_and_passes_stdin_and_stdout(self):
        output, _ = run("pohunek_run_with_timeout 10 cat || status=$?", stdin="hello\n")
        self.assertEqual(output, "hello\nstatus=0\n")

    def test_deadline_returns_124_and_reaps_the_command(self):
        output, elapsed = run("pohunek_run_with_timeout 1 sleep 31.5 || status=$?")
        self.assertEqual(output, "status=124\n")
        self.assertLess(elapsed, 6)
        self.assertFalse(running("sleep 31.5"), "timed-out command was left behind")

    def test_success_reaped_after_the_deadline_is_not_a_timeout(self):
        # The command stops the calling shell and exits 0 at once, so it stays
        # an unreaped zombie until a helper resumes the shell. The watchdog
        # fires in between and its TERM "succeeds" against the zombie; the
        # shell must still report the command's own success.
        resume_after = 1.5  # past the 1 s deadline, inside the 1 s TERM grace
        command = (
            f"(sleep {resume_after}; kill -CONT \"$1\") >/dev/null 2>&1 & "
            'kill -STOP "$1"; exit 0'
        )
        output, elapsed = run(
            f"pohunek_run_with_timeout 1 sh -c '{command}' _ \"$$\" || status=$?"
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
        output, elapsed = run(
            f"pohunek_run_with_timeout 1 python3 -c '{exit_on_term}' || status=$?"
        )
        self.assertEqual(output, "status=124\n")
        self.assertLess(elapsed, 6)
        self.assertFalse(running("time.sleep(33.5)"), "timed-out command survived")

    def test_a_command_ignoring_term_is_killed_after_the_grace(self):
        ignore_term = (
            "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); "
            "time.sleep(32.5)"
        )
        output, elapsed = run(
            f"pohunek_run_with_timeout 1 python3 -c '{ignore_term}' || status=$?"
        )
        self.assertIn("status=124", output)
        self.assertLess(elapsed, 8)
        self.assertFalse(running("time.sleep(32.5)"), "TERM-ignoring command survived")


if __name__ == "__main__":
    unittest.main()
