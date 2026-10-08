"""Integration scenarios for the test-partitions CLI (stdlib only).

Every scenario runs `scripts/test-partitions` as a program, as CI runs it,
with its nextest side represented by a private fake `cargo`/`cargo-nextest`
executable and a fixture inventory: the CLI keeps talking to the nextest
service it selects on PATH, and the fake answers each `nextest list` with the
fixture's JSON and records its invocation. Nothing is compiled and nothing of
the real checkout is touched, so an `--archive-file` run proves the no-build
path without a real archive.

The process-cleanup scenarios at the bottom (LeakedWorkerTests) keep real
fixture processes: their workers and workloads are spawned, recognised in the
real process table, and reaped; the scenarios never signal a process outside
their own fixture root.
"""

import contextlib
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "test-partitions"
REPO_ROOT = SCRIPT.parent.parent
LOADER = importlib.machinery.SourceFileLoader("partitions", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
partitions = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(partitions)

# The CLI's shard arguments, as the operations they name are documented (the
# per-package fast shards, the relay-PostgreSQL shard, and the heavy
# complement): the inventory every check run must split exactly once.
SHARD_NAMES = ("unit", "daemon", "relay", "cli", "relay-db", "heavy")

# How long one scenario lets the script + its fake run before the scenario
# fails instead of hanging CI; a scenario finishes well under a second.
SCRIPT_TIMEOUT_SECONDS = 120


def shard_expressions(fast):
    """The six shard selections, composed from the repository's own decisions.

    Not a copy of the script's code, but of its documented composition: the
    fast filter from `.config/nextest.toml` bounds the per-package shards, and
    the fast complement is split between relay-db (the relay packages, the
    PostgreSQL fixture consumers) and heavy. `filters()` must keep producing
    exactly these strings.
    """
    daemon = ("package(=pohunek-daemon) or package(=pohunek-session-worker) "
              "or package(=pohunek-client)")
    relay = ("package(=pohunek-relay) or package(=pohunek-relay-client) "
             "or package(=pohunek-relay-protocol)")
    return {
        "daemon": f"({fast}) and ({daemon})",
        "relay": f"({fast}) and ({relay})",
        "cli": f"({fast}) and (package(=pohunek-cli))",
        "unit": f"({fast}) and (not (({daemon}) or ({relay}) or (package(=pohunek-cli))))",
        "relay-db": f"(not ({fast})) and ({relay})",
        "heavy": f"(not ({fast})) and not ({relay})",
    }


FAKE_NEXTEST = '''#!/usr/bin/env python3
"""A private cargo/nextest stand-in for the test-partitions scenarios.

It records every invocation it sees and answers a `nextest list` from the
fixture inventory ($PARTITIONS_FIXTURE, written by the scenario). It writes
nothing else, so an `--archive-file` run extracts nothing into any checkout
and only the fixture decides what the script's coverage guard observes.

Which fixture tests a filter selects is the fixture's business; the fake only
recognizes which shard a selection denotes, judged from the markers the
script's composition leaves in the expression itself, so a future edit that
breaks the composition makes the fake report the wrong shard and the check
fail loudly instead of passing on a shape the fixture agrees with anyway.
"""

import json
import os
from pathlib import Path
import sys


def shard_of(expression, fast):
    """The shard the filter expression denotes, or exit non-zero.

    The expression must be the fast filter with the wrap the script composes
    it in: `(fast) and (owner)` for the owned shards, `not ((owners))` for
    unit, and the fast complement split by the conjunction that follows it:
    relay-db takes the relay owners, heavy rejects them.
    """
    if expression.startswith("(not ("):
        rest = expression.removeprefix(f"(not ({fast})) ")
        if rest.startswith("and (package(=pohunek-relay"):
            return "relay-db"
        if rest.startswith("and not (package(=pohunek-relay"):
            return "heavy"
        # An owned shard composes `(fast) and (owner)`, and a fast filter
        # starting with `not (` makes that expression start `(not (` too.
        rest = expression.removeprefix(f"({fast}) and (")
        if rest.startswith("not ((package(=pohunek-daemon"):
            return "unit"
    else:
        rest = expression.removeprefix(f"({fast}) and (")
    for name in ("daemon", "relay", "cli"):
        if rest.startswith(f"package(=pohunek-{name}"):
            return name
    raise SystemExit(
        "fake nextest: unrecognized filter expression " + repr(expression))


def main():
    argv = sys.argv[1:]
    if argv[:2] not in (["nextest", "list"], ["nextest", "run"]):
        raise SystemExit("fake nextest: unexpected invocation " + repr(sys.argv))
    with open(os.environ["PARTITIONS_LOG"], "a") as log:
        stale_report = os.environ.get("PARTITIONS_STALE_REPORT")
        log.write(json.dumps({
            "name": Path(sys.argv[0]).name,
            "argv": argv,
            "stale_report_exists": Path(stale_report).exists() if stale_report else None,
        }) + "\\n")
    if argv[1] != "list":
        return

    fast = os.environ["PARTITIONS_FAST"]
    selected = None if "-E" not in argv else shard_of(argv[argv.index("-E") + 1], fast)
    fixture = json.loads(Path(os.environ["PARTITIONS_FIXTURE"]).read_text())
    suites = {}
    for entry in fixture["binaries"]:
        cases = {}
        for name, test in entry["tests"].items():
            if selected is None:
                status = "matches" if test.get("listed", True) else "mismatch"
            elif selected in test["matches"]:
                status = "matches"
            else:
                status = "mismatch"
            cases[name] = {
                "ignored": bool(test.get("ignored", False)),
                "filter-match": {"status": status},
            }
        suites[entry["id"]] = {"testcases": cases}
    print(json.dumps({"rust-suites": suites}))


main()
'''


def nextest_inventory():
    """A fixture inventory where each test matches exactly one shard.

    Recorded per test as the shards whose filter matches it. The names mirror
    the repository's package split: fast daemon tests belong to the daemon
    shard, their compiled companions without the fast filter to the heavy
    complement (which keeps ignored tests), non-fast relay tests to relay-db,
    and the remaining fast tests to their own shards.
    """
    return [
        {"id": "pohunek-daemon-111111", "tests": {
            "store::migrate": {"matches": ["daemon"], "ignored": False},
            "reconcile::worker_leak": {"matches": ["heavy"], "ignored": False},
            "upgrade_preflight::refuse": {"matches": ["heavy"], "ignored": True},
        }},
        {"id": "pohunek-relay-222222", "tests": {
            "acl::owner_allow": {"matches": ["relay"], "ignored": False},
            "marshal::append": {"matches": ["relay-db"], "ignored": False},
        }},
        {"id": "pohunek-cli-333333", "tests": {
            "service::upgrade": {"matches": ["cli"], "ignored": False},
        }},
        {"id": "pohunek-knowledge-444444", "tests": {
            "bundle::materialize": {"matches": ["unit"], "ignored": False},
            "source_map::drift": {"matches": ["heavy"], "ignored": False},
        }},
    ]


class ScriptScenario(unittest.TestCase):
    """Runs scripts/test-partitions as a program, with cargo/nextest faked.

    The script runs from a private checkout: its own file, copied
    byte-identical, with the repository's nextest.toml beside it, so whatever
    the script writes (its ROOT) stays in the fixture and a later scenario can
    also exercise the run mode safely. The private bin directory holds both
    `cargo` and `cargo-nextest`, the script's PATH lookup finds the fake
    either way, and the recorded invocations say which one it picked.
    """

    timeout_seconds = SCRIPT_TIMEOUT_SECONDS

    def setUp(self):
        root = tempfile.TemporaryDirectory(prefix="pohunek-partitions-")
        # The fixture root must live for the whole test: creating it inside a
        # `with` block removes it before the script runs, and the script's
        # own mkdir(parents=True) would then recreate an unmanaged root.
        self.addCleanup(root.cleanup)
        self.root = Path(root.name).resolve()
        self.script = self.root / "scripts" / "test-partitions"
        self.script.parent.mkdir(parents=True)
        # The actual CLI, byte-identical to the checkout's own file.
        self.script.write_bytes(SCRIPT.read_bytes())
        self.assertEqual(self.script.read_bytes(), SCRIPT.read_bytes())
        self.config = self.root / ".config" / "nextest.toml"
        self.config.parent.mkdir(parents=True)
        shutil.copyfile(REPO_ROOT / ".config" / "nextest.toml", self.config)
        self.bin = self.root / "bin"
        self.calls_log = self.root / "nextest-calls.jsonl"
        self.inventory = self.root / "inventory.json"
        self.bin.mkdir()
        for name in ("cargo", "cargo-nextest"):
            fake = self.bin / name
            fake.write_text(FAKE_NEXTEST, encoding="utf-8")
            fake.chmod(0o755)
        self.set_inventory([])

    def set_inventory(self, binaries):
        self.inventory.write_text(json.dumps({"binaries": binaries}))

    def run_script(self, *arguments, extra_env=None):
        environment = {**os.environ}
        environment.update(extra_env or {})
        # Our bin first: `which cargo-nextest` and every bare `cargo` lookup
        # find the fake, while the rest of PATH still resolves python3 for the
        # fake's shebang.
        environment["PATH"] = os.pathsep.join(
            (str(self.bin), environment.get("PATH", "")))
        environment["PARTITIONS_LOG"] = str(self.calls_log)
        environment["PARTITIONS_FIXTURE"] = str(self.inventory)
        # The fast filter the fake must strip from the shard expressions.
        environment["PARTITIONS_FAST"] = self.config_fast_filter()
        command = [sys.executable, str(self.script), *arguments]
        process = subprocess.Popen(
            command, cwd=self.root, env=environment, text=True,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            stdout, stderr = process.communicate(timeout=self.timeout_seconds)
        except subprocess.TimeoutExpired:
            # The CLI can have a fake nextest child. Kill the whole private
            # process group before reaping it, so a hung child cannot leak.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            _, stderr = process.communicate()
            self.fail(
                f"the script did not finish within {self.timeout_seconds} s: "
                f"{stderr[-500:]}")
        return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)

    def nextest_calls(self):
        if not self.calls_log.exists():
            return []
        return [json.loads(line) for line in self.calls_log.read_text().splitlines()]

    def config_fast_filter(self):
        """The fixture checkout's nextest.toml is what the script reads."""
        with self.config.open("rb") as config:
            return tomllib.load(config)["profile"]["fast"]["default-filter"]


class CheckActionTests(ScriptScenario):
    """`check` verifies the whole partition through nextest itself."""

    def test_a_covering_partition_is_reported_test_by_test(self):
        # The numbers pin the ignored case: heavy's three include the one
        # ignored fixture test, and the eight are only exact with it.
        self.set_inventory(nextest_inventory())
        result = self.run_script("check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("unit: 1 tests", result.stdout)
        self.assertIn("daemon: 1 tests", result.stdout)
        self.assertIn("relay: 1 tests", result.stdout)
        self.assertIn("cli: 1 tests", result.stdout)
        self.assertIn("relay-db: 1 tests", result.stdout)
        self.assertIn("heavy: 3 tests", result.stdout)
        self.assertIn(
            "Coverage OK: 8 tests selected exactly once (including ignored tests)",
            result.stdout,
        )

    def test_the_build_mode_asks_nextest_for_the_compiled_workspace(self):
        self.set_inventory(nextest_inventory())
        self.assertEqual(self.run_script("check").returncode, 0)
        calls = self.nextest_calls()
        # Without an archive the script asks for the workspace build: the full
        # inventory plus one list per shard.
        self.assertEqual([call["name"] for call in calls], ["cargo"] * 7)
        argvs = [call["argv"] for call in calls]
        first, rest = argvs[0], argvs[1:]
        self.assertEqual(first[:4], ["nextest", "list", "--profile", "ci"])
        self.assertIn("--workspace", first)
        self.assertIn("--all-features", first)
        self.assertNotIn("-E", first)
        for argv in rest:
            self.assertIn("--workspace", argv)
            self.assertIn("--all-features", argv)
            self.assertIn("-E", argv)
        # Every list, base inventory or shard selection, runs with nextest's
        # ignored tests included: the coverage check counts them.
        for argv in argvs:
            self.assertEqual(argv[argv.index("--run-ignored") + 1], "all")
            self.assertEqual(argv[argv.index("--message-format") + 1], "json")
        self.assertEqual(len({argv[argv.index("-E") + 1] for argv in rest}), 6)

    def test_a_test_two_shards_match_is_rejected_as_an_overlap(self):
        # A nextest whose daemon filter also swallowed a cli test: the guard
        # must catch selections that stop partitioning.
        inventory = nextest_inventory()
        inventory[2]["tests"]["service::upgrade"]["matches"] = ["cli", "daemon"]
        self.set_inventory(inventory)
        result = self.run_script("check")
        self.assertEqual(result.returncode, 1)
        self.assertIn("partition coverage failed", result.stderr)
        self.assertIn("overlap=", result.stderr)
        self.assertIn("service::upgrade", result.stderr)

    def test_a_test_no_shard_matches_is_reported_missing(self):
        inventory = nextest_inventory()
        inventory[3]["tests"]["source_map::drift"]["matches"] = []
        self.set_inventory(inventory)
        result = self.run_script("check")
        self.assertEqual(result.returncode, 1)
        self.assertIn("partition coverage failed", result.stderr)
        self.assertIn("missing=", result.stderr)
        self.assertIn("source_map::drift", result.stderr)

    def test_a_selection_outside_the_inventory_is_rejected_as_extra(self):
        # A nextest whose shard selection reports a test its full inventory
        # omitted: the guard must refuse the inconsistent inventory.
        inventory = nextest_inventory()
        inventory[3]["tests"]["source_map::drift"] = {
            "matches": ["heavy"], "ignored": False, "listed": False,
        }
        self.set_inventory(inventory)
        result = self.run_script("check")
        self.assertEqual(result.returncode, 1)
        self.assertIn("partition coverage failed", result.stderr)
        self.assertIn("extra=", result.stderr)
        self.assertIn("source_map::drift", result.stderr)

    def test_an_empty_shard_selection_is_rejected(self):
        # A fast-filter edit that cut the relay shard's whole selection.
        inventory = nextest_inventory()
        inventory[1]["tests"]["acl::owner_allow"]["matches"] = ["relay-db"]
        self.set_inventory(inventory)
        result = self.run_script("check")
        self.assertEqual(result.returncode, 1)
        self.assertIn("partition coverage failed", result.stderr)
        self.assertIn("empty=['relay']", result.stderr)

    def test_an_empty_workspace_inventory_is_rejected(self):
        # The default fixture: a nextest that lists no compiled test at all.
        result = self.run_script("check")
        self.assertEqual(result.returncode, 1)
        self.assertIn("empty workspace test inventory", result.stderr)
        self.assertEqual(len(self.nextest_calls()), 7)


class ArchiveActionTests(ScriptScenario):
    """The archive keeps CI's shared-build rules: extract once, never build."""

    def setUp(self):
        super().setUp()
        self.archive = self.root / "built-elsewhere.tar.zst"
        self.archive.write_bytes(b"cargo nextest archive built at another path")

    def test_check_extracts_once_and_lists_without_building(self):
        self.set_inventory(nextest_inventory())
        result = self.run_script("check", "--archive-file", str(self.archive))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(
            "Coverage OK: 8 tests selected exactly once (including ignored tests)",
            result.stdout,
        )
        calls = self.nextest_calls()
        # One extraction for the whole check, then only the extracted state's
        # metadata: no `cargo` run, no workspace or feature selection.
        self.assertEqual([call["name"] for call in calls], ["cargo-nextest"] * 7)
        argvs = [call["argv"] for call in calls]
        first, rest = argvs[0], argvs[1:]
        self.assertEqual(first[:4], ["nextest", "list", "--profile", "ci"])
        self.assertEqual(first[first.index("--archive-file") + 1], str(self.archive))
        self.assertIn("--extract-overwrite", first)
        self.assertEqual(first[first.index("--extract-to") + 1], str(self.root))
        self.assertEqual(first[first.index("--workspace-remap") + 1], str(self.root))
        self.assertNotIn("--binaries-metadata", first)
        self.assertEqual(["--extract-to" in argv for argv in argvs].count(True), 1)
        store = self.root / "target" / "nextest"
        for argv in rest:
            self.assertEqual(
                argv[argv.index("--binaries-metadata") + 1],
                str(store / "binaries-metadata.json"))
            self.assertEqual(
                argv[argv.index("--cargo-metadata") + 1],
                str(store / "cargo-metadata.json"))
            self.assertEqual(argv[argv.index("--workspace-remap") + 1], str(self.root))
            self.assertEqual(argv[argv.index("--target-dir-remap") + 1], str(store.parent))
            self.assertNotIn("--archive-file", argv)
            self.assertNotIn("--extract-to", argv)
            self.assertIn("-E", argv)
        self.assertEqual(len({argv[argv.index("-E") + 1] for argv in rest}), 6)

    def test_the_archive_mode_still_fails_an_overlapping_partition(self):
        inventory = nextest_inventory()
        inventory[2]["tests"]["service::upgrade"]["matches"] = ["cli", "daemon"]
        self.set_inventory(inventory)
        result = self.run_script("check", "--archive-file", str(self.archive))
        self.assertEqual(result.returncode, 1)
        self.assertIn("partition coverage failed", result.stderr)
        self.assertIn("overlap=", result.stderr)
        self.assertEqual(len(self.nextest_calls()), 7)

    def test_an_archive_built_at_any_path_is_accepted_without_calling_nextest(self):
        result = self.run_script("filter", "unit", "--archive-file", str(self.archive))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout.strip(),
            shard_expressions(self.config_fast_filter())["unit"])
        self.assertEqual(self.nextest_calls(), [])

    def test_a_missing_archive_is_refused_without_calling_nextest(self):
        result = self.run_script(
            "filter", "unit", "--archive-file", str(self.root / "absent.tar.zst"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("nextest archive not found", result.stderr)
        self.assertEqual(self.nextest_calls(), [])


class EntrypointTests(ScriptScenario):
    """The script run as a program, as CI runs it."""

    def test_filter_prints_the_shard_expression(self):
        expected = shard_expressions(self.config_fast_filter())
        printed = {}
        for shard in SHARD_NAMES:
            result = self.run_script("filter", shard)
            self.assertEqual(result.returncode, 0, result.stderr)
            printed[shard] = result.stdout.strip()
        self.assertEqual(printed, expected)
        self.assertEqual(len(set(printed.values())), len(printed))

    def test_an_invalid_shard_or_action_fails(self):
        for arguments in (("filter", "bogus"), ("bogus", "unit"), ("run",), ()):
            with self.subTest(arguments=arguments):
                self.assertNotEqual(self.run_script(*arguments).returncode, 0)


def wait_for(condition, seconds=30):
    """Poll `condition` until it holds; fail the test after `seconds`."""
    deadline = time.monotonic() + seconds
    while not condition():
        if time.monotonic() > deadline:
            raise AssertionError("condition not reached before the deadline")
        time.sleep(0.01)


def gone_or_zombie(pid):
    """True once `pid` was reaped or is a zombie.

    One read, not exists-then-read: the process can be reaped between two
    calls, and reading the stat of an exiting process can also fail with ESRCH.
    """
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except (FileNotFoundError, ProcessLookupError):
        return True
    return ") Z " in stat


class LeakedWorkerTests(unittest.TestCase):
    def worker(self, socket, pid=100, executable="/w/target/debug/pohunek-sessiond", start="1"):
        argv = [executable, "--session-id", "s-1", "--daemon-socket-path", socket]
        return partitions.Process(pid, start, argv)

    def test_only_workers_below_the_run_base_are_leaks(self):
        processes = [
            self.worker("/tmp/ab12/ph-x/run/pohunek/daemon.sock", pid=100),
            self.worker("/tmp/ab123/ph-x/run/pohunek/daemon.sock", pid=101),
            self.worker("/run/user/1000/pohunek/daemon.sock", pid=102),
            partitions.Process(7, "1", ["/usr/bin/sleep", "1"]),
        ]
        found = partitions.leaked_workers(processes, Path("/tmp/ab12"))
        self.assertEqual([process.pid for process, _ in found], [100])

    def test_spaces_in_the_executable_and_the_socket_path_are_kept(self):
        process = self.worker("/tmp/ab 12/ph-x/daemon.sock", executable="/home/a b/target/pohunek-sessiond")
        found = partitions.leaked_workers([process], Path("/tmp/ab 12"))
        self.assertEqual([socket for _, socket in found], ["/tmp/ab 12/ph-x/daemon.sock"])
        self.assertEqual(partitions.leaked_workers([process], Path("/tmp/ab")), [])

    def test_a_worker_without_a_socket_argument_is_not_a_leak(self):
        process = partitions.Process(5, "1", ["/w/pohunek-sessiond", "--session-id", "s-1"])
        self.assertEqual(partitions.leaked_workers([process], Path("/tmp/ab12")), [])

    def test_the_replaced_executable_is_recognized_by_its_link(self):
        process = partitions.Process(
            5, "1", ["renamed", "--daemon-socket-path", "/tmp/ab12/d.sock"],
            executable="/w/pohunek-sessiond (deleted)",
        )
        self.assertEqual(len(partitions.leaked_workers([process], Path("/tmp/ab12"))), 1)

    def test_stat_fields_are_counted_after_the_last_parenthesis(self):
        stat = "42 (we ird) name) S 7 42 42 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 987654 1 2"
        self.assertEqual(partitions.start_time_of(stat), "987654")

    def test_the_base_fits_the_deepest_nested_socket_layout(self):
        self.assertEqual(partitions.RUN_BASE_MAX_LENGTH, 9)
        base = partitions.make_run_base()
        try:
            self.assertLessEqual(len(str(base)), partitions.RUN_BASE_MAX_LENGTH)
            self.assertEqual(base.stat().st_mode & 0o777, 0o700)
        finally:
            base.rmdir()

    def test_the_workload_is_the_descendants_and_the_sessions_they_lead(self):
        def proc(pid, parent, session):
            return partitions.Process(pid, f"s{pid}", [], None, parent, session)

        worker = proc(100, 1, 50)
        processes = [
            worker,
            proc(101, 100, 50),
            proc(102, 100, 102),   # PTY child leading its own session
            proc(103, 102, 102),
            proc(104, 1, 102),     # straggler reparented to init, same session
            proc(105, 101, 50),    # descendant of a descendant
            proc(106, 1, 50),      # shares only the worker's session
            proc(107, 1, 107),     # unrelated
        ]
        self.assertEqual(
            sorted(p.pid for p in partitions.workload_of(processes, worker)),
            [101, 102, 103, 104, 105],
        )

    @unittest.skipUnless(sys.platform == "linux", "reads /proc and uses setsid")
    def test_a_signal_resistant_workload_is_killed_with_its_leaked_worker(self):
        with tempfile.TemporaryDirectory() as root, contextlib.ExitStack() as scope:
            base = Path(root)
            pid_file = base / "workload.pid"
            executable = base / "pohunek-sessiond"
            executable.symlink_to("/bin/sh")
            script = (
                f"setsid -w sh -c 'trap \"\" HUP TERM; echo $$ > {pid_file}; sleep 613; true' & wait"
            )
            worker = subprocess.Popen(
                [str(executable), "-c", script, "--daemon-socket-path", str(base / "d.sock")],
                start_new_session=True,
            )
            # The scope exits before the temporary directory is removed, so the
            # recorded PID file still exists when the workload is killed, also
            # when an assertion fails early.
            scope.callback(worker.wait)
            scope.callback(self.kill_group, worker)
            scope.callback(self.kill_recorded_workload, pid_file)
            wait_for(lambda: pid_file.exists() and pid_file.read_text().strip())
            workload = int(pid_file.read_text())
            self.assertTrue(Path(f"/proc/{workload}").exists())
            self.assertFalse(partitions.check_no_leaked_workers(base))
            worker.wait()
            wait_for(lambda: gone_or_zombie(workload))

    def test_a_retained_session_is_followed_after_its_leader_was_reparented(self):
        def proc(pid, parent, session):
            return partitions.Process(pid, f"s{pid}", [], None, parent, session)

        worker = proc(100, 1, 50)
        processes = [worker, proc(102, 1, 102), proc(103, 102, 102)]
        self.assertEqual(partitions.workload_of(processes, worker), [])
        found = partitions.workload_of(processes, worker, {(102, "s102")})
        self.assertEqual(sorted(p.pid for p in found), [102, 103])
        recycled = partitions.workload_of(processes, worker, {(102, "other")})
        self.assertEqual(recycled, [])

    @unittest.skipUnless(sys.platform == "linux", "reads /proc and uses setsid")
    def test_a_workload_whose_intermediate_parent_exited_is_still_killed(self):
        with tempfile.TemporaryDirectory() as root, contextlib.ExitStack() as scope:
            base = Path(root)
            pid_file = base / "workload.pid"
            inner = base / "inner.sh"
            inner.write_text(f"trap '' HUP TERM; echo $$ > {pid_file}; sleep 613; true")
            executable = base / "pohunek-sessiond"
            executable.symlink_to("/bin/sh")
            script = f"sleep 613 & sh -c 'setsid sh {inner}; true' & wait"
            worker = subprocess.Popen(
                [str(executable), "-c", script, "--daemon-socket-path", str(base / "d.sock")],
                start_new_session=True,
            )
            # The scope exits before the temporary directory is removed, so the
            # recorded PID file still exists when the workload is killed, also
            # when an assertion fails early.
            scope.callback(worker.wait)
            scope.callback(self.kill_group, worker)
            scope.callback(self.kill_recorded_workload, pid_file)
            wait_for(lambda: pid_file.exists() and pid_file.read_text().strip())
            leader = int(pid_file.read_text())

            retained = set()
            partitions.retain_sessions(partitions.read_processes(), base, retained)
            self.assertEqual({session for session, _ in retained}, {leader})

            def parent_of(pid):
                stat = Path(f"/proc/{pid}/stat").read_text()
                return partitions.stat_fields(stat)[0]

            intermediate = parent_of(leader)
            self.assertNotEqual(intermediate, worker.pid)
            os.kill(intermediate, signal.SIGKILL)
            wait_for(lambda: parent_of(leader) != intermediate)

            processes = partitions.read_processes()
            found = partitions.leaked_workers(partitions.read_processes(), base)
            self.assertNotIn(leader, [p.pid for p in partitions.workload_of(processes, found[0][0])])

            self.assertFalse(partitions.check_no_leaked_workers(base, retained))
            wait_for(lambda: gone_or_zombie(leader))

    @unittest.skipUnless(sys.platform == "linux", "reads /proc and uses setsid")
    def test_the_workload_dies_even_when_the_test_fails_early(self):
        pid_file_seen = []

        class Failing(unittest.TestCase):
            def runTest(inner):
                with tempfile.TemporaryDirectory() as root, contextlib.ExitStack() as scope:
                    pid_file = Path(root) / "workload.pid"
                    worker = subprocess.Popen(
                        ["/bin/sh", "-c",
                         f"setsid -w sh -c 'echo $$ > {pid_file}; sleep 613; true' & wait"],
                        start_new_session=True,
                    )
                    scope.callback(worker.wait)
                    scope.callback(self.kill_group, worker)
                    scope.callback(self.kill_recorded_workload, pid_file)
                    wait_for(lambda: pid_file.exists() and pid_file.read_text().strip())
                    pid_file_seen.append(int(pid_file.read_text()))
                    inner.fail("early assertion failure")

        result = unittest.TestResult()
        Failing().run(result)
        self.assertEqual(len(result.failures), 1)
        wait_for(lambda: gone_or_zombie(pid_file_seen[0]))

    @staticmethod
    def kill_group(child):
        """SIGKILL the stand-in and the `sleep` it started, which share its group."""
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass

    @staticmethod
    def kill_recorded_workload(pid_file):
        """SIGKILL the session the stand-in recorded: its leader and the `sleep` it started."""
        try:
            os.killpg(int(pid_file.read_text()), signal.SIGKILL)
        except (OSError, ValueError):
            pass

    def spawn_stand_in(self, base, scope, name="pohunek-sessiond"):
        """A real process whose argv names a worker below `base`, and its Process."""
        executable = base / name
        executable.symlink_to("/bin/sh")
        child = subprocess.Popen(
            [str(executable), "-c", "sleep 30; true", "--daemon-socket-path", str(base / "d.sock")],
            start_new_session=True,
        )
        scope.callback(child.wait)
        scope.callback(self.kill_group, child)
        found = None
        while found is None:
            found = next(
                (p for p in partitions.read_processes() if p.pid == child.pid
                 and partitions.SOCKET_PATH_ARGUMENT in p.argv),
                None,
            )
        return found

    @unittest.skipUnless(sys.platform == "linux", "reads /proc")
    def test_a_recycled_pid_is_not_killed_and_a_matching_process_is(self):
        with tempfile.TemporaryDirectory() as root, contextlib.ExitStack() as scope:
            process = self.spawn_stand_in(Path(root), scope)
            recycled = partitions.Process(process.pid, process.start + "1", process.argv)
            self.assertFalse(partitions.kill_if_unchanged(recycled))
            self.assertIn(process.pid, [p.pid for p in partitions.read_processes()])
            self.assertTrue(partitions.kill_if_unchanged(process))

    @unittest.skipUnless(sys.platform == "linux", "reads /proc")
    def test_paths_with_spaces_survive_the_real_process_table(self):
        with tempfile.TemporaryDirectory(prefix="a b ") as root, contextlib.ExitStack() as scope:
            process = self.spawn_stand_in(Path(root), scope)
            found = partitions.leaked_workers(partitions.read_processes(), Path(root))
            self.assertEqual([p.pid for p, _ in found], [process.pid])
            partitions.kill_if_unchanged(process)

    def run_with(self, runner, leaks):
        """`run_checked` with a mocked runner process and a fixed set of leaks."""
        events = []
        base = Path("/tmp/zz99")
        with mock.patch.object(sys, "platform", "linux"), \
                mock.patch.object(partitions, "make_run_base", return_value=base), \
                mock.patch.object(partitions.subprocess, "Popen", return_value=runner), \
                mock.patch.object(partitions, "read_processes", return_value=leaks), \
                mock.patch.object(partitions, "kill_if_unchanged",
                                  side_effect=lambda p: events.append(("kill", p.pid))), \
                mock.patch.object(partitions, "remove_tree",
                                  side_effect=lambda *a, **k: events.append(("rmtree",))):
            try:
                return partitions.run_checked(["nextest"]), events
            except BaseException as error:
                return error, events

    def stand_in_runner(self, wait, running=False):
        runner = mock.Mock()
        runner.wait.side_effect = wait
        runner.poll.return_value = None if running else 0
        return runner

    def test_a_leak_fails_a_run_that_passed_and_is_terminated(self):
        leaks = [self.worker("/tmp/zz99/ph-x/d.sock", pid=100)]
        status, events = self.run_with(self.stand_in_runner([0]), leaks)
        self.assertEqual(status, 1)
        self.assertEqual(events, [("kill", 100), ("rmtree",)])

    def test_remove_tree_removes_directories_without_permissions(self):
        with tempfile.TemporaryDirectory() as root:
            tree = Path(root) / "base"
            locked = tree / "a" / "b"
            locked.mkdir(parents=True)
            (locked / "f").write_text("x")
            locked.chmod(0)
            partitions.remove_tree(tree)
            self.assertFalse(tree.exists())

    def test_a_clean_run_keeps_its_status(self):
        status, events = self.run_with(self.stand_in_runner([3]), [])
        self.assertEqual(status, 3)
        self.assertEqual(events, [("rmtree",)])

    def test_a_cancelled_run_stops_the_runner_cleans_workers_and_reraises(self):
        leaks = [self.worker("/tmp/zz99/ph-x/d.sock", pid=100)]
        runner = self.stand_in_runner([KeyboardInterrupt(), 0], running=True)
        error, events = self.run_with(runner, leaks)
        self.assertIsInstance(error, KeyboardInterrupt)
        runner.terminate.assert_called_once()
        self.assertEqual(events, [("kill", 100), ("rmtree",)], "workers are cleaned before the base is removed")

    def test_the_run_sees_a_base_of_its_own_as_tmpdir(self):
        seen = []

        def fake_popen(command, **kwargs):
            seen.append(kwargs["env"]["TMPDIR"])
            runner = mock.Mock()
            runner.wait.return_value = 0
            runner.poll.return_value = 0
            return runner

        with mock.patch.object(partitions, "read_processes", return_value=[]), \
                mock.patch.object(sys, "platform", "linux"), \
                mock.patch.object(partitions.subprocess, "Popen", side_effect=fake_popen):
            self.assertEqual(partitions.run_checked(["nextest"]), 0)
        self.assertEqual(len(seen[0]), partitions.RUN_BASE_MAX_LENGTH)
        self.assertFalse(Path(seen[0]).exists(), "the base is removed after the run")



class JunitReportActionTests(ScriptScenario):
    """The run command discards only the previous report for its nextest profile."""

    def stale_report(self, profile, store="target/nextest"):
        report = self.root / store / profile / "junit.xml"
        report.parent.mkdir(parents=True, exist_ok=True)
        report.write_text("<testsuites/>")
        return report

    def test_run_removes_the_effective_profiles_report_before_nextest(self):
        profiles = (("unit", "ci"), ("heavy", "heavy"), ("relay-db", "relay-db"))
        for shard, profile in profiles:
            with self.subTest(shard=shard):
                stale = self.stale_report(profile)
                unrelated = self.stale_report("local")
                result = self.run_script(
                    "run", shard,
                    extra_env={
                        "CARGO_TARGET_DIR": str(self.root / "elsewhere"),
                        "PARTITIONS_STALE_REPORT": str(stale),
                    },
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertFalse(stale.exists())
                self.assertTrue(unrelated.exists())
                self.assertEqual(
                    self.nextest_calls()[-1]["argv"][1:4],
                    ["run", "--profile", profile],
                )
                self.assertFalse(self.nextest_calls()[-1]["stale_report_exists"])

    def test_run_honors_configured_nextest_store_dir(self):
        with self.config.open("a") as config:
            config.write('\n[store]\ndir = "out/nx"\n')
        stale = self.stale_report("ci", store="out/nx")
        result = self.run_script(
            "run", "unit", extra_env={"PARTITIONS_STALE_REPORT": str(stale)})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(stale.exists())
        self.assertFalse(self.nextest_calls()[-1]["stale_report_exists"])

    def test_cyclic_profile_inheritance_terminates_without_deleting_report(self):
        config = self.config.read_text()
        config = config.replace(
            '[profile.heavy]\ninherits = "ci"',
            '[profile.heavy]\ninherits = "a"',
        )
        config += '\n[profile.a]\ninherits = "b"\n[profile.b]\ninherits = "a"\n'
        self.config.write_text(config)
        stale = self.stale_report("heavy")
        result = self.run_script("run", "heavy")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(stale.exists())


if __name__ == "__main__":
    unittest.main()
