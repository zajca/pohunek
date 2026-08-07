"""Stdlib-only checks for the embedded Hermes plugin runtime."""

from __future__ import annotations

import importlib.util
import base64
import json
import os
import subprocess
import sys
import tempfile
import time
import unicodedata
import unittest
from pathlib import Path
from unittest import mock


ASSETS = Path(__file__).parents[1]
PACKAGE = ASSETS / "pohunek"
SPEC = importlib.util.spec_from_file_location(
    "pohunek", PACKAGE / "__init__.py", submodule_search_locations=[str(PACKAGE)]
)
assert SPEC is not None and SPEC.loader is not None
PLUGIN = importlib.util.module_from_spec(SPEC)
sys.modules["pohunek"] = PLUGIN
PLUGIN.__dict__["__POHUNEK_POLICY_PATH__"] = "/tmp/pohunek-plugin-test-policy.json"
SPEC.loader.exec_module(PLUGIN)

from pohunek.cli import CliError, CliRunner, Invocation, _minimal_env, _stdout_wire_cap
from pohunek.hooks import HookReporter
from pohunek.policy import POLICY_PATH_TOKEN, Policy, PolicyError, load_policy
from pohunek.redact import diagnostic
from pohunek.tools import TOOL_SCHEMAS, Tools, _MAX_INPUT_BYTES, _input


def policy(
    cli: str = "/bin/true",
    mode: str = "full",
    max_output_bytes: int = 1024,
    max_screen_bytes: int = 1024,
) -> Policy:
    return Policy(cli, 1, 3, mode, frozenset(("local",)), 100, max_output_bytes, max_screen_bytes, 1)


class PolicyTests(unittest.TestCase):
    def test_token_and_extra_fields_fail_closed(self) -> None:
        with self.assertRaises(PolicyError):
            load_policy(POLICY_PATH_TOKEN)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "policy.json"
            path.write_text(json.dumps({"unexpected": True}), encoding="utf-8")
            path.chmod(0o600)
            with self.assertRaises(PolicyError):
                load_policy(str(path))

    def test_policy_requires_owner_private_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "policy.json"
            path.write_text("{}", encoding="utf-8")
            path.chmod(0o644)
            with self.assertRaises(PolicyError):
                load_policy(str(path))

    def test_policy_rejects_symlink_ip_path_and_control_hosts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "target.json"
            target.write_text("{}", encoding="utf-8")
            target.chmod(0o600)
            link = Path(directory) / "policy.json"
            link.symlink_to(target)
            with self.assertRaises(PolicyError):
                load_policy(str(link))
        for host in ("127.0.0.1", "/tmp/socket", "bad\nhost"):
            raw = {"schema_version": 1, "pohunek_cli": "/bin/true", "protocol_min": 1, "protocol_max": 1, "access_mode": "read_only", "allowed_hosts": [host], "tool_timeout_ms": 1, "max_output_bytes": 1, "max_screen_bytes": 1, "max_concurrency": 1}
            with mock.patch("pohunek.policy.os.path.isfile", return_value=True), mock.patch("pohunek.policy.os.access", return_value=True):
                with self.assertRaises(PolicyError):
                    __import__("pohunek.policy", fromlist=["_validate"])._validate(raw)

    def test_policy_rejects_boolean_numeric_fields(self) -> None:
        raw = {"schema_version": 1, "pohunek_cli": "/bin/true", "protocol_min": True, "protocol_max": 1, "access_mode": "read_only", "allowed_hosts": ["local"], "tool_timeout_ms": 1, "max_output_bytes": 1, "max_screen_bytes": 1, "max_concurrency": 1}
        with mock.patch("pohunek.policy.os.path.isfile", return_value=True), mock.patch("pohunek.policy.os.access", return_value=True):
            with self.assertRaises(PolicyError):
                __import__("pohunek.policy", fromlist=["_validate"])._validate(raw)


