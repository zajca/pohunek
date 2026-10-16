"""Scenarios for the helper scripts of the release workflows (stdlib only).

They cover the scripts that need neither the network nor a built xtask:
version resolution, the row matrix, the policy filter, input collection, the
refusals of the assembler wrapper, build-packages' own steps around the
builder (where the evidence goes, the checksum cross-check), and run-row's own
steps around the consumer (checksums, extraction, the timed consumer run and
its JUnit report), with a stand-in xtask and consumer. Staging the real upstreams, the real consumer and
signing are exercised by the release rehearsal job of ci.yml.
"""

import hashlib
import importlib.machinery
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
import xml.etree.ElementTree as ElementTree

ROOT = Path(__file__).resolve().parents[2]
SCRIPTS = ROOT / "scripts" / "release-workflow"


def run(script: str, *args: str, cwd: Path | None = None, env: dict[str, str] | None = None):
    base = {"PATH": os.environ["PATH"], "HOME": os.environ.get("HOME", "/")}
    base.update(env or {})
    return subprocess.run(
        [str(SCRIPTS / script), *args],
        cwd=cwd or ROOT,
        env=base,
        capture_output=True,
        text=True,
        check=False,
    )


class VersionTests(unittest.TestCase):
    def checkout(self, cargo: str) -> Path:
        directory = Path(self.enterContext(tempfile.TemporaryDirectory()))
        (directory / "Cargo.toml").write_text(cargo)
        return directory

    def test_the_workspace_version_is_printed_and_a_matching_tag_accepted(self):
        cargo = '[package]\nversion = "9.9.9"\n\n[workspace.package]\nversion = "1.2.3"\nedition = "2021"\n'
        result = run("version", "release", "v1.2.3", cwd=self.checkout(cargo))
        self.assertEqual((result.returncode, result.stdout), (0, "1.2.3\n"))

    def test_a_tag_that_differs_from_the_workspace_version_is_refused(self):
        cargo = '[workspace.package]\nversion = "1.2.3"\n'
        for tag in ("v1.2.4", "1.2.3", "v1.2.3-rc1", ""):
            with self.subTest(tag=tag):
                result = run("version", "release", tag, cwd=self.checkout(cargo))
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")

    def test_a_rehearsal_ignores_the_ref_name(self):
        cargo = '[workspace.package]\nversion = "1.2.3"\n'
        result = run("version", "rehearsal", "123/merge", cwd=self.checkout(cargo))
        self.assertEqual((result.returncode, result.stdout), (0, "1.2.3\n"))

    def test_a_missing_or_malformed_version_is_refused(self):
        for cargo in (
            '[package]\nversion = "1.2.3"\n',
            '[workspace.package]\nversion = "1.2"\n',
            '[workspace.package]\nversion = "1.2.3-beta"\n',
            '[workspace.package]\nversion = ""\n',
        ):
            with self.subTest(cargo=cargo):
                result = run("version", "rehearsal", "x", cwd=self.checkout(cargo))
                self.assertNotEqual(result.returncode, 0)

    def test_an_unknown_mode_is_refused(self):
        cargo = '[workspace.package]\nversion = "1.2.3"\n'
        self.assertNotEqual(run("version", "publish", "v1.2.3", cwd=self.checkout(cargo)).returncode, 0)

    def test_the_repository_version_resolves(self):
        result = run("version", "rehearsal", "x")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertRegex(result.stdout, r"^\d+\.\d+\.\d+\n$")


class RowMatrixTests(unittest.TestCase):
    def matrix(self, rows: list[dict[str, str]]) -> str:
        directory = Path(self.enterContext(tempfile.TemporaryDirectory()))
        path = directory / "matrix.json"
        path.write_text(json.dumps({"schema": 1, "suite_version": 1, "rows": rows}))
        return str(path)

    def test_the_rows_are_grouped_into_one_entry_per_runner(self):
        rows = [
            {"runtime": "alpha", "target": "x86_64-unknown-linux-gnu"},
            {"runtime": "alpha", "target": "x86_64-unknown-linux-musl"},
            {"runtime": "alpha", "target": "aarch64-apple-darwin"},
            {"runtime": "beta", "target": "x86_64-unknown-linux-gnu"},
        ]
        result = run("row-matrix", self.matrix(rows))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout),
            {
                "include": [
                    {
                        "runner": "ubuntu-latest",
                        "os": "linux",
                        "daemon_pattern": "build-daemon-*",
                        "rows": "alpha:x86_64-unknown-linux-gnu alpha:x86_64-unknown-linux-musl beta:x86_64-unknown-linux-gnu",
                        "row_limit_seconds": 900,
                        "timeout_minutes": 40 + 3 * 15,
                    },
                    {
                        "runner": "macos-15",
                        "os": "macos",
                        "daemon_pattern": "macos-signed-daemon",
                        "rows": "alpha:aarch64-apple-darwin",
                        "row_limit_seconds": 900,
                        "timeout_minutes": 40 + 15,
                    },
                ]
            },
        )

    def test_targets_restrict_the_rows_and_every_runner_keeps_its_entry(self):
        rows = [
            {"runtime": "alpha", "target": "x86_64-unknown-linux-gnu"},
            {"runtime": "alpha", "target": "x86_64-unknown-linux-musl"},
            {"runtime": "alpha", "target": "aarch64-apple-darwin"},
        ]
        result = run("row-matrix", self.matrix(rows), "x86_64-unknown-linux-musl")
        self.assertEqual(result.returncode, 0, result.stderr)
        entries = {entry["runner"]: entry for entry in json.loads(result.stdout)["include"]}
        self.assertEqual(entries["ubuntu-latest"]["rows"], "alpha:x86_64-unknown-linux-musl")
        # The macOS job still builds the consumer that the macOS smoke runs.
        self.assertEqual(entries["macos-15"]["rows"], "")
        self.assertNotEqual(run("row-matrix", self.matrix(rows), "riscv64gc-unknown-linux-gnu").returncode, 0)

    def test_a_target_without_a_runner_or_an_unsafe_name_is_refused(self):
        for row in (
            {"runtime": "alpha", "target": "riscv64gc-unknown-linux-gnu"},
            {"runtime": "al pha", "target": "x86_64-unknown-linux-gnu"},
            {"runtime": "../x", "target": "x86_64-unknown-linux-gnu"},
            {"runtime": "alpha", "target": "x86_64-unknown-linux-gnu; echo"},
        ):
            with self.subTest(row=row):
                result = run("row-matrix", self.matrix([row]))
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")

    def test_the_repository_matrix_has_a_row_for_every_package_directory(self):
        result = run("row-matrix", "compat/matrix.json")
        self.assertEqual(result.returncode, 0, result.stderr)
        runtimes = {
            word.split(":")[0]
            for entry in json.loads(result.stdout)["include"]
            for word in entry["rows"].split()
        }
        packages = {path.name for path in (ROOT / "runtime-packages").iterdir() if path.is_dir()}
        self.assertEqual(runtimes, packages)



