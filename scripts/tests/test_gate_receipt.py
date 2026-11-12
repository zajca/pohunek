"""Behavioural scenarios for scripts/gate-receipt, run through its command line.

Each scenario drives the real script as a subprocess against a real git
repository in a temporary directory, with fake `rustc`/`cargo` on PATH that
print fixed `-vV` text, a temporary `--store`, and a fake command that
appends to a marker file. A scenario asserts what an operator observes: the
command ran (marker lines), the exit code, and the printed verdict.
"""

import importlib.machinery
import importlib.util
import json
import os
import re
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


def load_gate_receipt():
    """The gate-receipt script as an importable module (it has no side
    effects on import: execution is guarded by `__name__ == "__main__"`)."""
    spec = importlib.util.spec_from_file_location(
        "gate_receipt", str(TOOL),
        loader=importlib.machinery.SourceFileLoader("gate_receipt", str(TOOL)),
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def gate_runtime_names():
    """The session runtime list from the script under test."""
    return set(load_gate_receipt().SESSION_RUNTIME_VARS)


class SessionRuntimeListTests(unittest.TestCase):
    """Guards the shared list against scripts-side drift.

    Compares SESSION_RUNTIME_VARS with SESSION_ENVIRONMENT_NAMES (the names
    command_from_initialize injects, owned by server.rs) plus
    HOOK_RETRY_ENV_NAMES in env.rs. When the worker injects a new session
    variable, add its name here (a name the worker no longer injects
    becomes class 2 and is keyed)."""

    WORKER_ENV = ROOT / "crates" / "session-worker" / "src" / "server.rs"
    RETRY_ENV = ROOT / "crates" / "worker-protocol" / "src" / "env.rs"

    def worker_injected_names(self):
        """POHUNEK_* literals in the SESSION_ENVIRONMENT_NAMES const."""
        source = self.WORKER_ENV.read_text()
        start = source.index("SESSION_ENVIRONMENT_NAMES")
        end = source.index("];", start)
        return set(re.findall(r'"(POHUNEK_[A-Z0-9_]+)"', source[start:end]))

    def retry_names(self):
        source = self.RETRY_ENV.read_text()
        start = source.index("HOOK_RETRY_ENV_NAMES")
        end = source.index("];", start)
        return set(re.findall(r'"(POHUNEK_[A-Z0-9_]+)"', source[start:end]))

    def test_runtime_list_matches_what_the_daemon_injects(self):
        gate_receipt = load_gate_receipt()
        expected = self.worker_injected_names() | self.retry_names()
        self.assertEqual(
            set(gate_receipt.SESSION_RUNTIME_VARS), expected,
            "SESSION_RUNTIME_VARS drifted from what the daemon injects; "
            "add the new session name, or remove a name the daemon no longer "
            "injects (an unlisted name becomes class 2 and is keyed)",
        )


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
        for name in list(self.env):
            if name.startswith("POHUNEK_"):
                del self.env[name]
        self.env["PATH"] = f"{self.bin}{os.pathsep}{self.env['PATH']}"

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

    def run_tool(self, *args, env=None, cwd=None):
        return subprocess.run(
            [sys.executable, str(TOOL), *args],
            cwd=self.repo if cwd is None else cwd,
            env=env or self.env,
            capture_output=True,
            text=True,
            check=False,
        )

    def run_gate(self, *args, env=None, cwd=None):
        return self.run_tool(
            "run", "--gate", "test-gate", "--max-age", "24h",
            "--store", str(self.store), "--", *args, env=env, cwd=cwd,
        )

    def check_gate(self, *args, env=None, cwd=None):
        return self.run_tool(
            "check", "--gate", "test-gate", "--max-age", "24h",
            "--store", str(self.store), "--", *args, env=env, cwd=cwd,
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
        self.assertIn("readable or writable by group or others", result.stdout)

    def test_store_with_wrong_mode_is_never_repaired(self):
        self.assertEqual(self.run_gate("ok").returncode, 0)
        self.assertEqual(self.marker_lines(), 1)
        mode = 0o750
        self.store.chmod(mode)
        run_result = self.run_gate("ok")
        check_result = self.check_gate("ok")
        mode_after = oct(self.store.stat().st_mode & 0o777)
        self.store.chmod(0o700)
        self.assertEqual(run_result.returncode, 0, run_result.stderr)
        self.assertEqual(self.marker_lines(), 2)
        self.assertIn("is not mode 0700", run_result.stdout)
        self.assertIn("no receipt written", run_result.stdout)
        self.assertEqual(check_result.returncode, 1)
        self.assertIn("is not mode 0700", check_result.stdout)
        self.assertEqual(mode_after, oct(mode))
        self.assertEqual(self.run_gate("ok").returncode, 0)
        self.assertEqual(self.marker_lines(), 2)

    def test_configuration_reaches_the_gate_and_changes_the_key(self):
        # POHUNEK_PYTHON_BIN is contributor configuration, not session
        # runtime: it must reach the wrapped command, and changing its value
        # must rerun the gate. The marker has one line per execution: the
        # value the gate saw, or `unset`.
        probe = self.bin / "probe"
        probe.write_text(
            "#!/bin/sh\n"
            f'echo "${{POHUNEK_PYTHON_BIN-unset}}" >> "{self.marker}"\n'
        )
        probe.chmod(0o755)
        first_env = dict(self.env, POHUNEK_PYTHON_BIN="/custom/python")
        result = self.run_gate("probe", env=first_env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.marker.read_text().splitlines(), ["/custom/python"])
        other = dict(self.env, POHUNEK_PYTHON_BIN="/other/python")
        result = self.run_gate("probe", env=other)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("running the command", result.stdout)
        self.assertEqual(
            self.marker.read_text().splitlines(),
            ["/custom/python", "/other/python"],
        )
        self.assertEqual(len(self.receipts()), 2)
        for receipt in self.receipts():
            names = json.loads(receipt.read_text())["env"]
            self.assertIn("POHUNEK_PYTHON_BIN", names)

    def test_session_state_neither_reaches_the_gate_nor_changes_the_key(self):
        # A gate that records whether POHUNEK_SESSION_ID reached it. The
        # marker has two lines per execution: `ran`, then the id or `unset`.
        probe = self.bin / "probe"
        probe.write_text(
            "#!/bin/sh\n"
            f'echo ran >> "{self.marker}"\n'
            f'echo "${{POHUNEK_SESSION_ID-unset}}" >> "{self.marker}"\n'
        )
        probe.chmod(0o755)
        first_env = dict(
            self.env,
            POHUNEK_SESSION_ID="s-first",
            POHUNEK_WORKER_ID="worker-first",
            POHUNEK_NATIVE_REFERENCE_KIND="id",
        )
        result = self.run_gate("probe", env=first_env)
        self.assertEqual(result.returncode, 0, result.stderr)
        runs = self.marker.read_text().splitlines()
        self.assertEqual(runs[-2:], ["ran", "unset"])
        self.assertEqual(self.read_receipt().get("session"), "s-first")
        # A second session with different POHUNEK_* state reuses the receipt:
        # no POHUNEK_* name is keyed, so the key is unchanged.
        other = dict(
            self.env,
            POHUNEK_SESSION_ID="s-second",
            POHUNEK_WORKER_ID="worker-second",
            POHUNEK_DAEMON_ID="d-second",
            POHUNEK_HOOK_TRACE_PATH="/other/hooks.jsonl",
        )
        result = self.run_gate("probe", env=other)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("verified; skipping", result.stdout)
        self.assertEqual(len(self.receipts()), 1)
        self.assertEqual(self.marker.read_text().splitlines(), runs)
        receipt = self.read_receipt()
        for name in receipt["env"]:
            self.assertNotIn(name, gate_runtime_names())

    def test_malformed_receipt_is_rejected(self):
        for payload in ("not json\n", '{"schema": 1}\n'):
            with self.subTest(payload=payload):
                self.assertEqual(self.run_gate("ok").returncode, 0)
                before = self.marker_lines()
                receipt_path = self.receipts()[0]
                receipt_path.write_text(payload)
                try:
                    result = self.run_gate("ok")
                finally:
                    receipt_path.unlink()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.marker_lines(), before + 1)
                self.assertIn("running the command", result.stdout)

    def test_future_end_beyond_skew_is_rejected(self):
        self.assertEqual(self.run_gate("ok").returncode, 0)
        receipt_path = self.receipts()[0]
        receipt = json.loads(receipt_path.read_text())
        receipt["end"] = "2999-01-01T00:00:00+00:00"
        receipt_path.write_text(json.dumps(receipt, sort_keys=True, indent=2) + "\n")
        before = self.marker_lines()
        result = self.run_gate("ok")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.marker_lines(), before + 1)
        self.assertIn("more than 60s in the future", result.stdout)

    def test_working_directory_is_part_of_the_key(self):
        self.assertEqual(self.run_gate("ok").returncode, 0)
        self.assertEqual(self.marker_lines(), 1)
        subdir = self.repo / "sub"
        subdir.mkdir()
        result = self.run_gate("ok", cwd=subdir)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.marker_lines(), 2)
        self.assertIn("running the command", result.stdout)

    def test_committing_identical_content_keeps_the_key(self):
        self.assertEqual(self.run_gate("ok").returncode, 0)
        self.assertEqual(self.marker_lines(), 1)
        head_before = self.read_receipt()["head"]
        self.git("commit", "-qm", "same content", "--allow-empty")
        head_after = self.git("rev-parse", "HEAD")
        self.assertNotEqual(head_before, head_after)
        result = self.run_gate("ok")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.marker_lines(), 1)
        self.assertIn("verified; skipping", result.stdout)
        self.assertEqual(self.read_receipt()["head"], head_before)

    def test_receipt_copied_under_another_key_is_rejected(self):
        self.assertEqual(self.run_gate("ok").returncode, 0)
        first_path = self.receipts()[0]
        first_key = json.loads(first_path.read_text())["key"]
        (self.repo / "untracked.txt").write_text("new\n")
        before = self.marker_lines()
        second = self.run_gate("ok")
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(self.marker_lines(), before + 1)
        second_path = next(
            path for path in self.receipts() if path != first_path
        )
        second_key = json.loads(second_path.read_text())["key"]
        self.assertNotEqual(first_key, second_key)
        forged = second_path.read_text().replace(second_key, first_key)
        second_path.write_text(forged)
        try:
            result = self.run_gate("ok")
        finally:
            second_path.unlink()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.marker_lines(), before + 2)
        self.assertIn("is for a different key", result.stdout)

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
        # The gate command resolves through PATH on the shared bin dir, so the
        # clone run uses the same argv to match the key; the second session
        # carries different POHUNEK_* state.
        self.bin.joinpath("ok").write_text(
            "#!/bin/sh\n" f'echo ran >> "{marker}"\n' "exit 0\n"
        )
        env = dict(
            self.env,
            POHUNEK_SESSION_ID="s-clone",
            POHUNEK_WORKER_ID="worker-clone",
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
