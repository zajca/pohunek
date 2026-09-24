#!/usr/bin/env python3
"""Evaluate and assemble the macOS launchd lifetime acceptance evidence.

`scripts/acceptance/macos-launchd-lifetime` drives the manual procedure and
stores every raw observation (CLI JSON, `ps`, `launchctl` exit statuses, boot
time) in its state directory. This helper is the only code that interprets
those files: small query commands answer the driver's questions, and
`assemble` turns the whole state directory into the evidence document
described in `docs/acceptance/README.md`. Evaluation is a pure function of the
state directory, so it is unit-tested on any host
(`scripts/tests/test_launchd_lifetime_evidence.py`).

Stdlib only and Python 3.9 compatible: the Xcode Command Line Tools ship
Python 3.9 as `/usr/bin/python3`.
"""

from __future__ import annotations

import argparse
import base64
import datetime
import json
from pathlib import Path
import sys
from typing import Any, Dict, List, Optional

SCHEMA = "pohunek.acceptance.launchd-lifetime"
SCHEMA_VERSION = 1
ISSUE = 100

# Ordered phases and the outcome each one must show. Lock and terminal close
# must leave every worker running; logout and reboot end the gui/<uid>
# domain, so reconciliation after the next login must report the runtime
# lost without restarting it.
PHASES = (
    ("screen-lock", "survive"),
    ("terminal-close", "survive"),
    ("logout-login", "lost"),
    ("reboot", "lost"),
)
EXPECTATIONS = dict(PHASES)

# Loss reason reconciliation records when a worker job ended while its
# journal still said live and the ownership-marker sweep confirmed cleanup.
EXPECTED_LOSS_REASON = "runtime_lost"

# `launchctl print gui/<uid>/<label>` exit status for an absent label.
LAUNCHCTL_ABSENT = 113

# Runtime states from which `session.resume` may start a new generation.
RECOVERABLE_STATES = ("lost", "terminal")

# Prefix the liveness probe's shell command prints before its token. The typed
# command echoes `ok-%s`, so only the executed command produces `ok-<token>`.
PROBE_PREFIX = "ok-"

# Worker arguments that tie a process to one session and generation.
SESSION_ID_ARGUMENT = "--session-id"
GENERATION_ARGUMENT = "--worker-generation"


class EvidenceError(Exception):
    """A raw observation file is missing or malformed."""


def read_text(path: Path) -> Optional[str]:
    """Return a file's stripped text, or None when it does not exist."""
    try:
        return path.read_text(encoding="utf-8").strip()
    except FileNotFoundError:
        return None


def read_envelope(path: Path) -> Any:
    """Return the `ok` payload of a CLI `--json` envelope."""
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise EvidenceError("missing {}".format(path)) from error
    except json.JSONDecodeError as error:
        raise EvidenceError("{} is not JSON: {}".format(path, error)) from error
    if not isinstance(document, dict) or "ok" not in document:
        raise EvidenceError("{} has no `ok` payload".format(path))
    return document["ok"]


def sessions_by_id(path: Path) -> Dict[str, Dict[str, Any]]:
    """Index a `pohunek session list --json` snapshot by session id."""
    payload = read_envelope(path)
    if not isinstance(payload, list):
        raise EvidenceError("{} is not a session list".format(path))
    return {entry["id"]: entry for entry in payload if isinstance(entry, dict) and "id" in entry}


def workers_by_session(path: Path) -> Dict[str, List[Dict[str, Any]]]:
    """Group the worker jobs of a `pohunek service status --json` snapshot."""
    payload = read_envelope(path)
    if not isinstance(payload, dict):
        raise EvidenceError("{} is not a service status".format(path))
    grouped: Dict[str, List[Dict[str, Any]]] = {}
    for worker in payload.get("workers") or []:
        grouped.setdefault(worker.get("session_id", ""), []).append(worker)
    return grouped


def runtime_field(session: Optional[Dict[str, Any]], field: str) -> Optional[Any]:
    """Return one field of a session's runtime, or None."""
    if not session:
        return None
    runtime = session.get("runtime") or {}
    return runtime.get(field)


def has_native_reference(session: Optional[Dict[str, Any]]) -> bool:
    """Whether the session captured the native reference `session.resume` needs."""
    if not session:
        return False
    return bool(session.get("native_session_id") or session.get("native_session_path"))


