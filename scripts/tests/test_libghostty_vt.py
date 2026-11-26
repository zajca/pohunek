"""libghostty-vt build infrastructure (issue #2538): pins, the action, the lane guard.

The action scenarios run the real `.github/actions/libghostty-vt` steps and the
real `scripts/build-libghostty-vt` through `composite_harness`, with a fake Zig
that writes the files a real build writes: a miss builds and saves, a later hit
restores with no Zig and no download, and a forced rebuild runs from the cached
Zig and tarballs alone.
"""

import hashlib
import io
import json
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest
from pathlib import Path
from urllib.parse import urlparse

import composite_harness as harness

ROOT = harness.ROOT
WORKFLOWS = ROOT / ".github" / "workflows"
ACTIONS = ROOT / ".github" / "actions"
SCRIPTS = ROOT / "scripts"
PIN = SCRIPTS / "libghostty-vt.pin.toml"
LIBRARY_ACTION = "./.github/actions/libghostty-vt"
MACOS_ACTION = "./.github/actions/libghostty-vt-macos"
ACTIONS_USED = (LIBRARY_ACTION, MACOS_ACTION)
CARGO_LANE = re.compile(
    r"\bcargo\s+(build|check|clippy|test|nextest|run|hack|udeps|xtask|install|doc|bench|llvm-cov|fuzz|zigbuild)\b"
    r"|packaging/macos/build-release|scripts/darwin-check|\bxtask\b"
)

# Jobs that compile a crate which will link the library and already provide it.
WIRED_LANES = {
    "libghostty-spike.yml:spike-libghostty-linux",
    "libghostty-spike.yml:spike-libghostty-macos",
    "release-build.yml:binaries",
    "release-build.yml:kit-linux",
    "release-build.yml:kit-macos",
    "release-build.yml:packages",
    "release-build.yml:rehearsal-macos",
    "release.yml:stage-macos",
}
# Jobs that never link the library.
EXEMPT_LANES = {
    "libghostty-spike.yml:spike-linux",
}
# Jobs that compile pohunek crates and get the library when `pohunek-terminal`
# links it (the default screen model switch of issue #2538). Moving a job to
# WIRED_LANES is the same change that adds the action to it; the list must be
# empty when the library is the default model.
PENDING_LANES = {
    "ci.yml:platform-contracts",
    "ci.yml:macos-tier",
    "ci.yml:macos-release-build",
    "ci.yml:macos-package",
    "ci.yml:test",
    "ci.yml:darwin-check",
    "ci.yml:test-selection",
    "ci.yml:build-tests",
    "ci.yml:systemd-supervision",
    "ci.yml:test-selection-audit",
    "ci.yml:doctests",
    "ci.yml:release-build",
    "ci.yml:ts-drift",
    "ci.yml:hermes-package",
    "ci.yml:package-selection",
    "ci.yml:package-version-bump",
    "ci.yml:pi-package",
    "ci.yml:codex-package",
    "ci.yml:claude-package",
    "ci.yml:hermes-compatibility-macos",
    "ci.yml:build-bins",
    "ci.yml:sdk-macos",
    "ci.yml:docs",
    "ci.yml:feature-matrix",
    "ci.yml:udeps",
    "gate-tests.yml:tests",
    # Both build the core checkout of a plugin repository; the action lives in
    # that checkout (`core/.github/actions/...`).
    "plugin-ci.yml:check",
    "plugin-release.yml:build",
    "release-evidence.yml:rows",
    "release-evidence.yml:assemble",
    "release-evidence.yml:assemble-rehearsal",
    "release-evidence.yml:smoke",
    "release.yml:docs-gate",
    "release.yml:sdk-gate",
    "release.yml:verify-macos",
}

ZIG_VERSION = "9.9.9"


def pin():
    return tomllib.loads(PIN.read_text())


def workflow_jobs():
    """{"workflow.yml:job": parsed steps} for every workflow job that has steps."""
    found = {}
    for path in sorted(WORKFLOWS.glob("*.yml")):
        text = path.read_text()
        if "jobs:\n" not in text:
            continue
        for name in re.findall(r"(?m)^  ([A-Za-z0-9_-]+):\n", text.split("jobs:\n", 1)[1]):
            try:
                found[f"{path.name}:{name}"] = harness.job_steps(text, name)
            except AssertionError:
                continue
    return found