# The out-of-process test of crates/cli/tests/release_consumer.rs that a row
# runs, as packaging/smoke-archive also names it.
CONSUMER_TEST = "the_release_binaries_serve_the_runtime_out_of_process"


class RowFixture:
    """A checkout, row artifacts and stand-ins that take run-row through a row.

    The stand-in xtask stages an empty upstream and writes a placeholder
    attestation; the consumer is a shell script given by the scenario. Every
    other step is run-row's own: the checksum checks, the archive extraction,
    the scrubbed consumer run, its timing and the JUnit report.
    """

    RUNTIME = "alpha"
    TARGET = "x86_64-unknown-linux-gnu"
    VERSION = "1.0.0"

    def __init__(self, directory: Path):
        self.directory = directory
        self.checkout = directory / "checkout"
        (self.checkout / "runtime-packages" / self.RUNTIME).mkdir(parents=True)
        lock = self.checkout / "compat" / self.RUNTIME / "compatibility-lock.json"
        lock.parent.mkdir(parents=True)
        lock.write_text("{}\n")

        stem = f"pohunek-daemon-{self.VERSION}-{self.TARGET}"
        self.daemon_dir = directory / "daemon"
        self.daemon_dir.mkdir()
        binaries = directory / "binaries" / stem
        binaries.mkdir(parents=True)
        for name in ("pohunek", "pohunekd", "pohunek-sessiond"):
            (binaries / name).write_text("#!/bin/sh\n")
        archive = self.daemon_dir / f"{stem}.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            bundle.add(binaries, arcname=stem)
        self.sidecar(archive)

        self.packages_dir = directory / "packages"
        self.packages_dir.mkdir()
        self.package = self.packages_dir / f"pohunek-runtime-{self.RUNTIME}-{self.VERSION}.tar.zst"
        self.package.write_bytes(b"package")
        self.sidecar(self.package)

        tools = directory / "tools"
        tools.mkdir()
        self.xtask = tools / "xtask"
        self.xtask.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            'command="$1 $2"\n'
            "shift 2\n"
            'while [ "$#" -gt 1 ]; do\n'
            '    case $1 in --runtime) runtime=$2 ;; --out) out=$2 ;; --output) output=$2 ;; esac\n'
            "    shift 2\n"
            "done\n"
            'case $command in\n'
            '    "compat stage-upstream") mkdir -p "$out/$runtime/bin" ;;\n'
            "    \"compat attest\") printf '{}\\n' > \"$output\" ;;\n"
            '    *) echo "unexpected xtask call: $command" >&2; exit 2 ;;\n'
            "esac\n"
        )
        node = tools / "node"
        node.write_text("#!/bin/sh\n")
        for tool in (self.xtask, node):
            tool.chmod(0o755)
        self.tools = tools
        self.consumer_path = directory / "release_consumer"
        self.out_dir = directory / "row"
        # The fresh, owner-only root run-rows hands to each row.
        self.row_root = directory / "root"
        self.row_root.mkdir()
        self.row_root.chmod(0o700)

    @staticmethod
    def sidecar(archive: Path) -> None:
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        Path(f"{archive}.sha256").write_text(f"{digest}  {archive.name}\n")

    def consumer(self, body: str) -> None:
        """The consumer stand-in; it runs under run-row's scrubbed environment."""
        self.consumer_path.write_text(f"#!/bin/sh\n{body}\n")

    def run(self) -> subprocess.CompletedProcess:
        return run(
            "run-row",
            cwd=self.checkout,
            env={
                "PATH": f"{self.tools}{os.pathsep}{os.environ['PATH']}",
                "VERSION": self.VERSION,
                "COMMIT": "0" * 40,
                "RUNTIME": self.RUNTIME,
                "TARGET": self.TARGET,
                "DAEMON_DIR": str(self.daemon_dir),
                "PACKAGES_DIR": str(self.packages_dir),
                "CONSUMER": str(self.consumer_path),
                "OUT_DIR": str(self.out_dir),
                "XTASK": str(self.xtask),
                "ROW_ROOT": str(self.row_root),
            },
        )

    @property
    def junit(self) -> Path:
        return self.out_dir / f"junit-{self.RUNTIME}-{self.TARGET}.xml"

    @property
    def attestation(self) -> Path:
        return self.out_dir / f"attestation-{self.RUNTIME}-{self.TARGET}.json"


