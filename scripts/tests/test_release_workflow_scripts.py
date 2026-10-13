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
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
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

    def test_every_row_becomes_an_entry_with_a_runner(self):
        rows = [
            {"runtime": "alpha", "target": "x86_64-unknown-linux-gnu"},
            {"runtime": "alpha", "target": "x86_64-unknown-linux-musl"},
        ]
        result = run("row-matrix", self.matrix(rows))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout),
            {"include": [dict(row, runner="ubuntu-latest") for row in rows]},
        )

    def test_targets_restrict_the_rows_and_an_empty_selection_fails(self):
        rows = [
            {"runtime": "alpha", "target": "x86_64-unknown-linux-gnu"},
            {"runtime": "alpha", "target": "x86_64-unknown-linux-musl"},
        ]
        result = run("row-matrix", self.matrix(rows), "x86_64-unknown-linux-musl")
        self.assertEqual([row["target"] for row in json.loads(result.stdout)["include"]], ["x86_64-unknown-linux-musl"])
        self.assertNotEqual(run("row-matrix", self.matrix(rows), "aarch64-apple-darwin").returncode, 0)

    def test_a_target_without_a_runner_or_an_unsafe_name_is_refused(self):
        for row in (
            {"runtime": "alpha", "target": "aarch64-apple-darwin"},
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
        runtimes = {row["runtime"] for row in json.loads(result.stdout)["include"]}
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
