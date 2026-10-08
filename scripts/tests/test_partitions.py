"""Regression checks for the cost-shard coverage guard (stdlib only)."""

import contextlib
import importlib.machinery
import importlib.util
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "test-partitions"
LOADER = importlib.machinery.SourceFileLoader("partitions", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
partitions = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(partitions)


class CoverageTests(unittest.TestCase):
    def setUp(self):
        self.selections = {
            name: {(f"binary-{name}", "test")} for name in partitions.SHARDS
        }
        self.universe = set.union(*self.selections.values())

    def test_disjoint_complete_partition(self):
        partitions.validate_coverage(self.universe, self.selections)

    def test_missing_test(self):
        with self.assertRaisesRegex(ValueError, "missing="):
            partitions.validate_coverage(self.universe | {("new", "test")}, self.selections)

    def test_overlapping_test(self):
        self.selections["cli"] |= self.selections["unit"]
        with self.assertRaisesRegex(ValueError, "overlap="):
            partitions.validate_coverage(self.universe, self.selections)

    def test_extra_test(self):
        self.selections["cli"].add(("extra", "test"))
        with self.assertRaisesRegex(ValueError, "extra="):
            partitions.validate_coverage(self.universe, self.selections)

    def test_empty_shard(self):
        self.selections["heavy"].clear()
        with self.assertRaisesRegex(ValueError, "empty=\\['heavy'\\]"):
            partitions.validate_coverage(self.universe, self.selections)

    def test_empty_inventory(self):
        with self.assertRaisesRegex(ValueError, "empty workspace"):
            partitions.validate_coverage(set(), self.selections)

    def test_wrong_shard_names(self):
        self.selections["typo"] = self.selections.pop("cli")
        with self.assertRaisesRegex(ValueError, "unexpected shard names"):
            partitions.validate_coverage(self.universe, self.selections)

    def test_inventory_keeps_ignored_but_not_filtered_tests(self):
        document = {"rust-suites": {"bin": {"testcases": {
            "ignored": {"ignored": True, "filter-match": {"status": "matches"}},
            "selected": {"ignored": False, "filter-match": {"status": "matches"}},
            "filtered": {"ignored": False, "filter-match": {"status": "mismatch"}},
        }}}}
        self.assertEqual(partitions.selected_tests(document), {
            ("bin", "ignored"), ("bin", "selected"),
        })

    def test_heavy_is_fast_complement_minus_relay_db(self):
        import tomllib
        with (Path(__file__).resolve().parents[2] / ".config/nextest.toml").open("rb") as config:
            fast = tomllib.load(config)["profile"]["fast"]["default-filter"]
        expressions = partitions.filters()
        self.assertEqual(set(expressions), set(partitions.SHARDS))
        for name in ("unit", "daemon", "relay", "cli"):
            self.assertIn(f"({fast}) and (", expressions[name])
        # Relay heavy tests are the relay-db shard; heavy is the remaining
        # fast-filter complement. Together they re-cover the full complement.
        self.assertEqual(expressions["relay-db"], f"(not ({fast})) and ({partitions.RELAY})")
        self.assertEqual(expressions["heavy"], f"(not ({fast})) and not ({partitions.RELAY})")


class ArchiveModeTests(unittest.TestCase):
    ARCHIVE = Path("/archives/nextest-archive.tar.zst")

    def test_build_mode_compiles_the_workspace(self):
        command = partitions.nextest_command("run", "heavy", None)
        self.assertEqual(command[:3], ["cargo", "nextest", "run"])
        self.assertIn("--workspace", command)
        self.assertIn("--all-features", command)
        self.assertNotIn("--archive-file", command)

    def test_archive_mode_extracts_into_the_checkout_and_never_builds(self):
        for direct in ("/bin/cargo-nextest", None):
            with self.subTest(direct=direct), \
                    mock.patch.object(partitions.shutil, "which", return_value=direct):
                command = partitions.nextest_command(
                    "list", "ci", partitions.Archive(self.ARCHIVE)
                )
                self.assertEqual(
                    command[:3], [direct, "nextest", "list"] if direct else ["cargo", "nextest", "list"]
                )
                self.assertEqual(command[command.index("--archive-file") + 1], str(self.ARCHIVE))
                self.assertEqual(command[command.index("--extract-to") + 1], str(partitions.ROOT))
                self.assertEqual(command[command.index("--workspace-remap") + 1], str(partitions.ROOT))
                self.assertIn("--extract-overwrite", command)
                self.assertNotIn("--workspace", command)
                self.assertNotIn("--all-features", command)

    def test_only_the_first_call_extracts_the_archive(self):
        archive = partitions.Archive(self.ARCHIVE)
        calls = [partitions.nextest_command("list", "ci", archive) for _ in range(3)]
        self.assertEqual([("--extract-to" in c) for c in calls], [True, False, False])
        self.assertEqual([("--archive-file" in c) for c in calls], [True, False, False])
        store = partitions.ROOT / "target" / "nextest"
        for command in calls[1:]:
            self.assertEqual(
                command[command.index("--binaries-metadata") + 1],
                str(store / "binaries-metadata.json"),
            )
            self.assertEqual(
                command[command.index("--cargo-metadata") + 1],
                str(store / "cargo-metadata.json"),
            )
            self.assertEqual(command[command.index("--workspace-remap") + 1], str(partitions.ROOT))

    def test_check_extracts_once_and_still_detects_overlap_and_gaps(self):
        shards = {name: {(f"bin-{name}", "t")} for name in partitions.SHARDS}
        universe = set.union(*shards.values())

        def run_check(selections):
            commands = []

            def fake_run(command, **kwargs):
                commands.append(command)
                return subprocess.CompletedProcess(command, 0, stdout=b"{}")

            answers = iter([universe, *[selections[n] for n in partitions.SHARDS]])
            with mock.patch.object(partitions, "require_archive_file"), \
                    mock.patch.object(partitions.subprocess, "run", side_effect=fake_run), \
                    mock.patch.object(partitions, "selected_tests", side_effect=lambda _d: next(answers)), \
                    mock.patch.object(sys, "argv", [
                        "test-partitions", "--archive-file", str(self.ARCHIVE), "check",
                    ]):
                partitions.main()
            return commands

        commands = run_check(shards)
        self.assertEqual(len(commands), 1 + len(partitions.SHARDS))
        self.assertEqual(sum("--extract-to" in c for c in commands), 1)
        overlapping = {**shards, "cli": shards["cli"] | shards["unit"]}
        with self.assertRaisesRegex(ValueError, "overlap="):
            run_check(overlapping)
        gapped = {**shards, "heavy": set()}
        with self.assertRaisesRegex(ValueError, "empty=|missing="):
            run_check(gapped)

    def test_profile_and_filter_survive_in_archive_mode(self):
        with tempfile.TemporaryDirectory() as root:
            archive = Path(root) / "a.tar.zst"
            config = Path(root) / ".config" / "nextest.toml"
            config.parent.mkdir()
            config.write_bytes((SCRIPT.parent.parent / ".config/nextest.toml").read_bytes())
            seen = []

            def fake_run(command, **kwargs):
                seen.append(command)
                return 0

            with mock.patch.object(partitions, "ROOT", Path(root)), \
                    mock.patch.object(partitions, "require_archive_file"), \
                    mock.patch.object(partitions, "run_checked", side_effect=fake_run), \
                    mock.patch.object(sys, "argv", [
                        "test-partitions", "--archive-file", str(archive), "run", "relay-db",
                    ]):
                with self.assertRaises(SystemExit):
                    partitions.main()
        command = seen[-1]
        self.assertEqual(command[command.index("--profile") + 1], "relay-db")
        self.assertEqual(command[command.index("-E") + 1], partitions.filters()["relay-db"])
        self.assertIn("--archive-file", command)

    def test_archive_built_at_another_path_is_accepted(self):
        with tempfile.TemporaryDirectory() as root:
            archive = Path(root) / "a.tar.zst"
            archive.write_bytes(b"archive built elsewhere")
            partitions.require_archive_file(archive)

    def test_missing_archive_is_refused(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaisesRegex(ValueError, "not found"):
                partitions.require_archive_file(Path(root) / "absent.tar.zst")


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


class EntrypointTests(unittest.TestCase):
    """The script run as a program, as CI runs it."""

    def run_script(self, *arguments):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *arguments],
            capture_output=True, text=True, check=False,
        )

    def test_filter_prints_the_shard_expression(self):
        for shard in partitions.SHARDS:
            with self.subTest(shard=shard):
                result = self.run_script("filter", shard)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), partitions.filters()[shard])
                self.assertNotEqual(result.stdout.strip(), "")

    def test_an_invalid_shard_or_action_fails(self):
        for arguments in (("filter", "bogus"), ("bogus", "unit"), ("run",), ()):
            with self.subTest(arguments=arguments):
                self.assertNotEqual(self.run_script(*arguments).returncode, 0)


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
            found = partitions.leaked_workers(processes, base)
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


