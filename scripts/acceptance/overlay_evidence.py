#!/usr/bin/env python3
"""Interpret and assemble the Tailscale overlay acceptance evidence (#1112).

`scripts/acceptance/tailscale-overlay` drives the manual procedure and records
every command it runs as a step directory (`argv`, `rc`, `stdout`, `stderr`,
`at`) under `<state>/steps/<leg>/<NN>-<name>/`. This helper is the only code
that interprets those files: small query commands answer the driver's
questions, and `assemble` turns the whole state directory into the evidence
document described in `docs/acceptance/README.md`. Evaluation is a pure
function of the state directory, so it is unit-tested on any host
(`scripts/tests/test_overlay_evidence.py`).

Verdicts:

- `inconclusive`: a run-wide precondition failed (`inconclusive/<code>` exists
  for a run-wide code), so no product verdict exists.
- `failed`: a gating check failed while its preconditions held.
- `incomplete`: a requested leg lacks a recorded step.
- `inconclusive` (sub-step): only a sub-step code is present and every other
  check passed.
- `passed`: every gating check of every requested leg passed.

Evidence never carries a secret: `assemble` refuses to write when a pre-auth
key prefix appears anywhere in the state directory or the document, or when
the issuing server reports a known key in the bundle.

Stdlib only and Python 3.9 compatible.
"""

from __future__ import annotations

import argparse
import base64
import datetime
import ipaddress
import json
from pathlib import Path
import re
import sys
from typing import Any, Dict, Iterable, List, Optional, Tuple

SCHEMA = "pohunek.acceptance.tailscale-overlay"
SCHEMA_VERSION = 1
ISSUE = 1112

# Legs the driver can run. `pre` holds the preconditions of the two VMs; `ab`
# and `ba` are the two D4 directions; `extra` holds the recorded-only D5/D7
# observations; `netbird` is the absent-file host check; `macos` is the
# zajcaair leg.
LEGS = ("vms", "netbird", "macos")
DIRECTIONS = (("ab", "a", "b"), ("ba", "b", "a"))

# Run-wide inconclusive codes: the run is stopped and carries no verdict.
RUN_INCONCLUSIVE = {
    "checksum_mismatch": "a downloaded or cached artifact does not match its pinned checksum",
    "artifact_missing": "a required CI artifact or binary is missing",
    "artifact_unusable": "a CI binary needs a newer glibc than the guest image provides",
    "slots_unavailable": "the host-wide heavy-run slots were not available",
    "provisioning_failed": "a VM or the tailnet could not be provisioned",
    "no_direct_path": "the two VMs have no direct WireGuard path",
    "path_guard_violation": "a throwaway environment resolved a path outside its roots",
    "interrupted": "the run was interrupted before it finished",
}

# Sub-step inconclusive codes: only the named sub-step has no verdict.
SUBSTEP_INCONCLUSIVE = {
    "headscale_unreachable_from_macos": (
        "macos.userspace",
        "Headscale is not reachable from zajcaair (NetBird policy or firewall)",
    ),
    "macos_userspace_refused": (
        "macos.userspace",
        "userspace tailscaled refused to start as a non-root user on macOS",
    ),
    "macos_userspace_not_running": (
        "macos.userspace",
        "the userspace node did not reach Running after Headscale approved it",
    ),
    "macos_unresponsive": ("macos", "the zajcaair shell session stopped answering"),
    "macos_provisioning_failed": ("macos", "zajcaair could not be prepared for the leg"),
    "macos_homebrew_failed": ("macos", "Homebrew could not install or inspect Tailscale on zajcaair"),
    "macos_tailscale_pin_mismatch": ("macos", "the Homebrew Tailscale on zajcaair is not the pinned version"),
}

# Pre-auth key prefixes of Tailscale and Headscale. A hit anywhere fails closed.
SECRET_PATTERNS = (re.compile(r"tskey-[A-Za-z0-9_-]+"), re.compile(r"hskey-[A-Za-z0-9_-]+"))

# Text recorded per step stream in the evidence document. Longer output stays
# in the state directory and is cut here, with `truncated` set.
MAX_RECORDED_BYTES = 16384

# Wireguard and Tailscale public keys are 32 bytes of standard base64.
WIREGUARD_KEY = re.compile(r"(?<![A-Za-z0-9+/])[A-Za-z0-9+/]{43}=(?![A-Za-z0-9+/=])")
TAILSCALE_KEY = re.compile(r"\b(?:nodekey|mkey|discokey|privkey):[0-9a-f]{64}\b")
STATUS_STRING_FIELDS = re.compile(r'("(?:PublicKey|AuthURL|QR|CurAddr|Relay|PeerRelay)"\s*:\s*)"[^"]*"')
STATUS_ADDRS_FIELD = re.compile(r'("Addrs"\s*:\s*)\[(?:\s*"[^"]*"\s*,?)*\s*\]')
CGNAT_ADDRESS = re.compile(r"(?<![0-9.])100\.(?:6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]{1,3}\.[0-9]{1,3}(?![0-9])")

# Home directories name a person.
USER_DIRECTORY = re.compile(r"(/(?:home|Users))/[^/\s'\"]+")

# Any absolute path inside prose or a doctor detail.
ABSOLUTE_PATH = re.compile(r"(?<![\w.])/(?:[\w.@+~=-]+/?)+")

# Doctor checks whose detail is product text and may be recorded for the
# NetBird and macOS legs; every other detail can name a runtime path or a
# config home and is withheld.
DETAIL_ALLOW = ("tailscale_cli", "overlay_tailscale_listener", "overlay_netbird_listener", "overlay_config")

# The one host name the evidence may print; every other peer is a placeholder.
PUBLIC_HOST = "zajcaair"

WITHHELD = "<withheld: not in the evidence allow-list>"

# Doctor checks whose detail names a directory of the daemon's environment.
PATH_CHECK_NAME = re.compile(r"(_dir|_root|_writable|_private|socket_path)")

# Legs whose output may carry the owner's NetBird addresses.
NETBIRD_LEGS = ("netbird", "macos")

# Markers around a remote command result in a terminal capture. The typed
# command carries `%s` placeholders instead of the number, so only executed
# output matches the patterns built from these.
MARKER_BEGIN = "PHBEG-{n}-"
MARKER_END = "PHEND-{n}-"


class EvidenceError(Exception):
    """A raw observation file is missing or malformed."""


# --------------------------------------------------------------------------
# Raw observations


def utc_now() -> str:
    """Return the current UTC time in the evidence timestamp format."""
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def read_text(path: Path) -> Optional[str]:
    """Return a file's text, or None when it does not exist."""
    try:
        return path.read_text(encoding="utf-8", errors="replace")
    except FileNotFoundError:
        return None


class Step:
    """One recorded command: argv text, exit code and both output streams."""

    def __init__(self, directory: Path) -> None:
        self.directory = directory
        self.name = directory.name.split("-", 1)[1]
        self.argv = (read_text(directory / "argv") or "").rstrip("\n")
        rc_text = (read_text(directory / "rc") or "").strip()
        self.rc: Optional[int] = int(rc_text) if re.fullmatch(r"-?[0-9]+", rc_text) else None
        self.stdout = read_text(directory / "stdout") or ""
        self.stderr = read_text(directory / "stderr") or ""
        self.at = (read_text(directory / "at") or "").strip()

    def document(self) -> Any:
        """Return the parsed stdout JSON, or None when it is not JSON."""
        try:
            return json.loads(self.stdout)
        except json.JSONDecodeError:
            return None

    def payload(self) -> Any:
        """Return the `ok` payload of a CLI `--json` envelope, or the document."""
        document = self.document()
        if isinstance(document, dict) and "ok" in document:
            return document["ok"]
        return document

    def error(self) -> Optional[Dict[str, Any]]:
        """Return the `err` object of a CLI `--json` error envelope."""
        document = self.document()
        if isinstance(document, dict) and isinstance(document.get("err"), dict):
            return document["err"]
        return None