# The passing and failing consumers sleep this long, so the reported time is
# known to cover the consumer run.
CONSUMER_SECONDS = 0.2
PASSING_CONSUMER = f'sleep {CONSUMER_SECONDS}\nprintf "{{}}" > "$POHUNEK_CONSUMER_REPORT"'
FAILING_CONSUMER = f"sleep {CONSUMER_SECONDS}\nexit 3"


@unittest.skipUnless(sys.platform == "linux", "run-row targets the Linux row runners (row-matrix)")
class RunRowTests(unittest.TestCase):
    def setUp(self):
        self.row = RowFixture(Path(self.enterContext(tempfile.TemporaryDirectory())))

    def reported_case(self) -> ElementTree.Element:
        root = ElementTree.parse(self.row.junit).getroot()
        self.assertEqual(root.tag, "testsuites")
        cases = list(root.iter("testcase"))
        self.assertEqual(len(cases), 1)
        self.assertEqual(cases[0].get("name"), CONSUMER_TEST)
        self.assertGreaterEqual(float(cases[0].get("time")), CONSUMER_SECONDS)
        return cases[0]

    def test_a_passing_consumer_run_is_timed_as_one_passing_testcase(self):
        self.row.consumer(PASSING_CONSUMER)
        result = self.row.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        case = self.reported_case()
        self.assertEqual([child.tag for child in case], [])
        self.assertEqual(ElementTree.parse(self.row.junit).getroot().get("failures"), "0")
        self.assertTrue(self.row.attestation.is_file())

    def test_a_failing_consumer_run_is_reported_before_the_row_fails(self):
        self.row.consumer(FAILING_CONSUMER)
        result = self.row.run()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("the consumer run failed", result.stderr)
        failures = self.reported_case().findall("failure")
        self.assertEqual(len(failures), 1)
        self.assertIn("status 3", failures[0].get("message"))
        self.assertEqual(ElementTree.parse(self.row.junit).getroot().get("failures"), "1")
        self.assertFalse(self.row.attestation.exists())

    def test_a_consumer_that_writes_no_report_is_a_failure(self):
        self.row.consumer(f"sleep {CONSUMER_SECONDS}")
        result = self.row.run()
        self.assertNotEqual(result.returncode, 0)
        failures = self.reported_case().findall("failure")
        self.assertEqual([failure.get("message") for failure in failures], ["the consumer wrote no report"])
        self.assertFalse(self.row.attestation.exists())

    def test_a_row_that_stops_before_the_consumer_leaves_no_report(self):
        self.row.consumer(PASSING_CONSUMER)
        self.row.out_dir.mkdir()
        self.row.junit.write_text("<testsuites/>")
        Path(f"{self.row.package}.sha256").write_text(f"{'0' * 64}  {self.row.package.name}\n")
        result = self.row.run()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("package archive checksum mismatch", result.stderr)
        self.assertFalse(self.row.junit.exists())

    def test_a_root_that_is_not_empty_or_not_owner_only_is_refused(self):
        self.row.consumer(PASSING_CONSUMER)
        (self.row.row_root / "left-over").write_text("")
        result = self.row.run()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ROW_ROOT is not empty", result.stderr)
        (self.row.row_root / "left-over").unlink()
        self.row.row_root.chmod(0o755)
        result = self.row.run()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ROW_ROOT is not owner-only", result.stderr)
        self.assertFalse(self.row.attestation.exists())


