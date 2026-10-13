"""Behavior checks for the setup-sccache composite action (stdlib only).

The `seed` and `export` steps of action.yml are extracted and executed with a
`curl` shim that redirects the pinned sccache release URL to a local server
and keeps every retry flag the script passes, so the real curl retry logic
runs against connections that reset mid-transfer. The sccache-action step
itself is a remote action and is not run here; the export step checks the
path it leaves in SCCACHE_PATH.
"""

import hashlib
import io
import os
import re
import socket
import struct
import subprocess
import tarfile
import tempfile
import threading
import time
import unittest
from pathlib import Path

ACTION = (
    Path(__file__).resolve().parents[2]
    / ".github"
    / "actions"
    / "setup-sccache"
    / "action.yml"
)
SCCACHE_VERSION = "9.9.9"
SCCACHE_BANNER = "sccache-test-banner"
ARCHIVE_NAME = f"sccache-v{SCCACHE_VERSION}-x86_64-unknown-linux-musl"
RELEASE_URL_PREFIX = "https://github.com/mozilla/sccache/releases/download/"
# Flags that only make sense against the real HTTPS release CDN.
TLS_FLAGS_WITH_VALUE = {"--proto"}
TLS_FLAGS = {"--tlsv1.2"}
# Upper bound for one script run; with a delay of 0 curl uses its own short backoff.
SCRIPT_TIMEOUT_SECONDS = 60
# How often the server loop re-checks its stop flag while waiting for a client.
ACCEPT_POLL_SECONDS = 0.05
# Transfer limits the tests substitute for the action's production values, so a
# stalled transfer ends in about a second. Each name is an env value of the seed step.
TRANSFER_LIMITS_FOR_TESTS = {
    "DOWNLOAD_CONNECT_TIMEOUT_SECONDS": "1",
    "DOWNLOAD_STALL_BYTES_PER_SECOND": "1024",
    "DOWNLOAD_STALL_SECONDS": "1",
    "DOWNLOAD_MAX_TIME_SECONDS": "2",
    "DOWNLOAD_RETRY_MAX_TIME_SECONDS": "3",
}
# A stalled run ends after at most retry-max-time plus max-time; this bound
# leaves room for process start-up and is far below SCRIPT_TIMEOUT_SECONDS.
STALL_RUN_BOUND_SECONDS = 20
# Content-Length the stalling server declares; it never sends that many bytes.
STALL_DECLARED_LENGTH = 1048576
# A `run: |` block of an action step: its body is indented by eight spaces.
RUN_BLOCK = re.compile(r"^ {6}run: \|\n((?: {8}[^\n]*\n|\n)+)", re.M)


def step_run_script(step_id: str) -> str:
    """Return the dedented `run: |` body of the step whose `id` is `step_id`."""
    text = ACTION.read_text()
    anchor = re.search(rf"^ {{6}}id: {re.escape(step_id)}$", text, re.M)
    if anchor is None:
        raise AssertionError(f"step id {step_id} not found in {ACTION}")
    match = RUN_BLOCK.search(text, anchor.end())
    if match is None:
        raise AssertionError(f"`run: |` block of step {step_id} not found")
    return "".join(line[8:] if line.strip() else line for line in match.group(1).splitlines(True))


def action_env(name: str) -> str:
    """Return the quoted value of an `env:` entry of the seed step."""
    match = re.search(rf'^ {{8}}{re.escape(name)}: "([^"]*)"$', ACTION.read_text(), re.M)
    if match is None:
        raise AssertionError(f"env {name} not found in {ACTION}")
    return match.group(1)


def sccache_archive() -> bytes:
    """Build a tarball with the layout of an sccache release (`<top>/sccache`)."""
    buffer = io.BytesIO()
    script = f"#!/bin/sh\necho {SCCACHE_BANNER}\n".encode()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        for name, data, mode in (
            (f"{ARCHIVE_NAME}/sccache", script, 0o755),
            (f"{ARCHIVE_NAME}/README.md", b"readme\n", 0o644),
            (f"{ARCHIVE_NAME}/LICENSE", b"license\n", 0o644),
        ):
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = mode
            archive.addfile(info, io.BytesIO(data))
    return buffer.getvalue()