class State:
    """The recorded observations of one run."""

    def __init__(self, root: Path) -> None:
        self.root = root
        run = read_text(root / "run.json")
        if run is None:
            raise EvidenceError("{} has no run.json".format(root))
        try:
            self.run = json.loads(run)
        except json.JSONDecodeError as error:
            raise EvidenceError("run.json is not JSON: {}".format(error)) from error
        self.meta: Dict[str, str] = self.run.get("meta", {})
        self.legs: List[str] = self.run.get("legs", [])
        self._steps: Dict[str, Dict[str, Step]] = {}
        steps_root = root / "steps"
        if steps_root.is_dir():
            for leg_dir in sorted(steps_root.iterdir()):
                if not leg_dir.is_dir():
                    continue
                self._steps[leg_dir.name] = {
                    step.name: step
                    for step in (Step(d) for d in sorted(leg_dir.iterdir()) if d.is_dir())
                }

    def step(self, leg: str, name: str) -> Optional[Step]:
        """Return the recorded step, or None."""
        return self._steps.get(leg, {}).get(name)

    def step_legs(self) -> List[str]:
        """Return the legs that have recorded steps."""
        return list(self._steps)

    def steps(self, leg: str) -> List[Step]:
        """Return a leg's steps in execution order."""
        return list(self._steps.get(leg, {}).values())

    def require_meta(self, key: str) -> str:
        """Return a run metadata value; a missing one is an error."""
        value = self.meta.get(key)
        if not value:
            raise EvidenceError("run.json has no meta.{}".format(key))
        return value

    def inconclusive(self) -> Dict[str, str]:
        """Return the recorded inconclusive codes with their reasons."""
        found: Dict[str, str] = {}
        directory = self.root / "inconclusive"
        if directory.is_dir():
            for entry in sorted(directory.iterdir()):
                found[entry.name] = (read_text(entry) or "").strip()
        return found


# --------------------------------------------------------------------------
# Checks


class Checks:
    """Collects check results; a missing step is `missing`, never `passed`."""

    def __init__(self, state: State) -> None:
        self.state = state
        self.items: List[Dict[str, Any]] = []

    def add(
        self,
        leg: str,
        name: str,
        passed: bool,
        detail: str = "",
        gating: bool = True,
        substep: Optional[str] = None,
    ) -> None:
        """Record one decided check."""
        self.items.append(
            {
                "leg": leg,
                "name": name,
                "status": "passed" if passed else "failed",
                "detail": detail,
                "gating": gating,
                "substep": substep,
            }
        )

    def missing(self, leg: str, name: str, step: str, gating: bool = True, substep: Optional[str] = None) -> None:
        """Record a check whose step was never recorded."""
        self.items.append(
            {
                "leg": leg,
                "name": name,
                "status": "missing",
                "detail": "step {} was not recorded".format(step),
                "gating": gating,
                "substep": substep,
            }
        )

    def with_step(self, leg: str, name: str, step_name: str, decide, gating: bool = True, substep: Optional[str] = None) -> None:
        """Run `decide(step) -> (passed, detail)` on a recorded step."""
        step = self.state.step(leg, step_name)
        if step is None:
            self.missing(leg, name, step_name, gating, substep)
            return
        try:
            passed, detail = decide(step)
        except (EvidenceError, KeyError, TypeError, ValueError, AttributeError) as error:
            passed, detail = False, "unreadable output: {}".format(error)
        self.add(leg, name, passed, detail, gating, substep)


def doctor_checks(step: Step) -> Dict[str, Dict[str, Any]]:
    """Index the checks of a `pohunek doctor --json` result by name."""
    payload = step.payload()
    if not isinstance(payload, dict) or not isinstance(payload.get("checks"), list):
        raise EvidenceError("not a doctor report")
    return {check["name"]: check for check in payload["checks"] if isinstance(check, dict) and "name" in check}


def host_records(step: Step) -> List[Dict[str, Any]]:
    """Return the records of a `host list|discover --json` result."""
    payload = step.payload()
    if not isinstance(payload, list):
        raise EvidenceError("not a host record list")
    return [record for record in payload if isinstance(record, dict)]


def status_document(step: Optional[Step]) -> Dict[str, Any]:
    """Return a `tailscale status --json` document."""
    if step is None:
        raise EvidenceError("status step missing")
    document = step.document()
    if not isinstance(document, dict) or not isinstance(document.get("Self"), dict):
        raise EvidenceError("not a tailscale status document")
    return document


def self_ipv4(document: Dict[str, Any]) -> Optional[str]:
    """Return the single IPv4 address of the Self node, or None."""
    addresses = [a for a in (document["Self"].get("TailscaleIPs") or []) if "." in a]
    return addresses[0] if len(addresses) == 1 else None


def safe_status(state: "State", side: str) -> Optional[Dict[str, Any]]:
    """Return the recorded status document of a VM, or None when unusable."""
    try:
        return status_document(state.step("pre", "status-" + side))
    except EvidenceError:
        return None


def status_problem(document: Dict[str, Any]) -> Optional[str]:
    """Why a status document is not a usable kernel-TUN node, or None."""
    if document.get("BackendState") != "Running":
        return "BackendState is {!r}, not Running".format(document.get("BackendState"))
    if document.get("TUN") is not True:
        return "TUN is {!r}, not true".format(document.get("TUN"))
    if self_ipv4(document) is None:
        return "Self.TailscaleIPs does not hold exactly one IPv4 address"
    return None


def listener_lines(step: Step) -> List[List[str]]:
    """Split `ss -ltnH` output into columns, one list per listener."""
    return [line.split() for line in step.stdout.splitlines() if line.strip()]


def error_code(step: Step) -> Optional[str]:
    """Return the `code` of a `--json` error envelope."""
    error = step.error()
    return error.get("code") if error else None


def error_message(step: Step) -> str:
    """Return the `msg` of a `--json` error envelope."""
    error = step.error()
    return str(error.get("msg", "")) if error else ""


def error_recover(step: Step) -> str:
    """Return the `recover` hint of a `--json` error envelope."""
    error = step.error()
    return str(error.get("recover", "")) if error else ""


def succeeded(step: Step) -> Tuple[bool, str]:
    """A command that exited 0 with an `ok` payload."""
    document = step.document()
    ok = step.rc == 0 and isinstance(document, dict) and "ok" in document
    return ok, "rc={}".format(step.rc)


def evaluate_preconditions(state: State, checks: Checks) -> None:
    """Both VMs: running kernel-TUN node, direct path, active units."""
    for side in ("a", "b"):
        peer = "b" if side == "a" else "a"

        def status(step: Step, side: str = side) -> Tuple[bool, str]:
            problem = status_problem(status_document(step))
            return problem is None, problem or "Running, TUN, one IPv4"

        checks.with_step("pre", "status-" + side, "status-" + side, status)
        checks.with_step(
            "pre",
            "direct-path-" + side,
            "ping-" + side,
            lambda step, peer=peer: (step.rc == 0, "tailscale ping to {} rc={}".format(peer, step.rc)),
        )

        def units(step: Step) -> Tuple[bool, str]:
            lines = [line.strip() for line in step.stdout.splitlines() if line.strip()]
            return bool(lines) and all(line == "active" for line in lines) and step.rc == 0, ",".join(lines)

        checks.with_step("pre", "daemon-units-" + side, "units-" + side, units)
        checks.with_step(
            "pre",
            "service-install-health-" + side,
            "service-health-" + side,
            lambda step: (step.rc == 0, "health after `service install` rc={}".format(step.rc)),
        )
        checks.with_step(
            "pre",
            "hostname-" + side,
            "hostname-" + side,
            lambda step: (step.rc == 0 and bool(step.stdout.strip()), step.stdout.strip()),
        )


