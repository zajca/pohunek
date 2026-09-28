"""Regression checks for the local dev-loop measurement helper (stdlib only).

No test reaches a real cargo, hyperfine, or gh subprocess: the executor /
`which` functions are injected everywhere, and destructive behavior is
exercised inside temporary directories only.
"""

import argparse
import contextlib
import io
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
import unittest.mock

SCRIPT = Path(__file__).resolve().parents[1] / "measure-dev-loop"
LOADER = importlib.machinery.SourceFileLoader("measure_dev_loop", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
measure_dev_loop = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(measure_dev_loop)

HYPERFINE_PAYLOAD = {
    "results": [
        {
            "command": "cargo build --workspace --all-targets "
                       "--all-features --timings",
            "median": 0.912345,
            "stddev": 0.031212,
            "runs": [{"time": 0.9}, {"time": 1.0}, {"time": 0.85}],
        }
    ]
}

# A `/proc/cpuinfo` shape the metadata parser must survive verbatim.
CPUINFO = "\n".join(
    [
        "processor\t: 0",
        "vendor_id\t: GenuineIntel",
        "model name\t: Intel(R) Core(TM) i9-9900 CPU @ 3.10GHz",
        "",
        "processor\t: 1",
        "model name\t: Intel(R) Core(TM) i9-9900 CPU @ 3.10GHz",
    ]
)

MEMINFO = "\n".join(["MemTotal: 32768000 kB", "HugePages_Total: 0", ""])

RUSTC_VV = (
    "rustc 1.96.0 (abc123 2026-01-01)\nbinary: rustc\nrelease: 1.96.0\n"
)


def _args(tempdir, **overrides):
    """Run-mode args pointed at temporary directories."""
    defaults = {
        "cases": "all",
        "target_dir": str(Path(tempdir) / "target-dir"),
        "out": str(Path(tempdir) / "out"),
        "runs": 3,
        "warmup": 1,
        "dry_run": True,
    }
    return argparse.Namespace(**{**defaults, **overrides})


def present_tool(name):
    """A `shutil.which` stand-in that finds every tool.

    `command_run` binds its `which` default at definition time, so patching
    `shutil.which` cannot reach it; tests inject this instead, keeping them
    independent of the tools installed on the machine running them.
    """
    return "/usr/bin/" + name


class MeasuredProcessFakes(unittest.TestCase):
    """Shared fake executor covering whole runs end to end."""

    def fake_executor(self, args):
        """(executor, envs) tracking every env a measured command saw.

        hyperfine export paths named on the command line receive a
        synthetic export file; a cargo build also seeds a `debug` tree so
        the size walk has something to measure.
        """
        envs = []

        def executor(command, env=None, cwd=None):
            if env is not None:
                envs.append(env)
            # Version and git probes come first: hyperfine itself has a
            # --version probe and must not be confused with a timed run.
            if command[0] == "git":
                return "fake-head\n" if command[1] == "rev-parse" else " M sample\n"
            if command[:1] == ["rustc"]:
                return RUSTC_VV
            if command[-1] == "--version":
                return RUSTC_VV if command[0] == "rustc" else (
                    {"cargo": "cargo 1.96.0\n",
                     "hyperfine": "hyperfine 1.2.0\n",
                     "cargo-nextest": "cargo-nextest 0.9.115\n"}[command[0]]
                )
            if command[0] == "hyperfine" and "--export-json" in command:
                export = command[command.index("--export-json") + 1]
                Path(export).write_text(json.dumps(HYPERFINE_PAYLOAD))
                return ""
            if command[0] == "cargo" and command[1] == "build":
                # A real build lands artifacts in the dedicated target
                # dir; the size walk measures this tree.
                debug = Path(args.target_dir) / "debug"
                debug.mkdir(parents=True, exist_ok=True)
                (debug / "sample-artifact").write_bytes(b"x" * 1024)
                # `--timings` writes its HTML report under the target dir.
                timing = Path(args.target_dir) / "cargo-timings"
                timing.mkdir(parents=True, exist_ok=True)
                (timing / "cargo-timing.html").write_text("<html></html>")
                return "cargo 1.96.0 build ok"
            raise AssertionError(f"unexpected command hit a fake: {command}")

        return executor, envs


class MarkerSafetyTests(unittest.TestCase):
    def test_prepare_creates_a_marked_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "target"
            self.assertFalse(measure_dev_loop.prepare_target_dir(target))
            marker = target / measure_dev_loop.MARKER
            self.assertTrue(marker.exists())
            (target / "leftover").mkdir()
            self.assertTrue(measure_dev_loop.prepare_target_dir(target))
            # The wipe keeps the marker so later calls stay safe.
            self.assertEqual(list(target.iterdir()), [marker])

    def test_prepare_refuses_an_unmarked_non_empty_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "target"
            target.mkdir()
            guard = target / "someone-else" / "data.txt"
            guard.parent.mkdir()
            guard.write_text("not ours")
            with self.assertRaisesRegex(ValueError, "without the"):
                measure_dev_loop.prepare_target_dir(target)
            # Nothing was deleted.
            self.assertTrue(guard.exists())

    def test_prepare_adopts_an_unmarked_empty_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "target"
            target.mkdir()
            measure_dev_loop.prepare_target_dir(target)
            self.assertTrue((target / measure_dev_loop.MARKER).exists())

    def test_prepare_refuses_a_non_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "a-file"
            target.write_text("data")
            with self.assertRaisesRegex(ValueError, "not a directory"):
                measure_dev_loop.prepare_target_dir(target)


class SizeTests(unittest.TestCase):
    def test_debug_tree_size_counts_hardlinks_once_and_skips_symlinks(self):
        with tempfile.TemporaryDirectory() as directory:
            debug = Path(directory) / "target" / "debug"
            debug.mkdir(parents=True)
            payload = debug / "pohunekd"
            payload.write_bytes(b"x" * 4096)
            # cargo hardlinks dependency artifacts into `target/debug`; the
            # walk must count each inode exactly once.
            os.link(payload, debug / "pohunek")
            small = debug / "deps" / "small.rmeta"
            small.parent.mkdir()
            small.write_bytes(b"y" * 512)
            # A symlinked file and a symlinked dir must not be followed or
            # counted; point them at the enclosing tree root itself.
            os.symlink(payload, debug / "link.rmeta")
            os.symlink(directory, debug / "escape")
            size = measure_dev_loop.debug_tree_size(debug.parent)
            self.assertEqual(size["path"], str(debug))
            self.assertEqual(size["counted"], 2)
            self.assertEqual(size["apparent_bytes"], 4096 + 512)
            self.assertGreater(size["allocated_bytes"], 0)

    def test_debug_tree_size_requires_the_directory(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "debug"):
                measure_dev_loop.debug_tree_size(Path(directory))


class EnvTests(unittest.TestCase):
    def test_measurement_env_pins_target_and_disables_wrappers(self):
        environ = {
            "PATH": "/usr/bin",
            "POHUNEK_HOME": "~/pohunek",
            "POHUNEK_SESSION_ID": "ses_x",
            "RUSTC_WRAPPER": "sccache",
            "CARGO_BUILD_RUSTC_WRAPPER": "sccache",
        }
        target = Path(tempfile.gettempdir()) / "measure-target"
        env, touched = measure_dev_loop.measurement_env(environ, target)
        self.assertEqual([key for key in env if key.startswith("POHUNEK_")], [])
        self.assertEqual(env["RUSTC_WRAPPER"], "")
        self.assertEqual(env["CARGO_BUILD_RUSTC_WRAPPER"], "")
        self.assertEqual(env["CARGO_TARGET_DIR"], str(target.resolve()))
        self.assertEqual(
            sorted(touched["stripped_keys"]),
            ["POHUNEK_HOME", "POHUNEK_SESSION_ID"],
        )
        self.assertEqual(
            touched["disabled_keys"], ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"]
        )

    def test_measurement_env_records_keys_without_values(self):
        env, touched = measure_dev_loop.measurement_env(
            {"POHUNEK_SECRET": "value", "KEEP": "yes"}, "target/x"
        )
        self.assertEqual(touched["stripped_keys"], ["POHUNEK_SECRET"])
        self.assertNotIn("value", json.dumps(touched))
        self.assertEqual(
            env["CARGO_TARGET_DIR"],
            str(Path("target/x").resolve()),
        )

    def test_execute_reports_a_failing_command_with_the_command(self):
        # A harmless interpreter invocation replaces cargo: the failing
        # command itself reaches the error message, and no real tool runs.
        command = [sys.executable, "-c", "import sys; sys.exit(3)"]
        with self.assertRaisesRegex(ValueError, "exit 3"):
            measure_dev_loop.execute(command)


class HyperfineTests(unittest.TestCase):
    def test_parse_hyperfine_extracts_median_stddev_and_runs(self):
        parsed = measure_dev_loop.parse_hyperfine(HYPERFINE_PAYLOAD)
        self.assertEqual(len(parsed), 1)
        row = parsed[0]
        self.assertEqual(
            row["command"], HYPERFINE_PAYLOAD["results"][0]["command"]
        )
        self.assertEqual(row["median"], 0.912345)
        self.assertEqual(row["stddev"], 0.031212)
        self.assertEqual(row["runs"], 3)

    def test_parse_hyperfine_tolerates_missing_fields(self):
        parsed = measure_dev_loop.parse_hyperfine({"results": [{"command": "x"}]})
        self.assertEqual(
            parsed,
            [{"command": "x", "median": None, "stddev": None, "runs": None}],
        )
        self.assertEqual(measure_dev_loop.parse_hyperfine({}), [])

    def test_metadata_parsers(self):
        self.assertEqual(
            measure_dev_loop.parse_rustc_release(RUSTC_VV), "1.96.0"
        )
        self.assertIsNone(measure_dev_loop.parse_rustc_release("no release line"))
        self.assertEqual(
            measure_dev_loop.parse_cpu_model(CPUINFO),
            "Intel(R) Core(TM) i9-9900 CPU @ 3.10GHz",
        )
        self.assertEqual(measure_dev_loop.parse_logical_cores(CPUINFO), 2)
        self.assertEqual(measure_dev_loop.parse_meminfo(MEMINFO), 32768000 * 1024)

    def test_metadata_parsers_tolerate_missing_or_foreign_files(self):
        # Off Linux (or without the file) the parsers record unknown rather
        # than invent a value.
        self.assertEqual(measure_dev_loop.parse_cpu_model(None), "unknown")
        self.assertEqual(measure_dev_loop.parse_cpu_model(""), "unknown")
        self.assertIsNone(measure_dev_loop.parse_logical_cores(None))
        self.assertIsNone(measure_dev_loop.parse_meminfo(None))

    def test_mount_fstype_picks_the_longest_covering_mount_point(self):
        mounts = (
            "/dev/nvme0n1p2 / ext4 rw 0 0\n"
            "tmpfs /tmp tmpfs rw 0 0\n"
            "/dev/mapper/data /mnt/data\\040disk btrfs rw 0 0\n"
            "/dev/sdb1 /tmpfoo xfs rw 0 0\n"
        )
        parse = measure_dev_loop.parse_mount_fstype
        self.assertEqual(parse(mounts, "/tmp/wt/target/measure"), "tmpfs")
        self.assertEqual(parse(mounts, "/mnt/data disk/repo/target"), "btrfs")
        # A sibling that only shares a string prefix is not covered.
        self.assertEqual(parse(mounts, "/tmpfoo/target"), "xfs")
        self.assertEqual(parse(mounts, "/home/user/target"), "ext4")
        self.assertEqual(parse(None, "/tmp"), "unknown")
        self.assertEqual(parse("tmpfs /tmp tmpfs rw 0 0\n", "/home"), "unknown")


class PlanTests(unittest.TestCase):
    def test_parse_cases_selects_from_the_known_set(self):
        self.assertEqual(
            measure_dev_loop.parse_cases("all"), list(measure_dev_loop.CASES)
        )
        self.assertEqual(
            measure_dev_loop.parse_cases("incremental,cold"),
            ["cold", "incremental"],
        )

    def test_parse_cases_rejects_unknown_and_empty_selections(self):
        with self.assertRaisesRegex(ValueError, "unknown"):
            measure_dev_loop.parse_cases("cold,nope")
        with self.assertRaisesRegex(ValueError, "select"):
            measure_dev_loop.parse_cases(",")

    def test_incremental_targets_fail_clearly_when_sources_are_missing(self):
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(ValueError, "protocol-leaf"):
                measure_dev_loop.incremental_targets(
                    (("protocol-leaf", ("crates/protocol/src/lib.rs",)),
                     ("cli-root", ("crates/cli/src/main.rs",))),
                    Path(directory),
                )

    def test_incremental_targets_pick_existing_candidates(self):
        with tempfile.TemporaryDirectory() as directory:
            main = Path(directory) / "crates" / "cli" / "src" / "main.rs"
            main.parent.mkdir(parents=True)
            main.write_text("fn main() {}\n")
            targets = measure_dev_loop.incremental_targets(
                (("protocol-leaf", ("missing/lib.rs", "crates/cli/src/main.rs")),
                 ("cli-root", ("crates/cli/src/main.rs",))),
                directory,
            )
            self.assertEqual(
                [Path(path).name for _, path in targets], ["main.rs", "main.rs"]
            )
            for _, path in targets:
                # The touched path is absolute so `touch` works from the
                # hyperfine working directory.
                self.assertTrue(Path(path).is_absolute())


class DryRunTests(unittest.TestCase):
    def test_dry_run_prints_every_command_without_executing(self):
        def must_not_run(command, env=None, cwd=None):
            raise AssertionError(f"dry-run must not execute {command}")

        with tempfile.TemporaryDirectory() as directory:
            args = _args(directory)
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                code = measure_dev_loop.command_run(args, executor=must_not_run)
            rendered = stdout.getvalue()
        self.assertEqual(code, 0)
        # The dedicated target dir reaches the printed commands.
        self.assertIn("CARGO_TARGET_DIR", rendered)
        self.assertIn(args.target_dir, rendered)
        self.assertIn("RUSTC_WRAPPER=''", rendered)
        for needle in (
            "cargo build --workspace --all-targets --all-features --timings",
            "cargo nextest run --profile fast --workspace --all-features",
            "cargo clippy --workspace --all-targets --all-features",
            "crates/protocol/src/lib.rs",
            "crates/cli/src/lib.rs",
            "hyperfine",
            "size: (measured in-process",
        ):
            self.assertIn(needle, rendered)

    def test_dry_run_responds_to_case_selection(self):
        with tempfile.TemporaryDirectory() as directory:
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                measure_dev_loop.command_run(
                    _args(directory, cases="cold"),
                    executor=unittest.mock.Mock(),
                )
            case_lines = [
                line.split(":")[0]
                for line in stdout.getvalue().splitlines()[1:]
            ]
            self.assertEqual(case_lines, ["cold"])

    def test_missing_tool_is_named_before_measuring(self):
        with tempfile.TemporaryDirectory() as directory:
            args = _args(directory, dry_run=False, cases="warm")

            def no_hyperfine(name):
                # An injectable stand-in for `shutil.which`: only hyperfine
                # is missing, failing before any measurement starts.
                if name == "hyperfine":
                    return None
                return "/usr/bin/" + name

            with self.assertRaisesRegex(ValueError, "hyperfine"):
                measure_dev_loop.command_run(
                    args,
                    executor=unittest.mock.Mock(),
                    which=no_hyperfine,
                )


class RunResultsTests(MeasuredProcessFakes):
    def test_command_run_records_env_wrappers_and_target(self):
        with tempfile.TemporaryDirectory() as directory:
            args = _args(directory, dry_run=False)
            executor, envs = self.fake_executor(args)
            stdout = io.StringIO()
            with unittest.mock.patch.object(
                measure_dev_loop.shutil, "copy2"
            ), contextlib.redirect_stdout(stdout):
                self.assertEqual(measure_dev_loop.command_run(
                    args, executor=executor, which=present_tool
                ), 0)
            self.assertTrue(envs)
            for env in envs:
                self.assertEqual(
                    env["CARGO_TARGET_DIR"],
                    str(Path(args.target_dir).resolve()),
                )
                self.assertEqual(env["RUSTC_WRAPPER"], "")
                self.assertEqual(env["CARGO_BUILD_RUSTC_WRAPPER"], "")
                self.assertEqual(
                    [key for key in env if key.startswith("POHUNEK_")], []
                )

    def test_command_run_parses_hyperfine_exports_and_writes_baseline(self):
        with tempfile.TemporaryDirectory() as directory:
            args = _args(directory, dry_run=False, cases="warm,incremental")
            executor, _ = self.fake_executor(args)
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                self.assertEqual(
                    measure_dev_loop.command_run(
                    args, executor=executor, which=present_tool
                ), 0
                )
            baseline = json.loads(
                (Path(args.out) / "baseline.json").read_text()
            )
            labels = [row["case"] for row in baseline["cases"]]
            self.assertIn("warm", labels)
            self.assertIn("incremental", baseline["case_labels"])
            # Medians and stddevs come from the synthetic export, not from
            # a real tool run.
            warm = next(row for row in baseline["cases"] if row["case"] == "warm")
            self.assertEqual(warm["median"], 0.912345)
            self.assertEqual(warm["stddev"], 0.031212)
            self.assertEqual(warm["runs"], 3)
            # A baseline without a size case holds no size table.
            self.assertIsNone(baseline["size"])

    def test_command_run_records_size_and_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            args = _args(directory, dry_run=False, cases="cold,size")
            executor, envs = self.fake_executor(args)
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                self.assertEqual(
                    measure_dev_loop.command_run(
                    args, executor=executor, which=present_tool
                ), 0
                )
            baseline = json.loads(
                (Path(args.out) / "baseline.json").read_text()
            )
            self.assertEqual(baseline["size"]["apparent_bytes"], 1024)
            metadata = baseline["metadata"]
            self.assertEqual(
                metadata["cargo_target_dir"],
                str(Path(args.target_dir).resolve()),
            )
            self.assertEqual(
                metadata["wrappers_disabled"],
                ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"],
            )
            # `/proc/cpuinfo` may be present on the test host or not; the
            # record must carry the key either way.
            self.assertIn("cpu_model", metadata)
            self.assertTrue(metadata["git_dirty"])

    def test_command_run_fails_when_a_measured_command_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            args = _args(directory, dry_run=False, cases="cold")

            def failing(command, env=None, cwd=None):
                raise ValueError(
                    f"command failed with exit 1: {' '.join(command)}"
                )

            with self.assertRaises(ValueError):
                measure_dev_loop.command_run(
                    args, executor=failing, which=present_tool
                )


class ReportTests(unittest.TestCase):
    def test_command_report_renders_the_table_and_size_table(self):
        payload = {
            "cases": [
                {"case": "cold", "subcase": None, "command": "cargo build x",
                 "median": 12.345, "stddev": None, "runs": 1,
                 "unit": "seconds"},
                {"case": "warm", "subcase": None, "command": "hyperfine y",
                 "median": 0.9, "stddev": 0.1, "runs": 5, "unit": "seconds"},
            ],
            "size": {"path": "target/debug", "apparent_bytes": 4096,
                     "allocated_bytes": 8192, "counted": 2},
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "baseline.json"
            path.write_text(json.dumps(payload))
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                self.assertEqual(
                    measure_dev_loop.command_report(argparse.Namespace(path=str(path))),
                    0,
                )
            rendered = stdout.getvalue()
        for needle in (
            "| Case | Command | Median | Stddev | Runs |",
            "| cold | cargo build x | 12.345 | - | 1 |",
            "| warm | hyperfine y | 0.900 | 0.100 | 5 |",
            "Size of `target/debug`",
            "| Metric | Bytes | Human |",
            "| apparent | 4096 | 4.0 KiB |",
            "| allocated | 8192 | 8.0 KiB |",
        ):
            self.assertIn(needle, rendered)
        # The size values live in their own table, not the Median column.
        self.assertNotIn("| size |", rendered)

    def test_report_rejects_a_non_baseline(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "not.json"
            path.write_text('{"runs": []}')
            with self.assertRaisesRegex(ValueError, "cases"):
                measure_dev_loop.read_baseline(path)


class CompareTests(unittest.TestCase):
    def _payload(self, median, apparent, allocated=None):
        allocated = apparent * 2 if allocated is None else allocated
        return {
            "cases": [
                {"case": "warm", "subcase": None, "command": "cmd",
                 "median": median, "stddev": 0.1, "runs": 5,
                 "unit": "seconds"},
            ],
            "size": {"path": "target/debug", "apparent_bytes": apparent,
                     "allocated_bytes": allocated, "counted": 3},
        }

    def test_compare_baselines_compares_cases_and_size(self):
        rows = measure_dev_loop.compare_baselines(
            self._payload(10.0, 3000), self._payload(6.0, 1000)
        )
        warm = next(row for row in rows if row["case"] == "warm")
        self.assertEqual(warm["delta"], -4.0)
        self.assertAlmostEqual(warm["percent"], -40.0)
        apparent = next(row for row in rows if row["case"] == "size:apparent")
        self.assertEqual(apparent["unit"], "bytes")
        self.assertEqual(apparent["delta"], -2000)
        self.assertIn("size:allocated",
                      [row["case"] for row in rows])

    def test_compare_handles_rows_present_on_one_side_only(self):
        rows = measure_dev_loop.compare_baselines(
            self._payload(10.0, 3000),
            {"cases": [], "size": None},
        )
        warm = next(row for row in rows if row["case"] == "warm")
        self.assertIsNone(warm["after"])
        self.assertIsNone(warm["delta"])
        self.assertIsNone(warm["percent"])

    def test_render_compare_marks_absent_values_and_bytes(self):
        rows = measure_dev_loop.compare_baselines(
            self._payload(10.0, 3000), self._payload(6.0, 1000)
        )
        rendered = measure_dev_loop.render_compare_markdown(rows)
        self.assertIn("| warm | 10.000 | 6.000 | -4.000 | -40 % |", rendered)
        # Bytes deltas are rendered as bytes, not seconds.
        self.assertIn(
            "| size:apparent | 2.9 KiB | 1,000.0 B | -2.0 KiB | -67 % |",
            rendered,
        )

    def test_compare_json_output_carries_the_same_rows(self):
        with tempfile.TemporaryDirectory() as directory:
            before = Path(directory) / "before.json"
            after = Path(directory) / "after.json"
            before.write_text(json.dumps(self._payload(10.0, 3000)))
            after.write_text(json.dumps(self._payload(6.0, 1000)))
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                self.assertEqual(
                    measure_dev_loop.command_compare(
                        argparse.Namespace(
                            before=str(before), after=str(after), json=True
                        )
                    ),
                    0,
                )
            rows = json.loads(stdout.getvalue())
        self.assertEqual(
            next(row for row in rows if row["case"] == "warm")["delta"], -4.0
        )


class ValidationTests(unittest.TestCase):
    def test_short_runs_and_warmups_are_rejected_with_reasons(self):
        for flag, value in (("--runs", 1), ("--warmup", 0)):
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                code = measure_dev_loop.main(
                    ["run", "--dry-run", "--cases", "cold", flag, str(value),
                     "--target-dir", "x", "--out", "y"]
                )
            self.assertEqual(code, 1)
            self.assertIn(flag, stderr.getvalue())

    def test_defaults_satisfy_the_bounds(self):
        self.assertEqual(measure_dev_loop.DEFAULT_RUNS, 5)
        self.assertEqual(measure_dev_loop.DEFAULT_WARMUP, 1)
        self.assertGreaterEqual(measure_dev_loop.DEFAULT_RUNS, 2)
        self.assertGreaterEqual(measure_dev_loop.DEFAULT_WARMUP, 1)


if __name__ == "__main__":
    unittest.main()
