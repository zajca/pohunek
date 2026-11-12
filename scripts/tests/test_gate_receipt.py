"""Behavioural scenarios for scripts/gate-receipt, run through its command line.

Each scenario drives the real script as a subprocess against a real git
repository in a temporary directory, with fake `rustc`/`cargo` on PATH that
print fixed `-vV` text, a temporary `--store`, and a fake command that
appends to a marker file. A scenario asserts what an operator observes: the
command ran (marker lines), the exit code, and the printed verdict.
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import hermetic_git  # noqa: F401  (isolates git from the developer's configuration)

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "scripts" / "gate-receipt"

RUSTC_TEXT = "rustc 1.96.0 (fake abcdef)\nbinary: rustc\n"
CARGO_TEXT = "cargo 1.96.0 (fake 123456)\nrelease: 1.96.0\n"


class GateReceiptCase(unittest.TestCase):
    def git(self, *args, env=None):
        git_env = dict(getattr(self, "env", os.environ))
        git_env.update(env or {})
        git_env.update(
            GIT_CONFIG_COUNT="2",
            GIT_CONFIG_KEY_0="maintenance.auto",
            GIT_CONFIG_VALUE_0="false",
            GIT_CONFIG_KEY_1="gc.auto",
            GIT_CONFIG_VALUE_1="0",
        )
        completed = subprocess.run(
            ["git", *args], cwd=self.repo, env=git_env,
            capture_output=True, text=True, check=False,
        )
        if completed.returncode != 0:
            raise AssertionError(f"git {' '.join(args)} failed: {completed.stderr}")
        return completed.stdout.strip()

    def git_clone(self, source, target):
        completed = subprocess.run(
            ["git", "clone", "-q", str(source), str(target)], cwd=self.tmp,
            env={
                **self.env,
                "GIT_CONFIG_COUNT": "2",
                "GIT_CONFIG_KEY_0": "maintenance.auto",
                "GIT_CONFIG_VALUE_0": "false",
                "GIT_CONFIG_KEY_1": "gc.auto",
                "GIT_CONFIG_VALUE_1": "0",
            },
            capture_output=True, text=True, check=False,
        )
        if completed.returncode != 0:
            raise AssertionError(f"git clone failed: {completed.stderr}")

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        self.repo = self.tmp / "repo"
        self.repo.mkdir()
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        self.store = self.tmp / "store"
        self.marker = self.tmp / "marker"
        self.git("init", "-q", ".")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "user.name", "test")
        (self.repo / "file.txt").write_text("content\n")
        self.git("add", "-A")
        self.git("commit", "-qm", "init")
        self.write_tool("rustc", RUSTC_TEXT)
        self.write_tool("cargo", CARGO_TEXT)
        self.write_command("ok", "exit 0")
        self.env = dict(os.environ)
        self.env["PATH"] = f"{self.bin}{os.pathsep}{self.env['PATH']}"
        self.env.pop("POHUNEK_SESSION_ID", None)

    def write_tool(self, name, text):
        path = self.bin / name
        path.write_text(f"#!/bin/sh\nprintf '%s' {text!r}\n")
        path.chmod(0o755)

    def write_command(self, name, body):
        """A fake gate command appending one line to the marker, then `body`."""
        path = self.bin / name
        path.write_text(
            "#!/bin/sh\n"
            f'echo ran >> "{self.marker}"\n'
            f"{body}\n"
        )
        path.chmod(0o755)

    def run_tool(self, *args, env=None):
        return subprocess.run(
            [sys.executable, str(TOOL), *args],
            cwd=self.repo,
            env=env or self.env,
            capture_output=True,
            text=True,
            check=False,
        )

    def run_gate(self, *args, env=None):
        return self.run_tool(
            "run", "--gate", "test-gate", "--max-age", "24h",
            "--store", str(self.store), "--", *args, env=env,
        )

    def check_gate(self, *args, env=None):
        return self.run_tool(
            "check", "--gate", "test-gate", "--max-age", "24h",
            "--store", str(self.store), "--", *args, env=env,
        )

    def marker_lines(self):
        if not self.marker.exists():
            return 0
        return len(self.marker.read_text().splitlines())

    def receipts(self):
        if not self.store.is_dir():
            return []
        return sorted(self.store.glob("test-gate-*.json"))

    def read_receipt(self):
        return json.loads(self.receipts()[0].read_text())

    def test_first_run_executes_and_second_run_skips(self):
        first = self.run_gate("ok")
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(self.marker_lines(), 1)
        self.assertEqual(len(self.receipts()), 1)
        receipt = self.receipts()[0]
        self.assertEqual(oct(receipt.stat().st_mode & 0o777), "0o600")
        self.assertEqual(oct(self.store.stat().st_mode & 0o777), "0o700")
        second = self.run_gate("ok")
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(self.marker_lines(), 1)
        self.assertIn("verified; skipping", second.stdout)

    def test_untracked_staged_and_unstaged_changes_each_rerun(self):
        self.assertEqual(self.run_gate("ok").returncode, 0)
        self.assertEqual(self.marker_lines(), 1)
        (self.repo / "untracked.txt").write_text("new\n")
        self.assertEqual(self.run_gate("ok").returncode, 0)
        self.assertEqual(self.marker_lines(), 2)
        (self.repo / "untracked.txt").unlink()
        (self.repo / "file.txt").write_text("staged\n")
        self.git("add", "file.txt")
        self.assertEqual(self.run_gate("ok").returncode, 0)
        self.assertEqual(self.marker_lines(), 3)
        self.git("reset", "-q", "HEAD", "file.txt")
        self.git("checkout", "-q", "--", "file.txt")
        (self.repo / "file.txt").write_text("unstaged\n")
        self.assertEqual(self.run_gate("ok").returncode, 0)
        self.assertEqual(self.marker_lines(), 4)

    def test_failing_command_gives_no_receipt(self):
        self.write_command("fail", "exit 3")
        result = self.run_gate("fail")
        self.assertEqual(result.returncode, 3)
        self.assertEqual(self.marker_lines(), 1)
        self.assertEqual(self.receipts(), [])

    def test_different_argv_executes(self):
        self.write_command("other", "exit 0")
        self.assertEqual(self.run_gate("ok").returncode, 0)
        result = self.run_gate("other")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.marker_lines(), 2)
        self.assertEqual(len(self.receipts()), 2)

    def test_changed_rustflags_executes_and_value_stays_secret(self):
        self.assertEqual(self.run_gate("ok").returncode, 0)
        secret = "s3cr3t-rustflags-value"
        env = dict(self.env, RUSTFLAGS=secret)
        result = self.run_gate("ok", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.marker_lines(), 2)
        self.assertEqual(len(self.receipts()), 2)
        for receipt in self.receipts():
            self.assertNotIn(secret, receipt.read_text())
        self.assertNotIn(secret, result.stdout)

    def test_changed_toolchain_executes(self):
        self.assertEqual(self.run_gate("ok").returncode, 0)
        self.write_tool("rustc", "rustc 1.97.0 (fake changed)\nbinary: rustc\n")
        result = self.run_gate("ok")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.marker_lines(), 2)

    def test_group_writable_receipt_is_rejected(self):
        self.assertEqual(self.run_gate("ok").returncode, 0)
        self.assertEqual(self.marker_lines(), 1)
        receipt = self.receipts()[0]
        receipt.chmod(0o660)
        try:
            result = self.run_gate("ok")
        finally:
            receipt.chmod(0o600)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.marker_lines(), 2)
        self.assertIn("group- or world-writable", result.stdout)

    def test_expired_receipt_is_rejected_without_sleep(self):
        self.assertEqual(self.run_gate("ok").returncode, 0)
        self.assertEqual(self.marker_lines(), 1)
        receipt_path = self.receipts()[0]
        receipt = json.loads(receipt_path.read_text())
        receipt["start"] = "2001-01-01T00:00:00+00:00"
        receipt["end"] = "2001-01-01T00:00:01+00:00"
        receipt_path.write_text(json.dumps(receipt, sort_keys=True, indent=2) + "\n")
        result = self.run_tool(
            "run", "--gate", "test-gate", "--max-age", "1s",
            "--store", str(self.store), "--", "ok",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.marker_lines(), 2)
        self.assertIn("older than the max age", result.stdout)

    def test_command_rewriting_a_file_gets_no_receipt(self):
        self.write_command("rewrite", f"echo changed > \"{self.repo / 'file.txt'}\"")
        result = self.run_gate("rewrite")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.marker_lines(), 1)
        self.assertEqual(self.receipts(), [])
        self.assertIn("changed the working tree", result.stdout)

    def test_check_never_runs_the_command(self):
        before = self.check_gate("ok")
        self.assertEqual(before.returncode, 1)
        self.assertEqual(self.marker_lines(), 0)
        self.assertIn("invalid", before.stdout)
        self.assertEqual(self.run_gate("ok").returncode, 0)
        after = self.check_gate("ok")
        self.assertEqual(after.returncode, 0, after.stderr)
        self.assertEqual(self.marker_lines(), 1)
        self.assertIn("valid", after.stdout)

    def test_clone_of_the_same_content_accepts_the_receipt(self):
        self.assertEqual(self.run_gate("ok").returncode, 0)
        self.assertEqual(self.marker_lines(), 1)
        clone = self.tmp / "clone"
        self.git_clone(self.repo, clone)
        marker = self.tmp / "clone-marker"
        env = dict(self.env)
        (self.bin / "clone-ok").write_text(
            "#!/bin/sh\n" f'echo ran >> "{marker}"\n' "exit 0\n"
        )
        (self.bin / "clone-ok").chmod(0o755)
        # The gate command resolves through PATH, so the clone run must use the
        # same argv to match the key.
        (self.bin / "ok").write_text(
            "#!/bin/sh\n" f'echo ran >> "{marker}"\n' "exit 0\n"
        )
        result = subprocess.run(
            [sys.executable, str(TOOL), "run", "--gate", "test-gate",
             "--max-age", "24h", "--store", str(self.store), "--", "ok"],
            cwd=clone, env=env, capture_output=True, text=True, check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("verified; skipping", result.stdout)
        self.assertFalse(marker.exists())


if __name__ == "__main__":
    unittest.main()