class RunnerTests(unittest.TestCase):
    @staticmethod
    def _controlled_cli(directory: Path, payload: bytes, sleep_seconds: float = 0) -> tuple[str, Path]:
        executable = directory / "controlled-cli.py"
        pid_file = directory / "controlled-cli.pid"
        executable.write_text(
            "\n".join((
                f"#!{sys.executable}",
                "import os",
                "from pathlib import Path",
                "import sys",
                "import time",
                f"Path({str(pid_file)!r}).write_text(str(os.getpid()), encoding='ascii')",
                f"sys.stdout.buffer.write({payload!r})",
                "sys.stdout.buffer.flush()",
                f"time.sleep({sleep_seconds!r})",
            )),
            encoding="utf-8",
        )
        executable.chmod(0o700)
        return str(executable), pid_file

    def test_stdout_wire_cap_accounts_for_encoded_and_escaped_payloads(self) -> None:
        bounded = policy(max_output_bytes=10, max_screen_bytes=20)
        expected = max(
            6 * 1024 * 1024,  # Pinned typed pretty-JSON expansion floor.
            16,  # ceil(10 / 3) base64 groups times four bytes
            10 * 6,  # Every output byte may serialize as a JSON \\u00XX escape.
            20 * 6,  # Screen text has the same JSON-escape worst case.
        ) + 16 * 1024
        self.assertEqual(_stdout_wire_cap(bounded), expected)

    def test_collection_accepts_exact_wire_cap_and_rejects_one_byte_over_with_cleanup(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bounded = policy(max_output_bytes=64, max_screen_bytes=32)
            cap = _stdout_wire_cap(bounded)
            self.assertGreater(cap, 2 * 64 * 1024)
            exact_cli, _exact_pid = self._controlled_cli(root, b"x" * cap)
            runner = CliRunner(policy(exact_cli, max_output_bytes=64, max_screen_bytes=32))
            process = subprocess.Popen(
                [exact_cli], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                text=False, start_new_session=True,
            )
            with mock.patch("pohunek.cli.os.read", wraps=os.read) as read:
                stdout, stderr = runner._collect(process, b"")
            self.assertEqual(stdout, b"x" * cap)
            self.assertEqual(stderr, b"")
            self.assertGreater(read.call_count, 2)
            self.assertEqual(process.wait(timeout=1), 0)

            over_cli, over_pid = self._controlled_cli(root, b"x" * (cap + 1), sleep_seconds=10)
            over_runner = CliRunner(policy(over_cli, max_output_bytes=64, max_screen_bytes=32))
            with self.assertRaises(CliError) as raised:
                over_runner.run(Invocation(("ignored",)))
            self.assertEqual(raised.exception.code, "plugin_output_limit_exceeded")
            process_id = int(over_pid.read_text(encoding="ascii"))
            with self.assertRaises(ProcessLookupError):
                os.kill(process_id, 0)

    def test_low_policy_accepts_session_info_with_full_valid_metadata_map(self) -> None:
        metadata = {
            "m0": "x" * 4096,
            "m1": "x" * 4096,
            "m2": "x" * 4096,
            "m3": "x" * 4063,
        }
        self.assertEqual(len(json.dumps(metadata, separators=(",", ":")).encode("utf-8")), 16 * 1024)
        session = {
            "id": "s-42",
            "external": False,
            "capabilities": {"resume": False, "fork": False},
            "agent": "shell",
            "agent_base": "shell",
            "cwd": "/workspace/project",
            "cwd_source": "launch",
            "pid": 42,
            "cols": 80,
            "rows": 24,
            "state": "running",
            "state_source": "process",
            "warnings": [],
            "metadata": metadata,
            "created_at": "2026-08-06T00:00:00Z",
            "updated_at": "2026-08-06T00:00:00Z",
        }
        response = json.dumps({
            "cli_version": "test",
            "protocol": {"minimum": 1, "maximum": 3},
            "ok": session,
        }, separators=(",", ":")).encode("utf-8")
        self.assertGreater(len(response), 16 * 1024)
        low_policy = policy(max_output_bytes=1, max_screen_bytes=1)
        self.assertLessEqual(len(response), _stdout_wire_cap(low_policy))
        with tempfile.TemporaryDirectory() as directory:
            executable, _pid_file = self._controlled_cli(Path(directory), response)
            runner = CliRunner(policy(executable, max_output_bytes=1, max_screen_bytes=1))
            self.assertEqual(runner.run(Invocation(("session", "inspect", "s-42", "--json"))), session)

    def test_low_policy_accepts_pretty_session_info_collection_near_control_line_limit(self) -> None:
        sessions = [{
            "id": f"s-{index:04d}",
            "capabilities": {"resume": False, "fork": False},
            "agent": "shell",
            "agent_base": "shell",
            "cwd": "/w",
            "pid": 1,
            "cols": 1,
            "rows": 1,
            "state": "running",
            "state_source": "process",
            "metadata": {"m": "x" * 60},
            "created_at": "2026-08-06T00:00:00Z",
            "updated_at": "2026-08-06T00:00:00Z",
        } for index in range(3000)]
        daemon_document = json.dumps(
            {"v": 2, "id": "r", "ok": sessions}, separators=(",", ":"),
        ).encode("utf-8")
        response = json.dumps({
            "cli_version": "test",
            "protocol": {"minimum": 1, "maximum": 3},
            "ok": sessions,
        }, indent=2).encode("utf-8")
        self.assertLess(len(daemon_document), 1024 * 1024)
        self.assertGreater(len(response), 1024 * 1024)
        low_policy = policy(max_output_bytes=1, max_screen_bytes=1)
        self.assertLessEqual(len(response), _stdout_wire_cap(low_policy))
        with tempfile.TemporaryDirectory() as directory:
            executable, _pid_file = self._controlled_cli(Path(directory), response)
            runner = CliRunner(policy(executable, max_output_bytes=1, max_screen_bytes=1))
            self.assertEqual(runner.run(Invocation(("session", "list", "--json"))), sessions)

    def test_maximum_decoded_base64_response_and_envelope_fit_wire_cap(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            bounded = policy(max_output_bytes=1024, max_screen_bytes=1)
            encoded = base64.b64encode(b"x" * bounded.max_output_bytes).decode("ascii")
            response = json.dumps({
                "cli_version": "test",
                "protocol": {"minimum": 1, "maximum": 3},
                "ok": {"data_base64": encoded},
            }, separators=(",", ":")).encode("utf-8")
            self.assertLessEqual(len(response), _stdout_wire_cap(bounded))
            executable, _pid_file = self._controlled_cli(Path(directory), response)
            runner = CliRunner(policy(executable, max_output_bytes=1024, max_screen_bytes=1))

            self.assertEqual(
                runner.run(Invocation(("session", "output", "--json"))),
                {"data_base64": encoded},
            )

    def test_verify_compatibility_uses_exact_fixed_daemon_free_argv(self) -> None:
        runner = CliRunner(policy())
        output = json.dumps({
            "cli_version": "x",
            "protocol": {"minimum": 1, "maximum": 3},
            "ok": {"overall": "ok"},
        }).encode()
        process = mock.Mock()
        process.wait.return_value = 0
        with mock.patch("pohunek.cli.subprocess.Popen", return_value=process) as popen, mock.patch.object(runner, "_collect", return_value=(output, b"")) as collect:
            runner.verify_compatibility()

        self.assertEqual(popen.call_args.args[0], ["/bin/true", "doctor", "--json"])
        collect.assert_called_once_with(process, b"")

    def test_verify_compatibility_rejects_incompatible_malformed_and_failed_cli(self) -> None:
        cases = (
            (0, json.dumps({"protocol": {"minimum": 4, "maximum": 4}, "ok": {}}).encode(), "pohunek_cli_incompatible"),
            (0, b"not-json", "pohunek_cli_invalid_json"),
            (1, json.dumps({"protocol": {"minimum": 1, "maximum": 3}, "err": {"code": "doctor_failed"}}).encode(), "doctor_failed"),
        )
        for returncode, output, expected in cases:
            with self.subTest(expected=expected):
                runner = CliRunner(policy())
                process = mock.Mock()
                process.wait.return_value = returncode
                with mock.patch("pohunek.cli.subprocess.Popen", return_value=process) as popen, mock.patch.object(runner, "_collect", return_value=(output, b"")):
                    with self.assertRaises(CliError) as raised:
                        runner.verify_compatibility()
                self.assertEqual(raised.exception.code, expected)
                self.assertEqual(popen.call_args.args[0], ["/bin/true", "doctor", "--json"])

    def test_validates_protocol_envelope(self) -> None:
        runner = CliRunner(policy())
        output = json.dumps({"cli_version": "x", "protocol": {"minimum": 2, "maximum": 3}, "ok": {"id": "s"}}).encode()
        process = mock.Mock()
        process.wait.return_value = 0
        with mock.patch("pohunek.cli.subprocess.Popen", return_value=process), mock.patch.object(runner, "_collect", return_value=(output, b"")):
            self.assertEqual(runner.run(Invocation(("session", "list", "--json"))), {"id": "s"})

    def test_runner_uses_closed_fds_and_reaps_on_collection_failure(self) -> None:
        runner = CliRunner(policy())
        process = mock.Mock()
        with mock.patch("pohunek.cli.subprocess.Popen", return_value=process) as popen, mock.patch.object(runner, "_collect", side_effect=CliError("plugin_output_limit_exceeded")), mock.patch.object(runner, "_terminate") as terminate:
            with self.assertRaises(CliError):
                runner.run(Invocation(("session", "list", "--json")))
        self.assertTrue(popen.call_args.kwargs["close_fds"])
        self.assertTrue(popen.call_args.kwargs["start_new_session"])
        terminate.assert_called_once_with(process)

    def test_rejects_incompatible_or_non_json_output(self) -> None:
        runner = CliRunner(policy())
        for output in (b"not-json", json.dumps({"protocol": {"minimum": 4, "maximum": 4}, "ok": {}}).encode()):
            process = mock.Mock()
            process.wait.return_value = 0
            with mock.patch("pohunek.cli.subprocess.Popen", return_value=process), mock.patch.object(runner, "_collect", return_value=(output, b"")):
                with self.assertRaises(CliError):
                    runner.run(Invocation(("session", "list", "--json")))

    def test_nonzero_json_error_is_typed_before_exit_mapping(self) -> None:
        runner = CliRunner(policy())
        output = json.dumps({"protocol": {"minimum": 1, "maximum": 3}, "err": {"code": "agent_fork_unsupported"}}).encode()
        process = mock.Mock()
        process.wait.return_value = 1
        with mock.patch("pohunek.cli.subprocess.Popen", return_value=process), mock.patch.object(runner, "_collect", return_value=(output, b"untrusted stderr")):
            with self.assertRaisesRegex(CliError, "") as raised:
                runner.run(Invocation(("session", "fork", "--json")))
        self.assertEqual(raised.exception.code, "agent_fork_unsupported")

    def test_exit_envelope_consistency_and_null_success(self) -> None:
        runner = CliRunner(policy())
        null_ok = json.dumps({"protocol": {"minimum": 1, "maximum": 1}, "ok": None}).encode()
        process = mock.Mock()
        process.wait.return_value = 0
        with mock.patch("pohunek.cli.subprocess.Popen", return_value=process), mock.patch.object(runner, "_collect", return_value=(null_ok, b"")):
            self.assertIsNone(runner.run(Invocation(("doctor", "--json"))))
        process.wait.return_value = 1
        with mock.patch("pohunek.cli.subprocess.Popen", return_value=process), mock.patch.object(runner, "_collect", return_value=(null_ok, b"")):
            with self.assertRaises(CliError) as raised:
                runner.run(Invocation(("doctor", "--json")))
        self.assertEqual(raised.exception.code, "pohunek_cli_invalid_envelope")

    def test_minimal_env_keeps_only_complete_origin_pair(self) -> None:
        with mock.patch.dict(os.environ, {"POHUNEK_SESSION_ID": "s-1", "POHUNEK_DAEMON_ID": "d-1", "POHUNEK_SOCKET_PATH": "/bad", "SECRET": "no"}, clear=True):
            env = _minimal_env()
        self.assertEqual({key: env[key] for key in ("POHUNEK_SESSION_ID", "POHUNEK_DAEMON_ID")}, {"POHUNEK_SESSION_ID": "s-1", "POHUNEK_DAEMON_ID": "d-1"})
        self.assertNotIn("POHUNEK_SOCKET_PATH", env)
        self.assertNotIn("SECRET", env)

    def test_minimal_env_forwards_only_non_secret_path_roots(self) -> None:
        expected = {
            "HOME": "/controlled/home",
            "XDG_RUNTIME_DIR": "/controlled/runtime",
            "XDG_STATE_HOME": "/controlled/state",
            "XDG_CONFIG_HOME": "/controlled/config",
            "XDG_DATA_HOME": "/controlled/data",
            "XDG_CACHE_HOME": "/controlled/cache",
            "PATH": "/usr/bin:/bin",
        }
        ambient = {
            **{key: value for key, value in expected.items() if key != "PATH"},
            "HTTPS_PROXY": "https://credential.invalid",
            "OPENAI_API_KEY": "secret",
            "POHUNEK_SOCKET_PATH": "/controlled/override.sock",
            "HERMES_HOME": "/controlled/ambient-hermes",
        }
        with mock.patch.dict(os.environ, ambient, clear=True):
            env = _minimal_env()

        self.assertEqual(env, expected)

    def test_minimal_env_omits_missing_empty_and_relative_path_roots(self) -> None:
        ambient = {
            "XDG_RUNTIME_DIR": "",
            "XDG_STATE_HOME": "relative-state",
            "XDG_DATA_HOME": "./relative-data",
            "XDG_CACHE_HOME": "../relative-cache",
        }
        with mock.patch.dict(os.environ, ambient, clear=True):
            env = _minimal_env()

        self.assertEqual(env, {"PATH": "/usr/bin:/bin"})

    def test_redaction_covers_bearer_api_key_and_paths(self) -> None:
        value = diagnostic("Authorization: Bearer abc.def api_key=key /home/me/private/file")
        self.assertNotIn("abc.def", value)
        self.assertNotIn("=key", value)
        self.assertNotIn("/home/me/private/file", value)


class ToolTests(unittest.TestCase):
    def test_access_mode_registration_and_full_only_tools(self) -> None:
        read = Tools(policy(mode="read_only"), None).handlers()
        manage = Tools(policy(mode="manage"), None).handlers()
        full = Tools(policy(mode="full"), None).handlers()
        self.assertEqual(len(read), 7)
        self.assertEqual(len(manage), 14)
        self.assertEqual(len(full), 16)
        self.assertNotIn("pohunek_session_stop", manage)
        self.assertIn("pohunek_session_stop", full)

    def test_verify_cli_uses_shared_runner(self) -> None:
        tools = Tools(policy(), None)
        runner = mock.Mock()
        tools._runner = runner
        tools.verify_cli()
        runner.verify_compatibility.assert_called_once_with()

    def test_wildcard_policy_still_rejects_unsafe_runtime_hosts(self) -> None:
        wildcard = Policy("/bin/true", 1, 3, "full", frozenset(("*",)), 100, 1024, 1024, 1)
        tools = Tools(wildcard, None)
        for host in ("127.0.0.1", "/tmp/socket", "bad\nhost", "*"):
            result = json.loads(tools.handlers()["pohunek_sessions"]({"host": host}))
            self.assertEqual(result["error"]["code"], "plugin_host_denied", host)

    def test_cursor_preconditions_fail_before_runner(self) -> None:
        tools = Tools(policy(), None)
        runner = mock.Mock()
        tools._runner = runner
        calls = [
            ("pohunek_session_output", {"session": "s", "wait_ms": 10}),
            ("pohunek_session_wait", {"session": "s", "timeout_ms": 10, "after_output_offset": "1"}),
            ("pohunek_session_wait", {"session": "s", "timeout_ms": 10, "after_terminal_watermark": "1"}),
        ]
        for name, args in calls:
            result = json.loads(tools.handlers()[name](args))
            self.assertEqual(result["error"]["code"], "plugin_invalid_request")
        runner.run.assert_not_called()

    def test_tool_schemas_are_closed_and_unknown_force_is_rejected(self) -> None:
        def assert_closed(schema: object) -> None:
            if isinstance(schema, dict):
                if schema.get("type") == "object":
                    self.assertIs(schema.get("additionalProperties"), False)
                for value in schema.values():
                    assert_closed(value)
            elif isinstance(schema, list):
                for value in schema:
                    assert_closed(value)
        for schema in __import__("pohunek.tools", fromlist=["TOOL_SCHEMAS"]).TOOL_SCHEMAS.values():
            assert_closed(schema)
        result = json.loads(Tools(policy(), None).handlers()["pohunek_session_stop"]({"session": "s", "force": True}))
        self.assertEqual(result["error"]["code"], "plugin_invalid_request")

    def test_cursor_schemas_use_canonical_decimal_strings(self) -> None:
        output = TOOL_SCHEMAS["pohunek_session_output"]["parameters"]["properties"]
        wait = TOOL_SCHEMAS["pohunek_session_wait"]["parameters"]["properties"]
        expected = {"type": "string", "pattern": "^(0|[1-9][0-9]*)$", "maxLength": 20}
        for schema in (
            output["runtime_generation"], output["after_offset"],
            wait["runtime_generation"], wait["after_terminal_watermark"],
            wait["after_output_offset"],
        ):
            self.assertEqual(schema, expected)
        for schema in (output["max_bytes"], output["wait_ms"], wait["timeout_ms"]):
            self.assertEqual(schema, {"type": "integer"})

    def test_origin_mutation_is_rejected_before_any_runner_call(self) -> None:
        tools = Tools(policy(), "origin")
        runner = mock.Mock()
        tools._runner = runner
        response = json.loads(tools.handlers()["pohunek_session_send"]({"session": "origin", "input": "x"}))
        self.assertEqual(response["error"]["code"], "plugin_self_target_denied")
        runner.run.assert_not_called()

    def test_session_send_passes_newline_input_to_the_fixed_runner_contract(self) -> None:
        tools = Tools(policy(), None)
        runner = mock.Mock()
        runner.run.side_effect = [[{"id": "s-1", "name": "peer"}], {"accepted": True}]
        tools._runner = runner

        shell_input = "printf 'ready'\n"
        response = json.loads(tools.handlers()["pohunek_session_send"]({"session": "peer", "input": shell_input}))

        self.assertTrue(response["ok"])
        invocation = runner.run.call_args.args[0]
        self.assertEqual(
            invocation.argv,
            ("--host", "local", "session", "input", "s-1", "--stdin", "--json"),
        )
        self.assertEqual(invocation.stdin.encode("utf-8"), b"printf 'ready'\n")

    def test_session_send_allows_carriage_return_and_tab_input(self) -> None:
        tools = Tools(policy(), None)
        runner = mock.Mock()
        runner.run.side_effect = [[{"id": "s-1", "name": "peer"}], {"accepted": True}]
        tools._runner = runner

        shell_input = "\tfirst\rsecond\n"
        response = json.loads(tools.handlers()["pohunek_session_send"]({"session": "peer", "input": shell_input}))

        self.assertTrue(response["ok"])
        self.assertEqual(runner.run.call_args.args[0].stdin, shell_input)

    def test_session_send_rejects_disallowed_control_empty_and_oversize_input(self) -> None:
        invalid_inputs = (
            ("empty", ""),
            ("character limit", "x" * (_MAX_INPUT_BYTES + 1)),
            ("UTF-8 byte limit", "é" * ((_MAX_INPUT_BYTES // 2) + 1)),
            ("NUL", "nul\x00"),
            ("C0 start", "start\x01"),
            ("C0 unit separator", "unit\x1f"),
            ("delete", "delete\x7f"),
            ("C1 control", "control\x9f"),
        )
        for case, shell_input in invalid_inputs:
            with self.subTest(case=case):
                tools = Tools(policy(), None)
                runner = mock.Mock()
                tools._runner = runner

                response = json.loads(tools.handlers()["pohunek_session_send"]({"session": "peer", "input": shell_input}))

                self.assertEqual(response["error"]["code"], "plugin_invalid_request")
                runner.run.assert_not_called()

    def test_input_rejects_every_unicode_control_except_terminal_separators(self) -> None:
        disallowed_controls = (
            character
            for codepoint in range(sys.maxunicode + 1)
            if unicodedata.category(character := chr(codepoint)) == "Cc"
            and character not in "\n\r\t"
        )

        rejected = 0
        for character in disallowed_controls:
            with self.subTest(codepoint=f"U+{ord(character):04X}"):
                with self.assertRaises(CliError) as raised:
                    _input(f"before{character}after")
                self.assertEqual(raised.exception.code, "plugin_invalid_request")
                rejected += 1
        self.assertEqual(rejected, 62)

    def test_start_rejects_invalid_initial_input_before_inventory_or_project_lookup(self) -> None:
        invalid_inputs = (
            ("empty", ""),
            ("oversize", "x" * (_MAX_INPUT_BYTES + 1)),
            ("control", "bad\x00"),
        )
        for case, initial_input in invalid_inputs:
            with self.subTest(case=case):
                tools = Tools(policy(), None)
                runner = mock.Mock()
                tools._runner = runner

                response = json.loads(tools.handlers()["pohunek_session_start"]({
                    "agent_profile": "hermes",
                    "project": {"id": "p-1"},
                    "initial_input": initial_input,
                }))

                self.assertEqual(response["error"]["code"], "plugin_invalid_request")
                runner.run.assert_not_called()

    def test_all_eight_origin_mutations_are_rejected_before_any_subprocess(self) -> None:
        tools = Tools(policy(), "origin")
        runner = mock.Mock()
        tools._runner = runner
        calls = {
            "pohunek_session_send": {"session": "origin", "input": "x"},
            "pohunek_session_resume": {"session": "origin"},
            "pohunek_session_fork": {"session": "origin"},
            "pohunek_session_resize": {"session": "origin", "cols": 80, "rows": 24},
            "pohunek_session_rename": {"session": "origin", "name": "new"},
            "pohunek_session_set_metadata": {"session": "origin", "metadata": {"label": "x"}},
            "pohunek_session_stop": {"session": "origin"},
            "pohunek_session_remove": {"session": "origin"},
        }
        for name, args in calls.items():
            response = json.loads(tools.handlers()[name](args))
            self.assertEqual(response["error"]["code"], "plugin_self_target_denied", name)
        runner.run.assert_not_called()

    def test_unique_name_is_resolved_before_mutation(self) -> None:
        tools = Tools(policy(), None)
        runner = mock.Mock()
        runner.run.side_effect = [[{"id": "s-1", "name": "peer"}], {"accepted": True}]
        tools._runner = runner
        response = json.loads(tools.handlers()["pohunek_session_resume"]({"session": "peer"}))
        self.assertTrue(response["ok"])
        self.assertEqual(runner.run.call_args_list[1].args[0].argv[4], "s-1")

    def test_fork_unsupported_is_data(self) -> None:
        tools = Tools(policy(), None)
        runner = mock.Mock()
        runner.run.side_effect = [[{"id": "s-1"}], CliError("agent_fork_unsupported")]
        tools._runner = runner
        response = json.loads(tools.handlers()["pohunek_session_fork"]({"session": "s-1"}))
        self.assertTrue(response["ok"])
        self.assertFalse(response["result"]["fork_supported"])

    def test_qualified_and_missing_session_are_typed_rejections(self) -> None:
        tools = Tools(policy(), None)
        runner = mock.Mock()
        tools._runner = runner
        qualified = json.loads(tools.handlers()["pohunek_session_get"]({"session": "remote/s-1"}))
        self.assertEqual(qualified["error"]["code"], "plugin_invalid_request")
        runner.run.side_effect = [[]]
        missing = json.loads(tools.handlers()["pohunek_session_resume"]({"session": "missing"}))
        self.assertEqual(missing["error"]["code"], "plugin_session_not_found")

    def test_user_controlled_positional_operands_follow_end_of_options(self) -> None:
        tools = Tools(policy(), None)
        runner = mock.Mock()

        def response(invocation: Invocation) -> object:
            command = invocation.argv[3]
            if command == "list":
                return [{"id": "s-1", "name": "peer"}]
            if command == "output":
                return {"data_base64": ""}
            return {}

        runner.run.side_effect = response
        tools._runner = runner
        leading_option = "--host=other.example"
        calls = (
            (tools.session_get, {"session": leading_option}, (leading_option,)),
            (tools.screen, {"session": leading_option}, (leading_option,)),
            (tools.output, {"session": leading_option}, (leading_option,)),
            (tools.wait, {"session": leading_option, "timeout_ms": 10}, (leading_option,)),
            (tools.diff, {"session": leading_option}, (leading_option,)),
            (tools.rename, {"session": "peer", "name": "--json"}, ("s-1", "--json")),
        )
        for function, args, operands in calls:
            with self.subTest(function=function.__name__):
                runner.run.reset_mock()
                function(args)
                argv = runner.run.call_args.args[0].argv
                self.assertEqual(argv[-len(operands) - 1], "--")
                self.assertEqual(argv[-len(operands):], operands)

    def test_hosts_uses_policy_without_discovery(self) -> None:
        tools = Tools(policy(), None)
        runner = mock.Mock()
        tools._runner = runner
        result = json.loads(tools.handlers()["pohunek_hosts"]({}))
        self.assertEqual(result["result"]["hosts"], ["local"])
        runner.run.assert_not_called()

    def test_output_decodes_base64_and_preserves_cursors(self) -> None:
        tools = Tools(policy(), None)
        runner = mock.Mock()
        runner.run.return_value = {"session_id": "s", "runtime_id": "r", "runtime_generation": "1", "next_offset": "4", "gap": None, "data_base64": "aGn/"}
        tools._runner = runner
        result = json.loads(tools.handlers()["pohunek_session_output"]({"session": "s", "max_bytes": 16}))
        self.assertEqual(result["result"]["text"], "hi�")
        self.assertNotIn("data_base64", result["result"])
        self.assertEqual(result["result"]["runtime_generation"], "1")
        self.assertEqual(result["result"]["next_offset"], "4")
        self.assertTrue(result["result"]["utf8_replaced"])

    def test_output_and_wait_pass_canonical_decimal_argv_unchanged(self) -> None:
        tools = Tools(policy(), None)
        runner = mock.Mock()
        runner.run.return_value = {"data_base64": ""}
        tools._runner = runner
        output = json.loads(tools.handlers()["pohunek_session_output"]({
            "session": "s", "runtime_id": "r", "runtime_generation": "0",
            "after_offset": "18446744073709551615", "max_bytes": 16, "wait_ms": 10,
        }))
        self.assertTrue(output["ok"])
        self.assertEqual(runner.run.call_args.args[0].argv, (
            "--host", "local", "session", "output", "--max-bytes", "16", "--json",
            "--runtime-id", "r", "--runtime-generation", "0",
            "--after-offset", "18446744073709551615", "--wait-ms", "10",
            "--", "s",
        ))
        runner.reset_mock()
        runner.run.return_value = {}
        waited = json.loads(tools.handlers()["pohunek_session_wait"]({
            "session": "s", "runtime_id": "r", "runtime_generation": "18446744073709551615",
            "after_terminal_watermark": "0", "after_output_offset": "18446744073709551615",
            "timeout_ms": 10,
        }))
        self.assertTrue(waited["ok"])
        self.assertEqual(runner.run.call_args.args[0].argv, (
            "--host", "local", "session", "wait", "--timeout-ms", "10", "--json",
            "--runtime-id", "r", "--runtime-generation", "18446744073709551615",
            "--after-terminal-watermark", "0",
            "--after-output-offset", "18446744073709551615",
            "--", "s",
        ))

    def test_output_cursor_roundtrips_u64_max_into_next_input(self) -> None:
        maximum = "18446744073709551615"
        tools = Tools(policy(), None)
        runner = mock.Mock()
        runner.run.side_effect = [
            {"session_id": "s", "runtime_id": "r", "runtime_generation": maximum, "next_offset": maximum, "data_base64": ""},
            {"session_id": "s", "runtime_id": "r", "runtime_generation": maximum, "next_offset": maximum, "data_base64": ""},
        ]
        tools._runner = runner
        first = json.loads(tools.handlers()["pohunek_session_output"]({"session": "s", "max_bytes": 16}))
        cursor = first["result"]["next_offset"]
        generation = first["result"]["runtime_generation"]
        second = json.loads(tools.handlers()["pohunek_session_output"]({
            "session": "s", "runtime_id": "r", "runtime_generation": generation,
            "after_offset": cursor, "max_bytes": 16,
        }))
        self.assertTrue(second["ok"])
        self.assertEqual(runner.run.call_args.args[0].argv[-8:], (
            "--runtime-id", "r", "--runtime-generation", maximum,
            "--after-offset", maximum, "--", "s",
        ))

    def test_all_cursor_fields_reject_noncanonical_or_out_of_range_values(self) -> None:
        invalid = (
            "", "00", "01", "+1", "-1", " 1", "1 ", "1a", "１２",
            "18446744073709551616", True, 0, 1,
        )
        for field in ("runtime_generation", "after_offset"):
            for value in invalid:
                tools = Tools(policy(), None)
                runner = mock.Mock()
                tools._runner = runner
                args = {"session": "s", "runtime_id": "r", "runtime_generation": "1"}
                args[field] = value
                response = json.loads(tools.handlers()["pohunek_session_output"](args))
                self.assertEqual(response["error"]["code"], "plugin_invalid_request", (field, value))
                runner.run.assert_not_called()
        for field in ("runtime_generation", "after_terminal_watermark", "after_output_offset"):
            for value in invalid:
                tools = Tools(policy(), None)
                runner = mock.Mock()
                tools._runner = runner
                args = {"session": "s", "runtime_id": "r", "runtime_generation": "1", "timeout_ms": 10}
                args[field] = value
                response = json.loads(tools.handlers()["pohunek_session_wait"](args))
                self.assertEqual(response["error"]["code"], "plugin_invalid_request", (field, value))
                runner.run.assert_not_called()

    def test_start_resolves_structured_project_and_rejects_raw_repo(self) -> None:
        tools = Tools(policy(), None)
        runner = mock.Mock()
        runner.run.side_effect = [
            {"supported_agents": ["hermes"], "runtimes": [{"agent": "hermes", "available": True, "supported": True}]},
            [{"id": "p-1", "label": "project"}],
            {"id": "s-1"},
        ]
        tools._runner = runner
        result = json.loads(tools.handlers()["pohunek_session_start"]({"agent_profile": "hermes", "project": {"label": "project"}}))
        self.assertTrue(result["ok"])
        argv = runner.run.call_args_list[-1].args[0].argv
        self.assertIn("p-1", argv)
        self.assertNotIn("--repo", argv)
        rejected = json.loads(tools.handlers()["pohunek_session_start"]({"agent_profile": "hermes", "worktree": {"repo": "/tmp/repo"}}))
        self.assertEqual(rejected["error"]["code"], "plugin_invalid_request")


class HookTests(unittest.TestCase):
    def test_unmanaged_process_never_reports(self) -> None:
        reporter = HookReporter({})
        with mock.patch.object(reporter, "_send", wraps=reporter._send) as send:
            reporter.pre_llm_call({"session_id": "native"})
        send.assert_not_called()

    def test_endpoint_failure_respects_short_deadline_and_excludes_payload(self) -> None:
        reporter = HookReporter({
            "POHUNEK_ENV": "1", "POHUNEK_SESSION_ID": "s-1", "POHUNEK_RUNTIME_ID": "r-1",
            "POHUNEK_SOCKET_PATH": "/tmp/does-not-exist", "POHUNEK_PROTOCOL_VERSION": "1",
            "POHUNEK_HOOK_TIMEOUT_MS": "10",
        })
        reporter._start_identity = 1
        started = time.monotonic()
        reporter.pre_llm_call({"session_id": "native", "prompt": "must-not-leak"})
        self.assertLess(time.monotonic() - started, 0.2)
        self.assertGreaterEqual(reporter.failures, 1)

    def test_send_reads_fragmented_first_response_line(self) -> None:
        reporter = HookReporter({})
        client = mock.MagicMock()
        client.__enter__.return_value = client
        client.recv.side_effect = [b'{"ok":', b'true}\nignored']

        with mock.patch("pohunek.hooks.socket.socket", return_value=client):
            response = reporter._send("/tmp/worker", {"type": "identity_report"})

        self.assertEqual(response, {"ok": True})
        self.assertEqual(client.recv.call_count, 2)

    def test_continuation_identity_uses_monotonic_sequences(self) -> None:
        reporter = HookReporter({
            "POHUNEK_ENV": "1", "POHUNEK_SESSION_ID": "s-1", "POHUNEK_RUNTIME_ID": "r-1",
            "POHUNEK_WORKER_SOCKET_PATH": "/tmp/worker", "POHUNEK_PROTOCOL_VERSION": "1",
        })
        reporter._start_identity = 1
        captured: list[dict[str, object]] = []
        with mock.patch.object(reporter, "_send_worker", side_effect=lambda payload: captured.append(payload) or True):
            reporter.on_session_start({"session_id": "launch"})
            reporter.pre_llm_call({"session_id": "continuation"})
        self.assertEqual([item["native_reference"] for item in captured], ["launch", "continuation"])
        self.assertLess(captured[0]["sequence"], captured[1]["sequence"])

    def test_finalize_prefers_private_release_and_transition_mapping_is_payload_free(self) -> None:
        reporter = HookReporter({
            "POHUNEK_ENV": "1", "POHUNEK_SESSION_ID": "s-1", "POHUNEK_RUNTIME_ID": "r-1",
            "POHUNEK_WORKER_SOCKET_PATH": "/tmp/worker", "POHUNEK_SOCKET_PATH": "/tmp/daemon",
            "POHUNEK_PROTOCOL_VERSION": "1", "POHUNEK_DAEMON_ID": "host-a",
        })
        reporter._start_identity = 1
        worker: list[dict[str, object]] = []
        with mock.patch.object(reporter, "_send_worker", side_effect=lambda payload: worker.append(payload) or True), mock.patch.object(reporter, "_send_public") as public:
            reporter.on_session_finalize()
            reporter.on_session_end(interrupted=True, user_message="secret")
        self.assertEqual(worker[0]["type"], "identity_release")
        self.assertEqual(public.call_args_list[0].args[0], "session.report_agent")
        self.assertEqual(public.call_args_list[1].args[0], "notification.create")
        self.assertEqual(public.call_args_list[1].args[1]["source"]["host_local_source_id"], "host-a")
        self.assertNotIn("secret", repr(public.call_args_list))

    def test_end_outcomes_map_completed_interrupted_and_failed(self) -> None:
        reporter = HookReporter({"POHUNEK_ENV": "1", "POHUNEK_SESSION_ID": "s", "POHUNEK_RUNTIME_ID": "r", "POHUNEK_SOCKET_PATH": "/tmp/d", "POHUNEK_PROTOCOL_VERSION": "1"})
        reporter._start_identity = 1
        with mock.patch.object(reporter, "_send_public") as public:
            reporter.on_session_end(completed=True)
            reporter.on_session_end(interrupted=True)
            reporter.on_session_end(failed=True)
        methods = [call.args[0] for call in public.call_args_list]
        self.assertEqual(methods.count("session.report_agent"), 3)
        kinds = [call.args[1]["kind"] for call in public.call_args_list if call.args[0] == "notification.create"]
        self.assertEqual(kinds, ["agent_blocked", "error"])

    def test_pinned_false_completed_maps_to_failed_and_semantic_response_errors_count(self) -> None:
        reporter = HookReporter({"POHUNEK_ENV": "1", "POHUNEK_SESSION_ID": "s", "POHUNEK_RUNTIME_ID": "r", "POHUNEK_SOCKET_PATH": "/tmp/d", "POHUNEK_WORKER_SOCKET_PATH": "/tmp/w", "POHUNEK_PROTOCOL_VERSION": "1"})
        reporter._start_identity = 1
        with mock.patch.object(reporter, "_send_public") as public:
            reporter.on_session_end(completed=False, interrupted=False)
        self.assertEqual(public.call_args_list[1].args[1]["kind"], "error")
        with mock.patch.object(reporter, "_send", side_effect=[{"err": {"code": "denied"}}, {"malformed": True}]):
            self.assertFalse(reporter._send_worker({"type": "identity_report"}))
            reporter._send_public("session.report_agent", {})
        self.assertGreaterEqual(reporter.failures, 2)


class RegistrationTests(unittest.TestCase):
    def test_missing_materialized_policy_registers_only_hooks(self) -> None:
        class Context:
            def __init__(self) -> None:
                self.tools: list[str] = []
                self.hooks: list[str] = []
                self.skills: list[str] = []
            def register_tool(self, name: str, handler: object) -> None:
                del handler
                self.tools.append(name)
            def register_hook(self, name: str, callback: object) -> None:
                del callback
                self.hooks.append(name)
            def register_skill(self, name: str, path: str) -> None:
                del path
                self.skills.append(name)
        context = Context()
        PLUGIN.register(context)
        self.assertEqual(context.tools, [])
        self.assertEqual(len(context.hooks), 7)
        self.assertEqual(context.skills, [])

    def test_registration_uses_supported_hermes_tool_signature(self) -> None:
        class Context:
            def __init__(self) -> None:
                self.tools: list[dict[str, object]] = []
            def register_tool(self, *, name: str, toolset: str, schema: dict[str, object], handler: object, description: str = "") -> None:
                self.tools.append({"name": name, "toolset": toolset, "schema": schema, "handler": handler, "description": description})
            def register_hook(self, name: str, callback: object) -> None:
                del name, callback
            def register_skill(self, name: str, path: Path, description: str = "") -> None:
                del name, path, description
        context = Context()
        with mock.patch.object(PLUGIN, "load_policy", return_value=policy()), mock.patch("pohunek.tools.Tools.verify_cli"):
            PLUGIN.register(context)
        self.assertEqual(len(context.tools), 16)
        self.assertTrue(all(item["toolset"] == "pohunek" for item in context.tools))
        self.assertTrue(all(isinstance(item["schema"], dict) and "parameters" in item["schema"] for item in context.tools))

    def test_cli_mismatch_keeps_best_effort_hooks_but_disables_tools(self) -> None:
        class Context:
            def __init__(self) -> None:
                self.tools: list[str] = []
                self.hooks: list[str] = []
            def register_tool(self, **kwargs: object) -> None:
                self.tools.append(str(kwargs["name"]))
            def register_hook(self, name: str, callback: object) -> None:
                del callback
                self.hooks.append(name)
            def register_skill(self, name: str, path: Path, description: str = "") -> None:
                del name, path, description
        context = Context()
        with mock.patch.object(PLUGIN, "load_policy", return_value=policy()), mock.patch("pohunek.tools.Tools.verify_cli", side_effect=CliError("pohunek_cli_incompatible")):
            PLUGIN.register(context)
        self.assertEqual(context.tools, [])
        self.assertEqual(len(context.hooks), 7)
        status = PLUGIN.integration_status()
        self.assertEqual(status["state"], "cli_incompatible")
        self.assertGreaterEqual(status["failure_count"], 1)
        self.assertNotIn("path", repr(status).lower())


if __name__ == "__main__":
    unittest.main()
