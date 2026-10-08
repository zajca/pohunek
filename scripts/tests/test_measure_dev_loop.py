"""Integration scenarios for `scripts/measure-dev-loop` (stdlib only).

Every test drives the real script as a subprocess over disposable
fixtures: fake `cargo`, `hyperfine`, `git`, and `rustc` executables on a
private PATH stand in for the measurement tools, and every target and
results directory lives inside a temporary root. No test reaches a real
cargo, hyperfine, or `git status`: the whole preparation, measurement,
and reporting cooperation runs against the fakes, and destructive
behavior can only ever touch the marked fixture target directory.
"""

import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "measure-dev-loop"

# A fake `cargo` with the argv shapes `measure-dev-loop` uses: version
# probes succeed, `build` populates the dedicated target dir exactly like
# the real one does (hardlinks for dedup, a symlink to skip, `--timings`
# HTML), and `FAKE_MEASURE_FAIL` turns one kind of child into a failure.
FAKE_CARGO = '''#!/usr/bin/env python3
"""Fake cargo: version probes succeed, `build` builds a tiny tree."""
import json, os, sys
from pathlib import Path

argv = sys.argv[1:]
log = os.environ.get("FAKE_MEASURE_LOG")
if log:
    # Only the pinned target dir and the (disabled) wrappers are recorded
    # with values; POHUNEK_* survive as key names only, so their values
    # can never reach the log.
    record = {
        "tool": Path(sys.argv[0]).name,
        "argv": argv,
        "env": {key: os.environ[key] for key in
                ("CARGO_TARGET_DIR", "RUSTC_WRAPPER",
                 "CARGO_BUILD_RUSTC_WRAPPER") if key in os.environ},
        "pohunek_keys": sorted(key for key in os.environ
                               if key.startswith("POHUNEK_")),
    }
    with (Path(log) / "calls.jsonl").open("a") as stream:
        stream.write(json.dumps(record) + "\\n")

if argv[:1] == ["--version"]:
    print("cargo 1.96.0 (fake)")
elif argv[:2] == ["nextest", "--version"]:
    print("cargo-nextest 0.9.115")
elif argv[:1] == ["build"]:
    if os.environ.get("FAKE_MEASURE_FAIL") == "build":
        print("error: fake build failure", file=sys.stderr)
        sys.exit(7)
    target = Path(os.environ["CARGO_TARGET_DIR"])
    debug = target / "debug"
    (debug / "deps").mkdir(parents=True, exist_ok=True)
    binary = debug / "pohunekd"
    binary.write_bytes(b"x" * 4096)
    os.link(binary, debug / "pohunek")  # dedup: one inode, counted once
    (debug / "deps" / "small.rmeta").write_bytes(b"y" * 512)
    os.symlink(binary.name, debug / "link.rmeta")  # never followed
    timings = target / "cargo-timings"
    timings.mkdir(parents=True, exist_ok=True)
    (timings / "cargo-timing.html").write_text("<html></html>")
else:
    raise SystemExit(f"fake cargo: unexpected argv {argv}")
'''

# A fake `hyperfine` that accepts `--version` and writes a deterministic
# export payload; `FAKE_MEASURE_MEDIAN` shifts the payload so two runs
# measure different numbers. The `--prepare` command is logged, never
# run: it `touch`es tracked repository sources, and the invocation wiring
# is what the scenarios pin down, not the side effect.
FAKE_HYPERFINE = '''#!/usr/bin/env python3
"""Fake hyperfine: writes a fixed export payload beside --export-json."""
import json, os, sys
from pathlib import Path

argv = sys.argv[1:]
log = os.environ.get("FAKE_MEASURE_LOG")
if log:
    record = {
        "tool": Path(sys.argv[0]).name,
        "argv": argv,
        "env": {key: os.environ[key] for key in
                ("CARGO_TARGET_DIR", "RUSTC_WRAPPER",
                 "CARGO_BUILD_RUSTC_WRAPPER") if key in os.environ},
        "pohunek_keys": sorted(key for key in os.environ
                               if key.startswith("POHUNEK_")),
    }
    with (Path(log) / "calls.jsonl").open("a") as stream:
        stream.write(json.dumps(record) + "\\n")

if argv[:1] == ["--version"]:
    print("hyperfine 1.2.0")
else:
    median = float(os.environ.get("FAKE_MEASURE_MEDIAN", "0.912345"))
    stddev = float(os.environ.get("FAKE_MEASURE_STDDEV", "0.031212"))
    payload = {
        "results": [{
            "command": argv[-1],
            "median": median,
            "stddev": stddev,
            "runs": [{"time": median - stddev}, {"time": median},
                     {"time": median + stddev}],
        }],
    }
    Path(argv[argv.index("--export-json") + 1]).write_text(json.dumps(payload))
'''

