"""The pinned cache (issue #2202): guards and behaviour scenarios.

Every scenario runs the real action and job step bodies through
`composite_harness`: a miss on an empty cache, then a hit on a fresh runner
that shares only the cache, with the network shims offline. A hit can only
succeed from the entry the miss saved, so each pair proves that the save
received exactly the key and the path the restore computed.
"""

import hashlib
import io
import json
import re
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

import composite_harness as harness

ROOT = harness.ROOT
ACTIONS = ROOT / ".github" / "actions"
WORKFLOWS = ROOT / ".github" / "workflows"
PINNED_CACHE = ACTIONS / "pinned-cache" / "action.yml"
PINNED_USES = ("./.github/actions/pinned-cache", "./pinned-cache-action")
# Inputs a restore call may pass: the item and the pins that select its bytes.
RESTORE_INPUTS = {"item", "version", "sha256", "npm-scripts"}
SAVE_INPUTS = {"phase", "key", "path", "hit"}
# Items whose install and check run in the caller, which saves afterwards.
CALLER_SAVED = ("mold", "zig", "uv", "npm:")
UV_TARGETS = ("X86_64_LINUX", "AARCH64_LINUX", "AARCH64_DARWIN", "X86_64_DARWIN")
CI_RETRY_FAST = {"CI_RETRY_BACKOFF_SECONDS": "1"}


def ci_text():
    return (WORKFLOWS / "ci.yml").read_text()


def tarball(mode, members):
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode=mode) as archive:
        for name, body in members.items():
            info = tarfile.TarInfo(name)
            info.size = len(body)
            info.mode = 0o755
            archive.addfile(info, io.BytesIO(body))
    return buffer.getvalue()


def pinned_cache_violations(files):
    """Steps outside the composite that cache a pinned item themselves.

    `files` maps a path to its text. Every `actions/cache` step's `with:`
    block is read whole, so a key or path on any line of the step counts.
    """
    found = []
    for path, text in files.items():
        if path == PINNED_CACHE:
            continue
        for step in harness.iter_steps(text, str(path)):
            if not str(step.get("uses", "")).startswith("actions/cache"):
                continue
            values = " ".join(str(value) for value in (step.get("with") or {}).values())
            if re.search(r"pinned-|pinned-cache", values):
                found.append(f"{path}: {step.get('name') or step['uses']}")
    return found


def repository_files():
    return {path: path.read_text() for path in [*sorted(WORKFLOWS.glob("*.yml")), *sorted(ACTIONS.glob("*/action.yml"))]}


def workflow_job_steps(text):
    """The parsed steps of each job of a workflow that has steps (a job
    that calls a reusable workflow has none)."""
    jobs = {}
    for name in re.findall(r"(?m)^  ([a-z][a-z0-9-]*):\n", text.split("jobs:\n", 1)[1]):
        block = re.search(rf"(?ms)^  {re.escape(name)}:\n(.*?)(?=^  [A-Za-z0-9_-]+:\n|\Z)", text).group(1)
        if re.search(r"(?m)^    steps:\n", block):
            jobs[name] = harness.job_steps(text, name)
    return jobs


def pinned_sites(text, workflow):
    """Every nextest pinned-cache step with its own job: the job is needed
    to resolve a relocated `./pinned-cache-action` the way the runner does."""
    return [
        (name, step)
        for name, steps in workflow_job_steps(text).items()
        for step in steps
        if step.get("uses") in PINNED_USES and (step.get("with") or {}).get("item") == "nextest"
    ]


def pinned_step_lists(text, name):
    """The pinned-cache steps of each job (or action), in order."""
    for steps in harness.iter_step_lists(text, name):
        yield [step for step in steps if step.get("uses") in PINNED_USES]