if __name__ == "__main__":
    unittest.main()


class JunitReportTests(unittest.TestCase):
    def setUp(self):
        with (SCRIPT.parent.parent / ".config/nextest.toml").open("rb") as config:
            self.config = tomllib.load(config)

    def test_report_paths_follow_profile_inheritance(self):
        cases = {
            "ci": Path("r/target/nextest/ci/junit.xml"),
            "fast": Path("r/target/nextest/fast/junit.xml"),
            "heavy": Path("r/target/nextest/heavy/junit.xml"),
            "relay-db": Path("r/target/nextest/relay-db/junit.xml"),
            "local": None,
        }
        for profile, expected in cases.items():
            with self.subTest(profile=profile):
                self.assertEqual(partitions.junit_report(self.config, profile, "r"), expected)

    def test_store_dir_setting_moves_reports(self):
        config = {"store": {"dir": "out/nx"}, "profile": {"ci": {"junit": {"path": "j.xml"}}}}
        self.assertEqual(partitions.junit_report(config, "ci", "r"), Path("r/out/nx/ci/j.xml"))

    def test_inheritance_cycle_ends(self):
        config = {"profile": {"a": {"inherits": "b"}, "b": {"inherits": "a"}}}
        self.assertIsNone(partitions.junit_report(config, "a", "r"))

    def test_run_removes_a_stale_report_before_nextest_starts(self):
        with tempfile.TemporaryDirectory() as root:
            # CARGO_TARGET_DIR does not move nextest's store; the root does.
            stale = Path(root) / "target" / "nextest" / "heavy" / "junit.xml"
            stale.parent.mkdir(parents=True)
            stale.write_text("<testsuites/>")
            config = Path(root) / ".config" / "nextest.toml"
            config.parent.mkdir()
            config.write_bytes((SCRIPT.parent.parent / ".config/nextest.toml").read_bytes())
            seen = []

            def fake_run(command, **kwargs):
                seen.append(stale.exists())
                return 0

            with mock.patch.object(partitions, "ROOT", Path(root)), \
                    mock.patch.dict("os.environ", {"CARGO_TARGET_DIR": str(Path(root) / "elsewhere")}), \
                    mock.patch.object(partitions, "run_checked", side_effect=fake_run), \
                    mock.patch.object(sys, "argv", ["test-partitions", "run", "heavy"]):
                with self.assertRaises(SystemExit) as exit_:
                    partitions.main()
            self.assertEqual(exit_.exception.code, 0)
            self.assertEqual(seen, [False])