class ReleaseServer:
    """Serves one archive after resetting the first `resets` connections."""

    def __init__(self, body: bytes, resets: int):
        self.body = body
        self.resets = resets
        self.connections = 0
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen()
        self.listener.settimeout(ACCEPT_POLL_SECONDS)
        self.stopped = threading.Event()
        self.port = self.listener.getsockname()[1]
        self.thread = threading.Thread(target=self._serve, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.stopped.set()
        self.thread.join(timeout=SCRIPT_TIMEOUT_SECONDS)
        self.listener.close()

    def _serve(self):
        while not self.stopped.is_set():
            try:
                connection, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            self.connections += 1
            with connection:
                connection.recv(65536)
                if self.connections <= self.resets:
                    # SO_LINGER {on, 0} makes close() send RST instead of FIN.
                    connection.setsockopt(
                        socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0)
                    )
                    continue
                head = (
                    "HTTP/1.1 200 OK\r\n"
                    f"Content-Length: {len(self.body)}\r\n"
                    "Connection: close\r\n\r\n"
                ).encode()
                connection.sendall(head + self.body)


CURL_SHIM = """#!/usr/bin/env python3
import os
import sys

args = sys.argv[1:]
out = []
skip = False
for arg in args:
    if skip:
        skip = False
    elif arg in {value_flags!r}:
        skip = True
    elif arg in {flags!r}:
        pass
    elif arg.startswith({prefix!r}):
        out.append("http://127.0.0.1:" + os.environ["TEST_SERVER_PORT"] + "/" + arg[len({prefix!r}):])
    else:
        out.append(arg)
os.execv({curl!r}, [{curl!r}] + out)
"""


class StallingServer:
    """Accepts every connection and then stalls it.

    With `partial` None the server sends nothing. Otherwise it sends headers
    declaring STALL_DECLARED_LENGTH bytes followed by `partial`, then stops.
    Connections stay open until the server exits, so only the client's own
    limits can end an attempt.
    """

    def __init__(self, partial: bytes | None):
        self.partial = partial
        self.connections = 0
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen()
        self.listener.settimeout(ACCEPT_POLL_SECONDS)
        self.stopped = threading.Event()
        self.port = self.listener.getsockname()[1]
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.held: list[socket.socket] = []
        self.handlers: list[threading.Thread] = []

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.stopped.set()
        self.thread.join(timeout=SCRIPT_TIMEOUT_SECONDS)
        self.listener.close()
        for connection in self.held:
            connection.close()
        for handler in self.handlers:
            handler.join(timeout=SCRIPT_TIMEOUT_SECONDS)

    def _serve(self):
        while not self.stopped.is_set():
            try:
                connection, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            self.connections += 1
            self.held.append(connection)
            handler = threading.Thread(target=self._hold, args=(connection,), daemon=True)
            self.handlers.append(handler)
            handler.start()

    def _hold(self, connection: socket.socket):
        try:
            connection.recv(65536)
            if self.partial is not None:
                head = (
                    "HTTP/1.1 200 OK\r\n"
                    f"Content-Length: {STALL_DECLARED_LENGTH}\r\n"
                    "Connection: close\r\n\r\n"
                ).encode()
                connection.sendall(head + self.partial)
            self.stopped.wait()
        except OSError:
            return


class Sandbox:
    """A temp directory with the shims, runner paths and output files of one run."""

    def __init__(self, tmp: Path):
        self.tmp = tmp
        self.shims = tmp / "shims"
        self.shims.mkdir()
        self.runner_temp = tmp / "runner-temp"
        self.runner_temp.mkdir()
        self.tool_cache = tmp / "tool-cache"
        self.tool_cache.mkdir()
        self.github_output = tmp / "github_output"
        self.github_env = tmp / "github_env"
        self.github_output.touch()
        self.github_env.touch()
        real_curl = subprocess.run(
            ["sh", "-c", "command -v curl"], capture_output=True, text=True, check=True
        ).stdout.strip()
        curl = self.shims / "curl"
        curl.write_text(
            CURL_SHIM.format(
                value_flags=TLS_FLAGS_WITH_VALUE,
                flags=TLS_FLAGS,
                prefix=RELEASE_URL_PREFIX,
                curl=real_curl,
            )
        )
        curl.chmod(0o755)
        uname = self.shims / "uname"
        uname.write_text("#!/bin/sh\necho x86_64\n")
        uname.chmod(0o755)

    def env(self, **extra: str) -> dict[str, str]:
        base = {
            "PATH": f"{self.shims}:{os.environ['PATH']}",
            "RUNNER_TEMP": str(self.runner_temp),
            "RUNNER_TOOL_CACHE": str(self.tool_cache),
            "GITHUB_OUTPUT": str(self.github_output),
            "GITHUB_ENV": str(self.github_env),
        }
        base.update(extra)
        return base

    def run(self, script: str, env: dict[str, str]) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", "-c", script],
            env=env,
            capture_output=True,
            text=True,
            timeout=SCRIPT_TIMEOUT_SECONDS,
        )

    @property
    def binary(self) -> Path:
        return self.tool_cache / "sccache" / SCCACHE_VERSION / "x64" / "sccache"

    @property
    def complete_marker(self) -> Path:
        return self.tool_cache / "sccache" / SCCACHE_VERSION / "x64.complete"

    def outputs(self) -> str:
        return self.github_output.read_text()