def evaluate_direction(state: State, checks: Checks, leg: str, src: str, dst: str) -> None:
    """One D4 direction, section 2.3 of the design."""
    port = int(state.require_meta("overlay_port"))
    domain = state.require_meta("lab_domain")
    node = {"a": state.require_meta("node_a"), "b": state.require_meta("node_b")}
    dst_status = safe_status(state, dst)
    dst_hostname_step = state.step("pre", "hostname-" + dst)
    dst_hostname = dst_hostname_step.stdout.strip() if dst_hostname_step else ""
    dst_ip = self_ipv4(dst_status) if dst_status else None
    dst_id = str(dst_status["Self"].get("ID", "")) if dst_status else ""
    fqdn = "{}.{}".format(node[dst], domain)
    mark = "D4-MARK-" + dst_hostname

    for side, label in ((src, "src"), (dst, "dst")):
        side_status = safe_status(state, side)
        side_ip = self_ipv4(side_status) if side_status else None

        def doctor(step: Step, side_ip: Optional[str] = side_ip) -> Tuple[bool, str]:
            found = doctor_checks(step)
            problems = []
            cli = found.get("tailscale_cli")
            if not cli or cli.get("status") != "ok" or not side_ip or side_ip not in cli.get("detail", ""):
                problems.append("tailscale_cli is not ok with {}".format(side_ip))
            listener = found.get("overlay_tailscale_listener")
            if not listener or not listener.get("detail", "").startswith("port={};".format(port)):
                problems.append("overlay_tailscale_listener does not start with port={};".format(port))
            config = found.get("overlay_config")
            if not config or config.get("status") != "ok":
                problems.append("overlay_config is not ok")
            if "netbird_cli" in found:
                problems.append("a netbird_cli check is present")
            return not problems, "; ".join(problems) or "tailscale_cli, listener port and overlay_config as expected"

        checks.with_step(leg, "doctor-" + label, "doctor-" + label, doctor)

        def listeners(step: Step, side_ip: Optional[str] = side_ip) -> Tuple[bool, str]:
            rows = listener_lines(step)
            local = [row[3] for row in rows if len(row) >= 4]
            expected = "{}:{}".format(side_ip, port)
            return step.rc == 0 and local == [expected], "listeners={} expected={}".format(local, expected)

        checks.with_step(leg, "listener-" + label, "ss-" + label, listeners)

    def host_list(step: Step) -> Tuple[bool, str]:
        for record in host_records(step):
            if record.get("fqdn") == fqdn:
                problems = []
                if record.get("overlay") != "tailscale":
                    problems.append("overlay={!r}".format(record.get("overlay")))
                if str(record.get("peer_id")) != dst_id or not dst_id:
                    problems.append("peer_id={!r} want {!r}".format(record.get("peer_id"), dst_id))
                if record.get("name") != node[dst]:
                    problems.append("name={!r}".format(record.get("name")))
                if record.get("port") != port:
                    problems.append("port={!r}".format(record.get("port")))
                if record.get("address") != dst_ip:
                    problems.append("address={!r} want {!r}".format(record.get("address"), dst_ip))
                return not problems, "; ".join(problems) or "{} listed".format(fqdn)
        return False, "{} is not listed".format(fqdn)

    checks.with_step(leg, "host-list", "host-list", host_list)

    def discover(step: Step) -> Tuple[bool, str]:
        for record in host_records(step):
            if record.get("fqdn") == fqdn:
                return record.get("classification") == "reachable_daemon", str(record.get("classification"))
        return False, "{} is not discovered".format(fqdn)

    checks.with_step(leg, "host-discover", "discover", discover)
    for name in ("inspect-short", "health-fqdn", "health-peer-id", "session-list-qualified"):
        checks.with_step(leg, name, name, succeeded)

    checks.with_step(leg, "session-new", "session-new", lambda step: (session_id(step) is not None and step.rc == 0, str(session_id(step))))
    checks.with_step(leg, "session-input", "session-input", succeeded)
    checks.with_step(
        leg,
        "session-screen-mark",
        "session-screen",
        lambda step: (
            bool(dst_hostname) and step.rc == 0 and any(mark in line for line in screen_lines(step)),
            "screen holds {!r}: {}".format(mark, any(mark in line for line in screen_lines(step))),
        ),
    )
    attach_mark = "ATTACH-MARK-" + dst_hostname
    checks.with_step(
        leg,
        "attach",
        "attach",
        lambda step: (
            bool(dst_hostname) and step.rc == 0 and attach_mark in step.stdout,
            "rc={} output holds {!r}: {}".format(step.rc, attach_mark, attach_mark in step.stdout),
        ),
    )

    def probe(step: Step) -> Tuple[bool, str]:
        document = step.document()
        if not isinstance(document, dict):
            return False, "probe printed no JSON"
        problems = []
        if step.rc != 0 or document.get("ok") is not True:
            problems.append("probe failed at {}: {}".format(document.get("step"), document.get("error")))
            return False, "; ".join(problems)
        route = document.get("route", {})
        if route.get("overlay") != "tailscale":
            problems.append("route.overlay={!r}".format(route.get("overlay")))
        if str(route.get("peer_id")) != dst_id or not dst_id:
            problems.append("route.peer_id={!r} want {!r}".format(route.get("peer_id"), dst_id))
        if route.get("port") != port or route.get("address") != dst_ip:
            problems.append("route={}:{} want {}:{}".format(route.get("address"), route.get("port"), dst_ip, port))
        version = document.get("protocol_version")
        if not isinstance(version, int) or isinstance(version, bool) or version < 1:
            problems.append("protocol_version={!r}".format(version))
        if not document.get("session_list", {}).get("contains_session"):
            problems.append("session.list does not contain the session")
        if not any(c.get("name") == "overlay_tailscale_listener" for c in document.get("listener_checks", [])):
            problems.append("no overlay_tailscale_listener check in daemon_doctor")
        mark_line = document.get("screen", {}).get("mark_line")
        if not mark_line or mark not in mark_line:
            problems.append("screen mark line {!r} lacks {!r}".format(mark_line, mark))
        return not problems, "; ".join(problems) or "resolved, connected, listed, doctored and read the screen"

    checks.with_step(leg, "sdk-probe", "probe", probe)
    checks.with_step(leg, "session-stop", "session-stop", succeeded)
    checks.with_step(leg, "session-remove", "session-remove", succeeded)


def session_id(step: Step) -> Optional[str]:
    """Return the id of a `session new --json` result."""
    payload = step.payload()
    session = payload.get("session", payload) if isinstance(payload, dict) else None
    if isinstance(session, dict) and isinstance(session.get("id"), str):
        return session["id"]
    return None


def screen_lines(step: Step) -> List[str]:
    """Return the visible lines of a `session screen --json` result."""
    payload = step.payload()
    lines = payload.get("visible_lines") if isinstance(payload, dict) else None
    return [line for line in lines if isinstance(line, str)] if isinstance(lines, list) else []


def evaluate_extras(state: State, checks: Checks) -> None:
    """Recorded-only D7 unreachable hint and D5 rebind observations."""
    port = int(state.require_meta("overlay_port"))
    for side in ("a", "b"):
        leg = "extra"

        def hint(step: Step, side: str = side) -> Tuple[bool, str]:
            message = error_message(step)
            ok = (
                step.rc != 0
                and error_code(step) == "host_unreachable"
                and "tailscale" in message
                and str(port) in message
                and "overlays.toml" in message + error_recover(step)
            )
            return ok, "code={} msg={}".format(error_code(step), message[:200])

        checks.with_step(leg, "d7-unreachable-hint-" + side, "d7-health-" + side, hint, gating=False)

        def rebind_warn(step: Step) -> Tuple[bool, str]:
            listener = doctor_checks(step).get("overlay_tailscale_listener", {})
            return listener.get("status") == "warn", "status={}".format(listener.get("status"))

        checks.with_step(leg, "d5-listener-warn-" + side, "d5-doctor-down-" + side, rebind_warn, gating=False)
        checks.with_step(
            leg,
            "d5-rebound-" + side,
            "d5-journal-" + side,
            lambda step: ("serving control protocol over overlay" in step.stdout, "journal after tailscale up"),
            gating=False,
        )