FAKE_GIT = '''#!/usr/bin/env python3
"""Fake git: a fixed HEAD and exactly one dirty file."""
import sys
argv = sys.argv[1:]
if argv == ["rev-parse", "HEAD"]:
    print("fa11ceca111edba5ef0000000000000000000000")
elif argv == ["status", "--porcelain"]:
    print(" M sample")
else:
    raise SystemExit(f"fake git: unexpected argv {argv}")
'''

FAKE_RUSTC = '''#!/usr/bin/env python3
"""Fake rustc: the fixed -vV block a real rustc prints."""
import sys
argv = sys.argv[1:]
if argv == ["-vV"]:
    print("rustc 1.96.0 (abc123 2026-01-01)")
    print("binary: rustc")
    print("release: 1.96.0")
else:
    raise SystemExit(f"fake rustc: unexpected argv {argv}")
'''

# One `run`-produced baseline row set comes from the specification of the
# format in the script docstring: cases, units, and run counts below are
# the documented shapes, not values regurgitated by the code under test.
REPORT_BASELINE = {
    "cases": [
        {"case": "cold", "subcase": None, "command": "cargo build x",
         "median": 12.345, "stddev": None, "runs": 1, "unit": "seconds"},
        {"case": "warm", "subcase": None, "command": "hyperfine y",
         "median": 0.9, "stddev": 0.1, "runs": 5, "unit": "seconds"},
    ],
    "size": {"path": "target/debug", "apparent_bytes": 4096,
             "allocated_bytes": 8192, "counted": 2},
}

COMPARE_BEFORE = {
    "cases": [
        {"case": "warm", "subcase": None, "command": "cmd",
         "median": 10.0, "stddev": 0.1, "runs": 5, "unit": "seconds"},
        {"case": "incremental", "subcase": "protocol-leaf-clippy",
         "command": "cmd", "median": 7.5, "stddev": 0.1, "runs": 3,
         "unit": "seconds"},
    ],
    "size": {"path": "target/debug", "apparent_bytes": 3000,
             "allocated_bytes": 6000, "counted": 3},
}

COMPARE_AFTER = {
    "cases": [
        {"case": "warm", "subcase": None, "command": "cmd",
         "median": 6.0, "stddev": 0.1, "runs": 5, "unit": "seconds"},
    ],
    "size": {"path": "target/debug", "apparent_bytes": 1000,
             "allocated_bytes": 2000, "counted": 3},
}


class DevLoopFixture:
    """Disposable PATH, target dir, and results dir for one CLI invocation.

    The fakes are the only tools the measured commands can reach except
    `/usr/bin`-level basics, so a host cargo or hyperfine can never leak
    into a scenario.
    """

    def __init__(self, missing=()):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name, body in (("cargo", FAKE_CARGO),
                           ("hyperfine", FAKE_HYPERFINE),
                           ("git", FAKE_GIT),
                           ("rustc", FAKE_RUSTC)):
            if name in missing:
                continue
            path = self.bin / name
            path.write_text(body)
            path.chmod(0o755)
        self.log = self.root / "fake-tool-log"
        self.log.mkdir()
        self.target = self.root / "measure-target"
        self.out = self.root / "results"

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self._tmp.cleanup()

    def environment(self, **extra):
        environ = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("POHUNEK_")
        }
        if "PATH" in environ:
            environ["PATH"] = os.pathsep.join([str(self.bin), environ["PATH"]])
        else:
            environ["PATH"] = str(self.bin)
        environ["FAKE_MEASURE_LOG"] = str(self.log)
        environ.update(extra)
        return environ

    def run(self, *args, **env_extra):
        """Invoke the script with the fake tools, returning a CompletedProcess."""
        return subprocess.run(
            [sys.executable, str(SCRIPT), *[str(arg) for arg in args]],
            env=self.environment(**env_extra),
            capture_output=True,
            text=True,
            timeout=120,  # hung child processes fail loudly, not slowly
        )

    def calls(self, tool):
        """Every recorded call one fake tool made, newest first."""
        path = self.log / "calls.jsonl"
        if not path.exists():
            return []
        entries = [json.loads(line) for line in path.read_text().splitlines()]
        return [entry for entry in entries if entry["tool"] == tool]

    def build_calls(self):
        """The `cargo build ...` invocations (not the version probes)."""
        return [
            entry for entry in self.calls("cargo")
            if entry["argv"][:1] == ["build"]
        ]