class RowsFixture:
    """Two rows of one runner (alpha, then beta) through run-rows and run-row.

    The stand-ins are those of RowFixture. The consumer is one shell script for
    both rows; it learns its row from POHUNEK_CONSUMER_RUNTIME. The checkout is
    a committed git repository, because run-rows records its state. A stand-in
    `systemctl` prints the units listed in a file, so a scenario can add a user
    service the way a row could.
    """

    RUNTIMES = ("alpha", "beta")
    TARGET = "x86_64-unknown-linux-gnu"
    VERSION = "1.0.0"

    def __init__(self, directory: Path):
        self.directory = directory
        self.checkout = directory / "checkout"
        stem = f"pohunek-daemon-{self.VERSION}-{self.TARGET}"
        self.daemon_dir = directory / "daemon"
        self.daemon_dir.mkdir()
        binaries = directory / "binaries" / stem
        binaries.mkdir(parents=True)
        for name in ("pohunek", "pohunekd", "pohunek-sessiond"):
            (binaries / name).write_text("#!/bin/sh\n")
        archive = self.daemon_dir / f"{stem}.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            bundle.add(binaries, arcname=stem)
        RowFixture.sidecar(archive)
        self.packages_dir = directory / "packages"
        self.packages_dir.mkdir()
        for runtime in self.RUNTIMES:
            (self.checkout / "runtime-packages" / runtime).mkdir(parents=True)
            lock = self.checkout / "compat" / runtime / "compatibility-lock.json"
            lock.parent.mkdir(parents=True)
            lock.write_text("{}\n")
            package = self.packages_dir / f"pohunek-runtime-{runtime}-{self.VERSION}.tar.zst"
            package.write_bytes(runtime.encode())
            RowFixture.sidecar(package)
        git = ["git", "-c", "user.name=fixture", "-c", "user.email=fixture@invalid", "-c", "commit.gpgsign=false"]
        for command in (["init", "-q"], ["add", "-A"], ["commit", "-q", "-m", "fixture"]):
            subprocess.run([*git, *command], cwd=self.checkout, check=True, capture_output=True)

        # The stand-in xtask and node of RowFixture, in a tools directory of
        # their own.
        self.tools = directory / "tools"
        donor = RowFixture(directory / "donor")
        shutil.copytree(donor.tools, self.tools)
        self.xtask = self.tools / "xtask"
        self.units = directory / "units"
        self.units.write_text("dbus.socket\n")
        systemctl = self.tools / "systemctl"
        systemctl.write_text(f"#!/bin/sh\ncat {self.units}\n")
        systemctl.chmod(0o755)
        self.consumer_path = directory / "release_consumer"
        self.out_dir = directory / "rows"
        self.log = directory / "log"
        self.log.mkdir()

    def consumer(self, body: str) -> None:
        """The consumer stand-in, run under run-row's scrubbed environment."""
        self.consumer_path.write_text(f"#!/bin/sh\nset -eu\nrow=$POHUNEK_CONSUMER_RUNTIME\n{body}\n")

    def run(self, limit: float = 60) -> subprocess.CompletedProcess:
        return run(
            "run-rows",
            cwd=self.checkout,
            env={
                "PATH": f"{self.tools}{os.pathsep}/usr/bin{os.pathsep}/bin",
                "ROWS": " ".join(f"{runtime}:{self.TARGET}" for runtime in self.RUNTIMES),
                "ROW_LIMIT_SECONDS": str(limit),
                "VERSION": self.VERSION,
                "COMMIT": "0" * 40,
                "DAEMON_DIR": str(self.daemon_dir),
                "PACKAGES_DIR": str(self.packages_dir),
                "CONSUMER": str(self.consumer_path),
                "OUT_DIR": str(self.out_dir),
                "XTASK": str(self.xtask),
                "GITHUB_STEP_SUMMARY": str(self.directory / "summary.md"),
            },
        )

    def results(self) -> dict[str, dict]:
        lines = (self.out_dir / "row-results.jsonl").read_text().splitlines()
        return {document["runtime"]: document for document in map(json.loads, lines)}

    def attestation(self, runtime: str) -> Path:
        return self.out_dir / f"attestation-{runtime}-{self.TARGET}.json"

    def recorded(self, runtime: str, name: str) -> str:
        return (self.log / f"{runtime}.{name}").read_text().strip()


# Writes a marker into every private place of the row and records where; beta
# also reports which of alpha's markers it can still see.
MARKING_CONSUMER = f"""
stage=${{PATH%%:*}}
for place in "$HOME" "$TMPDIR" "$PWD" "$stage"; do
    printf '%s\\n' "$row" > "$place/marker"
done
printf '%s\\n' "$HOME" > "$LOG/$row.home"
printf '%s\\n' "$TMPDIR" > "$LOG/$row.tmp"
printf '%s\\n' "$PWD" > "$LOG/$row.cwd"
printf '%s\\n' "$stage" > "$LOG/$row.stage"
if [ "$row" = beta ]; then
    for name in home tmp cwd stage; do
        seen=$(cat "$LOG/alpha.$name")
        if [ -e "$seen/marker" ] || [ -e "$seen" ]; then
            printf '%s\\n' "$seen" >> "$LOG/beta.visible"
        fi
    done
fi
{PASSING_CONSUMER}
"""