def peers_summary(step: Step) -> Optional[Tuple[Any, Any]]:
    """Return (peers, connected) of the recorded NetBird peer counts."""
    document = step.document()
    if isinstance(document, dict) and "peers" in document and "connected" in document:
        return document["peers"], document["connected"]
    return None


def evaluate_netbird(state: State, checks: Checks) -> None:
    """The absent-file NetBird-only check, section 2.4."""
    leg = "netbird"
    nb_address = state.require_meta("netbird_address")
    root = state.require_meta("netbird_root")

    def pid(name_before: str, name_after: str):
        before, after = state.step(leg, name_before), state.step(leg, name_after)
        if before is None or after is None:
            return None
        return before.stdout.strip(), after.stdout.strip()

    pids = pid("owner-pid-before", "owner-pid-after")
    if pids is None:
        checks.missing(leg, "owner-daemon-untouched", "owner-pid-before/after")
    else:
        checks.add(
            leg,
            "owner-daemon-untouched",
            pids[0] == pids[1] and pids[0].isdigit() and pids[0] != "0",
            "MainPID {} -> {}".format(pids[0], pids[1]),
        )
    before, after = state.step(leg, "owner-peers-before"), state.step(leg, "owner-peers-after")
    if before is None or after is None:
        checks.missing(leg, "owner-netbird-untouched", "owner-peers-before/after")
    else:
        first = peers_summary(before) or ("?", "?")
        last = peers_summary(after) or ("?", "?")
        checks.add(
            leg,
            "owner-netbird-untouched",
            peers_summary(before) is not None and peers_summary(after) is not None and first[0] == last[0],
            "peer totals {} -> {}; connected {} -> {} (recorded, not gating)".format(first[0], last[0], first[1], last[1]),
        )
    for name in ("owner-health-before", "owner-health-after"):
        checks.with_step(leg, name, name, succeeded)

    def guard(step: Step) -> Tuple[bool, str]:
        offenders = path_offenders(step, [root])
        return not offenders, "paths outside {}: {}".format(root, offenders) if offenders else "all paths under {}".format(root)

    checks.with_step(leg, "path-guard", "guard-doctor", guard)
    checks.with_step(leg, "daemon-start", "daemon-start", lambda step: (step.rc == 0, "rc={}".format(step.rc)))

    def doctor(step: Step) -> Tuple[bool, str]:
        found = doctor_checks(step)
        problems = []
        if found.get("netbird_cli", {}).get("status") != "ok":
            problems.append("netbird_cli is not ok")
        listener = found.get("overlay_netbird_listener", {})
        if not listener.get("detail", "").startswith("port=18722;") or nb_address not in listener.get("detail", ""):
            problems.append("overlay_netbird_listener does not start with port=18722; and the host address")
        if not found.get("overlay_config", {}).get("detail", "").startswith("the default (no overlays.toml)"):
            problems.append("overlay_config does not report the absent file")
        return not problems, "; ".join(problems) or "netbird_cli, listener and the default configuration"

    checks.with_step(leg, "doctor", "doctor", doctor)
    checks.with_step(
        leg,
        "bind-attempt-logged",
        "daemon-log",
        lambda step: ("remote control listener not bound" in step.stdout, "daemon log line recorded"),
        gating=False,
    )

    def netbird_hosts(step: Step) -> Tuple[bool, str]:
        records = [r for r in host_records(step) if not r.get("local")]
        names = [r.get("name") for r in records]
        ok = bool(records) and all(r.get("overlay") == "netbird" for r in records) and "zajcaair" in names
        return ok, "{} remote record(s), all netbird: {}, zajcaair present: {}".format(
            len(records), all(r.get("overlay") == "netbird" for r in records), "zajcaair" in names
        )

    checks.with_step(leg, "host-list", "host-list", netbird_hosts)
    checks.with_step(leg, "host-discover", "discover", netbird_hosts)
    checks.with_step(leg, "inspect-zajcaair", "inspect-zajcaair", succeeded)
    checks.with_step(leg, "daemon-stop", "daemon-stop", lambda step: (step.rc == 0, "rc={}".format(step.rc)))


def path_offenders(step: Step, roots: Iterable[str]) -> List[str]:
    """Absolute paths of a doctor report that lie outside every root.

    The report must name at least one path, so an empty report cannot pass.
    """
    found: List[str] = []
    for check in doctor_checks(step).values():
        if not PATH_CHECK_NAME.search(check.get("name", "")):
            continue
        found.extend(re.findall(r"(?<![\w.])/(?:[\w.@+-]+/?)+", check.get("detail", "")))
    if not found:
        return ["<no path reported>"]
    prefixes = tuple(root.rstrip("/") + "/" for root in roots)
    exact = {root.rstrip("/") for root in roots}
    return sorted({p for p in found if p.rstrip("/") not in exact and not p.startswith(prefixes)})


