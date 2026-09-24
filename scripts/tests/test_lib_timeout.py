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
        self.assertIn("status=3", output)
        self.assertLess(elapsed, 5)
        self.assertFalse(running("sleep 17.25"), "watchdog sleeper was left behind")

    def test_success_returns_zero_and_passes_stdin_and_stdout(self):
        output, _ = run("pohunek_run_with_timeout 10 cat || status=$?", stdin="hello\n")
        self.assertEqual(output, "hello\nstatus=0\n")

    def test_deadline_returns_124_and_reaps_the_command(self):
        output, elapsed = run("pohunek_run_with_timeout 1 sleep 31.5 || status=$?")
        self.assertIn("status=124", output)
        self.assertLess(elapsed, 6)
        self.assertFalse(running("sleep 31.5"), "timed-out command was left behind")

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