class DryRunScenarios(unittest.TestCase):
    """The plan is printed, and nothing is executed."""

    def test_dry_run_prints_the_whole_plan_and_executes_nothing(self):
        with DevLoopFixture() as fixture:
            process = fixture.run(
                "run", "--dry-run", "--cases", "all",
                "--target-dir", fixture.target, "--out", fixture.out,
            )
            self.assertEqual(process.returncode, 0, process.stderr)
            rendered = process.stdout
            # No command ran and no output was written.
            self.assertFalse((fixture.log / "calls.jsonl").exists())
            self.assertFalse(fixture.out.exists())
            self.assertIn(
                "env: CARGO_TARGET_DIR=%s RUSTC_WRAPPER='' "
                "CARGO_BUILD_RUSTC_WRAPPER='' (POHUNEK_* variables removed)"
                % shlex.quote(str(fixture.target.resolve())),
                rendered,
            )
            for needle in (
                "cargo build --workspace --all-targets --all-features "
                "--timings",
                "cargo nextest run --profile fast --workspace "
                "--all-features",
                "cargo clippy --workspace --all-targets --all-features",
                "crates/protocol/src/lib.rs",
                "crates/cli/src/lib.rs",
                "hyperfine",
                "size: (measured in-process; no command)",
            ):
                self.assertIn(needle, rendered, needle)
            cases = {
                line.split(":")[0] for line in rendered.splitlines()[1:]
            }
            self.assertEqual(
                cases, {"cold", "warm", "incremental", "size"},
            )

    def test_dry_run_responds_to_case_selection(self):
        with DevLoopFixture() as fixture:
            process = fixture.run(
                "run", "--dry-run", "--cases", "cold",
                "--target-dir", fixture.target, "--out", fixture.out,
            )
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertIn("cold: cargo build", process.stdout)
        self.assertNotIn("incremental", process.stdout)
        self.assertNotIn("clippy", process.stdout)
        self.assertNotIn("size:", process.stdout)


