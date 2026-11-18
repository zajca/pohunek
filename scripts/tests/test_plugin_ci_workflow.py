"""Structural checks on the reusable plugin CI workflow (`plugin-ci.yml`).

The workflow runs the plugin kit (lint, double build with byte compare,
verify, detect-check, smoke, version-bump check) per row and platform in a
single job with a read-only token. The checks pin its security model:

- one job only, holding exactly `contents: read` and no other scope;
- no secret is minted or forwarded (the read-only `github.token` reaches
  only the `gh` steps through the environment, never a secret input);
- no step restores a cache;
- Linux runs on `ubuntu-24.04`, macOS on the pinned `macos-15` label;
- every third-party action is pinned to a full commit sha;
- the trigger is `workflow_call` with the documented inputs and defaults;
- the matrix runs every row of `rows` against every target of `platforms`;
- the `release` row verifies the attested daemon archive of `core-tag`;
- the `previous-minor` row resolves the newest release of the previous
  minor with a version compare, never a lexical one;
- detect-check runs only when `corpus-dir` is set;
- the optional `smoke-script` runs inside the generic smoke, on the same
  live isolated root, with the isolated daemon on `PATH` and the live
  session id exported.

The `run:` bodies of the path check and the version-bump check also run
against fixtures (with a fake `gh` on `PATH`), so a shell regression
fails here instead of in a caller repository.
"""

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "plugin-ci.yml"

USES = re.compile(r"^\s+(?:- )?uses:\s*(\S+)", re.M)
PINNED = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}$")
LOCAL_ACTION = re.compile(r"^\./\.github/actions/[a-z-]+$")
# A local composite action of the checked-out core, the same pattern
# `plugin-release.yml` uses for the pinned toolchain install.
CORE_ACTION = re.compile(r"^\./core/\.github/actions/[a-z-]+$")


def canonical_action(use: str) -> str:
    ref = use.split("@", 1)[1].split()[0] if "@" in use else ""
    repo = use.split("@", 1)[0]
    return f"{repo}@{ref}" if ref else repo


class PluginCiWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text()

    def test_the_workflow_exists(self):
        self.assertTrue(WORKFLOW.is_file(), "plugin-ci.yml exists")

    def test_the_trigger_is_workflow_call_only(self):
        header = self.text.partition("\njobs:\n")[0]
        self.assertIn("on:\n  workflow_call:\n", header)
        for trigger in ("push:", "pull_request", "schedule:", "workflow_dispatch:"):
            self.assertNotIn(trigger, header)

    def test_the_inputs_match_the_documented_contract(self):
        header = self.text.partition("\njobs:\n")[0]
        for name in (
            "core-tag:",
            "rows:",
            "platforms:",
            "package-dir:",
            "corpus-dir:",
            "upstream-install:",
            "smoke-script:",
        ):
            self.assertIn(name, header)
        self.assertIn("required: true", header)
        self.assertIn("default: '[\"release\", \"nightly\"]'", header)
        self.assertIn("default: '[\"x86_64-unknown-linux-gnu\"]'", header)
        self.assertIn("default: package", header)

    def test_every_action_is_pinned_to_a_commit_or_local(self):
        for action in USES.findall(self.text):
            with self.subTest(action=action):
                if LOCAL_ACTION.match(action):
                    nested = (ROOT / action[2:] / "action.yml").read_text()
                    for inner in USES.findall(nested):
                        if LOCAL_ACTION.match(inner):
                            continue
                        self.assertRegex(canonical_action(inner), PINNED)
                elif CORE_ACTION.match(action):
                    # The core checkout this row pinned: its composite runs
                    # from the same commit as the workflow.
                    continue
                else:
                    self.assertRegex(canonical_action(action), PINNED)

    def test_exactly_one_job_holds_a_read_only_token(self):
        body = self.text.partition("\njobs:\n")[2]
        names = re.findall(r"(?m)^  ([a-z][a-z0-9-]*):$", body)
        self.assertEqual(names, ["check"])
        self.assertIn("permissions:\n      contents: read\n", body)
        for scope in ("id-token:", "attestations:", "contents: write", "secrets."):
            self.assertNotIn(scope, body)

    def test_no_cache_is_restored(self):
        for action in USES.findall(self.text):
            self.assertNotIn("actions/cache", action)

    def test_linux_runs_on_the_pinned_runner(self):
        self.assertIn("ubuntu-24.04", self.text)
        self.assertIsNone(re.search(r"(?m)^[^#\n]*ubuntu-latest", self.text))

    def test_the_matrix_runs_every_row_against_every_platform(self):
        self.assertIn("matrix:\n        row: ${{ fromJson(inputs.rows) }}", self.text)
        self.assertIn("platform: ${{ fromJson(inputs.platforms) }}", self.text)
        for row in ("release", "previous-minor", "nightly"):
            self.assertIn(row, self.text)

    def test_the_release_row_verifies_the_attested_daemon_archive(self):
        release = self.text.partition("Fetch the attested daemon archive")[2]
        self.assertIn("gh attestation verify", release)
        self.assertIn("--signer-workflow", release)
        self.assertIn("--deny-self-hosted-runners", release)

    def test_the_previous_minor_row_compares_versions_not_strings(self):
        resolve = self.text.partition("Resolve the core ref of the row")[2]
        self.assertIn("sort -V", resolve)
        self.assertNotIn("sort -u", resolve.replace("sort -Vu", ""))

    def test_detect_check_runs_only_with_a_corpus(self):
        self.assertIn("if: inputs.corpus-dir != ''", self.text)

    def test_the_smoke_script_runs_inside_the_smoke_with_its_session(self):
        smoke = self.text.partition("Smoke the package and the smoke script")[2]
        self.assertIn("--script", smoke)
        self.assertIn('SMOKE_SCRIPT', smoke)
        self.assertIn("working-directory: plugin", smoke)
        header = self.text.partition("\njobs:\n")[0]
        self.assertIn('POHUNEK_SMOKE_SESSION', header)
        self.assertNotIn("POHUNEK_SOCKET", smoke)

    def test_the_platform_maps_to_a_runner(self):
        self.assertIn("matrix.platform == 'aarch64-apple-darwin'", self.text)
        self.assertIn("'macos-15'", self.text)

    def test_the_inputs_are_validated_before_use(self):
        validate = self.text.partition("Validate the inputs")[2]
        self.assertIn("core-tag must be vX.Y.Z", validate)
        self.assertIn("platforms holds only", validate)
        self.assertIn("unknown row", validate)

    def test_the_release_archive_keeps_the_release_layout(self):
        self.assertIn("--strip-components=1", self.text)

    def test_the_token_reaches_only_the_gh_steps(self):
        body = self.text.partition("\njobs:\n")[2]
        job_env = body.partition("steps:")[0]
        self.assertNotIn("GH_TOKEN", job_env)
        self.assertEqual(body.count("GH_TOKEN: ${{ github.token }}"), 3)

    def test_the_upstream_install_contract_is_documented(self):
        header = self.text.partition("\njobs:\n")[0]
        self.assertIn("UPSTREAM_BIN", header)
        smoke = self.text.partition("Smoke the package and the smoke script")[2]
        self.assertIn('export UPSTREAM_BIN=', smoke)
        self.assertIn('left $UPSTREAM_BIN empty', smoke)

    def test_the_version_bump_check_tells_absent_from_error(self):
        check = self.text.partition("Check the version bump")[2]
        self.assertIn("gh release view", check)
        self.assertIn("needs no baseline", check)