def resumable(session: Optional[Dict[str, Any]]) -> bool:
    """Whether the session has a frozen native resume operation."""
    if not session:
        return False
    return bool((session.get("capabilities") or {}).get("resume"))


def recovery_available(session: Optional[Dict[str, Any]]) -> bool:
    """Whether `session.resume` would accept the session right now."""
    return (
        resumable(session)
        and has_native_reference(session)
        and runtime_field(session, "state") in RECOVERABLE_STATES
    )


def single_worker(workers: Dict[str, List[Dict[str, Any]]], session_id: str) -> Optional[Dict[str, Any]]:
    """Return the only worker job of a session, or None when there is not exactly one."""
    jobs = workers.get(session_id) or []
    return jobs[0] if len(jobs) == 1 else None


def worker_label(namespace: str, session_id: str, generation: str) -> str:
    """Build the launchd label of one worker generation."""
    return "io.github.zajca.pohunek.{}.worker.{}.{}".format(namespace, session_id, generation)


def session_snapshot(session: Optional[Dict[str, Any]], worker: Optional[Dict[str, Any]]) -> Dict[str, Any]:
    """Summarize one session and its worker job for the evidence."""
    return {
        "present": session is not None,
        "session_state": session.get("state") if session else None,
        "runtime_state": runtime_field(session, "state"),
        "runtime_id": runtime_field(session, "runtime_id"),
        "loss_reason": runtime_field(session, "loss_reason"),
        "root_pid": session.get("pid") if session else None,
        "worker_generation": worker.get("generation") if worker else None,
        "worker_state": worker.get("state") if worker else None,
        "worker_pid": worker.get("pid") if worker else None,
    }


def check(name: str, passed: bool, detail: str = "") -> Dict[str, Any]:
    """Build one named check result."""
    return {"name": name, "passed": bool(passed), "detail": detail}


def read_launchctl(path: Path) -> Dict[str, int]:
    """Parse `<label> <exit status>` lines recorded by the driver."""
    text = read_text(path)
    statuses: Dict[str, int] = {}
    for line in (text or "").splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[1].lstrip("-").isdigit():
            statuses[parts[0]] = int(parts[1])
    return statuses


def worker_processes(ps_text: Optional[str], session_id: str) -> List[str]:
    """Return `ps` lines of processes that run as a worker of the session."""
    needle = "{} {}".format(SESSION_ID_ARGUMENT, session_id)
    return [
        line.strip()
        for line in (ps_text or "").splitlines()
        if needle in line and GENERATION_ARGUMENT in line
    ]


def probe_passed(path: Path, token: Optional[str]) -> bool:
    """Whether a `session screen --json` capture shows the probe's output line."""
    if not token:
        return False
    try:
        screen = read_envelope(path)
    except EvidenceError:
        return False
    expected = PROBE_PREFIX + token
    return any(line.strip() == expected for line in screen.get("visible_lines") or [])


def output_contains(path: Path, token: Optional[str]) -> bool:
    """Whether a `session output --json` capture contains the probe's output."""
    if not token:
        return False
    try:
        output = read_envelope(path)
        data = base64.b64decode(output.get("data_base64") or "")
    except (EvidenceError, ValueError):
        return False
    return (PROBE_PREFIX + token).encode("utf-8") in data


def phase_sessions(phase_dir: Path) -> List[str]:
    """Return the session ids a phase evaluates."""
    text = read_text(phase_dir / "sessions.txt")
    return [line.strip() for line in (text or "").splitlines() if line.strip()]


