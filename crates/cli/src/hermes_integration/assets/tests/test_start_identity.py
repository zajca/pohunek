"""Stdlib-only checks for the hook process-start identity on the host platform."""

from __future__ import annotations

import importlib.util
import json
import os
import socket
import struct
import sys
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock


HOOKS_PATH = Path(__file__).parents[1] / "pohunek" / "hooks.py"
SPEC = importlib.util.spec_from_file_location("pohunek_hooks_under_test", HOOKS_PATH)
assert SPEC is not None and SPEC.loader is not None
hooks = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = hooks
SPEC.loader.exec_module(hooks)
HookReporter = hooks.HookReporter


# `struct proc_bsdinfo` as returned by `proc_pidinfo(PROC_PIDTBSDINFO)` for pid
# 4242: `pbi_start_tvsec` 1_700_000_000 and `pbi_start_tvusec` 123_456. The
# Rust Darwin backend encodes the same start time as 1_700_000_000_123_456.
_BSDINFO_PID_4242 = bytes.fromhex(
    "0040000002000000000000009210000001000000f501000014000000f5010000"
    "14000000f50100001400000000000000707974686f6e33000000000000000000"
    "707974686f6e3300000000000000000000000000000000000000000000000000"
    "050000009210000001000000ffffffffffffffff0000000000f1536500000000"
    "40e2010000000000"
)


def _host_start_identity() -> int:
    """Derive this process's start identity independently of the plugin."""
    if sys.platform == "darwin":
        import ctypes

        libsystem = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
        buffer = ctypes.create_string_buffer(136)
        assert libsystem.proc_pidinfo(os.getpid(), 3, ctypes.c_uint64(0), buffer, 136) == 136
        seconds, microseconds = struct.unpack_from("=QQ", buffer, 120)
        return seconds * 1_000_000 + microseconds
    with open("/proc/self/stat", encoding="ascii") as handle:
        return int(handle.read().rsplit(")", 1)[1].split()[19])


class StartIdentityTests(unittest.TestCase):
    def test_darwin_layout_decodes_the_rust_backend_encoding(self) -> None:
        self.assertEqual(len(_BSDINFO_PID_4242), 136)
        self.assertEqual(hooks._darwin_start_identity_from_bsdinfo(_BSDINFO_PID_4242, 4242), 1_700_000_000_123_456)

    def test_darwin_layout_rejects_foreign_pid_short_buffer_and_bad_microseconds(self) -> None:
        self.assertIsNone(hooks._darwin_start_identity_from_bsdinfo(_BSDINFO_PID_4242, 4243))
        self.assertIsNone(hooks._darwin_start_identity_from_bsdinfo(_BSDINFO_PID_4242[:-1], 4242))
        self.assertIsNone(hooks._darwin_start_identity_from_bsdinfo(b"", 4242))
        scaled = bytearray(_BSDINFO_PID_4242)
        struct.pack_into("<Q", scaled, 128, 1_000_000)
        self.assertIsNone(hooks._darwin_start_identity_from_bsdinfo(bytes(scaled), 4242))
        overflowing = bytearray(_BSDINFO_PID_4242)
        struct.pack_into("<Q", overflowing, 120, 2**64 - 1)
        self.assertIsNone(hooks._darwin_start_identity_from_bsdinfo(bytes(overflowing), 4242))

    def test_platform_dispatch_selects_the_matching_derivation(self) -> None:
        with mock.patch.object(hooks.sys, "platform", "darwin"), \
                mock.patch.object(hooks, "_darwin_process_start_identity", return_value=7) as darwin, \
                mock.patch.object(hooks, "_linux_process_start_identity", return_value=9) as linux:
            self.assertEqual(hooks._process_start_identity(1), 7)
        darwin.assert_called_once_with(1)
        linux.assert_not_called()
        with mock.patch.object(hooks.sys, "platform", "linux"), \
                mock.patch.object(hooks, "_darwin_process_start_identity", return_value=7) as darwin, \
                mock.patch.object(hooks, "_linux_process_start_identity", return_value=9) as linux:
            self.assertEqual(hooks._process_start_identity(1), 9)
        linux.assert_called_once_with(1)
        darwin.assert_not_called()

    def test_host_derivation_matches_independent_computation(self) -> None:
        self.assertEqual(hooks._process_start_identity(os.getpid()), _host_start_identity())

    def test_real_hook_reports_the_host_start_identity_over_a_unix_socket(self) -> None:
        received: list[dict[str, object]] = []
        # The directory follows TMPDIR, which the Rust driver points at a short
        # private path so the socket path stays inside the Unix `sun_path` limit.
        with tempfile.TemporaryDirectory() as directory:
            endpoint = str(Path(directory) / "w.sock")
            server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            server.bind(endpoint)
            server.listen(1)
            server.settimeout(5)

            def serve() -> None:
                client, _ = server.accept()
                with client:
                    received.append(json.loads(client.recv(4096).splitlines()[0]))
                    client.sendall(b'{"ok":true}\n')

            thread = threading.Thread(target=serve, daemon=True)
            thread.start()
            reporter = HookReporter({
                "POHUNEK_ENV": "1", "POHUNEK_SESSION_ID": "s-1", "POHUNEK_WORKER_INSTANCE_ID": "r-1",
                "POHUNEK_WORKER_SOCKET_PATH": endpoint, "POHUNEK_PROTOCOL_VERSION": "1",
                "POHUNEK_HOOK_TIMEOUT_MS": "1000",
            })
            self.assertTrue(reporter.active)
            reporter.on_session_start({"session_id": "native"})
            thread.join(timeout=5)
            server.close()
        self.assertEqual(reporter.failures, 0)
        self.assertEqual(received[0]["type"], "identity_report")
        self.assertEqual(received[0]["pid"], os.getpid())
        self.assertEqual(received[0]["start_identity"], _host_start_identity())



if __name__ == "__main__":
    unittest.main()