class PinnedCacheGuardTests(unittest.TestCase):
    def test_only_the_composite_touches_actions_cache_for_the_pinned_items(self):
        self.assertEqual(pinned_cache_violations(repository_files()), [])
        steps = list(harness.iter_steps(PINNED_CACHE.read_text(), "pinned-cache"))
        uses = [str(step.get("uses", "")).split("@")[0] for step in steps if "uses" in step]
        self.assertEqual(uses.count("actions/cache/restore"), 1)
        self.assertEqual(uses.count("actions/cache/save"), 1)

    def test_the_guard_catches_a_setup_action_with_its_own_save_step(self):
        path = ACTIONS / "setup-zig" / "action.yml"
        mutated = path.read_text() + (
            "\n    - name: Save the pinned zig\n"
            "      uses: actions/cache/save@0057852bfaa89a56745cba8c7296529d2fc39830 # v4.3.0\n"
            "      with:\n"
            "        path: ${{ runner.temp }}/pinned-cache/zig\n"
            "        key: >-\n"
            "          pinned-zig-${{ runner.os }}-${{ runner.arch }}-\n"
            "          ${{ inputs.version }}-${{ inputs.sha256 }}-\n"
        )
        files = repository_files()
        files[path] = mutated
        self.assertEqual(pinned_cache_violations(files), [f"{path}: Save the pinned zig"])

    def test_every_call_is_a_restore_with_pins_or_a_save_of_that_restore(self):
        restores = {}
        for path, text in repository_files().items():
            if path == PINNED_CACHE:
                continue
            for steps in pinned_step_lists(text, str(path)):
                self.check_calls(path, steps, restores)
        self.assertEqual(set(restores), {"mold", "zig", "uv", "nextest", "systemd-oomd", "npm:compat/pi/npm", "npm:compat/codex/npm", "npm:compat/claude/npm"})

    def check_calls(self, path, steps, restores):
        ids = {step.get("id"): step for step in steps if step.get("id")}
        for step in steps:
            inputs = step.get("with") or {}
            with self.subTest(file=path.name, step=step.get("name")):
                if inputs.get("phase", "restore") == "restore":
                    self.assertNotIn("phase", inputs)
                    self.assertLessEqual(set(inputs), RESTORE_INPUTS)
                    self.assertIn("item", inputs)
                    restores.setdefault(inputs["item"], []).append((path, step))
                    continue
                # A save takes back exactly the restore's three outputs
                # and never decides itself whether the cache hit.
                self.assertEqual(set(inputs), SAVE_INPUTS)
                self.assertNotIn("if", step)
                source = re.fullmatch(r"\$\{\{ steps\.([a-z-]+)\.outputs\.key \}\}", inputs["key"])
                self.assertIsNotNone(source, inputs["key"])
                restore_id = source.group(1)
                for name in ("path", "hit"):
                    self.assertEqual(inputs[name], f"${{{{ steps.{restore_id}.outputs.{name} }}}}")
                self.assertIn(restore_id, ids)
                self.assertTrue(ids[restore_id]["with"]["item"].startswith(CALLER_SAVED))

    def test_caller_saved_items_save_once_and_inside_saved_items_never(self):
        for path, text in repository_files().items():
            if path == PINNED_CACHE:
                continue
            for steps in pinned_step_lists(text, str(path)):
                self.check_saves(path, steps)

    def check_saves(self, path, steps):
        for step in steps:
            inputs = step.get("with") or {}
            if "item" not in inputs:
                continue
            saves = [
                other for other in steps
                if (other.get("with") or {}).get("key") == f"${{{{ steps.{step.get('id')}.outputs.key }}}}"
            ]
            with self.subTest(file=path.name, item=inputs["item"]):
                if inputs["item"].startswith(CALLER_SAVED):
                    self.assertEqual(len(saves), 1)
                    self.assertGreater(steps.index(saves[0]), steps.index(step))
                else:
                    self.assertEqual(saves, [])

    def test_each_npm_tree_saves_after_its_release_check(self):
        for job, title in (("pi-package", "Pi"), ("codex-package", "Codex"), ("claude-package", "Claude Code")):
            with self.subTest(job=job):
                names = [step.get("name") for step in harness.job_steps(ci_text(), job)]
                self.assertLess(names.index(f"Restore the pinned {title} npm tree"), names.index(f"Verify the pinned {title} release"))
                self.assertEqual(
                    names.index(f"Save the pinned {title} npm tree"), names.index(f"Verify the pinned {title} release") + 1
                )

    def test_every_key_sha_prefix_matches_its_pin(self):
        # A key prefix that is not the head of its pinned sha would bind the
        # key to other bytes. The joined prefixes follow the seed table's
        # target order, so a rotated or truncated list fails.
        uv = (ACTIONS / "setup-uv" / "action.yml").read_text()
        prefixes = re.search(r"(?m)^        sha256: ([0-9a-f]{12}(?:-[0-9a-f]{12}){3})$", uv)
        self.assertIsNotNone(prefixes, uv)
        table = dict(re.findall(r"UV_SHA256_(\w+): ([0-9a-f]{64})", uv))
        self.assertEqual(prefixes.group(1).split("-"), [table[name][:12] for name in UV_TARGETS])
        self.assertEqual(re.findall(r'(?m)^        version: (\S+)$', uv), [re.search(r'UV_VERSION: "([^"]+)"', uv).group(1)])
        for name in ("setup-mold", "setup-zig"):
            with self.subTest(action=name):
                text = (ACTIONS / name / "action.yml").read_text()
                self.assertRegex(text, r"(?m)^        sha256: \$\{\{ inputs\.sha256 \}\}$")
                self.assertRegex(text, r"(?m)^        version: \$\{\{ inputs\.version \}\}$")