class PinTests(unittest.TestCase):
    def test_zig_version_and_sha256_equal_the_workflow_pins(self):
        zig = pin()["zig"]
        for name in ("ci.yml", "pinned-cache-offline.yml"):
            env = harness.workflow_env((WORKFLOWS / name).read_text())
            with self.subTest(workflow=name):
                self.assertEqual(env["ZIG_VERSION"], zig["version"])
                self.assertEqual(env["ZIG_SHA256"], zig["linux_x86_64_sha256"])

    def test_every_release_target_has_a_zig_target(self):
        release = (WORKFLOWS / "release-build.yml").read_text()
        wanted = set(re.findall(r"(?m)^\s+- target: (\S+)$", release)) | {"aarch64-apple-darwin"}
        self.assertTrue({"x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"} <= wanted, wanted)
        self.assertLessEqual(wanted, set(pin()["targets"]))

    def test_the_macos_zig_target_carries_the_release_deployment_target(self):
        deployment = (ROOT / "packaging" / "macos" / "DEPLOYMENT_TARGET").read_text().strip()
        self.assertTrue(pin()["targets"]["aarch64-apple-darwin"].endswith(f"macos.{deployment}"))

    def test_the_download_hosts_of_the_pin_are_offline_in_the_proof(self):
        env = harness.workflow_env((WORKFLOWS / "pinned-cache-offline.yml").read_text())
        offline = set(env["OFFLINE_HOSTS"].split())
        for section in ("source", "deps"):
            self.assertIn(urlparse(pin()[section]["url"]).hostname, offline)

    def test_the_size_limit_is_three_mebibytes(self):
        self.assertEqual(pin()["limits"]["max_stripped_size_delta_bytes"], 3 << 20)

    def test_the_scripts_read_the_pin_next_to_them(self):
        for name in ("libghostty-vt-size-gate", "build-libghostty-vt"):
            self.assertIn('"libghostty-vt.pin.toml"', (SCRIPTS / name).read_text())


class LaneGuardTests(unittest.TestCase):
    """Every job that compiles pohunek crates provides the library, or is a
    listed lane that does not yet."""

    def lanes(self):
        lanes = {}
        for job, steps in workflow_jobs().items():
            body = "\n".join(str(step.get("run", "")) for step in steps)
            if CARGO_LANE.search(body):
                lanes[job] = any(step.get("uses") in ACTIONS_USED for step in steps)
        return lanes

    def test_every_cargo_lane_is_classified(self):
        lanes = self.lanes()
        classified = WIRED_LANES | EXEMPT_LANES | PENDING_LANES
        self.assertEqual(set(lanes) - classified, set(), "a new cargo lane: add the libghostty-vt action or list it")
        self.assertEqual(classified - set(lanes), set(), "a listed lane that no longer exists")

    def test_wired_lanes_use_the_action_and_the_others_do_not(self):
        lanes = self.lanes()
        for job in sorted(WIRED_LANES):
            with self.subTest(job=job):
                self.assertTrue(lanes[job], f"{job} does not use a libghostty-vt action")
        for job in sorted(EXEMPT_LANES | PENDING_LANES):
            with self.subTest(job=job):
                self.assertFalse(lanes[job], f"{job} uses the action: move it to WIRED_LANES")

    def test_the_linux_lanes_build_the_target_they_link(self):
        jobs = workflow_jobs()
        expected = {
            "release-build.yml:binaries": "${{ matrix.target }}",
            "release-build.yml:kit-linux": "${{ env.TARGET }}",
            "release-build.yml:packages": "x86_64-unknown-linux-gnu",
        }
        for job, target in expected.items():
            steps = [step for step in jobs[job] if step.get("uses") == LIBRARY_ACTION]
            with self.subTest(job=job):
                self.assertEqual([step["with"]["target"] for step in steps], [target])
                self.assertRegex(str(steps[0].get("timeout-minutes")), r"^\d+$")

    def test_the_macos_lanes_download_the_cross_built_library_of_their_workflow(self):
        jobs = workflow_jobs()
        artifacts = {
            "release-build.yml:kit-macos": "libghostty-vt-darwin-release-build",
            "release-build.yml:rehearsal-macos": "libghostty-vt-darwin-release-build",
            "release.yml:stage-macos": "libghostty-vt-darwin-release",
        }
        for job, artifact in artifacts.items():
            steps = [step for step in jobs[job] if step.get("uses") == MACOS_ACTION]
            with self.subTest(job=job):
                self.assertEqual([step["with"]["artifact"] for step in steps], [artifact])
                producer = f"{job.split(':')[0]}:libghostty-darwin"
                uploads = [
                    step for step in jobs[producer]
                    if str(step.get("uses", "")).startswith("actions/upload-artifact@")
                    and (step.get("with") or {}).get("name") == artifact
                ]
                self.assertEqual(len(uploads), 1)

    def test_the_macos_jobs_need_their_producer(self):
        for name, jobs in (("release-build.yml", ("kit-macos", "rehearsal-macos")), ("release.yml", ("stage-macos",))):
            text = (WORKFLOWS / name).read_text()
            for job in jobs:
                block = re.search(rf"(?ms)^  {job}:\n(.*?)(?=^  [A-Za-z0-9_-]+:\n|\Z)", text).group(1)
                with self.subTest(workflow=name, job=job):
                    self.assertRegex(block, r"(?m)^    needs: \[[^\]]*\blibghostty-darwin\b")