@unittest.skipUnless(sys.platform == "linux", "the scenarios scan /proc, as run-rows does on the Linux runner")
class RunRowsTests(unittest.TestCase):
    def setUp(self):
        self.rows = RowsFixture(Path(self.enterContext(tempfile.TemporaryDirectory())))

    def consumer(self, body: str) -> None:
        self.rows.consumer(body.replace("$LOG", str(self.rows.log)))

    def test_two_rows_of_one_job_do_not_see_each_others_home_store_or_temp_root(self):
        self.consumer(MARKING_CONSUMER)
        result = self.rows.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        for name in ("home", "tmp", "cwd", "stage"):
            with self.subTest(place=name):
                alpha, beta = self.rows.recorded("alpha", name), self.rows.recorded("beta", name)
                self.assertNotEqual(alpha, beta)
                self.assertFalse(Path(alpha).exists(), "alpha's root is removed before beta runs")
                self.assertFalse(Path(beta).exists())
        roots = {Path(self.rows.recorded(runtime, "home")).parents[0] for runtime in self.rows.RUNTIMES}
        self.assertEqual(len(roots), 2)
        self.assertFalse((self.rows.log / "beta.visible").exists(), "beta saw alpha's markers")
        results = self.rows.results()
        self.assertEqual({runtime: results[runtime]["status"] for runtime in results}, {"alpha": "pass", "beta": "pass"})
        for runtime in self.rows.RUNTIMES:
            self.assertTrue(self.rows.attestation(runtime).is_file())
            self.assertGreaterEqual(results[runtime]["guard_seconds"], 0)
        self.assertIn("| alpha on x86_64-unknown-linux-gnu | pass |", (self.rows.directory / "summary.md").read_text())

    def test_a_failing_row_is_named_and_the_next_row_still_runs(self):
        self.consumer(f'if [ "$row" = alpha ]; then exit 3; fi\n{PASSING_CONSUMER}')
        result = self.rows.run()
        self.assertEqual(result.returncode, 1, result.stderr)
        results = self.rows.results()
        self.assertEqual(results["alpha"]["status"], "fail")
        self.assertIn("the consumer run failed for alpha on x86_64-unknown-linux-gnu", results["alpha"]["message"])
        self.assertEqual(results["beta"]["status"], "pass")
        self.assertFalse(self.rows.attestation("alpha").exists())
        self.assertTrue(self.rows.attestation("beta").is_file())
        self.assertIn("::error title=Row alpha on x86_64-unknown-linux-gnu::the consumer run failed", result.stdout)
        self.assertTrue((self.rows.out_dir / "junit-alpha-x86_64-unknown-linux-gnu.xml").is_file())

    def test_a_process_left_behind_fails_its_row_without_printing_its_environment(self):
        # The survivor leaves the row's process group and session, and carries
        # a secret-looking value in its environment and its command line.
        self.consumer(
            'if [ "$row" = alpha ]; then\n'
            '    PROBE_TOKEN="ghs_envsecretvalue:$HOME" setsid sh -c \'sleep 300; : ghs_argsecretvalue\' '
            '</dev/null >/dev/null 2>&1 &\n'
            "fi\n"
            f"{PASSING_CONSUMER}"
        )
        result = self.rows.run()
        self.assertEqual(result.returncode, 1, result.stderr)
        results = self.rows.results()
        self.assertEqual(results["alpha"]["status"], "fail")
        self.assertRegex(results["alpha"]["message"], r"left process \d+ \(/\S+\) running, matched by \w+")
        self.assertEqual(results["beta"]["status"], "pass")
        self.assertFalse(self.rows.attestation("alpha").exists())
        printed = "\n".join(
            [
                result.stdout,
                result.stderr,
                (self.rows.out_dir / "row-results.jsonl").read_text(),
                (self.rows.directory / "summary.md").read_text(),
            ]
        )
        self.assertNotIn("ghs_envsecretvalue", printed)
        self.assertNotIn("ghs_argsecretvalue", printed)
        pid = int(re.search(r"left process (\d+)", results["alpha"]["message"]).group(1))
        self.assertFalse(Path(f"/proc/{pid}").exists() and "sleep" in Path(f"/proc/{pid}/comm").read_text(), "the survivor is killed")

    def test_a_row_that_changes_what_later_rows_trust_stops_the_job(self):
        self.consumer(f'printf "tampered" >> {self.rows.consumer_path}\n{PASSING_CONSUMER}')
        result = self.rows.run()
        self.assertEqual(result.returncode, 1, result.stderr)
        results = self.rows.results()
        self.assertEqual(results["alpha"]["status"], "fail")
        self.assertIn("changed what later rows trust", results["alpha"]["message"])
        self.assertIn(str(self.rows.consumer_path), results["alpha"]["message"])
        self.assertEqual(results["beta"]["status"], "not-run")
        self.assertIn("the job stopped after the row alpha", results["beta"]["message"])
        self.assertFalse(self.rows.attestation("alpha").exists())
        self.assertFalse(self.rows.attestation("beta").exists())

    def test_a_row_that_changes_an_earlier_attestation_stops_the_job(self):
        attestation = self.rows.attestation("alpha")
        self.consumer(f'if [ "$row" = beta ]; then printf "forged" > {attestation}; fi\n{PASSING_CONSUMER}')
        result = self.rows.run()
        self.assertEqual(result.returncode, 1, result.stderr)
        results = self.rows.results()
        self.assertEqual(results["alpha"]["status"], "pass")
        self.assertEqual(results["beta"]["status"], "fail")
        self.assertIn(str(attestation), results["beta"]["message"])

    def test_a_user_service_left_behind_fails_its_row(self):
        self.consumer(
            f'if [ "$row" = alpha ]; then printf "pohunek-left.service\\n" >> {self.rows.units}; fi\n{PASSING_CONSUMER}'
        )
        result = self.rows.run()
        self.assertEqual(result.returncode, 1, result.stderr)
        results = self.rows.results()
        self.assertEqual(results["alpha"]["status"], "fail")
        self.assertIn("added service entry pohunek-left.service", results["alpha"]["message"])
        self.assertEqual(results["beta"]["status"], "pass")

    def test_a_row_over_its_limit_is_named_and_its_process_group_killed(self):
        self.consumer(
            'if [ "$row" = alpha ]; then\n'
            '    printf "%s\\n" "$$" > "$LOG/alpha.pid"\n'
            "    sleep 300 &\n"
            '    printf "%s\\n" "$!" > "$LOG/alpha.child"\n'
            "    wait\n"
            "fi\n"
            f"{PASSING_CONSUMER}"
        )
        started = time.monotonic()
        result = self.rows.run(limit=3)
        self.assertLess(time.monotonic() - started, 60)
        self.assertEqual(result.returncode, 1, result.stderr)
        results = self.rows.results()
        self.assertEqual(results["alpha"]["status"], "timeout")
        self.assertIn("the row exceeded its limit of 3 s", results["alpha"]["message"])
        self.assertEqual(results["beta"]["status"], "pass")
        for name in ("pid", "child"):
            pid = self.rows.recorded("alpha", name)
            with self.subTest(process=name):
                self.assertFalse(Path(f"/proc/{pid}").exists() and Path(f"/proc/{pid}/stat").read_text().split()[2] != "Z")

    def test_invalid_rows_and_a_used_output_directory_are_refused(self):
        self.consumer(PASSING_CONSUMER)
        for rows in ("alpha", "Alpha:x86_64-unknown-linux-gnu", "alpha:x86_64-unknown-linux-gnu alpha:x86_64-unknown-linux-gnu", ""):
            with self.subTest(rows=rows):
                result = run(
                    "run-rows",
                    cwd=self.rows.checkout,
                    env={"ROWS": rows, "ROW_LIMIT_SECONDS": "60"},
                )
                self.assertEqual(result.returncode, 2)
                self.assertIn("run-rows:", result.stderr)
        self.rows.out_dir.mkdir()
        (self.rows.out_dir / "stale").write_text("")
        result = self.rows.run()
        self.assertEqual(result.returncode, 2)
        self.assertIn("OUT_DIR is not empty", result.stderr)