def evaluate_macos(state: State, checks: Checks) -> None:
    """The zajcaair non-root leg, section 2.5."""
    leg = "macos"
    roots = [state.require_meta("macos_root"), state.require_meta("macos_runtime_root")]
    checks.with_step(leg, "brew-install", "brew-install", lambda step: (step.rc == 0, "rc={}".format(step.rc)), substep="macos.install")
    checks.with_step(
        leg,
        "brew-version",
        "brew-info",
        lambda step: (
            state.require_meta("tailscale_version") in step.stdout,
            "brew info reports the pinned {}".format(state.require_meta("tailscale_version")),
        ),
        substep="macos.install",
    )
    checks.with_step(
        leg,
        "brew-binaries",
        "ls-bins",
        lambda step: (
            step.rc == 0 and set(re.findall(r"/bin/(tailscaled?)\b", step.stdout)) == {"tailscale", "tailscaled"},
            "rc={}".format(step.rc),
        ),
        substep="macos.install",
    )
    checks.with_step(
        leg,
        "path-guard",
        "guard-doctor",
        lambda step: (not path_offenders(step, roots), "offenders {}".format(path_offenders(step, roots))),
    )

    def d6_doctor(step: Step) -> Tuple[bool, str]:
        cli = doctor_checks(step).get("tailscale_cli", {})
        detail = cli.get("detail", "")
        ok = cli.get("status") == "warn" and "state is unavailable" in detail and "not installed or not trusted" not in detail
        return ok, "tailscale_cli {}: {}".format(cli.get("status"), detail[:200])

    checks.with_step(leg, "d6-cli-found-state-unavailable", "d6-doctor-before", d6_doctor, substep="macos.d6")
    checks.with_step(
        leg,
        "d6-host-list-fails",
        "d6-host-list",
        lambda step: (step.rc != 0 and error_code(step) == "tailscale_state_unavailable", "code={}".format(error_code(step))),
        substep="macos.d6",
    )
    checks.with_step(leg, "d6-daemon-start", "d6-daemon-start", lambda step: (step.rc == 0, "rc={}".format(step.rc)), substep="macos.d6")
    checks.with_step(leg, "d6-daemon-health", "d6-wait", lambda step: (step.rc == 0, "rc={}".format(step.rc)), substep="macos.d6")

    def d6_listener(step: Step) -> Tuple[bool, str]:
        listener = doctor_checks(step).get("overlay_tailscale_listener", {})
        ok = listener.get("status") == "warn" and "no listener address" in listener.get("detail", "")
        return ok, "listener {}: {}".format(listener.get("status"), listener.get("detail", "")[:200])

    checks.with_step(leg, "d6-listener-warn", "d6-doctor-after", d6_listener, substep="macos.d6")
    checks.with_step(
        leg,
        "d6-serving-local-only",
        "d6-daemon-log",
        lambda step: ("overlay state unavailable; serving local-only" in step.stdout, "daemon log"),
        substep="macos.d6",
    )
    checks.with_step(leg, "d6-no-listener", "d6-lsof", lambda step: (step.rc == 1 and not step.stdout.strip(), "lsof rc={}".format(step.rc)), substep="macos.d6")

    sub = "macos.userspace"

    def recording(step: Step) -> Tuple[bool, str]:
        document = status_document(step)
        return document.get("BackendState") == "Running" and document.get("TUN") is False, "BackendState={} TUN={}".format(
            document.get("BackendState"), document.get("TUN")
        )

    checks.with_step(leg, "userspace-status-recording", "us-status-json", recording, substep=sub)
    checks.with_step(leg, "userspace-d6-daemon-stopped", "us-stop-daemon", lambda step: (step.rc == 0, "rc={}".format(step.rc)), substep=sub)
    checks.with_step(leg, "userspace-daemon-health", "us-wait", lambda step: (step.rc == 0, "rc={}".format(step.rc)), substep=sub)

    def us_doctor(step: Step) -> Tuple[bool, str]:
        found = doctor_checks(step)
        cli = found.get("tailscale_cli", {})
        listener = found.get("overlay_tailscale_listener", {})
        ok = (
            cli.get("status") == "warn"
            and "userspace networking" in cli.get("detail", "")
            and listener.get("status") == "warn"
            and "no listener address" in listener.get("detail", "")
        )
        return ok, "tailscale_cli {} / listener {}".format(cli.get("status"), listener.get("status"))

    checks.with_step(leg, "userspace-doctor-fails-closed", "us-doctor", us_doctor, substep=sub)
    checks.with_step(
        leg,
        "userspace-host-list-fails",
        "us-host-list",
        lambda step: (
            step.rc != 0 and error_code(step) == "tailscale_state_unavailable" and "userspace" in error_message(step).lower(),
            "code={}".format(error_code(step)),
        ),
        substep=sub,
    )
    checks.with_step(leg, "userspace-no-listener", "us-lsof", lambda step: (step.rc == 1 and not step.stdout.strip(), "lsof rc={}".format(step.rc)), substep=sub)
    if state.step(leg, "us-hostfwd-add") is not None:
        nb_address = state.require_meta("netbird_address")

        def window_address(step: Step) -> Tuple[bool, str]:
            rows = listener_lines(step)
            local = [row[3] for row in rows if len(row) >= 4]
            ok = step.rc == 0 and len(local) == 1 and local[0].startswith(nb_address + ":")
            return ok, "forward listeners {}".format(["<netbird-address>:" + a.rsplit(":", 1)[-1] if a.startswith(nb_address + ":") else a for a in local])

        checks.with_step(leg, "exposure-window-bound-to-netbird-only", "us-hostfwd-bind", window_address)
        checks.with_step(
            leg,
            "exposure-window-closed",
            "us-hostfwd-closed",
            lambda step: (step.rc == 0 and not step.stdout.strip(), "listeners after the forward was removed: {!r}".format(step.stdout.strip())),
        )
        checks.with_step(leg, "exposure-window-removed", "us-hostfwd-remove", lambda step: (step.rc == 0, "rc={}".format(step.rc)))
    if state.meta.get("macos_brew_installed_by_us") == "1":
        checks.with_step(leg, "cleanup-homebrew-restored", "cl-brew-uninstall", lambda step: (step.rc == 0, "rc={}".format(step.rc)))
    checks.with_step(leg, "cleanup-removed-roots", "cl-rm", lambda step: (step.rc == 0, "rc={}".format(step.rc)))
    checks.with_step(leg, "cleanup-owner-health", "cl-health", succeeded)


def evaluate(state: State) -> Dict[str, Any]:
    """Evaluate every requested leg into the evidence checks and verdict."""
    checks = Checks(state)
    legs = state.legs
    if "vms" in legs:
        evaluate_preconditions(state, checks)
        for leg, src, dst in DIRECTIONS:
            evaluate_direction(state, checks, leg, src, dst)
        evaluate_extras(state, checks)
    if "netbird" in legs:
        evaluate_netbird(state, checks)
    if "macos" in legs:
        evaluate_macos(state, checks)
    return verdict(state, checks.items)


def verdict(state: State, items: List[Dict[str, Any]]) -> Dict[str, Any]:
    """Combine checks and inconclusive codes into one verdict."""
    codes = state.inconclusive()
    for code in codes:
        if code not in RUN_INCONCLUSIVE and code not in SUBSTEP_INCONCLUSIVE:
            raise EvidenceError("unknown inconclusive code {}".format(code))
    run_codes = {c: r for c, r in codes.items() if c in RUN_INCONCLUSIVE}
    sub_codes = {c: r for c, r in codes.items() if c in SUBSTEP_INCONCLUSIVE}
    voided = {SUBSTEP_INCONCLUSIVE[c][0] for c in sub_codes}
    gating = [i for i in items if i["gating"]]
    # A leg-wide code voids the leg's checks, except a failed exposure-window or
    # cleanup check: those report harm to the owner's machines and are never
    # hidden (only an unresponsive session voids the cleanup checks).
    def hidden(item: Dict[str, Any]) -> bool:
        if item["substep"] in voided:
            return True
        if item["leg"] not in voided:
            return False
        if item["status"] == "failed":
            if item["name"].startswith("exposure-window-"):
                return False
            if item["name"].startswith("cleanup-") and "macos_unresponsive" not in sub_codes:
                return False
        return True

    live = [i for i in gating if not hidden(i)]
    failed = [i for i in live if i["status"] == "failed"]
    missing = [i for i in live if i["status"] == "missing"]
    if run_codes:
        outcome = "inconclusive"
    elif failed:
        outcome = "failed"
    elif missing:
        outcome = "incomplete"
    elif voided:
        outcome = "inconclusive"
    else:
        outcome = "passed"
    return {
        "verdict": outcome,
        "inconclusive": [
            {"code": c, "scope": "run", "reason": RUN_INCONCLUSIVE[c], "detail": d} for c, d in run_codes.items()
        ]
        + [
            {"code": c, "scope": SUBSTEP_INCONCLUSIVE[c][0], "reason": SUBSTEP_INCONCLUSIVE[c][1], "detail": d}
            for c, d in sub_codes.items()
        ],
        "checks": items,
    }


# --------------------------------------------------------------------------
# Redaction and the secret check


def scrub_paths(text: str) -> str:
    """Replace every absolute path in prose with a placeholder."""
    return ABSOLUTE_PATH.sub("<path>", text)


def phrases(*wanted: str):
    """A projector that prints only the allow-listed phrases found in a step."""

    def project(step: Step) -> str:
        return "".join(phrase + "\n" for phrase in wanted if phrase in step.stdout)

    return project


def project_doctor(step: Step) -> str:
    checks = []
    for check in doctor_checks(step).values():
        entry = {"name": check.get("name"), "status": check.get("status")}
        if check.get("name") in DETAIL_ALLOW:
            entry["detail"] = scrub_paths(str(check.get("detail", "")))
        checks.append(entry)
    return json.dumps({"checks": checks}, indent=2) + "\n"


