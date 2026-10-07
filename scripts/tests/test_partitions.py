"""Regression checks for the cost-shard coverage guard (stdlib only)."""

import importlib.machinery
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
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
                return subprocess.CompletedProcess(command, 0)

            with mock.patch.object(partitions, "ROOT", Path(root)), \
                    mock.patch.object(partitions, "require_archive_file"), \
                    mock.patch.object(partitions.subprocess, "run", side_effect=fake_run), \
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


class LeakedWorkerTests(unittest.TestCase):
    WORKER = (
        "/w/target/debug/pohunek-sessiond --session-id s-1 --worker-generation g "
        "--daemon-socket-path {socket}"
    )

    def table(self, *sockets):
        lines = [f"{100 + i} {self.WORKER.format(socket=s)}" for i, s in enumerate(sockets)]
        return "\n".join([*lines, "7 /usr/bin/sleep 1", "8 grep pohunek-sessiond"])

    def test_only_workers_below_the_run_base_are_leaks(self):
        table = self.table(
            "/tmp/pt-abc/ph-x/run/pohunek/daemon.sock",
            "/tmp/pt-abcd/ph-x/run/pohunek/daemon.sock",
            "/run/user/1000/pohunek/daemon.sock",
        )
        found = leaked_workers_for(table, "/tmp/pt-abc")
        self.assertEqual(found, [(100, "/tmp/pt-abc/ph-x/run/pohunek/daemon.sock")])

    def test_a_worker_without_a_socket_argument_is_not_a_leak(self):
        self.assertEqual(
            leaked_workers_for("5 /w/pohunek-sessiond --session-id s-1", "/tmp/pt-abc"), [],
        )

    def test_a_leak_fails_a_run_that_passed_and_is_terminated(self):
        table = self.table("/tmp/pt-run/ph-x/run/pohunek/daemon.sock")
        killed = []
        with mock.patch.object(partitions, "process_table", return_value=table), \
                mock.patch.object(partitions.os, "kill", side_effect=lambda *a: killed.append(a)), \
                mock.patch.object(sys, "platform", "linux"), \
                mock.patch.object(partitions.tempfile, "mkdtemp", return_value="/tmp/pt-run"), \
                mock.patch.object(partitions.shutil, "rmtree"), \
                mock.patch.object(Path, "resolve", lambda self: self), \
                mock.patch.object(
                    partitions.subprocess, "run",
                    return_value=subprocess.CompletedProcess([], 0),
                ):
            self.assertEqual(partitions.run_checked(["nextest"]), 1)
        self.assertEqual(killed, [(100, partitions.signal.SIGKILL)])

    def test_the_run_sees_a_base_of_its_own_as_tmpdir(self):
        seen = []

        def fake_run(command, **kwargs):
            seen.append(kwargs["env"]["TMPDIR"])
            return subprocess.CompletedProcess(command, 0)

        with mock.patch.object(partitions, "process_table", return_value=""), \
                mock.patch.object(sys, "platform", "linux"), \
                mock.patch.object(partitions.subprocess, "run", side_effect=fake_run):
            self.assertEqual(partitions.run_checked(["nextest"]), 0)
        self.assertTrue(Path(seen[0]).name.startswith(partitions.RUN_BASE_PREFIX))
        self.assertFalse(Path(seen[0]).exists(), "the base is removed after the run")


def leaked_workers_for(table, base):
    return [(pid, str(socket)) for pid, socket in partitions.leaked_workers(table, Path(base))]


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
                return subprocess.CompletedProcess(command, 0)

            with mock.patch.object(partitions, "ROOT", Path(root)), \
                    mock.patch.dict("os.environ", {"CARGO_TARGET_DIR": str(Path(root) / "elsewhere")}), \
                    mock.patch.object(partitions.subprocess, "run", side_effect=fake_run), \
                    mock.patch.object(sys, "argv", ["test-partitions", "run", "heavy"]):
                with self.assertRaises(SystemExit) as exit_:
                    partitions.main()
            self.assertEqual(exit_.exception.code, 0)
            self.assertEqual(seen, [False])