class RunScenarios(unittest.TestCase):
    """Real `run` mode against the fake measurement tools."""

    def test_run_measures_cases_and_writes_a_leak_free_baseline(self):
        incriminating = "leaky-session-environment-value-7"
        with DevLoopFixture() as fixture:
            process = fixture.run(
                "run", "--cases", "cold,warm,size", "--runs", "3",
                "--warmup", "1",
                "--target-dir", fixture.target, "--out", fixture.out,
                POHUNEK_HOME="~/pohunek", POHUNEK_SECRET=incriminating,
                RUSTC_WRAPPER="sccache",
                CARGO_BUILD_RUSTC_WRAPPER="sccache",
            )
            self.assertEqual(process.returncode, 0, process.stderr)
            baseline = json.loads(
                (fixture.out / "baseline.json").read_text()
            )
            self.assertEqual(
                baseline["case_labels"], ["cold", "warm", "size"]
            )
            rows = {row["case"]: row for row in baseline["cases"]}
            # The cold case is one real (here: fake-tool) timed run.
            self.assertEqual(rows["cold"]["runs"], 1)
            self.assertGreater(rows["cold"]["median"], 0)
            self.assertEqual(rows["cold"]["unit"], "seconds")
            # The warm case medians come from the hyperfine export.
            self.assertEqual(rows["warm"]["median"], 0.912345)
            self.assertEqual(rows["warm"]["stddev"], 0.031212)
            self.assertEqual(rows["warm"]["runs"], 3)
            # The size walk counted the hardlink pair once and skipped
            # the symlink.
            size = baseline["size"]
            self.assertEqual(size["path"],
                             str(fixture.target.resolve() / "debug"))
            self.assertEqual(size["apparent_bytes"], 4096 + 512)
            self.assertEqual(size["counted"], 2)
            self.assertGreater(size["allocated_bytes"], 0)
            # The output directory received the `--timings` HTML report.
            self.assertEqual(
                (fixture.out / "cargo-timing.html").read_text(),
                "<html></html>",
            )
            metadata = baseline["metadata"]
            self.assertEqual(metadata["cargo_target_dir"],
                             str(fixture.target.resolve()))
            self.assertEqual(metadata["git_head"],
                             "fa11ceca111edba5ef0000000000000000000000")
            self.assertTrue(metadata["git_dirty"])
            self.assertEqual(metadata["rustc"], "1.96.0")
            self.assertEqual(metadata["cargo"], "cargo 1.96.0 (fake)")
            self.assertEqual(
                metadata["wrappers_disabled"],
                ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"],
            )
            # `/proc` may be present on the test host or not; the record
            # must carry the key either way.
            self.assertIn("cpu_model", metadata)
            self.assertIn("filesystem", metadata)
            # The env record lists touched keys only, never values.
            self.assertEqual(baseline["environment"]["stripped_keys"],
                             ["POHUNEK_HOME", "POHUNEK_SECRET"])
            self.assertNotIn(incriminating, (fixture.out / "baseline.json")
                             .read_text())
            # The measured child itself saw the pinned, wrapper-free env,
            # and no POHUNEK_* key reached it. The log carries only the
            # nonsecret control values; `pohunek_keys` proves the stripped
            # variables by name.
            build_call = fixture.build_calls()[0]
            build_env = build_call["env"]
            self.assertEqual(build_env["CARGO_TARGET_DIR"],
                             str(fixture.target.resolve()))
            self.assertEqual(build_env["RUSTC_WRAPPER"], "")
            self.assertEqual(build_env["CARGO_BUILD_RUSTC_WRAPPER"], "")
            self.assertEqual(build_call["pohunek_keys"], [])
            # The stdout table rendered the same rows.
            self.assertIn(
                "| Case | Command | Median | Stddev | Runs |", process.stdout
            )
            self.assertIn(
                "| warm | cargo build --workspace --all-targets "
                "--all-features --timings | 0.912 | 0.031 | 3 |",
                process.stdout,
            )
            self.assertIn("| apparent | 4608 | 4.5 KiB |", process.stdout)
            self.assertIn("baseline written to", process.stdout)

    def test_run_records_incremental_preparations_and_exports(self):
        sources = [
            (Path(__file__).resolve().parents[2] / path).resolve()
            for path in ("crates/protocol/src/lib.rs", "crates/cli/src/lib.rs")
        ]
        with DevLoopFixture() as fixture:
            process = fixture.run(
                "run", "--cases", "incremental",
                "--target-dir", fixture.target, "--out", fixture.out,
            )
            self.assertEqual(process.returncode, 0, process.stderr)
            baseline = json.loads(
                (fixture.out / "baseline.json").read_text()
            )
            self.assertEqual(baseline["case_labels"], ["incremental"])
            subcases = sorted(
                row["subcase"] for row in baseline["cases"]
            )
            self.assertEqual(
                subcases,
                ["cli-root-clippy", "cli-root-nextest-fast",
                 "protocol-leaf-clippy", "protocol-leaf-nextest-fast"],
            )
            for name in subcases:
                self.assertTrue(
                    (fixture.out / f"{name}.json").is_file(), name
                )
            prepares = [
                shlex.split(call["argv"][call["argv"].index("--prepare") + 1])
                for call in fixture.calls("hyperfine")
                if "--prepare" in call["argv"]
            ]
            # Each measured command is handed `touch <one absolute source
            # path>` and each case source is prepared; the fake logs the
            # argument instead of running it (it points at tracked
            # repository sources).
            self.assertEqual(
                {command[0] for command in prepares}, {"touch"}
            )
            self.assertEqual(
                {command[1] for command in prepares},
                {str(path) for path in sources},
            )

    def test_run_aborts_when_a_measured_command_fails(self):
        with DevLoopFixture() as fixture:
            process = fixture.run(
                "run", "--cases", "cold",
                "--target-dir", fixture.target, "--out", fixture.out,
                FAKE_MEASURE_FAIL="build",
            )
            self.assertEqual(process.returncode, 1)
            self.assertIn("command failed with exit 7", process.stderr)
            self.assertIn(
                "cargo build --workspace --all-targets --all-features "
                "--timings", process.stderr,
            )
            self.assertIn("error: fake build failure", process.stderr)
            self.assertFalse((fixture.out / "baseline.json").exists())

    def test_run_names_a_missing_tool_before_measuring(self):
        with DevLoopFixture(missing=["hyperfine"]) as fixture:
            # Only the private bin dir is searched, so no host hyperfine
            # can cover for the missing fake.
            process = fixture.run(
                "run", "--cases", "cold",
                "--target-dir", fixture.target, "--out", fixture.out,
                PATH=str(fixture.bin),
            )
        self.assertEqual(process.returncode, 1)
        self.assertIn(
            "required tool(s) missing from PATH: hyperfine",
            process.stderr,
        )
        self.assertFalse((fixture.out / "baseline.json").exists())

    def test_run_size_alone_requires_a_prior_build(self):
        with DevLoopFixture() as fixture:
            process = fixture.run(
                "run", "--cases", "size",
                "--target-dir", fixture.target, "--out", fixture.out,
            )
        self.assertEqual(process.returncode, 1)
        self.assertIn("must run first", process.stderr)