def load_run_rows():
    """run-rows as a module, for the guards that differ between the OSes."""
    loader = importlib.machinery.SourceFileLoader("run_rows", str(SCRIPTS / "run-rows"))
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader("run_rows", loader))
    # Its dataclasses resolve their module through sys.modules.
    sys.modules["run_rows"] = module
    loader.exec_module(module)
    return module


@unittest.skipUnless(sys.platform in ("linux", "darwin"), "the row runners are Linux and macOS")
class RunRowsGuardTests(unittest.TestCase):
    """The account-wide guards on the OS that runs the tests."""

    def setUp(self):
        self.module = load_run_rows()

    def test_the_survivor_scan_names_a_process_by_its_variable_and_never_its_value(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        environment = dict(os.environ, PROBE_TOKEN=f"ghs_scansecretvalue:{root}/x")
        sleeper = subprocess.Popen(["sleep", "60"], env=environment, start_new_session=True)
        self.addCleanup(sleeper.wait)
        self.addCleanup(sleeper.kill)
        scan = self.module.macos_survivors if sys.platform == "darwin" else self.module.linux_survivors
        found = [survivor for survivor in scan([str(root), os.path.realpath(root)]) if survivor.pid == sleeper.pid]
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0].key, "PROBE_TOKEN")
        self.assertIn("sleep", found[0].executable)
        self.assertNotIn("ghs_scansecretvalue", repr(found))
        self.assertEqual([survivor for survivor in scan(["/nonexistent-scope"]) if survivor.pid == sleeper.pid], [])

    def test_the_service_and_keychain_listings_are_readable_where_the_runner_has_them(self):
        services = self.module.services()
        keychain = self.module.keychain_items()
        if sys.platform == "darwin":
            self.assertIsInstance(services, set)
            self.assertTrue(services)
            self.assertTrue(keychain is None or all("\n" not in item for item in keychain))
        else:
            self.assertIsNone(keychain)
        self.assertEqual(self.module.services() is None, services is None)

    def test_the_tool_manifest_is_stable_while_nothing_changes(self):
        manifest = self.module.Manifest([SCRIPTS / "run-row"]) if (ROOT / ".git").exists() else None
        if manifest is None:
            self.skipTest("the checkout state needs a git checkout")
        self.assertEqual(manifest.changes(), [])



class NodeVersionTests(unittest.TestCase):
    def test_the_repository_matrix_resolves_to_the_highest_declared_minimum(self):
        result = run("node-version", "compat/matrix.json")
        self.assertEqual(result.returncode, 0, result.stderr)
        minimums = []
        for lock in sorted((ROOT / "compat").glob("*/compatibility-lock.json")):
            data = json.loads(lock.read_text())
            node_min = data.get("upstream", {}).get("node_min") if isinstance(data.get("upstream"), dict) else None
            if node_min and (ROOT / "runtime-packages" / lock.parent.name).is_dir():
                minimums.append(tuple(int(part) for part in node_min.split(".")))
        self.assertEqual(result.stdout.strip(), ".".join(str(part) for part in max(minimums)))

    def test_a_matrix_without_a_declared_minimum_is_refused(self):
        directory = Path(self.enterContext(tempfile.TemporaryDirectory()))
        (directory / "compat" / "alpha").mkdir(parents=True)
        (directory / "compat" / "alpha" / "compatibility-lock.json").write_text('{"upstream": {}}')
        matrix = directory / "matrix.json"
        matrix.write_text(json.dumps({"rows": [{"runtime": "alpha", "target": "t"}]}))
        result = run("node-version", str(matrix), cwd=directory)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")

    def test_the_highest_minimum_wins_by_numeric_order(self):
        directory = Path(self.enterContext(tempfile.TemporaryDirectory()))
        rows = []
        for name, minimum in (("alpha", "20.9.0"), ("beta", "22.19.0"), ("gamma", "22.4.1")):
            (directory / "compat" / name).mkdir(parents=True)
            (directory / "compat" / name / "compatibility-lock.json").write_text(
                json.dumps({"upstream": {"node_min": minimum}})
            )
            rows.append({"runtime": name, "target": "t"})
        matrix = directory / "matrix.json"
        matrix.write_text(json.dumps({"rows": rows}))
        result = run("node-version", str(matrix), cwd=directory)
        self.assertEqual((result.returncode, result.stdout), (0, "22.19.0\n"))


class PreparePolicyTests(unittest.TestCase):
    def setUp(self):
        self.directory = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.policy = str(ROOT / "packaging" / "release-policy.json")

    def test_without_targets_the_policy_is_copied_unchanged(self):
        output = self.directory / "policy.json"
        self.assertEqual(run("prepare-policy", self.policy, str(output)).returncode, 0)
        self.assertEqual(output.read_bytes(), Path(self.policy).read_bytes())

    def test_targets_keep_only_their_archives_and_every_other_field(self):
        output = self.directory / "policy.json"
        result = run("prepare-policy", self.policy, str(output), "x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl")
        self.assertEqual(result.returncode, 0, result.stderr)
        original = json.loads(Path(self.policy).read_text())
        reduced = json.loads(output.read_text())
        self.assertEqual(
            reduced["archives"],
            [slot for slot in original["archives"] if "-linux-" in slot["target"]],
        )
        self.assertEqual({k: v for k, v in reduced.items() if k != "archives"}, {k: v for k, v in original.items() if k != "archives"})

    def test_an_existing_output_is_refused(self):
        output = self.directory / "policy.json"
        output.write_text("x")
        self.assertNotEqual(run("prepare-policy", self.policy, str(output)).returncode, 0)
        self.assertEqual(output.read_text(), "x")