def seed_env(sandbox: Sandbox, server, sha256: str, **extra: str) -> dict[str, str]:
    env = sandbox.env(
        SCCACHE_VERSION=SCCACHE_VERSION,
        SCCACHE_SHA256=sha256,
        DOWNLOAD_RETRIES=action_env("DOWNLOAD_RETRIES"),
        DOWNLOAD_RETRY_DELAY_SECONDS="0",
        TEST_SERVER_PORT=str(server.port),
    )
    env.update(TRANSFER_LIMITS_FOR_TESTS)
    env.update(extra)
    return env


class SetupSccacheActionTests(unittest.TestCase):
    def seed(self, server: ReleaseServer, sha256: str, **extra: str):
        with tempfile.TemporaryDirectory() as tmp:
            sandbox = Sandbox(Path(tmp))
            result = sandbox.run(step_run_script("seed"), seed_env(sandbox, server, sha256, **extra))
            return result, sandbox.outputs(), sandbox.binary, sandbox.complete_marker, Path(tmp)

    def test_verified_copy_is_seeded_in_the_tool_cache_layout(self):
        body = sccache_archive()
        with tempfile.TemporaryDirectory() as tmp:
            sandbox = Sandbox(Path(tmp))
            with ReleaseServer(body, resets=2) as server:
                result = sandbox.run(
                    step_run_script("seed"),
                    seed_env(sandbox, server, hashlib.sha256(body).hexdigest()),
                )
                connections = server.connections
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(connections, 3)
            self.assertIn("ok=true", sandbox.outputs())
            self.assertTrue(os.access(sandbox.binary, os.X_OK))
            self.assertTrue(sandbox.complete_marker.is_file())
            # The marker is a sibling of the directory the action's find() reads.
            self.assertEqual(sorted(p.name for p in sandbox.complete_marker.parent.iterdir()), ["x64", "x64.complete"])
            run = subprocess.run([str(sandbox.binary)], capture_output=True, text=True, check=True)
            self.assertIn(SCCACHE_BANNER, run.stdout)

    def test_persistent_download_failure_falls_back_without_a_marker(self):
        body = sccache_archive()
        # The overall limit is raised so that the retry count, not the time
        # budget, decides how many attempts are made.
        with ReleaseServer(body, resets=10**6) as server:
            result, outputs, binary, marker, _ = self.seed(
                server, hashlib.sha256(body).hexdigest(), DOWNLOAD_RETRY_MAX_TIME_SECONDS="60"
            )
            connections = server.connections
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("::warning::setup-sccache: download of", result.stdout + result.stderr)
        self.assertIn("builds without sccache", result.stdout + result.stderr)
        self.assertIn("ok=false", outputs)
        self.assertFalse(binary.exists())
        self.assertFalse(marker.exists())
        # One attempt plus the bounded retries, never more.
        self.assertEqual(connections, int(action_env("DOWNLOAD_RETRIES")) + 1)

    def test_checksum_mismatch_falls_back_before_extraction(self):
        body = sccache_archive()
        with tempfile.TemporaryDirectory() as tmp:
            sandbox = Sandbox(Path(tmp))
            with ReleaseServer(body, resets=0) as server:
                result = sandbox.run(step_run_script("seed"), seed_env(sandbox, server, "0" * 64))
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("::warning::setup-sccache: sha256 of", result.stdout + result.stderr)
            self.assertIn("ok=false", sandbox.outputs())
            self.assertFalse((sandbox.runner_temp / "sccache-download" / "extract").exists())
            self.assertFalse(sandbox.binary.exists())
            self.assertFalse(sandbox.complete_marker.exists())

    def test_misconfigured_inputs_fail_the_job(self):
        body = sccache_archive()
        digest = hashlib.sha256(body).hexdigest()
        cases = {
            "empty sha256": {"SCCACHE_SHA256": ""},
            "version with leading v": {"SCCACHE_VERSION": f"v{SCCACHE_VERSION}"},
            "uppercase sha256": {"SCCACHE_SHA256": digest.upper()},
        }
        for label, override in cases.items():
            with self.subTest(case=label), tempfile.TemporaryDirectory() as tmp:
                sandbox = Sandbox(Path(tmp))
                with ReleaseServer(body, resets=0) as server:
                    env = seed_env(sandbox, server, digest)
                    env.update(override)
                    result = sandbox.run(step_run_script("seed"), env)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("::error::setup-sccache", result.stderr)
                self.assertEqual(sandbox.outputs(), "")
                self.assertFalse(sandbox.binary.exists())


