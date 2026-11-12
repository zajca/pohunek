"""Unit tests for .github/scripts/ci-retry (#2201). Stdlib only."""

import contextlib
import os
import re
import signal
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / ".github" / "scripts" / "ci-retry"

FORBIDDEN_SUBSTRINGS = (
    "setsid",
    "mapfile",
    "declare -A",
    "wait -n",
)

# Regexes over code lines (no comment lines, no usage string): the `timeout`
# and `${...}` bans target the GNU timeout(1) command and bash 4
# lowercasing/uppercase expansions, not the words in prose or the POSIX
# `${var:-default}` fallback form the helper needs for GITHUB_ENV.
FORBIDDEN_PATTERNS = (
    r"(?m)(?:^|[\s;|&])timeout\s",
    r"\$\{[A-Za-z_][A-Za-z0-9_]*[,^]",
)


def code_of(text: str) -> str:
    """Helper text without comment lines and the usage string."""
    kept = []
    for line in text.splitlines():
        stripped = line.lstrip()
        if stripped.startswith("#"):
            continue
        if 'echo "usage:' in stripped:
            continue
        kept.append(line)
    return "\n".join(kept)


def run_helper(*args: str, env: dict | None = None, timeout: int = 120) -> subprocess.CompletedProcess:
    merged = dict(os.environ)
    if env is not None:
        merged.update(env)
    return subprocess.run(
        [str(HELPER), *args],
        capture_output=True,
        text=True,
        timeout=timeout,
        env=merged,
    )


class HelperScriptTests(unittest.TestCase):
    def test_helper_is_executable(self):
        self.assertTrue(HELPER.is_file())
        self.assertTrue(os.access(HELPER, os.X_OK))

    def test_helper_uses_no_forbidden_constructs(self):
        code = code_of(HELPER.read_text())
        for needle in FORBIDDEN_SUBSTRINGS:
            with self.subTest(needle=needle):
                self.assertNotIn(needle, code)
        for pattern in FORBIDDEN_PATTERNS:
            with self.subTest(pattern=pattern):
                self.assertIsNone(re.search(pattern, code))

    def test_injected_failure_retries_then_succeeds(self):
        with tempfile.TemporaryDirectory() as tmp:
            counter = Path(tmp) / "counter"
            flaky = Path(tmp) / "flaky.sh"
            flaky.write_text(
                "#!/bin/sh\n"
                f'if [ -f "{counter}" ]; then exit 0; fi\n'
                f'touch "{counter}"\n'
                "exit 3\n"
            )
            flaky.chmod(0o755)
            proc = run_helper(
                "--label", "flaky", "--attempts", "3", "--attempt-timeout", "30",
                "--backoff", "0", "--", str(flaky),
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("ci-retry: flaky attempt 1/3", proc.stdout)
            self.assertIn("ci-retry: flaky attempt 2/3", proc.stdout)
            self.assertIn("failed (exit 3)", proc.stdout)
            self.assertNotIn("::error", proc.stderr)

    def test_exhaustion_reports_last_exit_and_marks_infra(self):
        with tempfile.TemporaryDirectory() as tmp:
            env_file = Path(tmp) / "github_env"
            env_file.write_text("")
            proc = run_helper(
                "--label", "doomed", "--attempts", "2", "--attempt-timeout", "30",
                "--backoff", "0", "--", "sh", "-c", "exit 7",
                env={"GITHUB_ENV": str(env_file)},
            )
            self.assertEqual(proc.returncode, 7, proc.stderr)
            self.assertIn("::error title=Infrastructure::", proc.stderr)
            self.assertIn("doomed failed after 2 attempts (exit 7)", proc.stderr)
            self.assertIn("CI_INFRA_FAILURE=doomed", env_file.read_text().splitlines())

    def test_exhaustion_without_github_env_still_fails(self):
        env = dict(os.environ)
        env.pop("GITHUB_ENV", None)
        proc = run_helper(
            "--label", "plain", "--attempts", "1", "--attempt-timeout", "30",
            "--backoff", "0", "--", "sh", "-c", "exit 5",
            env=env,
        )
        self.assertEqual(proc.returncode, 5, proc.stderr)

    def test_attempt_timeout_kills_the_attempt_and_its_grandchild(self):
        with tempfile.TemporaryDirectory() as tmp:
            grandchild = Path(tmp) / "grandchild.pid"
            probe = Path(tmp) / "probe.sh"
            probe.write_text(
                "#!/bin/sh\n"
                "sleep 300 &\n"
                f'echo $! > "{grandchild}"\n'
                "sleep 300\n"
            )
            probe.chmod(0o755)
            proc = run_helper(
                "--label", "stuck", "--attempts", "1", "--attempt-timeout", "2",
                "--backoff", "0", "--", str(probe),
                timeout=60,
            )
            self.assertNotEqual(proc.returncode, 0)
            self.assertTrue(grandchild.is_file(), "probe never recorded its grandchild")
            pid = int(grandchild.read_text().strip())
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except (ProcessLookupError, PermissionError):
                    break
                time.sleep(0.2)
            else:
                with contextlib.suppress(ProcessLookupError):
                    os.kill(pid, signal.SIGKILL)
                self.fail("grandchild survived the attempt timeout")

    def test_missing_flag_exits_2_without_running_the_command(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker = Path(tmp) / "marker"
            proc = run_helper(
                "--label", "incomplete", "--attempts", "3", "--attempt-timeout", "30",
                "--", "sh", "-c", f"touch {marker}",
            )
            self.assertEqual(proc.returncode, 2, proc.stderr)
            self.assertFalse(marker.exists())


if __name__ == "__main__":
    unittest.main()