class WorkflowWiringTests(unittest.TestCase):
    def test_the_spike_builds_through_the_shared_action_only(self):
        text = (WORKFLOWS / "libghostty-spike.yml").read_text()
        self.assertNotIn("ZIG_VERSION", text)
        self.assertNotIn("setup-zig", text)
        self.assertNotIn("--fetch", text)
        self.assertNotIn("spikes/libghostty-vt/libghostty-vt.pin.toml", text)
        steps = harness.job_steps(text, "spike-libghostty-linux")
        targets = [step["with"]["target"] for step in steps if step.get("uses") == LIBRARY_ACTION]
        self.assertEqual(targets, ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl", "aarch64-apple-darwin"])

    def test_the_producer_workflow_saves_the_entries_on_main(self):
        text = (WORKFLOWS / "libghostty-vt.yml").read_text()
        self.assertRegex(text, r"(?m)^  push:\n    branches: \[main\]$")
        library = harness.job_steps(text, "library")
        self.assertEqual([step["with"]["target"] for step in library if step.get("uses") == LIBRARY_ACTION], ["${{ matrix.target }}"])
        matrix = set(re.findall(r"(?m)^          - (\S+)$", text.split("  macos:", 1)[0]))
        self.assertEqual(matrix, set(pin()["targets"]))
        macos = harness.job_steps(text, "macos")
        self.assertTrue(any(step.get("uses") == MACOS_ACTION for step in macos))
        self.assertTrue(any("libghostty-vt-size-gate" in str(step.get("run", "")) for step in macos))

    def test_the_offline_proof_runs_in_both_workflows(self):
        proof = (WORKFLOWS / "pinned-cache-offline.yml").read_text()
        producer = (WORKFLOWS / "libghostty-vt.yml").read_text()
        self.check_offline_job(proof, "libghostty-vt")
        self.check_offline_job(producer, "offline")
        # On a pull request the proof of pinned-cache-offline.yml would read
        # entries no main run has saved; the producer workflow proves it in
        # its own run, after the job that saved them.
        block = re.search(r"(?ms)^  libghostty-vt:\n(.*?)(?=^  [A-Za-z0-9_-]+:\n|\Z)", proof).group(1)
        self.assertRegex(block, r"(?m)^    if: \$\{\{ github\.event_name != 'pull_request' \}\}$")
        block = re.search(r"(?ms)^  offline:\n(.*?)(?=^  [A-Za-z0-9_-]+:\n|\Z)", producer).group(1)
        self.assertRegex(block, r"(?m)^    needs: library$")
        self.assertEqual(
            harness.workflow_env(producer)["OFFLINE_HOSTS"], harness.workflow_env(proof)["OFFLINE_HOSTS"]
        )

    def check_offline_job(self, text, job):
        steps = harness.job_steps(text, job)
        offline = [i for i, step in enumerate(steps) if "pinned-cache-offline apply" in str(step.get("run", ""))]
        calls = [i for i, step in enumerate(steps) if step.get("uses") == LIBRARY_ACTION]
        self.assertEqual(len(offline), 1)
        self.assertEqual(len(calls), 2)
        self.assertLess(offline[0], min(calls))
        restore, rebuild = (steps[i] for i in calls)
        self.assertNotIn("force-build", restore["with"])
        self.assertEqual(rebuild["with"]["force-build"], "true")
        last = steps[-1]
        self.assertEqual(last["name"], "Require a hit for every restore")
        self.assertNotIn("if", last)
        self.assertEqual(
            last["env"]["HITS"],
            f"library=${{{{ steps.{restore['id']}.outputs.hit }}}} "
            f"zig=${{{{ steps.{rebuild['id']}.outputs.zig-hit }}}} "
            f"tarballs=${{{{ steps.{rebuild['id']}.outputs.tarballs-hit }}}}",
        )

    def test_the_release_attests_the_pin_record(self):
        build = (WORKFLOWS / "release-build.yml").read_text()
        steps = harness.job_steps(build, "libghostty-darwin")
        uploads = [
            step["with"]["name"] for step in steps if str(step.get("uses", "")).startswith("actions/upload-artifact@")
        ]
        self.assertIn("libghostty-vt-evidence", uploads)
        self.assertTrue(any("--evidence" in str(step.get("run", "")) for step in steps))
        evidence = (WORKFLOWS / "release-evidence.yml").read_text()
        attest = harness.job_steps(evidence, "attest")
        downloads = [
            (step.get("with") or {}).get("name") for step in attest
            if str(step.get("uses", "")).startswith("actions/download-artifact@")
        ]
        self.assertIn("libghostty-vt-evidence", downloads)


def tar_xz(members):
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:xz") as archive:
        for name, body in members.items():
            info = tarfile.TarInfo(name)
            info.size = len(body)
            info.mode = 0o755
            archive.addfile(info, io.BytesIO(body))
    return buffer.getvalue()


def source_tarball(version, suffix):
    """A gzip tarball that unpacks to `libghostty-vt-<version><suffix>/`."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        directory = tarfile.TarInfo(f"libghostty-vt-{version}{suffix}")
        directory.type = tarfile.DIRTYPE
        directory.mode = 0o755
        archive.addfile(directory)
        body = b"content\n"
        info = tarfile.TarInfo(f"libghostty-vt-{version}{suffix}/build.zig")
        info.size = len(body)
        archive.addfile(info, io.BytesIO(body))
    return buffer.getvalue()


def fake_zig(state):
    """A zig that prints the test version and writes the files a real build writes."""
    return f"""#!/bin/sh
echo "zig $1" >> "{state}/zig-calls.log"
case "$1" in
  version) echo {ZIG_VERSION} ;;
  build)
    prefix=""
    while [ $# -gt 0 ]; do case "$1" in --prefix) prefix="$2"; shift 2 ;; *) shift ;; esac; done
    mkdir -p "$prefix/lib" "$prefix/include/ghostty"
    echo "!<arch>" > "$prefix/lib/libghostty-vt.a"
    echo "/* header */" > "$prefix/include/ghostty/vt.h" ;;
  cc)
    out=""
    while [ $# -gt 0 ]; do case "$1" in -o) out="$2"; shift 2 ;; *) shift ;; esac; done
    printf '#!/bin/sh\\necho GHOSTTY_CELL_SIZE=16\\n' > "$out"
    chmod +x "$out" ;;
  *) exit 2 ;;