class TransferLimitTests(unittest.TestCase):
    def assert_stalled_run_falls_back(self, partial: bytes | None):
        digest = hashlib.sha256(sccache_archive()).hexdigest()
        with tempfile.TemporaryDirectory() as tmp:
            sandbox = Sandbox(Path(tmp))
            with StallingServer(partial) as server:
                started = time.monotonic()
                result = sandbox.run(step_run_script("seed"), seed_env(sandbox, server, digest))
                elapsed = time.monotonic() - started
            output = result.stdout + result.stderr
            self.assertEqual(result.returncode, 0, output)
            self.assertLess(elapsed, STALL_RUN_BOUND_SECONDS)
            self.assertIn("::warning::setup-sccache: download of", output)
            self.assertIn("ok=false", sandbox.outputs())
            self.assertFalse(sandbox.binary.exists())
            self.assertFalse(sandbox.complete_marker.exists())
            # The timeout (curl exit 28) is retried, not treated as final.
            self.assertGreaterEqual(server.connections, 2)

    def test_server_that_never_sends_headers_falls_back(self):
        self.assert_stalled_run_falls_back(None)

    def test_server_that_stalls_mid_body_falls_back(self):
        self.assert_stalled_run_falls_back(sccache_archive()[:1000])

    def test_limits_are_named_env_values_passed_to_curl(self):
        script = step_run_script("seed")
        for name in TRANSFER_LIMITS_FOR_TESTS:
            with self.subTest(name=name):
                self.assertRegex(ACTION.read_text(), rf"(?m)^ {{8}}{name}: \"[0-9]+\"$")
                self.assertIn(f'"${name}"', script)
        for flag in ("--connect-timeout", "--speed-limit", "--speed-time", "--max-time", "--retry-max-time"):
            self.assertIn(flag, script)


class ExportSccacheSettingsTests(unittest.TestCase):
    def test_wrapper_is_exported_when_the_verified_binary_is_resolved(self):
        with tempfile.TemporaryDirectory() as tmp:
            sandbox = Sandbox(Path(tmp))
            expected = sandbox.tool_cache / "sccache" / SCCACHE_VERSION / "x64" / "sccache"
            result = sandbox.run(
                step_run_script("export"),
                sandbox.env(SCCACHE_VERSION=SCCACHE_VERSION, SCCACHE_PATH=str(expected)),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                sandbox.github_env.read_text().splitlines(),
                ["SCCACHE_GHA_ENABLED=true", "RUSTC_WRAPPER=sccache"],
            )

    def test_a_different_resolved_binary_is_refused_without_exporting_the_wrapper(self):
        with tempfile.TemporaryDirectory() as tmp:
            sandbox = Sandbox(Path(tmp))
            result = sandbox.run(
                step_run_script("export"),
                sandbox.env(SCCACHE_VERSION=SCCACHE_VERSION, SCCACHE_PATH="/usr/local/bin/sccache"),
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("::error::setup-sccache", result.stderr)
            self.assertNotIn("RUSTC_WRAPPER", sandbox.github_env.read_text())


if __name__ == "__main__":
    unittest.main()