class TargetDirSafetyScenarios(unittest.TestCase):
    """The script only ever empties a directory it marked itself."""

    def run_cold(self, fixture, target=None):
        return fixture.run(
            "run", "--cases", "cold",
            "--target-dir", target or fixture.target,
            "--out", fixture.out,
        )

    def test_run_adopts_an_empty_dir_then_wipes_only_marked_content(self):
        with DevLoopFixture() as fixture:
            fixture.target.mkdir()
            process = self.run_cold(fixture)
            self.assertEqual(process.returncode, 0, process.stderr)
            marker = fixture.target / ".measure-dev-loop"
            self.assertTrue(marker.is_file())
            self.assertTrue(
                marker.read_text().startswith("created by ")
            )
            # A later run empties the dir again but keeps the marker.
            (fixture.target / "leftover").write_text("not to keep")
            process = self.run_cold(fixture)
            self.assertEqual(process.returncode, 0, process.stderr)
            self.assertFalse((fixture.target / "leftover").exists())
            self.assertTrue(marker.is_file())

    def test_run_refuses_an_unmarked_non_empty_target_dir(self):
        with DevLoopFixture() as fixture:
            foreign = fixture.target / "someone-else"
            foreign.mkdir(parents=True)
            guarded = foreign / "data.txt"
            guarded.write_text("not ours")
            process = self.run_cold(fixture)
            self.assertEqual(process.returncode, 1)
            self.assertIn(".measure-dev-loop", process.stderr)
            self.assertIn("refusing", process.stderr)
            self.assertTrue(guarded.exists())
            self.assertFalse((fixture.out / "baseline.json").exists())

    def test_run_refuses_a_target_dir_that_is_a_file(self):
        with DevLoopFixture() as fixture:
            target = fixture.root / "a-file"
            target.write_text("data")
            process = self.run_cold(fixture, target=target)
        self.assertEqual(process.returncode, 1)
        self.assertIn("not a directory", process.stderr)


class EndToEndEvidenceFlow(unittest.TestCase):
    """`run` output is what `report` and `compare` consume."""

    def test_run_report_and_compare_render_a_real_baseline_chain(self):
        with DevLoopFixture() as fixture:
            first = fixture.run(
                "run", "--cases", "cold,warm,size", "--runs", "3",
                "--warmup", "1",
                "--target-dir", fixture.target,
                "--out", fixture.root / "results-a",
            )
            self.assertEqual(first.returncode, 0, first.stderr)
            second = fixture.run(
                "run", "--cases", "cold,warm,size", "--runs", "3",
                "--warmup", "1",
                "--target-dir", fixture.target,
                "--out", fixture.root / "results-b",
                FAKE_MEASURE_MEDIAN="1.5",
            )
            self.assertEqual(second.returncode, 0, second.stderr)
            baselines = Path(fixture.root) / "results-a" / "baseline.json"
            updated = Path(fixture.root) / "results-b" / "baseline.json"
            reported = fixture.run("report", baselines)
            self.assertEqual(reported.returncode, 0, reported.stderr)
            self.assertIn(
                "| Case | Command | Median | Stddev | Runs |", reported.stdout
            )
            self.assertIn("| cold | cargo build", reported.stdout)
            self.assertIn("Size of ", reported.stdout)
            compared = fixture.run("compare", baselines, updated)
            self.assertEqual(compared.returncode, 0, compared.stderr)
            # The warm deltas come from the shifted fake export, and the
            # identical rebuilt tree compares as a zero byte delta.
            self.assertIn(
                "| warm | 0.912 | 1.500 | 0.588 | +64 % |", compared.stdout
            )
            self.assertIn(
                "| size:apparent | 4.5 KiB | 4.5 KiB | 0.0 B | +0 % |",
                compared.stdout,
            )