def project_hosts(step: Step) -> str:
    error = step.error()
    if error is not None:
        return json.dumps({"err": {"code": error.get("code")}}, indent=2) + "\n"
    others = 0
    records = []
    for record in host_records(step):
        name = record.get("name")
        if name != PUBLIC_HOST and not record.get("local"):
            others += 1
            name = "<peer-{}>".format(others)
        elif record.get("local"):
            name = "<local>"
        records.append(
            {
                "name": name,
                "overlay": record.get("overlay"),
                "classification": record.get("classification"),
                "local": bool(record.get("local")),
                "port": record.get("port"),
            }
        )
    return json.dumps(records, indent=2) + "\n"


def project_peers(step: Step) -> str:
    counts = peers_summary(step)
    return json.dumps({"peers": counts[0], "connected": counts[1]}) + "\n" if counts else WITHHELD


def project_digits(step: Step) -> str:
    text = step.stdout.strip()
    return text + "\n" if text.isdigit() else WITHHELD


def project_brew_info(step: Step) -> str:
    document = step.document()
    formula = document["formulae"][0]
    return json.dumps(
        {"stable": formula["versions"]["stable"], "installed": [i.get("version") for i in formula.get("installed", [])]}
    ) + "\n"


def project_binaries(step: Step) -> str:
    return "".join(name + "\n" for name in sorted(set(re.findall(r"/bin/(tailscaled?)\b", step.stdout))))


def project_formula_lines(step: Step) -> str:
    return "".join(line + "\n" for line in step.stdout.splitlines() if re.fullmatch(r"tailscale [0-9][^\s]*", line.strip()))


def project_count(step: Step) -> str:
    return "{} listener line(s)\n".format(len([line for line in step.stdout.splitlines() if line.strip()]))


def project_verbatim(step: Step) -> str:
    return step.stdout


# Step name -> projector for the NetBird and macOS legs. The legs run against the
# owner's machines, so a step that is not listed is recorded with its exit code
# and size only.
PROJECTORS = {
    "owner-pid-before": project_digits,
    "owner-pid-after": project_digits,
    "owner-peers-before": project_peers,
    "owner-peers-after": project_peers,
    "guard-doctor": project_doctor,
    "doctor": project_doctor,
    "d6-doctor-before": project_doctor,
    "d6-doctor-after": project_doctor,
    "us-doctor": project_doctor,
    "host-list": project_hosts,
    "discover": project_hosts,
    "d6-host-list": project_hosts,
    "us-host-list": project_hosts,
    "daemon-start": phrases("started pohunekd in background"),
    "d6-daemon-start": phrases("started pohunekd in background"),
    "us-restart": phrases("started pohunekd in background"),
    "daemon-log": phrases("remote control listener not bound"),
    "d6-daemon-log": phrases("overlay state unavailable; serving local-only"),
    "brew-before": project_formula_lines,
    "brew-version": project_formula_lines,
    "cl-brew-recheck": project_formula_lines,
    "brew-info": project_brew_info,
    "ls-bins": project_binaries,
    "cl-ls": project_binaries,
    "mac-bin-sha": project_verbatim,
    "d6-lsof": project_count,
    "us-lsof": project_count,
    "us-hostfwd-bind": project_verbatim,
    "us-hostfwd-closed": project_verbatim,
    "us-reach": project_verbatim,
    "us-status-json": project_verbatim,
    "us-register": project_verbatim,
}


def project_step_text(leg: str, step: Step) -> str:
    """The stdout of a step as the evidence may record it."""
    if leg not in NETBIRD_LEGS:
        return step.stdout
    projector = PROJECTORS.get(step.name)
    if projector is None:
        return WITHHELD if step.stdout.strip() else ""
    try:
        return projector(step)
    except (EvidenceError, KeyError, IndexError, TypeError, ValueError, AttributeError):
        return WITHHELD


class Redactor:
    """Removes key material and the owner's NetBird addresses from recorded text.

    One instance serves a whole document, so the same address always maps to
    the same placeholder.
    """

    def __init__(self, netbird_addresses: Iterable[str] = (), roots: Iterable[Tuple[str, str]] = ()) -> None:
        self.netbird_addresses = [a for a in netbird_addresses if a]
        self.placeholders: Dict[str, str] = {}
        # (path, placeholder) pairs, longest path first so a root never
        # shadows the longer root it prefixes.
        self.roots = sorted([r for r in roots if r[0] and r[0] != "unset"], key=lambda r: -len(r[0]))

    def private(self, text: str) -> str:
        """Replace the throwaway roots of the owner's machines, then any path left."""
        for path, placeholder in self.roots:
            text = text.replace(path, placeholder)
        return text

    def prose(self, text: str, netbird_leg: bool) -> str:
        """Redact a check detail or reason; in NetBird legs every path goes too."""
        text = self.text(self.private(text), netbird_leg)
        return scrub_paths(text) if netbird_leg else text

    def placeholder(self, address: str) -> str:
        """Return the stable placeholder of one NetBird address."""
        return self.placeholders.setdefault(address, "<netbird-ip-{}>".format(len(self.placeholders) + 1))

    def text(self, text: str, netbird_leg: bool) -> str:
        """Redact one recorded text; NetBird addresses only in NetBird legs."""
        text = STATUS_STRING_FIELDS.sub(r'\1"<redacted>"', text)
        text = STATUS_ADDRS_FIELD.sub(r'\1["<redacted>"]', text)
        text = USER_DIRECTORY.sub(r"\1/<user>", text)
        text = TAILSCALE_KEY.sub("<redacted-key>", text)
        text = WIREGUARD_KEY.sub("<redacted-key>", text)
        if netbird_leg:
            for address in self.netbird_addresses:
                text = text.replace(address, self.placeholder(address))
            text = CGNAT_ADDRESS.sub(lambda match: self.placeholder(match.group(0)), text)
        return text


def clip(text: str) -> Tuple[str, bool]:
    """Cut text to the recorded size."""
    raw = text.encode("utf-8", errors="replace")
    if len(raw) <= MAX_RECORDED_BYTES:
        return text, False
    return raw[:MAX_RECORDED_BYTES].decode("utf-8", errors="ignore"), True


def secret_hits(text: str) -> List[str]:
    """Return the pre-auth key prefixes found in text (prefix only)."""
    return sorted({pattern.pattern.split("-")[0] + "-" for pattern in SECRET_PATTERNS if pattern.search(text)})


def scan_directory(root: Path) -> List[str]:
    """Return `<relative path>: <prefix>` for every secret hit under root."""
    hits: List[str] = []
    for path in sorted(root.rglob("*")):
        if path.is_file():
            for prefix in secret_hits(path.read_text(encoding="utf-8", errors="replace")):
                hits.append("{}: {}".format(path.relative_to(root), prefix))
    return hits


def step_record(redactor: Redactor, leg: str, step: Step) -> Dict[str, Any]:
    """Return one step as recorded in the evidence document."""
    netbird = leg in NETBIRD_LEGS
    out, out_cut = clip(redactor.text(redactor.private(project_step_text(leg, step)), netbird))
    err, err_cut = clip(WITHHELD if netbird and step.stderr.strip() else redactor.text(step.stderr, netbird))
    return {
        "name": step.name,
        "argv": redactor.text(redactor.private(step.argv), netbird),
        "rc": step.rc,
        "at": step.at,
        "stdout": out,
        "stderr": err,
        "truncated": out_cut or err_cut,
    }


def recordings(state: State, redactor: Redactor) -> Dict[str, Any]:
    """Return the redacted real `tailscale status --json` recordings (P3)."""
    found: Dict[str, Any] = {}
    sources = (("linux-kernel-tun", "pre", "status-a"), ("macos-userspace", "macos", "us-status-json"))
    for name, leg, step_name in sources:
        step = state.step(leg, step_name)
        if step is not None and step.document() is not None:
            try:
                found[name] = json.loads(redactor.text(step.stdout, leg in NETBIRD_LEGS))
            except json.JSONDecodeError as error:
                raise EvidenceError("the {} status recording is not JSON after redaction: {}".format(name, error)) from error
    return found