def evaluate_phase(state_dir: Path, name: str) -> Dict[str, Any]:
    """Evaluate one completed phase from its raw observations."""
    phase_dir = state_dir / "phases" / name
    expectation = EXPECTATIONS[name]
    result: Dict[str, Any] = {
        "name": name,
        "expectation": expectation,
        "started_at": read_text(phase_dir / "started_at"),
        "action_confirmed_at": read_text(phase_dir / "action_confirmed_at"),
        "completed_at": read_text(phase_dir / "completed_at"),
        "operator_note": read_text(phase_dir / "operator_note") or "",
        "checks": [],
        "sessions": [],
    }
    try:
        before_sessions = sessions_by_id(phase_dir / "before" / "sessions.json")
        after_sessions = sessions_by_id(phase_dir / "after" / "sessions.json")
        before_service = read_envelope(phase_dir / "before" / "service.json")
        after_service = read_envelope(phase_dir / "after" / "service.json")
        before_workers = workers_by_session(phase_dir / "before" / "service.json")
        after_workers = workers_by_session(phase_dir / "after" / "service.json")
    except EvidenceError as error:
        result["checks"].append(check("observations_readable", False, str(error)))
        result["passed"] = False
        return result

    ids = phase_sessions(phase_dir)
    result["checks"].append(check("sessions_prepared", bool(ids), "{} session(s)".format(len(ids))))

    boot_before = read_text(phase_dir / "before" / "boottime.txt")
    boot_after = read_text(phase_dir / "after" / "boottime.txt")
    daemon_before = (before_service.get("daemon") or {}).get("pid")
    daemon_after = (after_service.get("daemon") or {}).get("pid")
    daemon_state_after = (after_service.get("daemon") or {}).get("state")
    rebooted = boot_before is not None and boot_after is not None and boot_before != boot_after
    detail_boot = "before {!r}, after {!r}".format(boot_before, boot_after)
    detail_daemon = "pid before {}, after {} ({})".format(daemon_before, daemon_after, daemon_state_after)
    daemon_running = daemon_state_after == "running" and daemon_after is not None

    if name == "reboot":
        result["checks"].append(check("host_rebooted", rebooted, detail_boot))
    else:
        result["checks"].append(
            check("host_not_rebooted", boot_before is not None and not rebooted, detail_boot)
        )
    if expectation == "survive":
        result["checks"].append(
            check("daemon_unchanged", daemon_running and daemon_before == daemon_after, detail_daemon)
        )
    elif name == "logout-login":
        result["checks"].append(
            check(
                "daemon_restarted_at_login",
                daemon_running and daemon_before is not None and daemon_before != daemon_after,
                detail_daemon,
            )
        )
    else:
        result["checks"].append(check("daemon_running_after_login", daemon_running, detail_daemon))

    namespace = before_service.get("namespace") or ""
    launchctl = read_launchctl(phase_dir / "after" / "launchctl.txt")
    ps_text = read_text(phase_dir / "after" / "ps.txt")
    recovery_dir = phase_dir / "recovery"
    recovery_sessions: Dict[str, Dict[str, Any]] = {}
    recovery_workers: Dict[str, List[Dict[str, Any]]] = {}
    if expectation == "lost" and (recovery_dir / "sessions.json").exists():
        try:
            recovery_sessions = sessions_by_id(recovery_dir / "sessions.json")
            recovery_workers = workers_by_session(recovery_dir / "service.json")
        except EvidenceError as error:
            result["checks"].append(check("recovery_observations_readable", False, str(error)))

    recovery_expected_count = 0
    for session_id in ids:
        before = before_sessions.get(session_id)
        after = after_sessions.get(session_id)
        before_worker = single_worker(before_workers, session_id)
        after_worker = single_worker(after_workers, session_id)
        entry: Dict[str, Any] = {
            "session_id": session_id,
            "agent": before.get("agent") if before else None,
            "agent_base": before.get("agent_base") if before else None,
            "before": session_snapshot(before, before_worker),
            "after": session_snapshot(after, after_worker),
            "checks": [],
        }
        checks: List[Dict[str, Any]] = entry["checks"]
        checks.append(check("present_before", before is not None))
        checks.append(check("present_after", after is not None))
        checks.append(
            check(
                "one_worker_job_before",
                before_worker is not None and runtime_field(before, "state") == "live",
                "{} job(s), runtime {}".format(
                    len(before_workers.get(session_id) or []), runtime_field(before, "state")
                ),
            )
        )
        if expectation == "survive":
            evaluate_survival(entry, checks, before, after, before_worker, after_worker, phase_dir)
        else:
            expected = evaluate_loss(
                entry,
                checks,
                before,
                after,
                before_worker,
                after_workers,
                namespace,
                launchctl,
                ps_text,
                recovery_dir,
                recovery_sessions,
                recovery_workers,
            )
            if expected:
                recovery_expected_count += 1
        entry["passed"] = all(item["passed"] for item in checks)
        result["sessions"].append(entry)

    if expectation == "lost":
        result["checks"].append(
            check(
                "recovery_covered",
                recovery_expected_count > 0,
                "{} session(s) with a native recovery reference".format(recovery_expected_count),
            )
        )
    result["passed"] = all(item["passed"] for item in result["checks"]) and all(
        entry["passed"] for entry in result["sessions"]
    )
    return result


