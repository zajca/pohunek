"""Regression checks for the Tailscale overlay acceptance tooling (stdlib only).

The orchestrator needs KVM, a Mac and the owner's NetBird, so these tests cover
what can be proven on any host: it parses as POSIX sh (and is shellcheck-clean
when shellcheck is installed), refuses unsafe or ambiguous option sets, carries
the pinned checksums, and its remote-command protocol round-trips through a fake
`pohunek`; the evidence helper interprets recorded observations exactly as
`docs/acceptance/README.md` documents.
"""

import base64
import copy
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest

ACCEPTANCE = Path(__file__).resolve().parents[1] / "acceptance"
DRIVER = ACCEPTANCE / "tailscale-overlay"
HELPER = ACCEPTANCE / "overlay_evidence.py"
README = Path(__file__).resolve().parents[2] / "docs" / "acceptance" / "README.md"

sys.path.insert(0, str(ACCEPTANCE))
import overlay_evidence  # noqa: E402

PORT = 18733
DOMAIN = "ts.lab.internal"
NETBIRD_ADDRESS = "100.80.1.5"
NETBIRD_ROOT = "/dev/shm/ph1112"
MAC_ROOT = "/Users/u/.ph1112"
MAC_RUNTIME = "/private/tmp/ph1112"
MAC_COMM = "/private/tmp/ph1112c"
NODES = {"a": "ph-a", "b": "ph-b"}
IPS = {"a": "100.64.0.2", "b": "100.64.0.3"}
IDS = {"a": "nAAA1CNTRL", "b": "nBBB2CNTRL"}
HOSTNAMES = {"a": "lab-a", "b": "lab-b"}


def envelope(payload):
    return json.dumps({"cli_version": "0.37.0", "protocol": {"min": 4, "max": 4}, "ok": payload})


def error_envelope(code, msg):
    return json.dumps({"cli_version": "0.37.0", "protocol": {"min": 4, "max": 4}, "err": {"class": "discovery", "code": code, "msg": msg}})


def status_json(side, running=True, tun=True):
    return json.dumps(
        {
            "BackendState": "Running" if running else "Stopped",
            "TUN": tun,
            "Self": {
                "ID": IDS[side],
                "DNSName": "{}.{}.".format(NODES[side], DOMAIN),
                "TailscaleIPs": [IPS[side], "fd7a:115c:a1e0::{}".format(2 if side == "a" else 3)],
                "PublicKey": "nodekey:" + "ab" * 32,
            },
        },
        indent=2,
    )


def doctor(checks):
    return envelope({"checks": [{"name": n, "status": s, "detail": d} for n, s, d in checks], "overall": "ok"})


def vm_doctor(side):
    return doctor(
        [
            ("tailscale_cli", "ok", "found; this host's tailscale address is {}".format(IPS[side])),
            ("overlay_tailscale_listener", "ok", "port={}; the tailscale provider reports listener address {}:{}".format(PORT, IPS[side], PORT)),
            ("overlay_config", "ok", "overlays.toml configures tailscale on port {}, as the running daemon serves".format(PORT)),
        ]
    )


def host_records(dst, classification="reachable_daemon"):
    return envelope(
        [
            {"name": "lab", "fqdn": None, "address": None, "port": PORT, "overlay": "tailscale", "peer_id": None, "local": True, "classification": "reachable_daemon", "daemon_version": "0.37.0"},
            {
                "name": NODES[dst],
                "fqdn": "{}.{}".format(NODES[dst], DOMAIN),
                "address": IPS[dst],
                "port": PORT,
                "overlay": "tailscale",
                "peer_id": IDS[dst],
                "classification": classification,
                "daemon_version": "0.37.0",
            },
        ]
    )


class Run:
    """Builds a state directory the way the orchestrator records one."""

    def __init__(self, legs=("vms", "netbird", "macos")):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.legs = list(legs)
        self.seq = 0
        meta = {
            "overlay_port": str(PORT),
            "lab_domain": DOMAIN,
            "node_a": NODES["a"],
            "node_b": NODES["b"],
            "netbird_address": NETBIRD_ADDRESS,
            "netbird_root": NETBIRD_ROOT,
            "macos_root": MAC_ROOT,
            "macos_runtime_root": MAC_RUNTIME,
            "macos_comm_root": MAC_COMM,
            "tailscale_version": "1.104.2",
        }
        (self.root / "run.json").write_text(json.dumps({"started": "2026-10-25T00:00:00Z", "legs": self.legs, "meta": meta}))

    def close(self):
        self.tmp.cleanup()

    def step(self, leg, name, stdout="", rc=0, argv=None, stderr=""):
        directory = self.root / "steps" / leg
        existing = [d for d in directory.glob("[0-9][0-9][0-9]-" + name)] if directory.is_dir() else []
        if existing:
            target = existing[0]
        else:
            self.seq += 1
            target = directory / "{:03d}-{}".format(self.seq, name)
            target.mkdir(parents=True)
        (target / "argv").write_text(argv or name + "\n")
        (target / "rc").write_text("{}\n".format(rc))
        (target / "stdout").write_text(stdout)
        (target / "stderr").write_text(stderr)
        (target / "at").write_text("2026-10-25T00:00:01Z\n")
        return target

    def remove_step(self, leg, name):
        for directory in (self.root / "steps" / leg).glob("[0-9][0-9][0-9]-" + name):
            shutil.rmtree(directory)

    def vms(self):
        for side in ("a", "b"):
            self.step("pre", "status-" + side, status_json(side))
            self.step("pre", "hostname-" + side, HOSTNAMES[side] + "\n")
            self.step("pre", "units-" + side, "active\n")
            self.step("pre", "service-health-" + side, envelope({}))
            self.step("pre", "ping-" + side, "pong from ph via 10.77.0.2:41641 in 1ms\n")
        for leg, src, dst in (("ab", "a", "b"), ("ba", "b", "a")):
            self.step(leg, "doctor-src", vm_doctor(src))
            self.step(leg, "doctor-dst", vm_doctor(dst))
            self.step(leg, "ss-src", "LISTEN 0 4096 {}:{} 0.0.0.0:*\n".format(IPS[src], PORT))
            self.step(leg, "ss-dst", "LISTEN 0 4096 {}:{} 0.0.0.0:*\n".format(IPS[dst], PORT))
            self.step(leg, "host-list", host_records(dst))
            self.step(leg, "discover", host_records(dst))
            for name in ("inspect-short", "health-fqdn", "health-peer-id", "session-list-qualified"):
                self.step(leg, name, envelope({}))
            self.step(leg, "session-new", envelope({"session": {"id": "s-{}".format(leg)}}))
            self.step(leg, "session-input", envelope({}))
            self.step(leg, "session-screen", envelope({"visible_lines": ["$ echo D4-MARK-$(hostname)", "D4-MARK-" + HOSTNAMES[dst], "$ "]}))
            self.step(leg, "attach", "$ echo ATTACH-MARK-$(hostname)\r\nATTACH-MARK-{}\r\n".format(HOSTNAMES[dst]))
            self.step(
                leg,
                "probe",
                json.dumps(
                    {
                        "ok": True,
                        "route": {"overlay": "tailscale", "peer_id": IDS[dst], "fqdn": "{}.{}".format(NODES[dst], DOMAIN), "address": IPS[dst], "port": PORT, "pinned_route": "x"},
                        "protocol_version": 4,
                        "session_list": {"count": 1, "contains_session": True},
                        "listener_checks": [{"name": "overlay_tailscale_listener", "status": "ok", "detail": "port=18733;"}],
                        "screen": {"session_id": "s", "mark_line": "D4-MARK-" + HOSTNAMES[dst]},
                    }
                ),
            )
            self.step(leg, "session-stop", envelope({}))
            self.step(leg, "session-remove", envelope({}))
        for side in ("a", "b"):
            self.step(
                "extra",
                "d7-health-" + side,
                error_envelope("host_unreachable", "cannot reach tailscale host ph at 100.64.0.2:18733; check overlays.toml"),
                rc=1,
            )
            self.step(
                "extra",
                "d5-doctor-down-" + side,
                doctor([("overlay_tailscale_listener", "warn", "port=18733; the tailscale provider reports no listener address")]),
            )
            self.step("extra", "d5-journal-" + side, "serving control protocol over overlay\n")

    def netbird(self):
        leg = "netbird"
        self.step(leg, "owner-pid-before", "1234\n")
        self.step(leg, "owner-peers-before", '{"peers":5,"connected":4}\n')
        self.step(leg, "owner-health-before", envelope({}))
        self.step(
            leg,
            "guard-doctor",
            doctor(
                [
                    ("socket_dir_writable", "ok", "writable: {}/run".format(NETBIRD_ROOT)),
                    ("state_dir_writable", "ok", "writable: {}/data/pohunek".format(NETBIRD_ROOT)),
                    ("log_dir_writable", "ok", "writable: {}/state/pohunek/logs".format(NETBIRD_ROOT)),
                ]
            ),
        )
        self.step(leg, "daemon-start", "started pohunekd in background (pid 4321)\n")
        self.step(
            leg,
            "doctor",
            doctor(
                [
                    ("netbird_cli", "ok", "found; this host's netbird address is {}".format(NETBIRD_ADDRESS)),
                    ("overlay_netbird_listener", "ok", "port=18722; the netbird provider reports listener address {}:18722".format(NETBIRD_ADDRESS)),
                    ("overlay_config", "ok", "the default (no overlays.toml) configures netbird on port 18722"),
                ]
            ),
        )
        self.step(leg, "daemon-log", "remote control listener not bound: address in use\n")
        records = envelope([{"name": "zajcaair", "fqdn": "zajcaair.netbird.cloud", "address": "100.80.9.9", "port": 18722, "overlay": "netbird", "peer_id": "k" * 43 + "=", "classification": "reachable_daemon", "daemon_version": "0.37.0"}])
        self.step(leg, "host-list", records)
        self.step(leg, "discover", records)
        self.step(leg, "inspect-zajcaair", envelope({}))
        self.step(leg, "daemon-stop", "stopped\n")
        self.step(leg, "owner-pid-after", "1234\n")
        self.step(leg, "owner-peers-after", '{"peers":5,"connected":4}\n')
        self.step(leg, "owner-health-after", envelope({}))

    def macos(self):
        leg = "macos"
        self.step(leg, "brew-install", "")
        self.step(leg, "brew-info", '{"formulae":[{"versions":{"stable":"1.104.2"}}]}')
        self.step(leg, "ls-bins", "-rwxr-xr-x 1 u admin 1 Oct 1 /opt/homebrew/bin/tailscale\n-rwxr-xr-x 1 u admin 1 Oct 1 /opt/homebrew/bin/tailscaled\n")
        self.step(
            leg,
            "guard-doctor",
            doctor(
                [
                    ("socket_dir_writable", "ok", "control socket directory {}/run does not exist yet".format(MAC_RUNTIME)),
                    ("state_dir_writable", "ok", "state data directory {}/data does not exist yet".format(MAC_ROOT)),
                ]
            ),
        )
        self.step(leg, "d6-doctor-before", doctor([("tailscale_cli", "warn", "local tailscale state is unavailable: tailscale exited with status 1")]))
        self.step(leg, "d6-host-list", error_envelope("tailscale_state_unavailable", "local tailscale state is unavailable"), rc=1)
        self.step(leg, "d6-daemon-start", "started pohunekd in background (pid 99)\n")
        self.step(leg, "d6-wait", "")
        self.step(leg, "d6-doctor-after", doctor([("overlay_tailscale_listener", "warn", "port=18733; the tailscale provider reports no listener address: unavailable")]))
        self.step(leg, "d6-daemon-log", "overlay state unavailable; serving local-only\n")
        self.step(leg, "d6-lsof", "", rc=1)
        self.step(leg, "us-status-json", json.dumps({"BackendState": "Running", "TUN": False, "Self": {"ID": "nMAC", "TailscaleIPs": ["100.64.0.9"], "PublicKey": "nodekey:" + "cd" * 32}, "AuthURL": "http://x/register/abc"}, indent=2))
        self.step(
            leg,
            "us-doctor",
            doctor(
                [
                    ("tailscale_cli", "warn", "local tailscale state is unavailable: Tailscale runs in userspace networking mode, which has no kernel interface"),
                    ("overlay_tailscale_listener", "warn", "port=18733; the tailscale provider reports no listener address"),
                ]
            ),
        )
        self.step(leg, "us-stop-daemon", "")
        self.step(leg, "us-wait", "")
        self.step(leg, "us-host-list", error_envelope("tailscale_state_unavailable", "Tailscale runs in userspace networking mode"), rc=1)
        self.step(leg, "us-lsof", "", rc=1)
        self.step(leg, "us-hostfwd-add", "")
        self.step(leg, "us-hostfwd-bind", "LISTEN 0 4096 {}:41999 0.0.0.0:*\n".format(NETBIRD_ADDRESS))
        self.step(leg, "us-hostfwd-remove", "")
        self.step(leg, "us-hostfwd-closed", "")
        self.step(leg, "cl-rm", "")
        self.step(leg, "cl-health", envelope({}))

    def full(self):
        if "vms" in self.legs:
            self.vms()
        if "netbird" in self.legs:
            self.netbird()
        if "macos" in self.legs:
            self.macos()
        return self

    def inconclude(self, code, reason="reason"):
        directory = self.root / "inconclusive"
        directory.mkdir(exist_ok=True)
        (directory / code).write_text(reason + "\n")

    def evaluate(self):
        return overlay_evidence.evaluate(overlay_evidence.State(self.root))

    def failed(self):
        return sorted("{}/{}".format(c["leg"], c["name"]) for c in self.evaluate()["checks"] if c["status"] == "failed")

    def missing(self):
        return sorted("{}/{}".format(c["leg"], c["name"]) for c in self.evaluate()["checks"] if c["status"] == "missing")