class BuildPackagesFixture:
    """A checkout with two package directories and a stand-in xtask.

    The stand-in records its calls, writes placeholder archive bytes and,
    like the real builder, the four evidence files into `--evidence-dir`,
    with a `sha256sum`-format checksum unless `BAD_CHECKSUM` is set. Every
    other step is build-packages' own.
    """

    RUNTIMES = ("alpha", "beta")
    VERSION = "1.0.0"
    EVIDENCE_SUFFIXES = (".manifest.json", ".provenance-input.json", ".compat-vector.json")

    def __init__(self, directory: Path):
        self.directory = directory
        self.checkout = directory / "checkout"
        for runtime in self.RUNTIMES:
            (self.checkout / "runtime-packages" / runtime).mkdir(parents=True)
        self.calls = directory / "calls"
        self.xtask = directory / "tools" / "xtask"
        self.xtask.parent.mkdir()
        self.xtask.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            'printf \'%s\\n\' "$*" >> "$CALLS"\n'
            'case "$1 $2" in\n'
            '    "package verify") exit 0 ;;\n'
            '    "package build") ;;\n'
            '    *) echo "unexpected xtask call: $*" >&2; exit 2 ;;\n'
            "esac\n"
            'dir=$3\n'
            '[ "$4" = --output ] && [ "$6" = --evidence-dir ] || { echo "bad arguments: $*" >&2; exit 2; }\n'
            'archive=$5\n'
            'evidence=$7\n'
            'runtime=$(basename -- "$dir")\n'
            'name=$(basename -- "$archive")\n'
            'printf \'archive of %s\\n\' "$runtime" > "$archive"\n'
            'mkdir -p "$evidence"\n'
            'if [ -n "${BAD_CHECKSUM:-}" ]; then\n'
            '    printf \'%064d  %s\\n\' 0 "$name" > "$evidence/$name.sha256"\n'
            "else\n"
            '    (cd "$(dirname -- "$archive")" && sha256sum -- "$name") > "$evidence/$name.sha256"\n'
            "fi\n"
            'for suffix in .manifest.json .provenance-input.json .compat-vector.json; do\n'
            "    printf '{}\\n' > \"$evidence/$runtime$suffix\"\n"
            "done\n"
        )
        self.xtask.chmod(0o755)

    def run(self, *args: str, **env: str) -> subprocess.CompletedProcess:
        return run(
            "build-packages",
            *args,
            cwd=self.checkout,
            env={"XTASK": str(self.xtask), "CALLS": str(self.calls), **env},
        )

    def archive(self, runtime: str) -> str:
        return f"pohunek-runtime-{runtime}-{self.VERSION}.tar.zst"


