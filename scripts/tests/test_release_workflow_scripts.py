"""Scenarios for the helper scripts of the release workflows (stdlib only).

They cover the scripts that need neither the network nor a built xtask:
version resolution, the row matrix, the policy filter, input collection and
the refusals of the assembler wrapper. The scripts that run the consumer, stage
upstreams or sign are exercised by the release rehearsal job of ci.yml.
"""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

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

    def assemble(self, mode: str, **extra: str):
        env = {
            "VERSION": "1.0.0",
            "COMMIT": self.commit,
            "XTASK": "/bin/true",
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
            env={"XTASK": "/bin/true", "GITHUB_REF_TYPE": "tag"},
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("never derived for a tag", result.stderr)
        self.assertFalse((self.directory / "trust").exists())


if __name__ == "__main__":
    unittest.main()