def private_roots(state: State) -> List[Tuple[str, str]]:
    """The throwaway roots on the owner's machines and their placeholders."""
    macos_root = state.meta.get("macos_root", "")
    home = macos_root.rsplit("/", 1)[0] if macos_root.startswith("/") else ""
    return [
        (state.meta.get("macos_comm_root", ""), "<macos-comm-root>"),
        (state.meta.get("macos_runtime_root", ""), "<macos-runtime-root>"),
        (macos_root, "<macos-root>"),
        (home, "<macos-home>"),
    ]


def assemble(state_dir: Path, known_key_hits: Optional[int]) -> Dict[str, Any]:
    """Return the evidence document; refuse when a secret could be in it."""
    state = State(state_dir)
    result = evaluate(state)
    if (state_dir / "keys-issued").exists() and known_key_hits is None:
        raise EvidenceError("the server's issued-key scan result is required (--known-key-hits)")
    if known_key_hits:
        raise EvidenceError("the evidence bundle holds {} issued key value(s)".format(known_key_hits))
    leaks = scan_directory(state_dir)
    if leaks:
        raise EvidenceError("secret key prefix in the run state: {}".format("; ".join(leaks)))
    redactor = Redactor([state.meta.get("netbird_address", "")], private_roots(state))
    for entry in result["inconclusive"]:
        entry["detail"] = redactor.prose(entry["detail"], True)
    for item in result["checks"]:
        item["detail"] = redactor.prose(item["detail"], item["leg"] in NETBIRD_LEGS)
    document = {
        "schema": SCHEMA,
        "schema_version": SCHEMA_VERSION,
        "issue": ISSUE,
        "generated": utc_now(),
        "started": state.run.get("started"),
        "legs": state.legs,
        "meta": {k: redactor.prose(v, True) for k, v in state.meta.items() if k != "netbird_address"},
        "verdict": result["verdict"],
        "inconclusive": result["inconclusive"],
        "checks": result["checks"],
        "steps": {
            leg: [step_record(redactor, leg, step) for step in state.steps(leg)] for leg in sorted(state.step_legs())
        },
        "recordings": recordings(state, redactor),
    }
    text = json.dumps(document, indent=2)
    if secret_hits(text):
        raise EvidenceError("secret key prefix in the evidence document")
    return document


# --------------------------------------------------------------------------
# Query commands used by the driver


def command_begin(args: argparse.Namespace) -> int:
    """Create a state directory with run.json."""
    state = Path(args.state_dir)
    state.mkdir(parents=True, exist_ok=True)
    meta: Dict[str, str] = {}
    for pair in args.meta:
        key, separator, value = pair.partition("=")
        if not separator or not key:
            raise EvidenceError("--meta wants key=value, got {!r}".format(pair))
        meta[key] = value
    for leg in args.leg:
        if leg not in LEGS:
            raise EvidenceError("unknown leg {!r}".format(leg))
    document = {"started": utc_now(), "legs": args.leg, "meta": meta}
    (state / "run.json").write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    return 0


def command_inconclude(args: argparse.Namespace) -> int:
    """Record an inconclusive code with its reason."""
    if args.code not in RUN_INCONCLUSIVE and args.code not in SUBSTEP_INCONCLUSIVE:
        raise EvidenceError("unknown inconclusive code {!r}".format(args.code))
    directory = Path(args.state_dir) / "inconclusive"
    directory.mkdir(parents=True, exist_ok=True)
    target = directory / args.code
    if not target.exists():
        target.write_text(args.reason + "\n", encoding="utf-8")
    return 0


def command_check_status(args: argparse.Namespace) -> int:
    """Exit 0 for a running kernel-TUN node; print its IPv4 address."""
    document = status_document(Step_from_file(Path(args.file)))
    problem = status_problem(document)
    if problem:
        print(problem, file=sys.stderr)
        return 1
    print(self_ipv4(document))
    return 0


def Step_from_file(path: Path) -> Step:
    """Wrap a lone file as a step with that file as stdout."""
    step = Step.__new__(Step)
    step.directory = path.parent
    step.name = path.name
    step.argv = ""
    step.rc = 0
    step.stdout = read_text(path) or ""
    step.stderr = ""
    step.at = ""
    return step


def command_status_field(args: argparse.Namespace) -> int:
    """Print `Self.ID`, `Self.DNSName` or the IPv4 of a status document."""
    document = status_document(Step_from_file(Path(args.file)))
    if args.field == "id":
        print(document["Self"].get("ID", ""))
    elif args.field == "ipv4":
        address = self_ipv4(document)
        if address is None:
            return 1
        print(address)
    elif args.field == "idb64":
        identifier = str(document["Self"].get("ID", ""))
        if not identifier:
            return 1
        print(base64.urlsafe_b64encode(identifier.encode()).decode().rstrip("="))
    elif args.field == "dnsname":
        print(str(document["Self"].get("DNSName", "")).rstrip("."))
    else:
        raise EvidenceError("unknown status field {!r}".format(args.field))
    return 0


def command_session_id(args: argparse.Namespace) -> int:
    """Print the id of a `session new --json` result."""
    found = session_id(Step_from_file(Path(args.file)))
    if found is None:
        raise EvidenceError("{} has no session id".format(args.file))
    print(found)
    return 0


def command_register_id(args: argparse.Namespace) -> int:
    """Print the registration id of a `tailscale up --json` AuthURL."""
    text = read_text(Path(args.file)) or ""
    matches = re.findall(r'"AuthURL"\s*:\s*"([^"]+)"', text)
    if not matches:
        raise EvidenceError("no AuthURL in {}".format(args.file))
    identifier = matches[-1].rstrip("/").rsplit("/", 1)[-1]
    if not re.fullmatch(r"[A-Za-z0-9_-]+", identifier):
        raise EvidenceError("the AuthURL does not end in a registration id")
    print(identifier)
    return 0


def command_set_meta(args: argparse.Namespace) -> int:
    """Merge key=value pairs into run.json's meta."""
    path = Path(args.state_dir) / "run.json"
    document = json.loads(read_text(path) or "{}")
    for pair in args.pairs:
        key, separator, value = pair.partition("=")
        if not separator or not key:
            raise EvidenceError("set-meta wants key=value, got {!r}".format(pair))
        document.setdefault("meta", {})[key] = value
    path.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    return 0


def command_json_field(args: argparse.Namespace) -> int:
    """Print one field of a CLI JSON envelope (`session.id` style path)."""
    step = Step_from_file(Path(args.file))
    value: Any = step.payload()
    for part in args.path.split("."):
        if isinstance(value, dict) and part in value:
            value = value[part]
        else:
            raise EvidenceError("{} has no {}".format(args.file, args.path))
    print(json.dumps(value) if isinstance(value, (dict, list, bool)) else value)
    return 0


def command_screen_has(args: argparse.Namespace) -> int:
    """Exit 0 when a screen capture shows the text."""
    step = Step_from_file(Path(args.file))
    return 0 if any(args.text in line for line in screen_lines(step)) else 1


def command_guard_paths(args: argparse.Namespace) -> int:
    """Exit 0 when every path of a doctor report lies under a root."""
    offenders = path_offenders(Step_from_file(Path(args.file)), args.roots)
    for offender in offenders:
        print(offender, file=sys.stderr)
    return 1 if offenders else 0


def command_redact_status(args: argparse.Namespace) -> int:
    """Write a redacted copy of a `tailscale status --json` capture."""
    text = read_text(Path(args.input))
    if text is None:
        raise EvidenceError("missing {}".format(args.input))
    json.loads(text)
    Path(args.output).write_text(Redactor().text(text, args.netbird), encoding="utf-8")
    return 0