class BuildPackagesTests(unittest.TestCase):
    def setUp(self):
        self.fixture = BuildPackagesFixture(Path(self.enterContext(tempfile.TemporaryDirectory())))
        self.output = self.fixture.directory / "package-archives"
        self.evidence = self.fixture.directory / "package-evidence"

    def test_evidence_goes_outside_the_archives_which_hold_only_archives_and_checksums(self):
        result = self.fixture.run(self.fixture.VERSION, str(self.output), str(self.evidence))
        self.assertEqual(result.returncode, 0, result.stderr)
        archives = [self.fixture.archive(runtime) for runtime in self.fixture.RUNTIMES]
        self.assertEqual(
            sorted(path.name for path in self.output.iterdir()),
            sorted([*archives, *(f"{name}.sha256" for name in archives)]),
        )
        self.assertEqual(
            sorted(path.name for path in self.evidence.iterdir()),
            sorted(
                [
                    *(f"{name}.sha256" for name in archives),
                    *(
                        f"{runtime}{suffix}"
                        for runtime in self.fixture.RUNTIMES
                        for suffix in BuildPackagesFixture.EVIDENCE_SUFFIXES
                    ),
                ]
            ),
        )
        for name in archives:
            with self.subTest(archive=name):
                digest = hashlib.sha256((self.output / name).read_bytes()).hexdigest()
                self.assertEqual((self.output / f"{name}.sha256").read_text(), f"{digest}  {name}\n")
        builds = [line for line in self.fixture.calls.read_text().splitlines() if line.startswith("package build")]
        self.assertEqual(
            builds,
            [
                f"package build runtime-packages/{runtime} --output {self.output}/{self.fixture.archive(runtime)} "
                f"--evidence-dir {self.evidence}"
                for runtime in self.fixture.RUNTIMES
            ],
        )

    def assert_nothing_created(self, *paths: Path) -> None:
        self.assertFalse(self.fixture.calls.exists(), "nothing may be built")
        for path in paths:
            self.assertFalse(path.exists(), f"a refused run created {path}")

    def test_an_evidence_directory_inside_or_equal_to_the_output_is_refused(self):
        for evidence in (self.output, self.output / "evidence"):
            with self.subTest(evidence=evidence):
                result = self.fixture.run(self.fixture.VERSION, str(self.output), str(evidence))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("must lie outside the output directory", result.stderr)
                self.assert_nothing_created(self.output, evidence)

    def test_an_output_directory_inside_the_evidence_is_refused(self):
        output = self.evidence / "archives"
        result = self.fixture.run(self.fixture.VERSION, str(output), str(self.evidence))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must lie outside the evidence directory", result.stderr)
        self.assert_nothing_created(output, self.evidence)

    def test_an_overlap_through_a_symbolic_link_is_refused(self):
        # `links/out` resolves into the output directory although its spelling
        # does not start with it.
        self.output.mkdir()
        links = self.fixture.directory / "links"
        links.mkdir()
        (links / "out").symlink_to(self.output)
        evidence = links / "out" / "evidence"
        result = self.fixture.run(self.fixture.VERSION, str(self.output), str(evidence))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must lie outside the output directory", result.stderr)
        self.assert_nothing_created(self.output / "evidence")

    def test_a_builder_checksum_that_differs_from_sha256sum_fails(self):
        result = self.fixture.run(self.fixture.VERSION, str(self.output), str(self.evidence), BAD_CHECKSUM="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("builder checksum differs from sha256sum", result.stderr)

    def test_the_evidence_directory_is_required(self):
        result = self.fixture.run(self.fixture.VERSION, str(self.output))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("usage: build-packages <version> <output-dir> <evidence-dir>", result.stderr)


class CollectInputsTests(unittest.TestCase):
    def setUp(self):
        self.directory = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.out = self.directory / "inputs"

    def write(self, relative: str, content: bytes = b"data") -> Path:
        path = self.directory / "dl" / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        return path

    def sidecar(self, archive: Path, digest: str | None = None) -> None:
        digest = digest or hashlib.sha256(archive.read_bytes()).hexdigest()
        Path(f"{archive}.sha256").write_text(f"{digest}  {archive.name}\n")

    def collect(self, *dirs: str):
        return run("collect-inputs", str(self.out), *[str(self.directory / "dl" / d) for d in dirs])

    def test_files_from_every_download_directory_are_merged_flat(self):
        self.write("build/build-cli/pohunek-cli-1.0.0-t.tar.gz")
        self.write("sdk/pohunek-ts-sdk-1.0.0.tgz")
        result = self.collect("build", "sdk")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            sorted(path.name for path in self.out.iterdir()),
            ["pohunek-cli-1.0.0-t.tar.gz", "pohunek-ts-sdk-1.0.0.tgz"],
        )

    def test_package_checksums_are_checked_and_not_copied(self):
        archive = self.write("packages/pohunek-runtime-alpha-1.0.0.tar.zst")
        self.sidecar(archive)
        result = self.collect("packages")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([path.name for path in self.out.iterdir()], ["pohunek-runtime-alpha-1.0.0.tar.zst"])

    def test_a_package_checksum_mismatch_is_refused(self):
        archive = self.write("packages/pohunek-runtime-alpha-1.0.0.tar.zst")
        self.sidecar(archive, "0" * 64)
        self.assertNotEqual(self.collect("packages").returncode, 0)

    def test_a_repeated_name_is_refused(self):
        self.write("a/x.tar.gz")
        self.write("b/x.tar.gz")
        result = self.collect("a", "b")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("repeated input", result.stderr)

    def test_a_link_is_refused(self):
        target = self.write("a/real.tar.gz")
        (self.directory / "dl" / "a" / "link.tar.gz").symlink_to(target)
        self.assertNotEqual(self.collect("a").returncode, 0)

    def test_an_unsafe_file_name_is_refused(self):
        self.write("a/bad name.tar.gz")
        self.assertNotEqual(self.collect("a").returncode, 0)

    def test_an_existing_output_is_refused(self):
        self.write("a/x.tar.gz")
        self.out.mkdir()
        self.assertNotEqual(self.collect("a").returncode, 0)


class AssembleRefusalTests(unittest.TestCase):
    """Only the argument checks run: they precede every xtask call."""

    def setUp(self):
        self.directory = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.inputs = self.directory / "inputs"
        self.inputs.mkdir()
        self.commit = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=True
        ).stdout.strip()
        tools = self.directory / "tools"
        tools.mkdir()
        self.xtask = tools / "xtask"
        self.xtask.write_text("#!/bin/sh\nexit 0\n")
        self.xtask.chmod(0o755)

    def assemble(self, mode: str, **extra: str):
        env = {
            "VERSION": "1.0.0",
            "COMMIT": self.commit,
            "XTASK": str(self.xtask),
            "RUNNER_TEMP": str(self.directory),
        }
        env.update(extra)
        return run("assemble", mode, str(self.inputs), str(self.directory / "bundle"), env=env)

    def test_a_release_without_the_signing_secret_is_refused(self):
        result = self.assemble("release")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("CATALOG_SIGNING_KEY_CI is not set", result.stderr)

    def test_a_release_with_a_target_filter_is_refused(self):
        result = self.assemble("release", TARGETS="x86_64-unknown-linux-gnu", CATALOG_SIGNING_KEY_CI="0" * 64)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("full policy", result.stderr)
        self.assertNotIn("0" * 64, result.stderr + result.stdout)

    def test_a_rehearsal_that_can_see_the_release_key_is_refused(self):
        result = self.assemble("rehearsal", RUN_ID="1", CATALOG_SIGNING_KEY_CI="0" * 64)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must not see the release signing key", result.stderr)

    def test_a_checkout_at_another_commit_and_an_unknown_mode_are_refused(self):
        self.assertNotEqual(self.assemble("release", COMMIT="0" * 40).returncode, 0)
        self.assertNotEqual(self.assemble("publish").returncode, 0)

    def test_a_rehearsal_key_is_never_derived_for_a_tag(self):
        result = run(
            "rehearsal-trust",
            str(self.directory / "trust"),
            "1",
            self.commit,
            env={"XTASK": str(self.xtask), "GITHUB_REF_TYPE": "tag"},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("never derived for a tag", result.stderr)
        self.assertFalse((self.directory / "trust").exists())




if __name__ == "__main__":
    unittest.main()