class PluginCiShellTests(unittest.TestCase):
    """The path-check and version-bump `run:` bodies against fixtures."""

    @classmethod
    def step(cls, name):
        lines = WORKFLOW.read_text().splitlines()
        start = next(i for i, line in enumerate(lines) if line.strip() == f"- name: {name}")
        run_at = next(i for i in range(start, len(lines)) if lines[i].strip() == "run: |")
        indent = len(lines[run_at]) - len(lines[run_at].lstrip()) + 2
        body = []
        for line in lines[run_at + 1:]:
            if line.strip() == "" or len(line) - len(line.lstrip()) >= indent:
                body.append(line[indent:] if len(line) >= indent else "")
            elif line.strip().startswith("-"):
                break
            else:
                body.append(line)
        return "\n".join(body)

    def run_body(self, name, env, files=(), gh=None):
        root = Path(tempfile.mkdtemp(prefix="plugin-ci-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        for relative in files:
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("fixture")
        if gh is not None:
            bindir = root / "bin"
            bindir.mkdir(exist_ok=True)
            fake = bindir / "gh"
            fake.write_text(gh)
            fake.chmod(0o755)
            env = dict(env, PATH=str(bindir) + os.pathsep + os.environ["PATH"])
        env = {"PATH": "/usr/bin:/bin", "HOME": str(root), **env}
        try:
            completed = subprocess.run(
                ["bash", "-euo", "pipefail", "-c", self.step(name)],
                cwd=root, env=env, capture_output=True, text=True, timeout=60,
            )
        except TypeError:
            completed = subprocess.run(
                ["bash", "-c", self.step(name)],
                cwd=root, env=env, capture_output=True, text=True, timeout=60,
            )
        return completed

    def test_the_path_check_accepts_the_defaults(self):
        completed = self.run_body(
            "Set up the package inputs",
            {"PACKAGE_DIR": "package", "CORPUS_DIR": "", "UPSTREAM_INSTALL": "", "SMOKE_SCRIPT": ""},
            files=["plugin/package/x"],
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_the_path_check_rejects_an_empty_package_dir(self):
        completed = self.run_body(
            "Set up the package inputs",
            {"PACKAGE_DIR": "", "CORPUS_DIR": "", "UPSTREAM_INSTALL": "", "SMOKE_SCRIPT": ""},
            files=["plugin/package/x"],
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("PACKAGE_DIR must be a path below the repository root", completed.stderr)

    def test_the_path_check_runs_the_set_inputs(self):
        completed = self.run_body(
            "Set up the package inputs",
            {"PACKAGE_DIR": "package", "CORPUS_DIR": "corpus", "UPSTREAM_INSTALL": "install.sh", "SMOKE_SCRIPT": "smoke.sh"},
            files=["plugin/package/x", "plugin/corpus/cases.toml", "plugin/install.sh", "plugin/smoke.sh"],
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_the_version_bump_passes_without_a_previous_release(self):
        gh = "#!/bin/sh\nif [ \"$1 $2\" = \"release list\" ]; then exit 0; fi\nexit 1\n"
        completed = self.run_body(
            "Check the version bump",
            {"PACKAGE_DIR": "package", "GITHUB_REPOSITORY": "owner/repo"},
            files=["dist/a.tar.zst", "plugin/package/runtime.toml"],
            gh=gh,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("the first release sets the version", completed.stdout)

    def test_the_version_bump_passes_without_a_baseline_asset(self):
        gh = (
            "#!/bin/sh\n"
            "if [ \"$1 $2\" = \"release list\" ]; then echo v0.1.0; exit 0; fi\n"
            "if [ \"$1 $2\" = \"release view\" ]; then echo other.txt; exit 0; fi\n"
            "exit 1\n"
        )
        completed = self.run_body(
            "Check the version bump",
            {"PACKAGE_DIR": "package", "GITHUB_REPOSITORY": "owner/repo"},
            files=["dist/a.tar.zst", "plugin/package/runtime.toml"],
            gh=gh,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("needs no baseline", completed.stdout)

    def test_the_version_bump_fails_on_a_failed_download(self):
        gh = (
            "#!/bin/sh\n"
            "if [ \"$1 $2\" = \"release list\" ]; then echo v0.1.0; exit 0; fi\n"
            "if [ \"$1 $2\" = \"release view\" ]; then echo a.tar.zst; exit 0; fi\n"
            "echo network down >&2; exit 1\n"
        )
        completed = self.run_body(
            "Check the version bump",
            {"PACKAGE_DIR": "package", "GITHUB_REPOSITORY": "owner/repo"},
            files=["dist/a.tar.zst", "plugin/package/runtime.toml"],
            gh=gh,
        )
        self.assertNotEqual(completed.returncode, 0)

    def test_the_version_bump_fails_on_a_changed_archive_without_a_bump(self):
        (Path(tempfile.gettempdir()) / "x").exists()
        root_files = ["dist/a.tar.zst", "previous/a.tar.zst", "plugin/package/runtime.toml"]
        gh = (
            "#!/bin/sh\n"
            "if [ \"$1 $2\" = \"release list\" ]; then echo v0.1.0; exit 0; fi\n"
            "if [ \"$1 $2\" = \"release view\" ]; then echo a.tar.zst; exit 0; fi\n"
            "exit 0\n"
        )
        root = Path(tempfile.mkdtemp(prefix="plugin-ci-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        (root / "dist").mkdir()
        (root / "previous").mkdir()
        (root / "plugin" / "package").mkdir(parents=True)
        (root / "dist" / "a.tar.zst").write_bytes(b"new")
        (root / "previous" / "a.tar.zst").write_bytes(b"old")
        (root / "plugin" / "package" / "runtime.toml").write_text('version = "0.1.0"\n')
        bindir = root / "bin"
        bindir.mkdir()
        fake = bindir / "gh"
        fake.write_text(gh)
        fake.chmod(0o755)
        env = {"PATH": str(bindir) + os.pathsep + "/usr/bin:/bin", "HOME": str(root),
               "PACKAGE_DIR": "package", "GITHUB_REPOSITORY": "owner/repo"}
        completed = subprocess.run(
            ["bash", "-c", self.step("Check the version bump")],
            cwd=root, env=env, capture_output=True, text=True, timeout=60,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("but version is still 0.1.0", completed.stderr)


if __name__ == "__main__":
    unittest.main()