esac
"""


class ActionScenarioTests(unittest.TestCase):
    TARGET = "x86_64-unknown-linux-gnu"

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.cache = harness.FakeCache()
        root = Path(self.tmp.name)
        self.sources = root / "sources"
        self.sources.mkdir()
        version = pin()["library"]["version"]
        source = source_tarball(version, "")
        deps = source_tarball(version, "-deps")
        self.source_path = self.sources / f"libghostty-vt-{version}.tar.gz"
        self.deps_path = self.sources / f"libghostty-vt-{version}-deps.tar.gz"
        self.source_path.write_bytes(source)
        self.deps_path.write_bytes(deps)
        self.pin_text = self.fixture_pin(source, deps)

    def fixture_pin(self, source, deps):
        """The real pin with the Zig version, the tarball URLs and their hashes of the fixtures.

        The Zig archive hash is patched per runner in `prepare`: the fake zig
        embeds the runner's state directory.
        """
        real = pin()
        text = PIN.read_text()
        replacements = [
            (f'version = "{real["zig"]["version"]}"', f'version = "{ZIG_VERSION}"'),
            (real["source"]["url"], self.source_path.as_uri()),
            (real["source"]["sha256"], hashlib.sha256(source).hexdigest()),
            (real["deps"]["url"], self.deps_path.as_uri()),
            (real["deps"]["sha256"], hashlib.sha256(deps).hexdigest()),
        ]
        for old, new in replacements:
            self.assertIn(old, text)
            text = text.replace(old, new)
        return text

    def runner(self):
        return harness.Harness(self.tmp.name, self.cache, {"TMPDIR": str(Path(self.tmp.name) / "tmp")})

    def prepare(self, runner):
        """Put the scripts, the test pin and the fake Zig release on a fresh runner."""
        (Path(self.tmp.name) / "tmp").mkdir(exist_ok=True)
        scripts = runner.workspace / "scripts"
        scripts.mkdir()
        for name in ("build-libghostty-vt", "libghostty-vt-abi-probe.c"):
            shutil.copy2(SCRIPTS / name, scripts / name)
        archive = tar_xz({f"zig-x86_64-linux-{ZIG_VERSION}/zig": fake_zig(runner.state).encode()})
        (runner.state / "downloads" / f"zig-x86_64-linux-{ZIG_VERSION}.tar.xz").write_bytes(archive)
        text = self.pin_text.replace(
            pin()["zig"]["linux_x86_64_sha256"], hashlib.sha256(archive).hexdigest()
        )
        (scripts / "libghostty-vt.pin.toml").write_text(text)

    def provide(self, runner, **inputs):
        return runner.run_action(".github/actions/libghostty-vt", {"target": self.TARGET, **inputs})

    def zig_calls(self, runner):
        log = runner.state / "zig-calls.log"
        return log.read_text().splitlines() if log.exists() else []

    def populate(self):
        miss = self.runner()
        self.prepare(miss)
        outputs = self.provide(miss)
        return miss, outputs

    def test_a_miss_builds_and_saves_and_a_later_hit_restores_without_zig_or_a_download(self):
        miss, outputs = self.populate()
        self.assertEqual((outputs["hit"], outputs["zig-hit"], outputs["tarballs-hit"]), ("false", "false", "false"))
        self.assertIn("zig build", self.zig_calls(miss))
        keys = sorted(key for key, _ in self.cache.saves)
        self.assertEqual(len(keys), 3, keys)
        self.assertTrue(any(key.startswith("pinned-zig-") for key in keys))
        self.assertTrue(any(key.startswith("libghostty-vt-tarballs-") for key in keys))
        self.assertTrue(any(key.startswith(f"libghostty-vt-Linux-{self.TARGET}-") for key in keys))
        directory = Path(outputs["dir"])
        info = (directory / "BUILD_INFO").read_text()
        self.assertIn(f"rust_target={self.TARGET}", info)
        self.assertIn("archive_sha256=", info)
        self.assertIn(f"LIBGHOSTTY_VT_DIR={directory}", miss.github_env.read_text())

        # Both tarball sources are gone and the network is off: the hit needs neither.
        self.source_path.unlink()
        self.deps_path.unlink()
        hit = self.runner()
        hit.offline()
        self.prepare(hit)
        again = self.provide(hit)
        self.assertEqual(again["hit"], "true")
        self.assertEqual(again["dir"], outputs["dir"])
        self.assertEqual(self.zig_calls(hit), [])
        self.assertEqual(hit.calls("curl"), [])
        self.assertEqual(len(self.cache.saves), 3)
        self.assertIn("verified", hit.output())

    def test_a_forced_rebuild_runs_from_the_cached_zig_and_tarballs_alone(self):
        self.populate()
        saved = len(self.cache.saves)
        self.source_path.unlink()
        self.deps_path.unlink()
        offline = self.runner()
        offline.offline()
        self.prepare(offline)
        outputs = self.provide(offline, **{"force-build": "true"})
        self.assertEqual((outputs["hit"], outputs["zig-hit"], outputs["tarballs-hit"]), ("false", "true", "true"))
        self.assertEqual(offline.calls("curl"), [])
        self.assertTrue(any(call.startswith("zig build") for call in self.zig_calls(offline)))
        self.assertEqual(len(self.cache.saves), saved)

    def test_a_cached_library_that_differs_from_its_build_info_fails_the_step(self):
        self.populate()
        key, path = next(entry for entry in self.cache.saves if entry[0].startswith("libghostty-vt-Linux-"))
        files = {}
        with tarfile.open(fileobj=io.BytesIO(self.cache.entries[(key, path)])) as archive:
            for member in archive.getmembers():
                if member.isfile():
                    files[member.name] = archive.extractfile(member).read()
        files["lib/libghostty-vt.a"] = b"not the built archive"
        self.cache.replace(key, path, files)
        hit = self.runner()
        hit.offline()
        self.prepare(hit)
        with self.assertRaises(harness.StepFailed) as raised:
            self.provide(hit)
        self.assertIn("libghostty-vt.a", raised.exception.output)

    def test_a_target_without_a_pinned_zig_target_fails_before_any_cache_access(self):
        runner = self.runner()
        self.prepare(runner)
        with self.assertRaises(harness.StepFailed) as raised:
            runner.run_action(".github/actions/libghostty-vt", {"target": "riscv64-unknown-linux-gnu"})
        self.assertIn("no Zig target", raised.exception.output)
        self.assertEqual(self.cache.restores, [])

    def test_a_tarball_with_the_wrong_hash_fails_the_download(self):
        self.source_path.write_bytes(b"tampered")
        runner = self.runner()
        self.prepare(runner)
        with self.assertRaises(harness.StepFailed) as raised:
            self.provide(runner)
        self.assertIn("sha256 mismatch", raised.exception.output)
        self.assertEqual([key for key, _ in self.cache.saves if "tarballs" in key], [])


class SizeGateTests(unittest.TestCase):
    def gate(self, delta, limit=None):
        with tempfile.TemporaryDirectory() as tmp:
            measurements = Path(tmp) / "measurements.json"
            measurements.write_text(
                json.dumps({"target": "aarch64-apple-darwin", "size_probe": {"delta_stripped_bytes": delta}})
            )
            command = [sys.executable, "-I", str(SCRIPTS / "libghostty-vt-size-gate"), "--measurements", str(measurements)]
            if limit is not None:
                pin_file = Path(tmp) / "pin.toml"
                pin_file.write_text(f"[limits]\nmax_stripped_size_delta_bytes = {limit}\n")
                command += ["--pin", str(pin_file)]
            return subprocess.run(command, capture_output=True, text=True, timeout=60)

    def test_growth_up_to_the_pinned_limit_passes_and_above_it_fails(self):
        limit = pin()["limits"]["max_stripped_size_delta_bytes"]
        passed = self.gate(limit)
        self.assertEqual(passed.returncode, 0, passed.stderr)
        self.assertIn("limit 3145728 B", passed.stdout)
        failed = self.gate(limit + 1)
        self.assertEqual(failed.returncode, 1)
        self.assertIn(f"grows by {limit + 1} B", failed.stderr)

    def test_the_limit_comes_from_the_pin_file(self):
        self.assertEqual(self.gate(10, limit=10).returncode, 0)
        self.assertEqual(self.gate(11, limit=10).returncode, 1)


if __name__ == "__main__":
    unittest.main()