def helper(*args, stdin=None):
    return subprocess.run([sys.executable, str(HELPER)] + [str(a) for a in args], input=stdin, capture_output=True, text=True)


class DriverScriptTest(unittest.TestCase):
    def run_driver(self, *args):
        return subprocess.run(["sh", str(DRIVER)] + list(args), capture_output=True, text=True)

    def test_driver_is_executable_and_parses_as_posix_sh(self):
        self.assertTrue(os.access(DRIVER, os.X_OK))
        for shell in ("sh", "bash", "dash"):
            if shutil.which(shell) is None:
                continue
            result = subprocess.run([shell, "-n", str(DRIVER)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, "{}: {}".format(shell, result.stderr))

    def test_driver_is_shellcheck_clean(self):
        if shutil.which("shellcheck") is None:
            self.skipTest("shellcheck is not installed")
        result = subprocess.run(["shellcheck", "-s", "sh", str(DRIVER)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_a_run_without_a_leg_is_refused(self):
        result = self.run_driver("run", "--no-slot-wrapper", "--bins-dir", "/")
        self.assertEqual(result.returncode, 2)
        self.assertIn("--leg", result.stderr)

    def test_the_vms_leg_has_no_silent_slot_default(self):
        result = self.run_driver("run", "--leg", "vms", "--bins-dir", "/")
        self.assertEqual(result.returncode, 2)
        self.assertIn("--slot-wrapper", result.stderr)
        self.assertIn("--no-slot-wrapper", result.stderr)

    def test_both_slot_options_exclude_each_other(self):
        result = self.run_driver("run", "--leg", "vms", "--slot-wrapper", "/bin/true", "--no-slot-wrapper", "--bins-dir", "/")
        self.assertEqual(result.returncode, 2)
        self.assertIn("exclude each other", result.stderr)

    def test_a_slot_wrapper_must_be_executable(self):
        result = self.run_driver("run", "--leg", "vms", "--slot-wrapper", "/nonexistent/wrapper", "--bins-dir", "/")
        self.assertEqual(result.returncode, 2)
        self.assertIn("not an executable", result.stderr)

    def test_the_binary_source_is_explicit_and_single(self):
        none = self.run_driver("run", "--leg", "netbird", "--no-slot-wrapper")
        self.assertEqual(none.returncode, 2)
        self.assertIn("--ci-run", none.stderr)
        both = self.run_driver("run", "--leg", "netbird", "--no-slot-wrapper", "--ci-run", "1", "--bins-dir", "/")
        self.assertEqual(both.returncode, 2)
        self.assertIn("exclude each other", both.stderr)

    def test_the_macos_leg_needs_the_vms_leg(self):
        result = self.run_driver("run", "--leg", "macos", "--no-slot-wrapper", "--bins-dir", "/")
        self.assertEqual(result.returncode, 2)
        self.assertIn("needs the vms leg", result.stderr)

    def test_unknown_options_and_legs_are_refused(self):
        self.assertEqual(self.run_driver("run", "--leg", "bogus").returncode, 2)
        self.assertEqual(self.run_driver("run", "--frobnicate").returncode, 2)
        self.assertEqual(self.run_driver("bogus").returncode, 2)

    def test_the_netbird_legs_refuse_an_address_outside_cgnat(self):
        result = self.run_driver("run", "--leg", "netbird", "--no-slot-wrapper", "--bins-dir", "/", "--netbird-address", "192.168.1.1", "--owner-unit", "x")
        self.assertEqual(result.returncode, 2)
        self.assertIn("100.64.0.0/10", result.stderr)

    def test_pins_carry_checksums_of_the_documented_shape(self):
        pins = dict(line.split("=", 1) for line in self.run_driver("pins").stdout.splitlines())
        self.assertEqual(pins["headscale_version"], "0.29.5")
        self.assertEqual(pins["tailscale_version"], "1.104.2")
        self.assertRegex(pins["headscale_sha256"], r"^[0-9a-f]{64}$")
        self.assertRegex(pins["tailscale_sha256"], r"^[0-9a-f]{64}$")
        self.assertRegex(pins["debian_image_sha512"], r"^[0-9a-f]{128}$")
        self.assertRegex(pins["dbus_user_session_sha256"], r"^[0-9a-f]{64}$")
        self.assertEqual(pins["dbus_user_session_version"], "1.16.2-2")

    def test_the_readme_records_the_pins_the_driver_uses(self):
        readme = README.read_text()
        for line in self.run_driver("pins").stdout.splitlines():
            key, value = line.split("=", 1)
            if key.endswith(("sha256", "sha512")):
                self.assertIn(value, readme, key)

    def test_no_driver_command_prints_a_secret_or_traces(self):
        text = DRIVER.read_text()
        self.assertNotRegex(text, r"(?m)^\s*set\s+-[a-z]*x")
        self.assertNotIn("tee ", text.replace("tee -a /var/lib/headscale/issued-keys", ""))


class MacStepProtocolTest(unittest.TestCase):
    """`mac_step` round-trips a command through a fake `pohunek` shell session."""

    FAKE = textwrap.dedent(
        """\
        #!/bin/sh
        # Fake pohunek: `session input` runs the typed line locally and appends a
        # terminal-like transcript; `session output` returns it as the CLI would.
        set -eu
        log=$FAKE_DIR/transcript
        case "$1 $2" in
          "session input")
            [ "${FAKE_DROP:-0}" != 1 ] || { printf '{"cli_version":"x","ok":{}}\\n'; exit 0; }
            line=$4
            printf '%s\\r\\n' "$line" >>"$log"
            ${FAKE_SHELL:-sh} -c "$line" >"$FAKE_DIR/run.out" 2>&1 || true
            sed 's/$/\\r/' "$FAKE_DIR/run.out" >>"$log"
            printf '{"cli_version":"x","ok":{}}\\n'
            ;;
          "session output")
            data=$(base64 <"$log" | tr -d '\\n')
            printf '{"cli_version":"x","protocol":{"min":4,"max":4},"ok":{"data_base64":"%s"}}\\n' "$data"
            ;;
          *) exit 2 ;;
        esac
        """
    )

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.fake = self.dir / "pohunek"
        self.fake.write_text(self.FAKE)
        self.fake.chmod(0o755)
        (self.dir / "transcript").write_text("")
        (self.dir / "state").mkdir()
        (self.dir / "comm" / "out").mkdir(parents=True)

    def tearDown(self):
        self.tmp.cleanup()

    def run_step(self, command, timeout=10, drop=False, shell="sh"):
        script = textwrap.dedent(
            """\
            set -eu
            PH1112_LIB_ONLY=1 PH1112_LIB_PATH={driver} . {driver}
            state={state}
            macos_comm_root={comm}
            owner_pohunek={fake}
            macos_host=zajcaair
            mac_session_id=s-test
            mac_poll_s=1
            mac_step macos probe {timeout} "$1"
            printf 'RC=%s\\n' "$step_rc"
            cat "$step_dir/stdout"
            """
        ).format(driver=DRIVER, state=self.dir / "state", comm=self.dir / "comm", fake=self.fake, timeout=timeout)
        env = dict(os.environ, FAKE_DIR=str(self.dir), PH1112_LIB_PATH=str(DRIVER), FAKE_DROP="1" if drop else "0", FAKE_SHELL=shell)
        result = subprocess.run(["sh", "-c", script, "sh", command], capture_output=True, text=True, env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def test_output_and_exit_status_survive_the_round_trip(self):
        output = self.run_step("printf 'a b\\nc'; echo err >&2; exit 3")
        self.assertEqual(output, "RC=3\na b\ncerr\n")

    def test_quotes_dollars_and_backticks_are_not_interpreted_in_transit(self):
        output = self.run_step("printf '%s' 'a$b`c\"d'\"'\"'e'")
        self.assertEqual(output, "RC=0\na$b`c\"d'e")

    def test_the_typed_echo_never_matches_the_markers(self):
        self.run_step("true")
        transcript = (self.dir / "transcript").read_bytes().decode()
        typed = transcript.split("\r\n", 1)[0]
        self.assertIn("PHBEG-%s-", typed)
        self.assertNotIn("PHBEG-1-", typed)
        self.assertNotIn("PHEND-1-", typed)

    def test_the_typed_line_also_runs_in_the_macos_login_shells(self):
        for shell in ("zsh", "bash"):
            if shutil.which(shell) is None:
                continue
            with self.subTest(shell=shell):
                self.assertEqual(self.run_step("printf 'a b'; exit 4", shell=shell), "RC=4\na b")

    def test_a_session_that_never_answers_times_out_with_status_124(self):
        self.assertEqual(self.run_step("true", timeout=2, drop=True), "RC=124\n")


class DbusUserSessionTest(unittest.TestCase):
    """`install_dbus_session` and the pinned download, with stand-in VM commands."""

    def run_script(self, body, env=None):
        with tempfile.TemporaryDirectory() as tmp:
            script = textwrap.dedent(
                """\
                set -eu
                PH1112_LIB_ONLY=1 PH1112_LIB_PATH={driver} . {driver}
                state={tmp}/state
                cache_dir={tmp}/cache
                mkdir -p "$state" "$cache_dir"
                helper begin "$state" --leg vms
                poll_s=1
                service_timeout_s=2
                """
            ).format(driver=DRIVER, tmp=tmp) + textwrap.dedent(body).replace("TMP", tmp)
            result = subprocess.run(["sh", "-c", script], capture_output=True, text=True, env=dict(os.environ, PH1112_LIB_PATH=str(DRIVER), **(env or {})), timeout=60)
            codes = sorted(p.name for p in Path(tmp, "state", "inconclusive").glob("*")) if Path(tmp, "state", "inconclusive").is_dir() else []
            log = Path(tmp, "calls.log").read_text() if Path(tmp, "calls.log").exists() else ""
            return result, codes, log

    STUBS = """\
        vm_ssh() {{ shift; printf 'ssh %s\\n' "$*" >>TMP/calls.log; case "$*" in *dpkg*) return ${{DPKG_RC:-0}};; *is-active*) return ${{ACTIVE_RC:-0}};; esac; return 0; }}
        vm_ssh_stdin() {{ shift; printf 'stdin %s\\n' "$*" >>TMP/calls.log; cat >/dev/null; }}
        printf deb >"$cache_dir/dbus-user-session_${{dbus_user_session_version}}_amd64.deb"
        """

    def test_the_package_is_installed_before_the_user_manager_restart_and_the_wait(self):
        result, codes, log = self.run_script(self.STUBS.replace("{{", "{").replace("}}", "}") + "install_dbus_session a\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(codes, [])
        order = [log.index(token) for token in ("dpkg -i", "systemctl restart", "is-active dbus.socket")]
        self.assertEqual(order, sorted(order))

    def test_a_failing_dpkg_is_a_provisioning_failure_and_stops_before_the_restart(self):
        result, codes, log = self.run_script(self.STUBS.replace("{{", "{").replace("}}", "}") + "install_dbus_session a || true\n", env={"DPKG_RC": "1"})
        self.assertEqual(codes, ["provisioning_failed"])
        self.assertNotIn("systemctl restart", log)

    def test_a_socket_that_never_comes_up_is_a_bounded_provisioning_failure(self):
        result, codes, log = self.run_script(self.STUBS.replace("{{", "{").replace("}}", "}") + "install_dbus_session a || true\n", env={"ACTIVE_RC": "1"})
        self.assertEqual(codes, ["provisioning_failed"])
        self.assertGreaterEqual(log.count("is-active dbus.socket"), 2)

    def test_the_fallback_url_is_used_only_after_the_primary_fails_and_is_verified_too(self):
        body = """\
            curl() {{
              for last; do :; done
              printf 'curl %s\\n' "$last" >>TMP/calls.log
              out=
              while [ "$#" -gt 0 ]; do [ "$1" != --output ] || out=$2; shift; done
              case "$last" in
                *snapshot*) return 22 ;;
                *) printf %s "${{PAYLOAD:-good}}" >"$out" ;;
              esac
            }}
            expected=$(printf good | sha256sum | awk '{{print $1}}')
            fetch_pinned https://snapshot.example/x "$cache_dir/x" sha256 "$expected" https://pool.example/x || true
            """.replace("{{", "{").replace("}}", "}")
        result, codes, log = self.run_script(body)
        self.assertEqual(codes, [])
        self.assertIn("snapshot.example", log)
        self.assertIn("pool.example", log)
        result, codes, log = self.run_script(body, env={"PAYLOAD": "tampered"})
        self.assertEqual(codes, ["checksum_mismatch"])


class MacLegTest(unittest.TestCase):
    """The macOS leg against a fake `pohunek` session and a fake `brew`, on temp roots."""

    FAKE = MacStepProtocolTest.FAKE.replace(
        "*) exit 2 ;;",
        """"session new") printf '{"cli_version":"x","ok":{"session":{"id":"s-fake"}}}\\n' ;;
          "session stop") printf '{"cli_version":"x","ok":{}}\\n' ;;
          *) exit 2 ;;""",
    )
    BREW = textwrap.dedent(
        """\
        #!/bin/sh
        # Fake brew: `install` marks the formula installed at $FAKE_BREW_VERSION.
        printf '%s %s %s %s\\n' "$HOMEBREW_NO_AUTO_UPDATE" "$HOMEBREW_NO_INSTALL_CLEANUP" "$HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK" "$*" >>"$FAKE_DIR/brew.log"
        case "$1" in
          list)
            [ "${FAKE_BREW_LIST_RC:-}" = "" ] || exit "$FAKE_BREW_LIST_RC"
            if [ -e "$FAKE_DIR/installed" ]; then echo "tailscale $(cat "$FAKE_DIR/installed")"; exit 0; fi
            exit 1 ;;
          install) printf '%s' "$FAKE_BREW_VERSION" >"$FAKE_DIR/installed" ;;
          uninstall) rm -f "$FAKE_DIR/installed" ;;
          *) exit 0 ;;
        esac
        """
    )

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.fake = self.dir / "pohunek"
        self.fake.write_text(self.FAKE)
        self.fake.chmod(0o755)
        self.brew = self.dir / "brew"
        self.brew.write_text(self.BREW)
        self.brew.chmod(0o755)
        (self.dir / "transcript").write_text("")
        (self.dir / "state").mkdir()
        (self.dir / "home").mkdir()
        self.comm = self.dir / "comm"
        self.runtime = self.dir / "runtime"
        self.owner_root = self.dir / "home" / ".ph1112"

    def tearDown(self):
        self.tmp.cleanup()

    def run_leg(self, pre_existing=(), env=None):
        for path in pre_existing:
            Path(path).mkdir(parents=True)
            (Path(path) / "owner-file").write_text("keep")
        script = textwrap.dedent(
            """\
            set -eu
            PH1112_LIB_ONLY=1 PH1112_LIB_PATH={driver} . {driver}
            state={state}
            macos_comm_root={comm}
            macos_runtime_root={runtime}
            owner_pohunek={fake}
            macos_host=zajcaair
            macos_session_name=n
            macos_session_cwd=/tmp
            netbird_address=100.80.1.5
            macos_bin_dir=/nonexistent
            macos_brew_cmd="${{macos_brew_cmd%"$macos_brew"}}{brew}"
            mac_timeout_s=4
            mac_install_timeout_s=4
            mac_poll_s=1
            helper begin "$state" --leg macos --meta macos_root=unset
            macos_leg || true
            mac_cleanup || true
            """
        ).format(driver=DRIVER, state=self.dir / "state", comm=self.comm, runtime=self.runtime, fake=self.fake, brew=self.brew)
        run_env = dict(os.environ, FAKE_DIR=str(self.dir), HOME=str(self.dir / "home"), PH1112_LIB_PATH=str(DRIVER), **(env or {}))
        result = subprocess.run(["sh", "-c", script], capture_output=True, text=True, env=run_env, timeout=240)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def codes(self):
        directory = self.dir / "state" / "inconclusive"
        return sorted(p.name for p in directory.iterdir()) if directory.is_dir() else []

    def test_an_absent_comm_root_is_created_and_removed_again(self):
        self.run_leg(env={"FAKE_BREW_LIST_RC": "3"})
        self.assertFalse(self.comm.exists())
        self.assertNotIn("macos_provisioning_failed", self.codes())

    def test_an_existing_comm_root_stops_the_leg_and_is_left_alone(self):
        self.run_leg(pre_existing=[self.comm])
        self.assertEqual(self.codes(), ["macos_provisioning_failed"])
        self.assertEqual((self.comm / "owner-file").read_text(), "keep")
        self.assertFalse(self.runtime.exists())

    def test_an_existing_home_root_is_never_removed(self):
        self.run_leg(pre_existing=[self.owner_root])
        self.assertEqual(self.codes(), ["macos_provisioning_failed"])
        self.assertEqual((self.owner_root / "owner-file").read_text(), "keep")
        self.assertFalse(self.comm.exists())
        self.assertFalse(self.runtime.exists())

    def test_an_existing_runtime_root_is_never_removed_and_the_home_root_this_run_made_is(self):
        self.run_leg(pre_existing=[self.runtime])
        self.assertEqual(self.codes(), ["macos_provisioning_failed"])
        self.assertEqual((self.runtime / "owner-file").read_text(), "keep")
        self.assertFalse(self.owner_root.exists())
        self.assertFalse(self.comm.exists())

    def test_an_ambiguous_brew_list_is_inconclusive_and_nothing_is_installed_or_removed(self):
        self.run_leg(env={"FAKE_BREW_LIST_RC": "3"})
        self.assertEqual(self.codes(), ["macos_homebrew_failed"])
        log = (self.dir / "brew.log").read_text()
        self.assertNotIn("install", log)
        self.assertNotIn("uninstall", log)
        self.assertFalse(self.owner_root.exists())
        self.assertFalse(self.runtime.exists())

    def test_every_brew_call_disables_auto_update_and_cleanup(self):
        self.run_leg(env={"FAKE_BREW_LIST_RC": "3"})
        for line in (self.dir / "brew.log").read_text().splitlines():
            self.assertTrue(line.startswith("1 1 1 "), line)

    def test_a_wrong_version_we_installed_is_inconclusive_and_removed_after_a_re_check(self):
        self.run_leg(env={"FAKE_BREW_VERSION": "1.99.0"})
        self.assertEqual(self.codes(), ["macos_tailscale_pin_mismatch"])
        lines = (self.dir / "brew.log").read_text().splitlines()
        self.assertTrue(any(line.endswith("install tailscale") for line in lines))
        self.assertTrue(any(line.endswith("uninstall tailscale") for line in lines))
        self.assertFalse((self.dir / "installed").exists())

    def test_a_wrong_version_that_was_already_installed_is_left_alone(self):
        (self.dir / "installed").write_text("1.50.0")
        self.run_leg()
        self.assertEqual(self.codes(), ["macos_tailscale_pin_mismatch"])
        log = (self.dir / "brew.log").read_text()
        self.assertNotIn("install tailscale", log)
        self.assertNotIn("uninstall", log)
        self.assertEqual((self.dir / "installed").read_text(), "1.50.0")

    def test_several_installed_versions_are_not_the_pin(self):
        (self.dir / "installed").write_text("1.104.2 1.100.0")
        self.run_leg()
        self.assertEqual(self.codes(), ["macos_tailscale_pin_mismatch"])

    def uninstall_step(self):
        directory = next((self.dir / "state" / "steps" / "macos").glob("*-cl-brew-uninstall"))
        return int((directory / "rc").read_text())

    def test_an_install_that_installs_nothing_leaves_nothing_to_restore(self):
        # `install` exits 0 but never lists the formula, so the pin check fails
        # and the cleanup re-check finds nothing to uninstall.
        self.brew.write_text(self.BREW.replace('install) printf', 'install) : ;; xinstall) printf'))
        self.run_leg()
        self.assertEqual(self.codes(), ["macos_tailscale_pin_mismatch"])
        self.assertNotIn("uninstall", (self.dir / "brew.log").read_text())
        self.assertEqual(self.uninstall_step(), 0)

    def test_a_failing_install_that_installs_nothing_is_inconclusive_and_restores_cleanly(self):
        self.brew.write_text(self.BREW.replace('install) printf', 'install) exit 1 ;; xinstall) printf'))
        self.run_leg()
        self.assertEqual(self.codes(), ["macos_homebrew_failed"])
        self.assertNotIn("uninstall", (self.dir / "brew.log").read_text())
        self.assertEqual(self.uninstall_step(), 0)

    def test_an_ambiguous_re_check_after_our_install_is_a_failed_restore(self):
        self.brew.write_text(
            self.BREW.replace(
                "  list)\n",
                '  list)\n            [ ! -e "$FAKE_DIR/attempted" ] || exit 3\n',
            ).replace("install) printf", 'install) : >"$FAKE_DIR/attempted"; printf')
        )
        self.run_leg()
        self.assertEqual(self.uninstall_step(), 1)

    def test_a_session_that_never_answers_ends_the_leg_as_unresponsive(self):
        self.run_leg(env={"FAKE_DROP": "1"})
        self.assertEqual(self.codes(), ["macos_unresponsive"])
        self.assertFalse(self.owner_root.exists())


class CloseWindowTest(unittest.TestCase):
    """`close_window` with a stand-in monitor and a real listening socket."""

    def run_close(self, listen):
        with tempfile.TemporaryDirectory() as tmp:
            script = textwrap.dedent(
                """\
                set -eu
                PH1112_LIB_ONLY=1 PH1112_LIB_PATH={driver} . {driver}
                state={tmp}/state
                mkdir -p "$state"
                netbird_address=100.80.1.5
                hostfwd_port=$(free_port tcp)
                monitor() {{ printf '%s\n' "$*" >>{tmp}/monitor.log; }}
                if [ {listen} = 1 ]; then
                  python3 -c 'import socket,sys,time; s=socket.socket(); s.bind(("0.0.0.0", int(sys.argv[1]))); s.listen(); time.sleep(20)' "$hostfwd_port" &
                  listener=$!
                  sleep 1
                fi
                window_open=1
                close_window
                printf 'WINDOW=%s\n' "$window_open"
                [ {listen} != 1 ] || kill "$listener"
                """
            ).format(driver=DRIVER, tmp=tmp, listen=1 if listen else 0)
            result = subprocess.run(["sh", "-c", script], capture_output=True, text=True, env=dict(os.environ, PH1112_LIB_PATH=str(DRIVER)), timeout=60)
            self.assertEqual(result.returncode, 0, result.stderr)
            log = Path(tmp, "monitor.log").read_text()
            return result.stdout, log

    def test_a_closed_forward_clears_the_flag(self):
        out, log = self.run_close(listen=False)
        self.assertIn("WINDOW=0", out)
        self.assertNotIn("quit", log)

    def test_a_forward_that_stays_never_stops_vm_a_before_the_scan(self):
        out, log = self.run_close(listen=True)
        self.assertIn("WINDOW=1", out)
        self.assertNotIn("quit", log)
        self.assertEqual(log.count("hostfwd_remove"), 3)


class EvidenceTest(unittest.TestCase):
    def setUp(self):
        self.run_ = Run().full()

    def tearDown(self):
        self.run_.close()

    def test_complete_passing_run(self):
        result = self.run_.evaluate()
        self.assertEqual(result["verdict"], "passed", [c for c in result["checks"] if c["status"] != "passed"])
        self.assertGreater(len(result["checks"]), 60)

    def test_node_must_run_a_kernel_tun(self):
        self.run_.step("pre", "status-a", status_json("a", tun=False))
        self.assertIn("pre/status-a", self.run_.failed())

    def test_a_stopped_node_fails_its_precondition(self):
        self.run_.step("pre", "status-b", status_json("b", running=False))
        self.assertIn("pre/status-b", self.run_.failed())

    def test_two_ipv4_addresses_fail_the_precondition(self):
        document = json.loads(status_json("a"))
        document["Self"]["TailscaleIPs"] = ["100.64.0.2", "100.64.0.4"]
        self.run_.step("pre", "status-a", json.dumps(document))
        self.assertIn("pre/status-a", self.run_.failed())

    def test_listener_on_all_interfaces_fails(self):
        self.run_.step("ab", "ss-dst", "LISTEN 0 4096 0.0.0.0:{} 0.0.0.0:*\n".format(PORT))
        self.assertEqual(self.run_.failed(), ["ab/listener-dst"])

    def test_a_second_listener_fails(self):
        self.run_.step("ab", "ss-src", "LISTEN 0 4096 {ip}:{p} 0.0.0.0:*\nLISTEN 0 4096 10.77.0.1:{p} 0.0.0.0:*\n".format(ip=IPS["a"], p=PORT))
        self.assertEqual(self.run_.failed(), ["ab/listener-src"])

    def test_doctor_with_a_netbird_check_fails(self):
        checks = json.loads(vm_doctor("a"))["ok"]["checks"] + [{"name": "netbird_cli", "status": "warn", "detail": "x"}]
        self.run_.step("ab", "doctor-src", envelope({"checks": checks, "overall": "ok"}))
        self.assertEqual(self.run_.failed(), ["ab/doctor-src"])

    def test_doctor_with_the_wrong_listener_port_fails(self):
        checks = [
            ("tailscale_cli", "ok", "found; this host's tailscale address is {}".format(IPS["b"])),
            ("overlay_tailscale_listener", "ok", "port=18722; x"),
            ("overlay_config", "ok", "ok"),
        ]
        self.run_.step("ab", "doctor-dst", doctor(checks))
        self.assertEqual(self.run_.failed(), ["ab/doctor-dst"])

    def test_host_list_with_a_foreign_peer_id_fails(self):
        records = json.loads(host_records("b"))["ok"]
        records[1]["peer_id"] = "nOTHER"
        self.run_.step("ab", "host-list", envelope(records))
        self.assertEqual(self.run_.failed(), ["ab/host-list"])

    def test_host_list_with_a_trailing_dot_fqdn_does_not_match(self):
        records = json.loads(host_records("b"))["ok"]
        records[1]["fqdn"] += "."
        self.run_.step("ab", "host-list", envelope(records))
        self.assertEqual(self.run_.failed(), ["ab/host-list"])

    def test_an_unreachable_peer_fails_discovery(self):
        self.run_.step("ba", "discover", host_records("a", "unreachable"))
        self.assertEqual(self.run_.failed(), ["ba/host-discover"])

    def test_the_screen_must_show_the_hostname_of_the_peer_not_the_source(self):
        self.run_.step("ab", "session-screen", envelope({"visible_lines": ["D4-MARK-" + HOSTNAMES["a"]]}))
        self.assertEqual(self.run_.failed(), ["ab/session-screen-mark"])

    def test_the_typed_command_echo_alone_is_not_the_mark(self):
        self.run_.step("ab", "session-screen", envelope({"visible_lines": ["$ echo D4-MARK-$(hostname)"]}))
        self.assertEqual(self.run_.failed(), ["ab/session-screen-mark"])

    def test_attach_needs_the_mark_and_a_clean_exit(self):
        self.run_.step("ab", "attach", "ATTACH-MARK-{}\r\n".format(HOSTNAMES["b"]), rc=124)
        self.assertEqual(self.run_.failed(), ["ab/attach"])

    def test_the_probe_must_name_the_peer_and_the_session(self):
        document = json.loads((self.run_.root / "steps/ab").glob("*-probe").__next__().joinpath("stdout").read_text())
        document["route"]["peer_id"] = "nOTHER"
        document["session_list"]["contains_session"] = False
        self.run_.step("ab", "probe", json.dumps(document))
        self.assertEqual(self.run_.failed(), ["ab/sdk-probe"])

    def test_a_failed_probe_reports_its_step(self):
        self.run_.step("ab", "probe", json.dumps({"ok": False, "step": "handshake", "error": "boom"}), rc=1)
        result = self.run_.evaluate()
        probe = [c for c in result["checks"] if c["name"] == "sdk-probe" and c["leg"] == "ab"][0]
        self.assertIn("handshake", probe["detail"])

    def test_extras_never_gate(self):
        self.run_.step("extra", "d7-health-a", error_envelope("other", "x"), rc=1)
        self.run_.step("extra", "d5-journal-b", "")
        result = self.run_.evaluate()
        self.assertEqual(result["verdict"], "passed")
        gated = [c for c in result["checks"] if c["leg"] == "extra"]
        self.assertTrue(gated and not any(c["gating"] for c in gated))
        self.assertIn("extra/d7-unreachable-hint-a", self.run_.failed())

    def test_owner_daemon_pid_change_fails(self):
        self.run_.step("netbird", "owner-pid-after", "9999\n")
        self.assertEqual(self.run_.failed(), ["netbird/owner-daemon-untouched"])

    def test_owner_netbird_peer_count_change_fails(self):
        self.run_.step("netbird", "owner-peers-after", '{"peers":4,"connected":4}\n')
        self.assertEqual(self.run_.failed(), ["netbird/owner-netbird-untouched"])

    def test_an_unreadable_owner_netbird_after_the_leg_fails_the_check_instead_of_crashing(self):
        self.run_.step("netbird", "owner-peers-after", "netbird: daemon not running\n", rc=1)
        self.assertEqual(self.run_.failed(), ["netbird/owner-netbird-untouched"])

    def test_owner_netbird_connected_count_is_recorded_but_does_not_gate(self):
        self.run_.step("netbird", "owner-peers-after", '{"peers":5,"connected":2}\n')
        self.assertEqual(self.run_.failed(), [])
        check = [c for c in self.run_.evaluate()["checks"] if c["name"] == "owner-netbird-untouched"][0]
        self.assertIn("connected 4 -> 2", check["detail"])

    def test_a_service_installed_daemon_that_fails_health_is_a_product_failure(self):
        self.run_.step("pre", "service-health-b", error_envelope("daemon_unreachable", "no daemon"), rc=1)
        self.assertEqual(self.run_.failed(), ["pre/service-install-health-b"])
        self.assertEqual(self.run_.evaluate()["verdict"], "failed")

    def test_a_userspace_daemon_that_fails_health_is_a_product_failure(self):
        self.run_.step("macos", "us-wait", "", rc=1)
        self.assertEqual(self.run_.failed(), ["macos/userspace-daemon-health"])
        self.assertEqual(self.run_.evaluate()["verdict"], "failed")

    def test_a_d6_daemon_that_fails_health_is_a_product_failure(self):
        self.run_.step("macos", "d6-wait", "", rc=1)
        self.assertEqual(self.run_.failed(), ["macos/d6-daemon-health"])

    def test_the_exposure_checks_are_not_voided_by_a_userspace_code(self):
        self.run_.inconclude("macos_userspace_refused")
        self.run_.step("macos", "us-hostfwd-closed", "LISTEN 0 4096 {}:41999 0.0.0.0:*\n".format(NETBIRD_ADDRESS))
        self.assertEqual(self.run_.evaluate()["verdict"], "failed")

    def test_without_an_opened_window_no_exposure_check_is_required(self):
        self.run_.inconclude("macos_userspace_refused")
        for name in ("us-hostfwd-add", "us-hostfwd-bind", "us-hostfwd-remove", "us-hostfwd-closed"):
            self.run_.remove_step("macos", name)
        self.assertEqual(self.run_.evaluate()["verdict"], "inconclusive")
        self.assertEqual(self.run_.missing(), [])

    def homebrew_run(self, uninstall_rc, code=None):
        meta = json.loads((self.run_.root / "run.json").read_text())
        meta["meta"]["macos_brew_installed_by_us"] = "1"
        (self.run_.root / "run.json").write_text(json.dumps(meta))
        self.run_.step("macos", "cl-brew-uninstall", "", rc=uninstall_rc)
        if code:
            self.run_.inconclude(code)
        return self.run_.evaluate()["verdict"]

    def test_a_clean_homebrew_failure_is_inconclusive_with_the_restore_passed(self):
        for code in ("macos_homebrew_failed", "macos_tailscale_pin_mismatch"):
            self.assertEqual(self.homebrew_run(0, code), "inconclusive", code)
        self.assertEqual(self.run_.failed(), [])

    def test_a_failed_restore_of_a_listed_formula_is_a_failure(self):
        self.assertEqual(self.homebrew_run(1, "macos_homebrew_failed"), "failed")
        self.assertEqual(self.run_.failed(), ["macos/cleanup-homebrew-restored"])

    def test_homebrew_must_be_restored_only_when_this_run_installed_it(self):
        self.assertNotIn("cleanup-homebrew-restored", [c["name"] for c in self.run_.evaluate()["checks"]])
        meta = json.loads((self.run_.root / "run.json").read_text())
        meta["meta"]["macos_brew_installed_by_us"] = "1"
        (self.run_.root / "run.json").write_text(json.dumps(meta))
        self.assertEqual(self.run_.missing(), ["macos/cleanup-homebrew-restored"])
        self.run_.step("macos", "cl-brew-uninstall", "", rc=1)
        self.assertEqual(self.run_.failed(), ["macos/cleanup-homebrew-restored"])

    def test_path_guard_rejects_a_path_outside_the_root(self):
        self.run_.step("netbird", "guard-doctor", doctor([("socket_dir_writable", "ok", "writable: /run/user/1000/pohunek")]))
        self.assertIn("netbird/path-guard", self.run_.failed())

    def test_path_guard_rejects_a_report_without_paths(self):
        self.run_.step("netbird", "guard-doctor", doctor([("git", "ok", "found")]))
        self.assertIn("netbird/path-guard", self.run_.failed())

    def test_netbird_default_needs_the_host_address_and_the_absent_file(self):
        self.run_.step(
            "netbird",
            "doctor",
            doctor(
                [
                    ("netbird_cli", "ok", "found"),
                    ("overlay_netbird_listener", "ok", "port=18722; the netbird provider reports listener address 100.80.7.7:18722"),
                    ("overlay_config", "ok", "overlays.toml configures netbird on port 18722"),
                ]
            ),
        )
        self.assertEqual(self.run_.failed(), ["netbird/doctor"])

    def test_netbird_peers_must_not_be_tailscale(self):
        records = json.loads(host_records("b"))["ok"]
        self.run_.step("netbird", "host-list", envelope(records))
        self.assertEqual(self.run_.failed(), ["netbird/host-list"])

    def test_macos_d6_needs_the_state_unavailable_warning_not_a_missing_cli(self):
        self.run_.step("macos", "d6-doctor-before", doctor([("tailscale_cli", "warn", "the tailscale CLI is not installed or not trusted; x")]))
        self.assertEqual(self.run_.failed(), ["macos/d6-cli-found-state-unavailable"])

    def test_macos_userspace_must_fail_closed(self):
        self.run_.step("macos", "us-host-list", envelope([]), rc=0)
        self.assertEqual(self.run_.failed(), ["macos/userspace-host-list-fails"])
        self.run_.step("macos", "us-lsof", "tailscaled 1 u TCP *:18733 (LISTEN)\n", rc=0)
        self.assertIn("macos/userspace-no-listener", self.run_.failed())

    def test_the_exposure_window_must_bind_only_the_netbird_address(self):
        self.run_.step("macos", "us-hostfwd-bind", "LISTEN 0 4096 0.0.0.0:41999 0.0.0.0:*\n")
        self.assertEqual(self.run_.failed(), ["macos/exposure-window-bound-to-netbird-only"])
        self.run_.step("macos", "us-hostfwd-bind", "LISTEN 0 4096 {a}:41999 0.0.0.0:*\nLISTEN 0 4096 127.0.0.1:41999 0.0.0.0:*\n".format(a=NETBIRD_ADDRESS))
        self.assertEqual(self.run_.failed(), ["macos/exposure-window-bound-to-netbird-only"])

    def test_a_forward_that_stays_open_yields_a_failed_verdict_and_a_document(self):
        self.run_.step("macos", "us-hostfwd-closed", "LISTEN 0 4096 {}:41999 0.0.0.0:*\n".format(NETBIRD_ADDRESS))
        out = self.run_.root / "out.json"
        result = helper("assemble", self.run_.root, out)
        self.assertEqual(result.returncode, 1)
        document = json.loads(out.read_text())
        self.assertEqual(document["verdict"], "failed")
        self.assertIn("exposure-window-closed", [c["name"] for c in document["checks"] if c["status"] == "failed"])

    def test_the_exposure_window_must_be_closed_afterwards(self):
        self.run_.step("macos", "us-hostfwd-closed", "LISTEN 0 4096 {}:41999 0.0.0.0:*\n".format(NETBIRD_ADDRESS))
        self.assertEqual(self.run_.failed(), ["macos/exposure-window-closed"])

    def test_macos_recording_must_be_userspace_and_running(self):
        self.run_.step("macos", "us-status-json", status_json("a", tun=True))
        self.assertEqual(self.run_.failed(), ["macos/userspace-status-recording"])

    def test_a_missing_step_is_incomplete_not_passed(self):
        self.run_.remove_step("ba", "attach")
        self.assertEqual(self.run_.missing(), ["ba/attach"])
        self.assertEqual(self.run_.evaluate()["verdict"], "incomplete")

    def test_a_failed_check_wins_over_a_missing_one(self):
        self.run_.remove_step("ba", "attach")
        self.run_.step("ab", "session-stop", envelope({}), rc=1)
        self.assertEqual(self.run_.evaluate()["verdict"], "failed")

    def test_a_malformed_observation_fails_closed(self):
        self.run_.step("ab", "doctor-src", "not json")
        self.assertEqual(self.run_.failed(), ["ab/doctor-src"])


class InconclusiveTest(unittest.TestCase):
    def setUp(self):
        self.run_ = Run().full()

    def tearDown(self):
        self.run_.close()

    def test_a_run_wide_code_voids_the_verdict_even_with_failures(self):
        self.run_.step("ab", "attach", "", rc=1)
        self.run_.inconclude("no_direct_path")
        result = self.run_.evaluate()
        self.assertEqual(result["verdict"], "inconclusive")
        self.assertEqual(result["inconclusive"][0]["scope"], "run")

    def test_every_run_wide_code_is_accepted(self):
        for code in overlay_evidence.RUN_INCONCLUSIVE:
            run_ = Run().full()
            try:
                run_.inconclude(code)
                self.assertEqual(run_.evaluate()["verdict"], "inconclusive", code)
            finally:
                run_.close()

    def test_a_substep_code_voids_only_that_substep(self):
        self.run_.step("macos", "us-host-list", envelope([]), rc=0)
        self.run_.inconclude("headscale_unreachable_from_macos")
        result = self.run_.evaluate()
        self.assertEqual(result["verdict"], "inconclusive")
        self.assertEqual(result["inconclusive"][0]["scope"], "macos.userspace")
        self.assertEqual(self.run_.failed(), ["macos/userspace-host-list-fails"])

    def test_a_failure_outside_the_voided_substep_still_fails(self):
        self.run_.inconclude("macos_userspace_refused")
        self.run_.step("ab", "attach", "", rc=1)
        self.assertEqual(self.run_.evaluate()["verdict"], "failed")

    def test_a_macos_code_voids_every_macos_check_but_not_the_other_legs(self):
        for code in ("macos_unresponsive", "macos_provisioning_failed", "macos_homebrew_failed", "macos_tailscale_pin_mismatch"):
            run_ = Run().full()
            try:
                run_.step("macos", "d6-host-list", envelope([]), rc=0)
                run_.step("macos", "d6-lsof", "tcp\n", rc=0)
                run_.inconclude(code)
                self.assertEqual(run_.evaluate()["verdict"], "inconclusive", code)
                run_.step("ab", "attach", "", rc=1)
                self.assertEqual(run_.evaluate()["verdict"], "failed", code)
            finally:
                run_.close()

    def test_a_failed_exposure_or_cleanup_check_is_never_hidden_by_a_macos_code(self):
        for name, step, rc, out in (("exposure", "us-hostfwd-closed", 0, "LISTEN 0 4096 1.2.3.4:1 0.0.0.0:*\n"), ("cleanup", "cl-rm", 1, "")):
            run_ = Run().full()
            try:
                run_.step("macos", step, out, rc=rc)
                run_.inconclude("macos_homebrew_failed")
                self.assertEqual(run_.evaluate()["verdict"], "failed", name)
            finally:
                run_.close()

    def test_an_unresponsive_session_does_void_the_cleanup_checks(self):
        self.run_.step("macos", "cl-rm", "", rc=1)
        self.run_.inconclude("macos_unresponsive")
        self.assertEqual(self.run_.evaluate()["verdict"], "inconclusive")

    def test_an_unknown_code_is_refused(self):
        self.run_.inconclude("made_up")
        with self.assertRaises(overlay_evidence.EvidenceError):
            self.run_.evaluate()
        result = helper("inconclude", self.run_.root, "made_up", "x")
        self.assertEqual(result.returncode, 3)

    def test_the_first_reason_for_a_code_is_kept(self):
        helper("inconclude", self.run_.root, "slots_unavailable", "first")
        helper("inconclude", self.run_.root, "slots_unavailable", "second")
        text = (self.run_.root / "inconclusive" / "slots_unavailable").read_text()
        self.assertEqual(text.strip(), "first")


class AssembleTest(unittest.TestCase):
    def setUp(self):
        self.run_ = Run().full()
        self.out = self.run_.root / "out.json"

    def tearDown(self):
        self.run_.close()

    def test_assemble_writes_the_document_and_exits_zero_on_pass(self):
        result = helper("assemble", self.run_.root, self.out)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        document = json.loads(self.out.read_text())
        self.assertEqual(document["schema"], "pohunek.acceptance.tailscale-overlay")
        self.assertEqual(document["verdict"], "passed")
        self.assertIn("pre", document["steps"])

    def test_a_failed_run_exits_nonzero_but_still_documents(self):
        self.run_.step("ab", "attach", "", rc=1)
        result = helper("assemble", self.run_.root, self.out)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(json.loads(self.out.read_text())["verdict"], "failed")

    def test_the_owner_netbird_addresses_are_replaced_in_netbird_legs(self):
        self.run_.step("netbird", "doctor", (self.run_.root / "steps/netbird").glob("*-doctor").__next__().joinpath("stdout").read_text() + "\n" + NETBIRD_ADDRESS + " 100.80.9.9\n")
        self.run_.step("macos", "mac-note", "peer 100.80.9.9\n")
        helper("assemble", self.run_.root, self.out)
        text = self.out.read_text()
        self.assertNotIn(NETBIRD_ADDRESS, text)
        self.assertNotIn("100.80.9.9", text)
        self.assertIn("<netbird-ip-", text)

    def test_details_of_inconclusive_codes_and_netbird_checks_are_redacted(self):
        self.run_.inconclude("path_guard_violation", "forward on {}:4000".format(NETBIRD_ADDRESS))
        self.run_.step("netbird", "owner-health-after", envelope({}), rc=1)
        helper("assemble", self.run_.root, self.out)
        self.assertNotIn(NETBIRD_ADDRESS, self.out.read_text())

    def test_lab_addresses_stay_readable_in_the_vm_legs(self):
        helper("assemble", self.run_.root, self.out)
        self.assertIn(IPS["b"], self.out.read_text())

    LEAKS = (
        "secretpeer-laptop",
        "secretpeer-laptop.example-corp.netbird.cloud",
        "example-corp.netbird.cloud",
        "9.9.9-private",
        "cfg-1234abcd",
        "/Users/martin",
        "/private/var/folders/xx/T/pohunek-agent",
        "/home/martin",
        "config_home_id",
    )

    def plant_owner_material(self):
        records = envelope(
            [
                {"name": "zajcaair", "fqdn": "zajcaair.example-corp.netbird.cloud", "address": "100.80.9.9", "port": 18722, "overlay": "netbird", "peer_id": "p1", "classification": "reachable_daemon", "daemon_version": "9.9.9-private"},
                {"name": "secretpeer-laptop", "fqdn": "secretpeer-laptop.example-corp.netbird.cloud", "address": "100.80.9.10", "port": 18722, "overlay": "netbird", "peer_id": "p2", "classification": "no_daemon", "daemon_version": "9.9.9-private"},
            ]
        )
        for leg, names in (("netbird", ("host-list", "discover")),):
            for name in names:
                self.run_.step(leg, name, records, argv="/home/martin/bin/pohunek host list --json")
        noisy = doctor(
            [
                ("socket_dir_writable", "ok", "writable: {}/run".format(NETBIRD_ROOT)),
                ("agent_runtime", "ok", "/private/var/folders/xx/T/pohunek-agent config_home_id=cfg-1234abcd"),
                ("tailscale_cli", "warn", "local tailscale state is unavailable: /Users/martin/.ph1112/ts.sock"),
            ]
        )
        self.run_.step("macos", "d6-doctor-before", noisy, stderr="/Users/martin/.ph1112 failed")
        self.run_.step("macos", "d6-host-list", error_envelope("tailscale_state_unavailable", "state is unavailable at /Users/martin/.ph1112/ts.sock"), rc=1)
        self.run_.step("macos", "unlisted-step", "secretpeer-laptop.example-corp.netbird.cloud config_home_id cfg-1234abcd /Users/martin\n")
        self.run_.step("macos", "mac-home", "/Users/martin\n")
        self.run_.step("macos", "owner-note", "9.9.9-private", argv="cd /Users/martin/.ph1112 && true")

    def test_netbird_and_macos_evidence_is_an_allow_list_projection(self):
        self.plant_owner_material()
        helper("assemble", self.run_.root, self.out)
        text = self.out.read_text()
        for leak in self.LEAKS:
            self.assertNotIn(leak, text, leak)

    def test_projected_host_records_keep_the_classification_with_placeholders(self):
        self.plant_owner_material()
        helper("assemble", self.run_.root, self.out)
        document = json.loads(self.out.read_text())
        step = [s for s in document["steps"]["netbird"] if s["name"] == "host-list"][0]
        records = json.loads(step["stdout"])
        self.assertEqual([r["name"] for r in records], ["zajcaair", "<peer-1>"])
        self.assertEqual([r["classification"] for r in records], ["reachable_daemon", "no_daemon"])
        self.assertTrue(all(set(r) == {"name", "overlay", "classification", "local", "port"} for r in records))

    def test_steps_outside_the_allow_list_keep_only_their_exit_code(self):
        self.plant_owner_material()
        helper("assemble", self.run_.root, self.out)
        document = json.loads(self.out.read_text())
        step = [s for s in document["steps"]["macos"] if s["name"] == "unlisted-step"][0]
        self.assertEqual(step["stdout"], overlay_evidence.WITHHELD)
        self.assertEqual(step["rc"], 0)

    def test_doctor_details_outside_the_allow_list_are_withheld(self):
        self.plant_owner_material()
        helper("assemble", self.run_.root, self.out)
        document = json.loads(self.out.read_text())
        step = [s for s in document["steps"]["macos"] if s["name"] == "d6-doctor-before"][0]
        checks = {c["name"]: c for c in json.loads(step["stdout"])["checks"]}
        self.assertNotIn("detail", checks["agent_runtime"])
        self.assertIn("state is unavailable", checks["tailscale_cli"]["detail"])
        self.assertIn("<path>", checks["tailscale_cli"]["detail"])

    def test_check_details_never_carry_a_mac_path(self):
        self.run_.step("macos", "guard-doctor", doctor([("socket_dir_writable", "ok", "writable: /Users/martin/elsewhere/run")]))
        helper("assemble", self.run_.root, self.out)
        text = self.out.read_text()
        self.assertNotIn("/Users/martin", text)
        self.assertNotIn("elsewhere", text)

    def test_the_mac_roots_are_placeholders(self):
        self.run_.step("macos", "mac-roots", "", argv="mkdir -p {r}/home && mkdir {m} && mkdir {c}/out".format(r=MAC_ROOT, m=MAC_RUNTIME, c=MAC_COMM))
        helper("assemble", self.run_.root, self.out)
        document = json.loads(self.out.read_text())
        step = [s for s in document["steps"]["macos"] if s["name"] == "mac-roots"][0]
        self.assertEqual(step["argv"], "mkdir -p <macos-root>/home && mkdir <macos-runtime-root> && mkdir <macos-comm-root>/out")
        self.assertNotIn(MAC_ROOT, json.dumps(document["meta"]))

    def test_the_registration_qr_code_is_redacted(self):
        self.run_.step("macos", "us-register", json.dumps({"AuthURL": "http://x/register/abc", "QR": "data:image/png;base64,QQQQ"}))
        helper("assemble", self.run_.root, self.out)
        self.assertNotIn("QQQQ", self.out.read_text())

    def test_status_recordings_are_redacted_and_embedded(self):
        helper("assemble", self.run_.root, self.out)
        document = json.loads(self.out.read_text())
        self.assertEqual(set(document["recordings"]), {"linux-kernel-tun", "macos-userspace"})
        text = json.dumps(document)
        self.assertNotIn("ab" * 32, text)
        self.assertNotIn("cd" * 32, text)
        self.assertNotIn("http://x/register/abc", text)
        self.assertTrue(document["recordings"]["macos-userspace"]["TUN"] is False)

    def test_wireguard_style_keys_are_redacted(self):
        self.run_.step("netbird", "host-list", (self.run_.root / "steps/netbird").glob("*-host-list").__next__().joinpath("stdout").read_text())
        helper("assemble", self.run_.root, self.out)
        self.assertNotIn("k" * 43, self.out.read_text())

    def test_long_output_is_truncated_in_the_document(self):
        self.run_.step("macos", "us-reach", '{"status":"pass"}\n' + "x" * 40000)
        helper("assemble", self.run_.root, self.out)
        document = json.loads(self.out.read_text())
        log = [s for s in document["steps"]["macos"] if s["name"] == "us-reach"][0]
        self.assertTrue(log["truncated"])
        self.assertLessEqual(len(log["stdout"]), overlay_evidence.MAX_RECORDED_BYTES)


class SecretCheckTest(unittest.TestCase):
    def setUp(self):
        self.run_ = Run().full()
        self.out = self.run_.root / "out.json"

    def tearDown(self):
        self.run_.close()

    def test_a_preauth_key_prefix_in_any_observation_blocks_the_evidence(self):
        for prefix in ("tskey-auth-abc123", "hskey-auth-abc123"):
            self.run_.step("pre", "units-a", "active\n" + prefix + "\n")
            result = helper("assemble", self.run_.root, self.out)
            self.assertEqual(result.returncode, 3, prefix)
            self.assertFalse(self.out.exists())
            self.assertIn("secret key prefix", result.stderr)
            self.assertNotIn("abc123", result.stdout + result.stderr)

    def test_a_key_in_a_diagnostics_file_blocks_the_evidence(self):
        (self.run_.root / "diagnostics").mkdir()
        (self.run_.root / "diagnostics" / "headscale.log").write_text("created hskey-auth-zzz\n")
        result = helper("assemble", self.run_.root, self.out)
        self.assertEqual(result.returncode, 3)
        self.assertFalse(self.out.exists())

    def test_issued_keys_require_the_server_side_scan_result(self):
        (self.run_.root / "keys-issued").write_text("")
        missing = helper("assemble", self.run_.root, self.out)
        self.assertEqual(missing.returncode, 3)
        self.assertFalse(self.out.exists())
        hit = helper("assemble", self.run_.root, self.out, "--known-key-hits", "1")
        self.assertEqual(hit.returncode, 3)
        self.assertFalse(self.out.exists())
        clean = helper("assemble", self.run_.root, self.out, "--known-key-hits", "0")
        self.assertEqual(clean.returncode, 0, clean.stderr)
        self.assertTrue(self.out.exists())

    def test_scan_command_reports_the_file_not_the_value(self):
        (self.run_.root / "steps" / "pre" / "note").write_text("tskey-auth-secretvalue")
        result = helper("scan", self.run_.root)
        self.assertEqual(result.returncode, 1)
        self.assertIn("note", result.stderr)
        self.assertNotIn("secretvalue", result.stderr)


class DiagnosticsScanTest(unittest.TestCase):
    def test_scan_delete_removes_only_the_files_with_a_hit(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            (directory / "bad.log").write_text("x tskey-auth-secretvalue y")
            (directory / "good.log").write_text("fine")
            result = helper("scan", directory, "--delete")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse((directory / "bad.log").exists())
            self.assertTrue((directory / "good.log").exists())
            self.assertNotIn("secretvalue", result.stderr)

    def test_scan_delete_of_a_missing_directory_is_clean(self):
        self.assertEqual(helper("scan", "/nonexistent-ph1112-dir", "--delete").returncode, 0)


class MarkerSeenTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.tmp.cleanup()

    def capture(self, text):
        path = Path(self.tmp.name) / "c.json"
        path.write_text(json.dumps({"ok": {"data_base64": base64.b64encode(text.encode()).decode()}}))
        return path

    def test_prints_the_first_marker_that_shows(self):
        result = helper("marker-seen", self.capture("noise\r\nPHMK-3-NO\r\n"), "PHMK-3-OK", "PHMK-3-NO")
        self.assertEqual((result.returncode, result.stdout.strip()), (0, "PHMK-3-NO"))

    def test_the_typed_echo_is_not_a_marker(self):
        typed = "if mkdir x; then printf 'PHMK-%s-OK\\n' 3; else printf 'PHMK-%s-NO\\n' 3; fi\r\n"
        self.assertEqual(helper("marker-seen", self.capture(typed), "PHMK-3-OK", "PHMK-3-NO").returncode, 1)

    def test_a_longer_number_is_not_a_match(self):
        self.assertEqual(helper("marker-seen", self.capture("PHMK-31-OK\r\n"), "PHMK-3-OK").returncode, 1)


class QueryCommandTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, name, text):
        path = self.dir / name
        path.write_text(text)
        return path

    def test_check_status_prints_the_ipv4_of_a_running_node(self):
        result = helper("check-status", self.write("s.json", status_json("a")))
        self.assertEqual((result.returncode, result.stdout.strip()), (0, IPS["a"]))

    def test_check_status_refuses_a_userspace_node(self):
        result = helper("check-status", self.write("s.json", status_json("a", tun=False)))
        self.assertEqual(result.returncode, 1)
        self.assertIn("TUN", result.stderr)

    def test_status_fields(self):
        path = self.write("s.json", status_json("b"))
        self.assertEqual(helper("status-field", path, "id").stdout.strip(), IDS["b"])
        self.assertEqual(helper("status-field", path, "ipv4").stdout.strip(), IPS["b"])
        self.assertEqual(helper("status-field", path, "dnsname").stdout.strip(), "ph-b.ts.lab.internal")

    def test_session_id_reads_nested_and_flat_results(self):
        nested = self.write("n.json", envelope({"session": {"id": "s-1"}}))
        flat = self.write("f.json", envelope({"id": "s-2"}))
        self.assertEqual(helper("session-id", nested).stdout.strip(), "s-1")
        self.assertEqual(helper("session-id", flat).stdout.strip(), "s-2")
        self.assertEqual(helper("session-id", self.write("e.json", envelope({}))).returncode, 3)

    def test_screen_has_matches_visible_lines(self):
        path = self.write("sc.json", envelope({"visible_lines": ["a", "needle here"]}))
        self.assertEqual(helper("screen-has", path, "needle").returncode, 0)
        self.assertEqual(helper("screen-has", path, "absent").returncode, 1)

    def test_register_id_is_the_last_path_segment_of_the_auth_url(self):
        path = self.write("up.json", '{"AuthURL": "http://100.64.1.1:8080/register/AbC_123-x"}\n')
        self.assertEqual(helper("register-id", path).stdout.strip(), "AbC_123-x")
        self.assertEqual(helper("register-id", self.write("none.json", "{}")).returncode, 3)

    def test_status_field_idb64_is_unpadded_base64url_of_the_node_id(self):
        path = self.write("s.json", status_json("b"))
        encoded = helper("status-field", path, "idb64").stdout.strip()
        self.assertNotIn("=", encoded)
        self.assertEqual(base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4)).decode(), IDS["b"])

    def test_cgnat(self):
        self.assertEqual(helper("cgnat", "100.64.0.1").returncode, 0)
        self.assertEqual(helper("cgnat", "100.127.255.254").returncode, 0)
        self.assertEqual(helper("cgnat", "100.128.0.1").returncode, 1)
        self.assertEqual(helper("cgnat", "10.0.0.1").returncode, 1)
        self.assertEqual(helper("cgnat", "not-an-ip").returncode, 1)

    def test_glibc_check(self):
        newer = "0000 g DF .text 0 GLIBC_2.42 f\n0000 g DF .text 0 GLIBC_2.17 g\n"
        older = "0000 g DF .text 0 GLIBC_2.39 f\n0000 g DF .text 0 GLIBC_2.2.5 g\n"
        self.assertEqual(helper("glibc-check", "2.41", stdin=newer).returncode, 1)
        ok = helper("glibc-check", "2.41", stdin=older)
        self.assertEqual((ok.returncode, ok.stdout.strip()), (0, "2.39"))
        self.assertEqual(helper("glibc-check", "2.41", stdin="no symbols\n").returncode, 0)

    def test_guard_paths(self):
        inside = self.write("in.json", doctor([("socket_dir_writable", "ok", "writable: /dev/shm/ph1112/run")]))
        outside = self.write("out.json", doctor([("state_dir_writable", "ok", "writable: /home/u/.local/share/pohunek")]))
        self.assertEqual(helper("guard-paths", inside, "/dev/shm/ph1112").returncode, 0)
        refused = helper("guard-paths", outside, "/dev/shm/ph1112")
        self.assertEqual(refused.returncode, 1)
        self.assertIn("/home/u/.local/share/pohunek", refused.stderr)

    def test_guard_paths_rejects_a_sibling_prefix(self):
        sibling = self.write("sib.json", doctor([("socket_dir_writable", "ok", "writable: /dev/shm/ph1112-other/run")]))
        self.assertEqual(helper("guard-paths", sibling, "/dev/shm/ph1112").returncode, 1)

    def test_malformed_input_is_an_error_not_a_pass(self):
        self.assertEqual(helper("check-status", self.write("x.json", "nope")).returncode, 3)
        self.assertEqual(helper("guard-paths", self.write("y.json", "nope"), "/x").returncode, 3)


class MacExtractTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def capture(self, text):
        path = self.dir / "capture.json"
        path.write_text(envelope({"data_base64": base64.b64encode(text.encode()).decode()}))
        return path

    def test_extracts_payload_and_status_between_markers(self):
        payload = base64.b64encode(b"hello\nworld").decode()
        path = self.capture("$ typed PHBEG-%s-\\n' 7\r\nPHBEG-7-\r\n{}\r\nPHEND-7-3\r\n".format(payload))
        out = self.dir / "payload"
        result = helper("mac-extract", path, 7, out)
        self.assertEqual((result.returncode, result.stdout.strip()), (0, "3"))
        self.assertEqual(out.read_bytes(), b"hello\nworld")

    def test_not_ready_until_the_end_marker_arrives(self):
        path = self.capture("PHBEG-7-\r\nAAAA\r\n")
        self.assertEqual(helper("mac-extract", path, 7, self.dir / "p").returncode, 1)

    def test_other_command_numbers_do_not_match(self):
        path = self.capture("PHBEG-17-\r\nAAAA\r\nPHEND-17-0\r\n")
        self.assertEqual(helper("mac-extract", path, 7, self.dir / "p").returncode, 1)

    def test_terminal_escape_sequences_are_ignored(self):
        payload = base64.b64encode(b"x").decode()
        path = self.capture("\x1b[?2004l\x1b[KPHBEG-2-\r\n{}\r\n\x1b[0mPHEND-2-0\r\n".format(payload))
        out = self.dir / "payload"
        self.assertEqual(helper("mac-extract", path, 2, out).returncode, 0)
        self.assertEqual(out.read_bytes(), b"x")


class BeginTest(unittest.TestCase):
    def test_begin_and_set_meta_round_trip(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(helper("begin", tmp, "--leg", "vms", "--meta", "a=b").returncode, 0)
            self.assertEqual(helper("set-meta", tmp, "c=d").returncode, 0)
            document = json.loads((Path(tmp) / "run.json").read_text())
            self.assertEqual(document["legs"], ["vms"])
            self.assertEqual(document["meta"], {"a": "b", "c": "d"})

    def test_unknown_leg_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(helper("begin", tmp, "--leg", "bogus").returncode, 3)


if __name__ == "__main__":
    unittest.main()