def command_cgnat(args: argparse.Namespace) -> int:
    """Exit 0 when the argument is an IPv4 address in 100.64.0.0/10."""
    try:
        address = ipaddress.ip_address(args.address)
    except ValueError:
        return 1
    return 0 if address in ipaddress.ip_network("100.64.0.0/10") else 1


def command_glibc_check(args: argparse.Namespace) -> int:
    """Read `objdump -T` text on stdin; exit 0 when no GLIBC_ symbol exceeds the bound."""
    bound = tuple(int(part) for part in args.bound.split("."))
    highest: Tuple[int, ...] = (0,)
    for match in re.finditer(r"GLIBC_([0-9]+(?:\.[0-9]+)*)", sys.stdin.read()):
        highest = max(highest, tuple(int(part) for part in match.group(1).split(".")))
    print(".".join(str(part) for part in highest))
    return 0 if highest <= bound else 1


def command_mac_extract(args: argparse.Namespace) -> int:
    """Extract one marked command result from a `session output --json` capture.

    The driver sends a command that prints `PHBEG-<n>-`, the base64 of its
    output, and `PHEND-<n>-<rc>`. The typed command echoes the markers with
    format placeholders, so only executed output matches. Exit 1 when the end
    marker has not arrived yet.
    """
    capture = Step_from_file(Path(args.file)).payload()
    if not isinstance(capture, dict) or "data_base64" not in capture:
        raise EvidenceError("{} is not a session output capture".format(args.file))
    text = base64.b64decode(capture["data_base64"]).decode("utf-8", errors="replace")
    text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", text).replace("\r", "")
    begin = re.search(re.escape(MARKER_BEGIN.format(n=args.n)) + r"\n([A-Za-z0-9+/=]*)\n", text)
    end = re.search(re.escape(MARKER_END.format(n=args.n)) + r"(-?[0-9]+)\n", text)
    if not begin or not end:
        return 1
    Path(args.payload).write_bytes(base64.b64decode(begin.group(1)))
    print(end.group(1))
    return 0


def command_assemble(args: argparse.Namespace) -> int:
    """Write the evidence document and print one verdict line per check."""
    hits = None if args.known_key_hits is None else int(args.known_key_hits)
    document = assemble(Path(args.state_dir), hits)
    for item in document["checks"]:
        if item["status"] != "passed":
            print("{} {} {}: {}".format(item["status"].upper(), item["leg"], item["name"], item["detail"]))
    for item in document["inconclusive"]:
        print("INCONCLUSIVE {} ({}): {}".format(item["code"], item["scope"], item["reason"]))
    print("verdict: {}".format(document["verdict"]))
    Path(args.output).write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
    return 0 if document["verdict"] == "passed" else 1


def command_scan(args: argparse.Namespace) -> int:
    """Exit 0 when no pre-auth key prefix appears under a directory.

    With --delete every file that holds a hit is removed and the exit code is 0
    when the directory is clean afterwards.
    """
    root = Path(args.directory)
    hits = scan_directory(root) if root.is_dir() else []
    for hit in hits:
        print(hit, file=sys.stderr)
    if args.delete:
        for hit in hits:
            (root / hit.rsplit(": ", 1)[0]).unlink(missing_ok=True)
        return 0 if not (scan_directory(root) if root.is_dir() else []) else 1
    return 1 if hits else 0


def command_marker_seen(args: argparse.Namespace) -> int:
    """Print the first marker that shows in a `session output --json` capture."""
    capture = Step_from_file(Path(args.file)).payload()
    if not isinstance(capture, dict) or "data_base64" not in capture:
        raise EvidenceError("{} is not a session output capture".format(args.file))
    text = base64.b64decode(capture["data_base64"]).decode("utf-8", errors="replace")
    text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", text).replace("\r", "")
    for marker in args.markers:
        if re.search(r"(?<![\w-])" + re.escape(marker) + r"(?![\w-])", text):
            print(marker)
            return 0
    return 1


def parser() -> argparse.ArgumentParser:
    """Build the command-line parser."""
    root = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = root.add_subparsers(dest="command", required=True)

    init = commands.add_parser("begin", help="write run.json")
    init.add_argument("state_dir")
    init.add_argument("--leg", action="append", required=True)
    init.add_argument("--meta", action="append", default=[])
    init.set_defaults(handler=command_begin)

    inconclude = commands.add_parser("inconclude", help="record an inconclusive code")
    inconclude.add_argument("state_dir")
    inconclude.add_argument("code")
    inconclude.add_argument("reason")
    inconclude.set_defaults(handler=command_inconclude)

    check = commands.add_parser("check-status", help="validate a tailscale status capture")
    check.add_argument("file")
    check.set_defaults(handler=command_check_status)

    field = commands.add_parser("status-field", help="print id, ipv4 or dnsname of a status capture")
    field.add_argument("file")
    field.add_argument("field")
    field.set_defaults(handler=command_status_field)

    sid = commands.add_parser("session-id", help="print a created session's id")
    sid.add_argument("file")
    sid.set_defaults(handler=command_session_id)

    register = commands.add_parser("register-id", help="print the registration id of a tailscale up --json capture")
    register.add_argument("file")
    register.set_defaults(handler=command_register_id)

    meta = commands.add_parser("set-meta", help="merge key=value pairs into run.json")
    meta.add_argument("state_dir")
    meta.add_argument("pairs", nargs="+")
    meta.set_defaults(handler=command_set_meta)

    json_field = commands.add_parser("json-field", help="print a field of a CLI JSON envelope")
    json_field.add_argument("file")
    json_field.add_argument("path")
    json_field.set_defaults(handler=command_json_field)

    screen = commands.add_parser("screen-has", help="exit 0 when a screen capture shows text")
    screen.add_argument("file")
    screen.add_argument("text")
    screen.set_defaults(handler=command_screen_has)

    guard = commands.add_parser("guard-paths", help="exit 0 when a doctor report stays under the roots")
    guard.add_argument("file")
    guard.add_argument("roots", nargs="+")
    guard.set_defaults(handler=command_guard_paths)

    redact = commands.add_parser("redact-status", help="write a redacted status capture")
    redact.add_argument("input")
    redact.add_argument("output")
    redact.add_argument("--netbird", action="store_true")
    redact.set_defaults(handler=command_redact_status)

    cgnat = commands.add_parser("cgnat", help="exit 0 for an address in 100.64.0.0/10")
    cgnat.add_argument("address")
    cgnat.set_defaults(handler=command_cgnat)

    glibc = commands.add_parser("glibc-check", help="exit 0 when objdump -T text on stdin needs glibc <= bound")
    glibc.add_argument("bound")
    glibc.set_defaults(handler=command_glibc_check)

    extract = commands.add_parser("mac-extract", help="extract one marked remote command result")
    extract.add_argument("file")
    extract.add_argument("n")
    extract.add_argument("payload")
    extract.set_defaults(handler=command_mac_extract)

    scan = commands.add_parser("scan", help="exit 0 when no pre-auth key prefix is under a directory")
    scan.add_argument("directory")
    scan.add_argument("--delete", action="store_true", help="remove every file that holds a hit")
    scan.set_defaults(handler=command_scan)
    seen = commands.add_parser("marker-seen", help="print the first marker visible in a terminal capture")
    seen.add_argument("file")
    seen.add_argument("markers", nargs="+")
    seen.set_defaults(handler=command_marker_seen)

    build = commands.add_parser("assemble", help="write the evidence document")
    build.add_argument("state_dir")
    build.add_argument("output")
    build.add_argument("--known-key-hits")
    build.set_defaults(handler=command_assemble)
    return root


def main(argv: Optional[List[str]] = None) -> int:
    """Run one helper command."""
    args = parser().parse_args(argv)
    try:
        return args.handler(args)
    except EvidenceError as error:
        print("overlay_evidence: {}".format(error), file=sys.stderr)
        return 3


if __name__ == "__main__":
    sys.exit(main())