def evaluate_survival(
    entry: Dict[str, Any],
    checks: List[Dict[str, Any]],
    before: Optional[Dict[str, Any]],
    after: Optional[Dict[str, Any]],
    before_worker: Optional[Dict[str, Any]],
    after_worker: Optional[Dict[str, Any]],
    phase_dir: Path,
) -> None:
    """Check that one session kept the same worker, PTY, and child."""
    checks.append(check("runtime_live_after", runtime_field(after, "state") == "live",
                        "runtime {}".format(runtime_field(after, "state"))))
    runtime_before = runtime_field(before, "runtime_id")
    checks.append(
        check(
            "same_runtime_id",
            runtime_before is not None and runtime_before == runtime_field(after, "runtime_id"),
        )
    )
    generation_before = before_worker.get("generation") if before_worker else None
    generation_after = after_worker.get("generation") if after_worker else None
    checks.append(
        check(
            "same_worker_generation",
            generation_before is not None and generation_before == generation_after,
            "{} -> {}".format(generation_before, generation_after),
        )
    )
    pid_before = before_worker.get("pid") if before_worker else None
    pid_after = after_worker.get("pid") if after_worker else None
    checks.append(
        check(
            "same_worker_pid",
            pid_before is not None and pid_before == pid_after,
            "{} -> {}".format(pid_before, pid_after),
        )
    )
    root_before = before.get("pid") if before else None
    root_after = after.get("pid") if after else None
    checks.append(
        check(
            "same_child_pid",
            root_before is not None and root_before == root_after,
            "{} -> {}".format(root_before, root_after),
        )
    )
    if entry["agent_base"] == "shell":
        token = read_text(phase_dir / "probe" / "{}.token".format(entry["session_id"]))
        screen = phase_dir / "probe" / "{}.screen.json".format(entry["session_id"])
        output = phase_dir / "probe" / "{}.output.json".format(entry["session_id"])
        passed = probe_passed(screen, token) or output_contains(output, token)
        entry["liveness_probe"] = {"attempted": token is not None, "passed": passed}
        checks.append(check("pty_accepts_input_and_prints", passed))
    else:
        entry["liveness_probe"] = None


def evaluate_loss(
    entry: Dict[str, Any],
    checks: List[Dict[str, Any]],
    before: Optional[Dict[str, Any]],
    after: Optional[Dict[str, Any]],
    before_worker: Optional[Dict[str, Any]],
    after_workers: Dict[str, List[Dict[str, Any]]],
    namespace: str,
    launchctl: Dict[str, int],
    ps_text: Optional[str],
    recovery_dir: Path,
    recovery_sessions: Dict[str, Dict[str, Any]],
    recovery_workers: Dict[str, List[Dict[str, Any]]],
) -> bool:
    """Check that one session is lost, not resurrected, and recoverable.

    Returns whether the session was expected to be recoverable.
    """
    session_id = entry["session_id"]
    checks.append(check("runtime_lost", runtime_field(after, "state") == "lost",
                        "runtime {}".format(runtime_field(after, "state"))))
    reason = runtime_field(after, "loss_reason")
    checks.append(check("loss_reason", reason == EXPECTED_LOSS_REASON, "reason {}".format(reason)))
    remaining = after_workers.get(session_id) or []
    checks.append(
        check(
            "no_worker_job_after",
            not remaining,
            ", ".join(job.get("service_id", "?") for job in remaining),
        )
    )
    generation = before_worker.get("generation") if before_worker else None
    label = worker_label(namespace, session_id, generation) if generation and namespace else None
    status = launchctl.get(label) if label else None
    entry["old_generation"] = {"label": label, "launchctl_print_status": status}
    checks.append(
        check(
            "old_generation_label_absent",
            status == LAUNCHCTL_ABSENT,
            "launchctl print exit status {}".format(status),
        )
    )
    processes = worker_processes(ps_text, session_id)
    entry["old_generation"]["worker_processes"] = processes
    checks.append(
        check(
            "no_worker_process",
            ps_text is not None and not processes,
            "; ".join(processes) if ps_text is not None else "missing ps capture",
        )
    )

    expected = resumable(before) and has_native_reference(before)
    available = recovery_available(after)
    entry["recovery_expected"] = expected
    entry["recovery_available"] = available
    if not expected:
        entry["recovery"] = None
        return False
    checks.append(check("recovery_available", available))
    exit_status = read_text(recovery_dir / "{}.exit".format(session_id))
    recovered = recovery_sessions.get(session_id)
    recovered_worker = single_worker(recovery_workers, session_id)
    new_generation = recovered_worker.get("generation") if recovered_worker else None
    entry["recovery"] = {
        "attempted": exit_status is not None,
        "exit_status": int(exit_status) if exit_status and exit_status.lstrip("-").isdigit() else None,
        "runtime_state": runtime_field(recovered, "state"),
        "runtime_id": runtime_field(recovered, "runtime_id"),
        "worker_generation": new_generation,
    }
    recovery_ok = (
        exit_status == "0"
        and runtime_field(recovered, "state") == "live"
        and runtime_field(recovered, "runtime_id") not in (None, runtime_field(before, "runtime_id"))
        and new_generation is not None
        and new_generation != generation
    )
    checks.append(
        check(
            "explicit_recovery_starts_new_generation",
            recovery_ok,
            "exit {}, runtime {}, generation {} -> {}".format(
                exit_status, runtime_field(recovered, "state"), generation, new_generation
            ),
        )
    )
    return True