class Scenario(unittest.TestCase):
    """A miss runner and a later hit runner sharing one fake cache."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.cache = harness.FakeCache()
        self.env = {**harness.workflow_env(ci_text()), **CI_RETRY_FAST}
        self.runners = 0

    def runner(self, **extra_env):
        self.runners += 1
        return harness.Harness(self.tmp.name, self.cache, {**self.env, **extra_env})

    def assert_fails(self, run, step_fragment):
        with self.assertRaises(harness.StepFailed) as raised:
            run()
        self.assertIn(step_fragment, raised.exception.output)
        return raised.exception


class NextestScenarioTests(Scenario):
    VERSION = "0.9.151"

    def restore(self, runner, version=None):
        return runner.run_action(".github/actions/pinned-cache", {"item": "nextest", "version": version or self.VERSION})

    def test_a_miss_installs_checks_and_saves_and_a_later_hit_needs_no_install(self):
        miss = self.runner()
        outputs = self.restore(miss)
        self.assertEqual(outputs["hit"], "false")
        self.assertEqual(miss.tools, [f"cargo-nextest@{self.VERSION}"])
        self.assertEqual(self.cache.saves, [(outputs["key"], outputs["path"])])
        self.assertEqual(outputs["key"], f"pinned-nextest-Linux-X64-{self.VERSION}")
        self.assertEqual(self.cache.names(outputs["key"], outputs["path"]), ["cargo-nextest"])
        hit = self.runner()
        hit.offline()
        again = self.restore(hit)
        self.assertEqual((again["hit"], again["key"], again["path"]), ("true", outputs["key"], outputs["path"]))
        self.assertEqual(hit.tools, [])
        self.assertEqual(len(self.cache.saves), 1)
        self.assertTrue((hit.home / ".cargo" / "bin" / "cargo-nextest").exists())

    def test_a_cached_binary_of_another_version_fails_the_check(self):
        miss = self.runner()
        outputs = self.restore(miss)
        other = Path(self.tmp.name) / "other"
        harness.write_nextest(other, "0.9.150")
        self.cache.replace(outputs["key"], outputs["path"], {"cargo-nextest": other.read_bytes()})
        self.assert_fails(lambda: self.restore(self.runner()), "is not the pinned 0.9.151")

    def test_a_rustup_shim_on_path_does_not_shadow_the_checked_binary(self):
        # The check calls the binary directly (`"$CARGO_HOME/bin/cargo-nextest"
        # nextest -V`), never through `cargo`: a rustup `cargo` proxy earlier
        # on PATH that resolves the checkout's toolchain file (and downloads
        # the pinned toolchain inside this step, issue #2202) must not change
        # what the step executes. An extra PATH entry shadows the harness
        # `cargo` shim the same way a rustup proxy shadows a plain `cargo`.
        proxy = Path(self.tmp.name) / "proxy"
        proxy.mkdir()
        (proxy / "cargo").write_text(
            '#!/bin/sh\necho "rustup proxy must not run" >&2\nexit 42\n'
        )
        (proxy / "cargo").chmod(0o755)
        miss = self.runner()
        miss.extra_path.append(str(proxy))
        outputs = self.restore(miss)
        self.assertEqual(miss.tools, [f"cargo-nextest@{self.VERSION}"])
        hit = self.runner()
        hit.offline()
        hit.extra_path.append(str(proxy))
        again = self.restore(hit)
        self.assertEqual((again["hit"], again["key"]), ("true", outputs["key"]))

    def test_every_nextest_site_restores_the_same_entry(self):
        first = None
        for workflow in ("ci.yml", "gate-tests.yml"):
            text = (WORKFLOWS / workflow).read_text()
            env = {**harness.workflow_env(text), **CI_RETRY_FAST}
            sites = pinned_sites(text, workflow)
            self.assertTrue(sites, workflow)
            for job, step in sites:
                with self.subTest(workflow=workflow, job=job, site=step.get("name"), uses=step.get("uses")):
                    runner = harness.Harness(self.tmp.name, self.cache, env)
                    if first is not None:
                        runner.offline()
                    # Each site runs through its own `uses:` string: a
                    # relocated `./pinned-cache-action` resolves through the
                    # copy the containing job makes before the step.
                    if step.get("uses") == "./pinned-cache-action":
                        copies = [
                            copy for copy in harness.job_steps(text, job)
                            if "cp -R .github/actions/pinned-cache" in (copy.get("run") or "")
                        ]
                        self.assertEqual(len(copies), 1, f"{job} has no copy step of its own")
                        runner.run_steps(copies, {})
                    runner.run_steps([dict(step)], {})
                    first = first or self.cache.saves[0]
        self.assertEqual(len(self.cache.saves), 1)


class ArchiveScenarioTests(Scenario):
    """mold and zig: the setup action installs, the composite stores the archive."""

    CASES = {
        "mold": (
            "setup-mold",
            "mold-9.9.9-x86_64-linux.tar.gz",
            "w:gz",
            {"mold-9.9.9-x86_64-linux/bin/mold": b"#!/bin/sh\necho mold-test-banner\n"},
            "mold-test-banner",
        ),
        "zig": (
            "setup-zig",
            "zig-x86_64-linux-9.9.9.tar.xz",
            "w:xz",
            {"zig-x86_64-linux-9.9.9/zig": b"#!/bin/sh\necho 9.9.9\n"},
            "9.9.9",
        ),
    }

    def run_setup(self, runner, item, sha256):
        action, archive, mode, members, banner = self.CASES[item]
        return runner.run_action(f".github/actions/{action}", {"version": "9.9.9", "sha256": sha256})

    def fixture(self, runner, item):
        _, archive, mode, members, _ = self.CASES[item]
        body = tarball(mode, members)
        (runner.state / "downloads" / archive).write_bytes(body)
        return hashlib.sha256(body).hexdigest()

    def test_a_miss_saves_only_the_archive_and_a_later_hit_installs_offline(self):
        for item, (action, archive, _, _, banner) in self.CASES.items():
            with self.subTest(item=item):
                self.cache = harness.FakeCache()
                miss = self.runner()
                sha = self.fixture(miss, item)
                self.assertEqual(self.run_setup(miss, item, sha), {"hit": "false"})
                self.assertEqual(len(miss.calls("curl")), 1)
                self.assertEqual(len(self.cache.saves), 1)
                key, path = self.cache.saves[0]
                self.assertEqual(key, f"pinned-{item}-Linux-X64-9.9.9-{sha}")
                self.assertEqual(self.cache.names(key, path), [archive])
                self.assertIn(banner, miss.output())
                hit = self.runner()
                hit.offline()
                self.assertEqual(self.run_setup(hit, item, sha), {"hit": "true"})
                self.assertEqual(hit.calls("curl"), [])
                self.assertEqual(self.cache.restores[-1], (key, path))
                self.assertEqual(len(self.cache.saves), 1)
                self.assertIn(banner, hit.output())

    def test_a_hit_with_a_corrupted_archive_fails_the_sha256_check(self):
        for item, (action, archive, _, _, _) in self.CASES.items():
            with self.subTest(item=item):
                self.cache = harness.FakeCache()
                miss = self.runner()
                sha = self.fixture(miss, item)
                self.run_setup(miss, item, sha)
                key, path = self.cache.saves[0]
                self.cache.replace(key, path, {archive: b"not the pinned release"})
                hit = self.runner()
                hit.offline()
                self.assert_fails(lambda: self.run_setup(hit, item, sha), "sha256 verification of the restored")
                self.assertFalse((hit.temp / item).exists() and any((hit.temp / item).iterdir()))


class UvScenarioTests(Scenario):
    ARCHIVE = "uv-x86_64-unknown-linux-gnu.tar.gz"

    def prepare(self, runner, body):
        # The seed table carries the published shas; the fixture's sha
        # replaces this platform's row in the runner's copy of the action.
        action = runner.workspace / ".github" / "actions" / "setup-uv" / "action.yml"
        sha = hashlib.sha256(body).hexdigest()
        action.write_text(re.sub(r"(UV_SHA256_X86_64_LINUX: )[0-9a-f]{64}", rf"\g<1>{sha}", action.read_text()))
        (runner.state / "downloads" / self.ARCHIVE).write_bytes(body)

    def body(self):
        return tarball("w:gz", {"uv-x86_64-unknown-linux-gnu/uv": b"#!/bin/sh\necho 'uv 0.9.28 (0123abcd 2026-01-01)'\n"})

    def test_a_miss_seeds_and_saves_the_archive_and_a_later_hit_seeds_offline(self):
        body = self.body()
        miss = self.runner()
        self.prepare(miss, body)
        self.assertEqual(miss.run_action(".github/actions/setup-uv", {"enable-cache": "false"}), {"hit": "false"})
        self.assertEqual(len(miss.calls("curl")), 1)
        self.assertEqual(len(self.cache.saves), 1)
        key, path = self.cache.saves[0]
        self.assertEqual(key, "pinned-uv-Linux-X64-0.9.28-66ad1822dd9c-382c342735ff-12163fe09eb2-3a8030881d13")
        self.assertEqual(self.cache.names(key, path), [self.ARCHIVE])
        hit = self.runner()
        self.prepare(hit, body)
        hit.offline()
        self.assertEqual(hit.run_action(".github/actions/setup-uv", {"enable-cache": "false"}), {"hit": "true"})
        self.assertEqual(hit.calls("curl"), [])
        self.assertEqual(self.cache.restores[-1], (key, path))
        self.assertEqual(len(self.cache.saves), 1)
        self.assertTrue((hit.tool_cache / "uv" / "0.9.28" / "x86_64" / "uv").exists())

    def test_a_hit_with_a_corrupted_archive_fails_the_sha256_check(self):
        body = self.body()
        miss = self.runner()
        self.prepare(miss, body)
        miss.run_action(".github/actions/setup-uv", {"enable-cache": "false"})
        key, path = self.cache.saves[0]
        self.cache.replace(key, path, {self.ARCHIVE: b"not the pinned uv"})
        hit = self.runner()
        self.prepare(hit, body)
        hit.offline()
        self.assert_fails(
            lambda: hit.run_action(".github/actions/setup-uv", {"enable-cache": "false"}), "sha256 verification of"
        )
        self.assertFalse((hit.tool_cache / "uv" / "0.9.28" / "x86_64" / "uv").exists())


class NpmScenarioTests(Scenario):
    """The three package jobs, from their first step through the save."""

    RUNTIMES = {
        # job, step title, bin, version line from the release, native binary
        "pi": ("pi-package", "Pi", "pi", "{release}", False),
        "codex": ("codex-package", "Codex", "codex", "codex-cli {release}", False),
        "claude": ("claude-package", "Claude Code", "claude", "{release} (Claude Code)", True),
    }
    RELEASE = "1.2.3"

    def prepare(self, runner, runtime, node="v20.19.1"):
        job, title, bin_name, version, native = self.RUNTIMES[runtime]
        native_body = harness.npm_tree_spec(runner.state, bin_name, version.format(release=self.RELEASE), native)
        project = runner.workspace / "compat" / runtime
        (project / "npm").mkdir(parents=True, exist_ok=True)
        (project / "npm" / "package-lock.json").write_text(json.dumps({"name": runtime, "lockfileVersion": 3}))
        lock = {"upstream": {"release": self.RELEASE, "node_min": "20", "sha256": {"linux-x64": hashlib.sha256(native_body.encode()).hexdigest()}}}
        (project / "compatibility-lock.json").write_text(json.dumps(lock))
        (runner.state / "node-version").write_text(f"{node}\n")
        return job, title

    def run_job(self, runner, runtime, steps=None):
        job, title, *_ = self.RUNTIMES[runtime]
        steps = steps or harness.job_steps(ci_text(), job)
        runner.run_steps(steps, {}, until=f"Save the pinned {title} npm tree")

    def test_a_miss_installs_through_the_retry_helper_and_a_later_hit_installs_nothing(self):
        for runtime in self.RUNTIMES:
            with self.subTest(runtime=runtime):
                self.cache = harness.FakeCache()
                miss = self.runner()
                self.prepare(miss, runtime)
                (miss.state / "npm-flaky").write_text("")
                self.run_job(miss, runtime)
                npm = miss.calls("npm ci")
                # The first attempt fails, the helper runs the second.
                self.assertEqual(len(npm), 2, npm)
                self.assertIn(f"ci-retry: npm-ci-{runtime} attempt 2/", miss.output())
                scripts = "--ignore-scripts=false" if runtime == "claude" else "--ignore-scripts"
                self.assertEqual(npm[-1].split()[4], scripts, npm[-1])
                lock_hash = hashlib.sha256((miss.workspace / "compat" / runtime / "npm" / "package-lock.json").read_bytes()).hexdigest()
                key, path = self.cache.saves[0]
                self.assertEqual(
                    key,
                    f"pinned-npm-compat-{runtime}-npm-Linux-X64-{lock_hash}-node20-scripts{'true' if runtime == 'claude' else 'false'}",
                )
                self.assertEqual(path, str(miss.workspace / "compat" / runtime / "npm" / "node_modules"))
                hit = self.runner()
                self.prepare(hit, runtime)
                hit.offline()
                self.run_job(hit, runtime)
                self.assertEqual(hit.calls("npm"), [])
                self.assertEqual(self.cache.restores[-1], (key, str(hit.workspace / "compat" / runtime / "npm" / "node_modules")))
                self.assertEqual(len(self.cache.saves), 1)

    def test_claude_without_install_scripts_fails_the_release_check_and_saves_nothing(self):
        miss = self.runner()
        self.prepare(miss, "claude")
        steps = harness.job_steps(ci_text(), "claude-package")
        for step in steps:
            if (step.get("with") or {}).get("item") == "npm:compat/claude/npm":
                step["with"]["npm-scripts"] = "false"
        self.assert_fails(lambda: self.run_job(miss, "claude", steps), "is not the locked")
        self.assertEqual(self.cache.saves, [])
        self.assertEqual(self.cache.entries, {})

    def test_another_node_major_misses(self):
        miss = self.runner()
        self.prepare(miss, "codex")
        self.run_job(miss, "codex")
        other = self.runner()
        self.prepare(other, "codex", node="v22.11.0")
        self.run_job(other, "codex")
        self.assertEqual(len(other.calls("npm ci")), 1)
        self.assertEqual([key.rsplit("-", 2)[1] for key, _ in self.cache.saves], ["node20", "node22"])


class SystemdOomdScenarioTests(Scenario):
    STEP = "Install and start the pinned systemd-oomd"
    SET = ("systemd-oomd_255.4-1ubuntu8.11_amd64.deb", "libsystemd-shared_255.4-1ubuntu8.11_amd64.deb", "systemd_255.4-1ubuntu8.11_amd64.deb")
    IMAGE = {"ImageOS": "ubuntu26", "ImageVersion": "20261109.1"}

    def step(self):
        steps = [step for step in harness.job_steps(ci_text(), "systemd-supervision") if step.get("name") == self.STEP]
        self.assertEqual(len(steps), 1)
        return steps

    def runner(self, **extra_env):
        runner = super().runner(**{**self.IMAGE, **extra_env})
        (runner.state / "apt-set").write_text("\n".join(self.SET) + "\n")
        return runner

    def test_a_miss_downloads_the_set_through_the_retry_helper_and_a_later_hit_installs_it_offline(self):
        miss = self.runner()
        (miss.state / "apt-flaky").write_text("")
        miss.run_steps(self.step(), {})
        self.assertIn("ci-retry: apt-systemd-oomd attempt 2/", miss.output())
        install = [call for call in miss.calls("apt-get") if " install " in call]
        self.assertEqual(len(install), 1)
        self.assertIn("--download-only", install[0])
        self.assertEqual(sorted((miss.state / "dpkg-installed").read_text().split()), sorted(self.SET))
        self.assertIn("systemctl enable --now systemd-oomd", miss.calls("systemctl"))
        key, path = self.cache.saves[0]
        self.assertEqual(key, "pinned-systemd-oomd-Linux-X64-ubuntu26-20261109.1")
        # Only the .debs: apt's partial/ and its root-only lock never enter the entry.
        self.assertEqual(self.cache.names(key, path), sorted(self.SET))
        hit = self.runner()
        hit.offline()
        hit.run_steps(self.step(), {})
        self.assertEqual(hit.calls("apt-get"), [])
        self.assertEqual(sorted((hit.state / "dpkg-installed").read_text().split()), sorted(self.SET))
        self.assertIn("systemctl enable --now systemd-oomd", hit.calls("systemctl"))
        self.assertEqual(len(self.cache.saves), 1)

    def test_a_new_runner_image_misses(self):
        self.runner().run_steps(self.step(), {})
        newer = self.runner(ImageVersion="20261116.1")
        newer.run_steps(self.step(), {})
        self.assertEqual([key for key, _ in self.cache.saves], [
            "pinned-systemd-oomd-Linux-X64-ubuntu26-20261109.1",
            "pinned-systemd-oomd-Linux-X64-ubuntu26-20261116.1",
        ])

    def test_an_unknown_runner_image_fails_before_any_cache_access(self):
        runner = self.runner(ImageOS="", ImageVersion="")
        self.assert_fails(lambda: runner.run_steps(self.step(), {}), "needs the runner image")
        self.assertEqual(self.cache.restores, [])


class PinnedCachePhaseTests(Scenario):
    def test_a_save_without_the_restore_outputs_fails(self):
        runner = self.runner()
        for inputs in ({"phase": "save"}, {"phase": "save", "key": "pinned-x", "path": str(runner.temp)}, {"phase": "nope", "item": "nextest"}):
            with self.subTest(inputs=inputs):
                self.assert_fails(lambda: runner.run_action(".github/actions/pinned-cache", inputs), "pinned-cache")

    def test_a_save_of_a_missing_path_fails_instead_of_warning(self):
        runner = self.runner()
        missing = str(runner.temp / "pinned-cache" / "mold")
        self.assert_fails(
            lambda: runner.run_action(".github/actions/pinned-cache", {"phase": "save", "key": "pinned-mold-x", "path": missing, "hit": "false"}),
            "nothing to save",
        )
        self.assertEqual(self.cache.warnings, [])

    def test_a_save_after_a_hit_writes_nothing(self):
        runner = self.runner()
        stash = runner.temp / "pinned-cache" / "mold"
        stash.mkdir(parents=True)
        (stash / "a").write_text("a")
        runner.run_action(".github/actions/pinned-cache", {"phase": "save", "key": "pinned-mold-x", "path": str(stash), "hit": "true"})
        self.assertEqual(self.cache.saves, [])



OFFLINE_PROOF = WORKFLOWS / "pinned-cache-offline.yml"
OFFLINE_SCRIPT = ROOT / ".github" / "scripts" / "pinned-cache-offline"
PINNED_ITEMS = {"mold", "zig", "uv", "nextest", "systemd-oomd", "npm:compat/pi/npm", "npm:compat/codex/npm", "npm:compat/claude/npm"}
SETUP_ITEMS = {"./.github/actions/setup-mold": "mold", "./.github/actions/setup-zig": "zig", "./.github/actions/setup-uv": "uv"}
# Items whose key names the runner image (systemd-oomd) or the Node major
# (npm trees): the proof must run them on the image of their site job.
IMAGE_KEYED = ("systemd-oomd", "npm:")
# The runner downloads the actions nested in the composites from these
# hosts while the proof is offline, so they must stay reachable.
RUNNER_HOSTS = {"api.github.com", "codeload.github.com"}


def job_runner(text, job):
    block = re.search(rf"(?ms)^  {re.escape(job)}:\n(.*?)(?=^  [A-Za-z0-9_-]+:\n|\Z)", text).group(1)
    return re.search(r"(?m)^    runs-on: (\S+)", block).group(1)


def item_sites(text):
    """(item, job, step index, step) of every setup action and pinned-cache restore."""
    sites = []
    for job, steps in workflow_job_steps(text).items():
        for index, step in enumerate(steps):
            uses = step.get("uses")
            inputs = step.get("with") or {}
            if uses in SETUP_ITEMS:
                sites.append((SETUP_ITEMS[uses], job, index, step))
            elif uses in PINNED_USES and inputs.get("phase", "restore") == "restore":
                sites.append((inputs["item"], job, index, step))
    return sites


def offline_step_index(steps):
    found = [index for index, step in enumerate(steps) if "pinned-cache-offline apply" in (step.get("run") or "")]
    return found[0] if len(found) == 1 else None


def run_offline_script(*args):
    return subprocess.run(
        [sys.executable, "-I", str(OFFLINE_SCRIPT), *args], capture_output=True, text=True, timeout=60
    )


class OfflineProofGuardTests(unittest.TestCase):
    """The offline proof (#2202) runs every pinned item like its ci.yml site,
    after every miss-path download host is offline."""

    def setUp(self):
        self.proof = OFFLINE_PROOF.read_text()
        self.ci = ci_text()

    def test_every_pinned_item_runs_offline_with_the_inputs_and_image_of_its_site(self):
        ci_sites = {}
        for item, job, _, step in item_sites(self.ci):
            ci_sites.setdefault(item, (job, step))
        proof_sites = {}
        for item, job, _, step in item_sites(self.proof):
            self.assertNotIn(item, proof_sites, f"{item} runs twice in the proof")
            proof_sites[item] = (job, step)
        self.assertEqual(set(ci_sites), PINNED_ITEMS)
        self.assertEqual(set(proof_sites), PINNED_ITEMS)
        for item in sorted(PINNED_ITEMS):
            ci_job, ci_step = ci_sites[item]
            proof_job, proof_step = proof_sites[item]
            with self.subTest(item=item):
                self.assertEqual(proof_step["uses"], ci_step["uses"])
                ci_inputs = dict(ci_step.get("with") or {})
                proof_inputs = dict(proof_step.get("with") or {})
                if item == "uv":
                    # The uv dependency cache is not a pinned item: off, so
                    # the proof never saves one.
                    ci_inputs.pop("enable-cache")
                    self.assertEqual(proof_inputs.pop("enable-cache"), "false")
                self.assertEqual(proof_inputs, ci_inputs)
                if item.startswith(IMAGE_KEYED):
                    self.assertEqual(job_runner(self.proof, proof_job), job_runner(self.ci, ci_job))
                else:
                    self.assertRegex(job_runner(self.proof, proof_job), r"^ubuntu-\d")

    def test_every_pin_and_retry_bound_equals_ci(self):
        proof_env = harness.workflow_env(self.proof)
        ci_env = harness.workflow_env(self.ci)
        shared = [name for name in proof_env if name in ci_env]
        self.assertTrue({"NEXTEST_VERSION", "MOLD_VERSION", "MOLD_SHA256", "ZIG_VERSION", "ZIG_SHA256"} <= set(shared))
        for name in re.findall(r"\$\{\{ env\.([A-Z0-9_]+) \}\}", self.proof):
            self.assertIn(name, proof_env)
        for name in shared:
            with self.subTest(name=name):
                self.assertEqual(proof_env[name], ci_env[name])

    def test_every_job_goes_offline_before_its_first_item(self):
        jobs = workflow_job_steps(self.proof)
        self.assertTrue(jobs)
        sites = item_sites(self.proof)
        for job, steps in jobs.items():
            with self.subTest(job=job):
                offline = offline_step_index(steps)
                self.assertIsNotNone(offline, "exactly one offline step")
                first = min(index for _, site_job, index, _ in sites if site_job == job)
                self.assertLess(offline, first)
                run = steps[offline]["run"]
                # Without these the sinkhole takes only the apt and npm hosts
                # offline (or writes a file no resolver reads) and the
                # positive control still passes on the hosts it was given.
                self.assertRegex(run, r"\bsudo python3 -I \.github/scripts/pinned-cache-offline apply ")
                self.assertIn("--hosts-file /etc/hosts ", run)
                self.assertRegex(run, r"(?m)^\s*\$OFFLINE_HOSTS\s*$")
                self.assertIn("--apt-dir /etc/apt", run)
                for lock in sorted(ROOT.glob("compat/*/npm/package-lock.json")):
                    self.assertIn(f"--npm-lock {lock.relative_to(ROOT)}", steps[offline]["run"])

    def test_every_job_ends_by_requiring_the_hit_of_each_of_its_items(self):
        sites = item_sites(self.proof)
        for job, steps in workflow_job_steps(self.proof).items():
            with self.subTest(job=job):
                last = steps[-1]
                self.assertEqual(last.get("name"), "Require a hit for every restore")
                self.assertNotIn("if", last)
                self.assertIn('[ "${hit#*=}" = true ] ||', last["run"])
                hits = (last.get("env") or {}).get("HITS", "")
                entry = r"[a-z-]+=\$\{\{ steps\.([a-z-]+)\.outputs\.hit \}\}"
                named = re.findall(entry, hits)
                # Space-separated `name=<hit>` entries and nothing else.
                self.assertEqual(re.sub(entry, "", hits).strip(" "), "", hits)
                ids = [step.get("id") for item, site_job, _, step in sites if site_job == job]
                self.assertNotIn(None, ids, "every item step carries an id")
                self.assertEqual(sorted(named), sorted(ids))

    def test_the_setup_actions_report_the_hit_of_their_pinned_restore(self):
        for uses in SETUP_ITEMS:
            text = (ROOT / uses[2:] / "action.yml").read_text()
            definition = harness.parse_yaml(text, uses)
            restores = [
                step for step in definition["runs"]["steps"]
                if step.get("uses") in PINNED_USES and "phase" not in (step.get("with") or {})
            ]
            with self.subTest(action=uses):
                self.assertEqual(len(restores), 1)
                self.assertEqual(
                    definition["outputs"]["hit"]["value"], f"${{{{ steps.{restores[0]['id']}.outputs.hit }}}}"
                )

    def test_each_npm_tree_gets_the_node_of_its_site(self):
        # The Node major is part of an npm key: the proof installs the same
        # Node from the same lock field as the site job, or none at all.
        ci_sites = {item: job for item, job, _, _ in item_sites(self.ci)}
        for item, proof_job, _, _ in item_sites(self.proof):
            if not item.startswith("npm:"):
                continue
            ci_job = ci_sites[item]
            with self.subTest(item=item):
                setups = []
                for text, job in ((self.ci, ci_job), (self.proof, proof_job)):
                    steps = harness.job_steps(text, job)
                    node = [step.get("with") for step in steps if str(step.get("uses", "")).startswith("actions/setup-node@")]
                    sources = []
                    for value in node:
                        for source in re.findall(r"\$\{\{ steps\.([a-z-]+)\.outputs\.node \}\}", str(value)):
                            producer = next(step for step in steps if step.get("id") == source)
                            run = producer["run"]
                            sources.append((
                                source,
                                re.search(r'(?m)^\s*lock=(\S+)$', run).group(1),
                                re.search(r'(?m)^\s*echo "node=\$\((.*)\)"', run).group(1),
                            ))
                    setups.append((node, sources))
                self.assertEqual(setups[1], setups[0])

    def test_every_miss_path_download_host_is_offline(self):
        offline = set(harness.workflow_env(self.proof)["OFFLINE_HOSTS"].split())
        hosts = set()
        for name in ("pinned-cache", "setup-mold", "setup-zig", "setup-uv"):
            text = (ACTIONS / name / "action.yml").read_text()
            code = "\n".join(line for line in text.splitlines() if not line.lstrip().startswith("#"))
            hosts |= set(re.findall(r"https?://([A-Za-z0-9.-]+)", code))
        self.assertTrue({"github.com", "ziglang.org"} <= hosts, hosts)
        self.assertEqual(hosts - offline, set())
        self.assertEqual(offline & RUNNER_HOSTS, set())

    def test_only_a_change_to_the_proof_itself_triggers_it_on_a_pull_request(self):
        trigger = re.search(r"(?ms)^on:\n(.*?)(?=^\S)", self.proof).group(1)
        self.assertRegex(trigger, r"(?m)^  workflow_dispatch:$")
        paths = re.search(r"(?ms)^  pull_request:\n    paths:\n((?:      - \S+\n)+)", trigger)
        self.assertIsNotNone(paths, trigger)
        self.assertEqual(
            re.findall(r"- (\S+)", paths.group(1)),
            [".github/workflows/pinned-cache-offline.yml", ".github/scripts/pinned-cache-offline"],
        )


class OfflineHostsScenarioTests(unittest.TestCase):
    """`pinned-cache-offline entries`: the host set the proof takes offline."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.apt = self.base / "apt"
        (self.apt / "sources.list.d").mkdir(parents=True)

    def test_the_hosts_of_every_apt_source_and_npm_lock_go_to_loopback(self):
        mirrors = self.base / "apt-mirrors.txt"
        mirrors.write_text(
            "http://azure.archive.ubuntu.com/ubuntu/\tpriority:1\n"
            "# a comment\n"
            "https://archive.ubuntu.com/ubuntu/\tpriority:2\n"
        )
        (self.apt / "sources.list").write_text(
            "# deb http://commented.example.org/ubuntu noble main\n"
            "deb [arch=amd64 signed-by=/usr/share/keyrings/x.gpg] https://packages.microsoft.com/ubuntu/24.04/prod noble main\n"
        )
        (self.apt / "sources.list.d" / "local.list").write_text("deb file:/srv/repo ./\n")
        (self.apt / "sources.list.d" / "ubuntu.sources").write_text(
            "Types: deb\n"
            f"URIs: mirror+file:{mirrors}\n"
            "Suites: noble noble-updates\n"
            "\n"
            "Types: deb\n"
            "URIs: http://security.ubuntu.com/ubuntu/\n"
            " http://ports.ubuntu.com/ubuntu-ports/\n"
            "Suites: noble-security\n"
        )
        lock = self.base / "package-lock.json"
        lock.write_text(json.dumps({"packages": {"": {}, "node_modules/a": {"resolved": "https://registry.npmjs.org/a/-/a-1.tgz"}}}))
        result = run_offline_script("entries", "--apt-dir", str(self.apt), "--npm-lock", str(lock), "GitHub.com")
        self.assertEqual(result.returncode, 0, result.stderr)
        hosts = [
            "archive.ubuntu.com", "azure.archive.ubuntu.com", "github.com", "packages.microsoft.com",
            "ports.ubuntu.com", "registry.npmjs.org", "security.ubuntu.com",
        ]
        self.assertEqual(result.stdout.splitlines(), [f"{address} {host}" for host in hosts for address in ("127.0.0.1", "::1")])

    def test_a_source_without_a_known_host_fails_instead_of_staying_online(self):
        (self.apt / "sources.list.d" / "remote.sources").write_text("URIs: mirror+https://mirrors.example.org/list.txt\n")
        result = run_offline_script("entries", "--apt-dir", str(self.apt), "github.com")
        self.assertEqual(result.returncode, 1)
        self.assertIn("cannot map the apt source 'mirror+https://mirrors.example.org/list.txt'", result.stderr)
        self.assertEqual(result.stdout, "")

    def test_a_lock_without_a_registry_url_and_a_bad_host_name_fail(self):
        lock = self.base / "package-lock.json"
        lock.write_text(json.dumps({"packages": {"": {}}}))
        result = run_offline_script("entries", "--npm-lock", str(lock), "github.com")
        self.assertEqual(result.returncode, 1)
        self.assertIn("no resolved registry URL", result.stderr)
        result = run_offline_script("entries", "github.com evil.example")
        self.assertEqual(result.returncode, 1)
        self.assertIn("not a host name", result.stderr)

    def test_the_repository_locks_resolve_from_the_npm_registry_only(self):
        args = []
        for lock in sorted(ROOT.glob("compat/*/npm/package-lock.json")):
            args += ["--npm-lock", str(lock)]
        self.assertEqual(len(args), 6)
        result = run_offline_script("entries", *args)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ["127.0.0.1 registry.npmjs.org", "::1 registry.npmjs.org"])


if __name__ == "__main__":
    unittest.main()
