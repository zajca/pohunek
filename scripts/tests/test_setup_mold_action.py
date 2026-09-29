"""Behavior checks for the setup-mold composite action (stdlib only).

The action's `run` script is extracted from action.yml and executed with a
`curl` shim that redirects the pinned GitHub release URL to a local server and
keeps every retry flag the script passes, so the real curl retry logic runs
against connections that reset mid-transfer.
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
import unittest
from pathlib import Path

ACTION = (
    Path(__file__).resolve().parents[2]
    / ".github"
    / "actions"
    / "setup-mold"
    / "action.yml"
)
MOLD_VERSION = "9.9.9"
MOLD_BANNER = "mold-test-banner"
RELEASE_URL_PREFIX = "https://github.com/rui314/mold/releases/download/"
# Flags that only make sense against the real HTTPS release CDN.
TLS_FLAGS_WITH_VALUE = {"--proto"}
TLS_FLAGS = {"--tlsv1.2"}
# Upper bound for one script run; with a delay of 0 curl uses its own short backoff.
SCRIPT_TIMEOUT_SECONDS = 60
# How often the server loop re-checks its stop flag while waiting for a client.
ACCEPT_POLL_SECONDS = 0.05


def action_run_script() -> str:
    """Return the body of the action's single `run: |` block, dedented."""
    text = ACTION.read_text()
    match = re.search(r"^ {6}run: \|\n((?: {8}[^\n]*\n|\n)+)", text, re.M)
    if match is None:
        raise AssertionError(f"`run: |` block not found in {ACTION}")
    return "".join(line[8:] if line.strip() else line for line in match.group(1).splitlines(True))


def action_env(name: str) -> str:
    """Return the quoted value of an `env:` entry of the install step."""
    match = re.search(rf'^ {{8}}{re.escape(name)}: "([^"]*)"$', ACTION.read_text(), re.M)
    if match is None:
        raise AssertionError(f"env {name} not found in {ACTION}")
    return match.group(1)


def mold_archive() -> bytes:
    """Build a tarball with the layout of a mold release (`<top>/bin/mold`)."""
    buffer = io.BytesIO()
    script = f"#!/bin/sh\necho {MOLD_BANNER}\n".encode()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        info = tarfile.TarInfo(f"mold-{MOLD_VERSION}-x86_64-linux/bin/mold")
        info.size = len(script)
        info.mode = 0o755
        archive.addfile(info, io.BytesIO(script))
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


class SetupMoldActionTests(unittest.TestCase):
    def run_action(self, server: ReleaseServer, sha256: str):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            shims = tmp / "shims"
            shims.mkdir()
            real_curl = subprocess.run(
                ["sh", "-c", "command -v curl"], capture_output=True, text=True, check=True
            ).stdout.strip()
            curl = shims / "curl"
            curl.write_text(
                CURL_SHIM.format(
                    value_flags=TLS_FLAGS_WITH_VALUE,
                    flags=TLS_FLAGS,
                    prefix=RELEASE_URL_PREFIX,
                    curl=real_curl,
                )
            )
            curl.chmod(0o755)
            uname = shims / "uname"
            uname.write_text("#!/bin/sh\necho x86_64\n")
            uname.chmod(0o755)
            runner_temp = tmp / "runner"
            runner_temp.mkdir()
            github_path = tmp / "github_path"
            env = {
                "PATH": f"{shims}:{os.environ['PATH']}",
                "RUNNER_TEMP": str(runner_temp),
                "GITHUB_PATH": str(github_path),
                "MOLD_VERSION": MOLD_VERSION,
                "MOLD_SHA256": sha256,
                "DOWNLOAD_RETRIES": action_env("DOWNLOAD_RETRIES"),
                "DOWNLOAD_RETRY_DELAY_SECONDS": "0",
                "TEST_SERVER_PORT": str(server.port),
            }
            result = subprocess.run(
                ["bash", "-c", action_run_script()],
                env=env,
                capture_output=True,
                text=True,
                timeout=SCRIPT_TIMEOUT_SECONDS,
            )
            return result, (runner_temp / "mold" / "bin" / "mold").exists()

    def test_connection_resets_are_retried(self):
        body = mold_archive()
        with ReleaseServer(body, resets=2) as server:
            result, installed = self.run_action(server, hashlib.sha256(body).hexdigest())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(MOLD_BANNER, result.stdout)
        self.assertTrue(installed)
        self.assertEqual(server.connections, 3)

    def test_hash_mismatch_fails_before_extraction(self):
        body = mold_archive()
        with ReleaseServer(body, resets=0) as server:
            result, installed = self.run_action(server, "0" * 64)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(installed)


if __name__ == "__main__":
    unittest.main()