def read_json_file(path: Path) -> Dict[str, Any]:
    """Read a JSON object written by this helper."""
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise EvidenceError("missing {}".format(path)) from error
    except json.JSONDecodeError as error:
        raise EvidenceError("{} is not JSON: {}".format(path, error)) from error
    if not isinstance(value, dict):
        raise EvidenceError("{} is not a JSON object".format(path))
    return value


def assemble(state_dir: Path) -> Dict[str, Any]:
    """Build the evidence document from a state directory."""
    run = read_json_file(state_dir / "run.json")
    phases = []
    for name, _ in PHASES:
        if (state_dir / "phases" / name / "completed_at").exists():
            phases.append(evaluate_phase(state_dir, name))
    complete = [phase["name"] for phase in phases] == [name for name, _ in PHASES]
    return {
        "schema": SCHEMA,
        "schema_version": SCHEMA_VERSION,
        "issue": ISSUE,
        "run_id": run.get("run_id"),
        "started_at": run.get("started_at"),
        "completed_at": phases[-1]["completed_at"] if complete else None,
        "complete": complete,
        "passed": complete and all(phase["passed"] for phase in phases),
        "host": run.get("host"),
        "pohunek": run.get("pohunek"),
        "phases": phases,
    }


def utc_now() -> str:
    """Return the current UTC time in RFC 3339 form."""
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def command_init(args: argparse.Namespace) -> int:
    """Write run.json from the host facts the driver collected."""
    service = read_envelope(Path(args.service))
    run = {
        "run_id": args.run_id,
        "started_at": utc_now(),
        "host": {
            "product_name": args.product_name,
            "product_version": args.product_version,
            "build_version": args.build_version,
            "arch": args.arch,
            "hardware_model": args.hardware_model,
            "cpu": args.cpu,
            "uid": int(args.uid),
        },
        "pohunek": {
            "cli_version": args.cli_version,
            "active_version": service.get("active_version"),
            "namespace": service.get("namespace"),
            "prefix": service.get("prefix"),
        },
    }
    Path(args.output).write_text(json.dumps(run, indent=2) + "\n", encoding="utf-8")
    return 0


def command_service_field(args: argparse.Namespace) -> int:
    """Print one top-level field (or `daemon.<field>`) of a service status."""
    value: Any = read_envelope(Path(args.file))
    for part in args.field.split("."):
        value = value.get(part) if isinstance(value, dict) else None
    if value is None:
        print("")
    elif isinstance(value, (bool, dict, list)):
        print(json.dumps(value))
    else:
        print(value)
    return 0