class ReportCompareScenarios(unittest.TestCase):
    """Rendering cooperates with the persisted baseline format."""

    def write_baseline(self, fixture, name, payload):
        path = fixture.root / name
        path.write_text(json.dumps(payload))
        return path

    def test_report_renders_the_case_and_size_tables(self):
        with DevLoopFixture() as fixture:
            path = self.write_baseline(
                fixture, "baseline.json", REPORT_BASELINE
            )
            process = fixture.run("report", path)
            self.assertEqual(process.returncode, 0, process.stderr)
        for needle in (
            "| Case | Command | Median | Stddev | Runs |",
            "| cold | cargo build x | 12.345 | - | 1 |",
            "| warm | hyperfine y | 0.900 | 0.100 | 5 |",
            "Size of `target/debug`",
            "| Metric | Bytes | Human |",
            "| apparent | 4096 | 4.0 KiB |",
            "| allocated | 8192 | 8.0 KiB |",
        ):
            self.assertIn(needle, process.stdout, needle)
        # The size values live in their own table, not the Median column.
        self.assertNotIn("| size |", process.stdout)

    def test_report_rejects_a_non_baseline(self):
        with DevLoopFixture() as fixture:
            path = fixture.root / "not.json"
            path.write_text('{"runs": []}')
            process = fixture.run("report", path)
        self.assertEqual(process.returncode, 1)
        self.assertIn("cases", process.stderr)

    def test_compare_renders_deltas_percent_and_bytes(self):
        with DevLoopFixture() as fixture:
            before = self.write_baseline(
                fixture, "before.json", COMPARE_BEFORE
            )
            after = self.write_baseline(fixture, "after.json", COMPARE_AFTER)
            process = fixture.run("compare", before, after)
            self.assertEqual(process.returncode, 0, process.stderr)
        self.assertIn(
            "| Case | Before | After | Delta | Delta % |", process.stdout
        )
        self.assertIn(
            "| warm | 10.000 | 6.000 | -4.000 | -40 % |", process.stdout
        )
        # A case present on one side only has no comparison.
        self.assertIn(
            "| incremental:protocol-leaf-clippy | 7.500 | - | - | n/a |",
            process.stdout,
        )
        # Bytes are rendered as bytes, not seconds.
        self.assertIn(
            "| size:apparent | 2.9 KiB | 1,000.0 B | -2.0 KiB | -67 % |",
            process.stdout,
        )
        self.assertIn("| size:allocated |", process.stdout)

    def test_compare_json_output_carries_the_same_rows(self):
        with DevLoopFixture() as fixture:
            before = self.write_baseline(
                fixture, "before.json", COMPARE_BEFORE
            )
            after = self.write_baseline(fixture, "after.json", COMPARE_AFTER)
            process = fixture.run("compare", before, after, "--json")
        self.assertEqual(process.returncode, 0, process.stderr)
        rows = json.loads(process.stdout)
        warm = next(
            row for row in rows if row["case"] == "warm"
        )
        self.assertEqual(warm["delta"], -4.0)
        self.assertEqual(warm["percent"], -40.0)


class ArgumentValidationScenarios(unittest.TestCase):
    """Bad arguments fail with the offending flag named on stderr."""

    def test_short_runs_and_warmups_are_rejected_with_reasons(self):
        for flag, value in (("--runs", "1"), ("--warmup", "0")):
            with self.subTest(flag=flag):
                with DevLoopFixture() as fixture:
                    process = fixture.run(
                        "run", "--dry-run", "--cases", "cold", flag, value,
                        "--target-dir", fixture.target, "--out", fixture.out,
                    )
                self.assertEqual(process.returncode, 1)
                self.assertIn(flag, process.stderr)

    def test_unknown_case_selection_is_rejected(self):
        with DevLoopFixture() as fixture:
            process = fixture.run(
                "run", "--dry-run", "--cases", "cold,nope",
                "--target-dir", fixture.target, "--out", fixture.out,
            )
        self.assertEqual(process.returncode, 1)
        self.assertIn("unknown: nope", process.stderr)
        self.assertIn("select one or more", process.stderr)


if __name__ == "__main__":
    unittest.main()