def command_session_id(args: argparse.Namespace) -> int:
    """Print the id of a `session new --json` or `session resume --json` result."""
    payload = read_envelope(Path(args.file))
    session = payload.get("session", payload) if isinstance(payload, dict) else None
    if not isinstance(session, dict) or "id" not in session:
        raise EvidenceError("{} has no session id".format(args.file))
    print(session["id"])
    return 0


def command_states(args: argparse.Namespace) -> int:
    """Print `<id> <runtime state>` for each requested session."""
    sessions = sessions_by_id(Path(args.file))
    for session_id in args.ids:
        print(session_id, runtime_field(sessions.get(session_id), "state") or "absent")
    return 0


def command_missing_references(args: argparse.Namespace) -> int:
    """Print resumable sessions that have not captured a native reference yet."""
    sessions = sessions_by_id(Path(args.file))
    for session_id in args.ids:
        session = sessions.get(session_id)
        if resumable(session) and not has_native_reference(session):
            print(session_id)
    return 0


def command_recoverable(args: argparse.Namespace) -> int:
    """Print sessions `session.resume` would accept now."""
    sessions = sessions_by_id(Path(args.file))
    for session_id in args.ids:
        if recovery_available(sessions.get(session_id)):
            print(session_id)
    return 0


def command_shell_sessions(args: argparse.Namespace) -> int:
    """Print sessions whose agent base kind is a shell."""
    sessions = sessions_by_id(Path(args.file))
    for session_id in args.ids:
        if (sessions.get(session_id) or {}).get("agent_base") == "shell":
            print(session_id)
    return 0


def command_labels(args: argparse.Namespace) -> int:
    """Print the launchd label of each session's current worker generation."""
    service = read_envelope(Path(args.file))
    workers = workers_by_session(Path(args.file))
    namespace = service.get("namespace") or ""
    for session_id in args.ids:
        worker = single_worker(workers, session_id)
        if worker and namespace:
            print(worker_label(namespace, session_id, worker["generation"]))
    return 0


def command_assemble(args: argparse.Namespace) -> int:
    """Write the evidence document and print a one-line verdict per phase."""
    evidence = assemble(Path(args.state_dir))
    Path(args.output).write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
    for phase in evidence["phases"]:
        print("{:<15} {}".format(phase["name"], "PASS" if phase["passed"] else "FAIL"))
        for item in phase["checks"]:
            if not item["passed"]:
                print("  {} failed: {}".format(item["name"], item["detail"]))
        for session in phase["sessions"]:
            for item in session["checks"]:
                if not item["passed"]:
                    print("  {} {} failed: {}".format(session["session_id"], item["name"], item["detail"]))
    print("complete: {}, passed: {}".format(evidence["complete"], evidence["passed"]))
    return 0


def parser() -> argparse.ArgumentParser:
    """Build the command-line parser."""
    root = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = root.add_subparsers(dest="command", required=True)

    init = commands.add_parser("init", help="write run.json")
    for name in (
        "output", "service", "run-id", "product-name", "product-version", "build-version",
        "arch", "hardware-model", "cpu", "uid", "cli-version",
    ):
        init.add_argument("--" + name, required=True)
    init.set_defaults(handler=command_init)

    field = commands.add_parser("service-field", help="print a service status field")
    field.add_argument("file")
    field.add_argument("field")
    field.set_defaults(handler=command_service_field)

    session_id = commands.add_parser("session-id", help="print a created session's id")
    session_id.add_argument("file")
    session_id.set_defaults(handler=command_session_id)

    for name, handler in (
        ("states", command_states),
        ("missing-references", command_missing_references),
        ("recoverable", command_recoverable),
        ("shell-sessions", command_shell_sessions),
        ("labels", command_labels),
    ):
        sub = commands.add_parser(name)
        sub.add_argument("file")
        sub.add_argument("ids", nargs="*")
        sub.set_defaults(handler=handler)

    build = commands.add_parser("assemble", help="write the evidence document")
    build.add_argument("state_dir")
    build.add_argument("output")
    build.set_defaults(handler=command_assemble)
    return root


def main(argv: Optional[List[str]] = None) -> int:
    """Run one helper command."""
    args = parser().parse_args(argv)
    try:
        return args.handler(args)
    except EvidenceError as error:
        print("launchd_lifetime_evidence: {}".format(error), file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
