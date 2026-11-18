"""Structural and shell checks on the plugin kit publish workflow.

`plugin-kit-publish.yml` publishes the attested kit bundles (`build-kit-*`
artifacts of the Release and nightly CI runs) as releases of the kit
repository `pohunekd/pohunek-plugin-kit` (issue #1613). The checks pin its
security model:

- the trigger is `workflow_run` on `CI` and `Release`, `completed` only;
- one job only, holding exactly `contents: read` and `actions: read`;
- the job skips while the owner-side `PLUGIN_KIT_APP_ID` variable is empty
  (D3: the kit repository, its App and its secrets do not exist yet);
- the job runs for a successful Release push run or a successful scheduled
  CI run only, never for a pull request or a push run;
- the job checks out nothing and restores no cache;
- Linux runs on `ubuntu-24.04`;
- every third-party action is pinned to a full commit sha;
- a tag Release run publishes `core-vX.Y.Z`, a scheduled CI run the
  prerelease `nightly-YYYYMMDD-<sha12>`;
- the two kit bundles must agree on one X.Y.Z version, which a tag release
  must match; the release is created as a draft, uploaded, then undrafted;
- nightlies older than 30 days are pruned.

The `run:` bodies of the tag selection and the bundle check also run
against fixtures, so a shell regression fails here instead of at publish
time.
"""

import os
import re
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "plugin-kit-publish.yml"

USES = re.compile(r"^\s+(?:- )?uses:\s*(\S+)", re.M)
PINNED = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@[0-9a-f]{40}$")


def canonical_action(use: str) -> str:
    ref = use.split("@", 1)[1].split()[0] if "@" in use else ""
    repo = use.split("@", 1)[0]
    return f"{repo}@{ref}" if ref else repo


class PluginKitPublishWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text()
        cls.jobs = cls.text.partition("\njobs:\n")[2]

    def test_the_workflow_exists(self):
        self.assertTrue(WORKFLOW.is_file(), "plugin-kit-publish.yml exists")

    def test_the_trigger_is_workflow_run_completed_on_ci_and_release(self):
        header = self.text.partition("\njobs:\n")[0]
        self.assertIn("workflow_run:", header)
        self.assertIn("workflows: [CI, Release]", header)
        self.assertIn("types: [completed]", header)
        for trigger in ("push:", "pull_request", "schedule:", "workflow_dispatch:", "workflow_call:"):
            self.assertNotIn(trigger, header)

    def test_exactly_one_job_holds_a_read_only_token(self):
        permissions = re.findall(r"(?m)^    permissions:\n(?:      \S+: \S+\n)+", self.jobs)
        self.assertEqual(len(permissions), 1, "exactly one job declares permissions")
        scopes = dict(re.findall(r"(?m)^      (\S+): (\S+)$", permissions[0]))
        self.assertEqual(scopes, {"contents": "read", "actions": "read"})

    def test_the_job_is_gated_on_the_owner_side_app(self):
        condition = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", self.jobs)
        self.assertIsNotNone(condition, "the publish job carries an if gate")
        assert condition is not None
        expression = condition.group(1)
        self.assertIn("vars.PLUGIN_KIT_APP_ID != ''", expression)
        self.assertIn("github.event.workflow_run.conclusion == 'success'", expression)
        self.assertIn("github.event.workflow_run.name == 'Release'", expression)
        self.assertIn("github.event.workflow_run.event == 'push'", expression)
        self.assertIn("github.event.workflow_run.name == 'CI'", expression)
        self.assertIn("github.event.workflow_run.event == 'schedule'", expression)

    def test_no_checkout_and_no_cache(self):
        self.assertNotIn("actions/checkout", self.text)
        self.assertNotIn("rust-cache", self.text)
        self.assertNotIn("actions/cache", self.text)

    def test_no_secret_leaves_the_job_as_an_input(self):
        for use in USES.findall(self.text):
            if use.startswith("./"):
                continue
            with self.subTest(action=use):
                self.assertRegex(canonical_action(use), PINNED)
        self.assertNotIn("secrets.GITHUB_TOKEN", self.text)

    def test_linux_runs_on_the_pinned_runner(self):
        self.assertEqual(
            set(re.findall(r"(?m)^\s+(?:runs-on|os): (ubuntu-\S+)", self.text)),
            {"ubuntu-24.04"},
        )

    def test_the_download_reads_the_finished_run(self):
        download = re.search(
            r"(?m)^      - name: Download the kit bundles\n(?:.*\n)*?          github-token: (.+)$",
            self.text,
        )
        self.assertIsNotNone(download, "the kit download names its token")
        assert download is not None
        self.assertEqual(download.group(1).strip(), "${{ github.token }}")
        self.assertIn("run-id: ${{ env.RUN_ID }}", self.text)
        self.assertIn("pattern: build-kit-*", self.text)

    def test_the_publish_is_draft_first(self):
        publish = self.text.partition("- name: Publish the release")[2]
        create = publish.index("gh release create")
        upload = publish.index("gh release upload")
        edit = publish.index("gh release edit")
        self.assertLess(create, upload)
        self.assertLess(upload, edit)
        self.assertIn("--draft", publish)
        self.assertIn("--draft=false", publish)
        self.assertIn("--verify-tag", publish)
        self.assertIn("dist/SHA256SUMS", publish)
        self.assertIn("--prerelease", publish)

    def test_the_prune_keeps_a_bounded_nightly_history(self):
        prune = self.text.partition("- name: Prune nightlies older than 30 days")[2]
        self.assertIn("30 days ago", prune)
        self.assertIn('startswith("nightly-")', prune)
        self.assertIn("gh release delete", prune)
        self.assertIn("--cleanup-tag", prune)
        condition = re.search(r"(?m)^      - name: Prune nightlies older than 30 days\n        if: (.+)$", self.text)
        self.assertIsNotNone(condition, "the prune step runs for nightlies only")
        assert condition is not None
        self.assertIn("prerelease", condition.group(1))

    def test_concurrent_publishes_are_serialized(self):
        self.assertIn("concurrency:", self.text.partition("\njobs:\n")[0])
        self.assertIn("group: plugin-kit-publish", self.text)


class PluginKitPublishShellTests(unittest.TestCase):
    """The tag-selection and bundle-check `run:` bodies against fixtures."""

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

    def run_select(self, env):
        root = Path(tempfile.mkdtemp(prefix="plugin-kit-select-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        out = root / "GITHUB_OUTPUT"
        out.write_text("")
        env = {"PATH": "/usr/bin:/bin", "HOME": str(root), "GITHUB_OUTPUT": str(out), **env}
        completed = subprocess.run(
            ["bash", "-euo", "pipefail", "-c", self.step("Select the release tag")],
            cwd=root, env=env, capture_output=True, text=True, timeout=60,
        )
        return completed, out

    def run_body(self, name, env, files=(), checksums=True):
        root = Path(tempfile.mkdtemp(prefix="plugin-kit-publish-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        for relative in files:
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"fixture")
        if checksums:
            dist = root / "dist"
            lines = []
            for bundle in sorted(dist.glob("pohunek-plugin-kit-*.tar.zst")):
                digest = subprocess.run(
                    ["sha256sum", str(bundle)], capture_output=True, text=True, check=True
                ).stdout.split()[0]
                lines.append(f"{digest}  {bundle.name}")
            (dist / "kit.tar.zst.sha256").write_text("\n".join(lines) + "\n")
        env = {"PATH": "/usr/bin:/bin", "HOME": str(root), **env}
        completed = subprocess.run(
            ["bash", "-euo", "pipefail", "-c", self.step(name)],
            cwd=root, env=env, capture_output=True, text=True, timeout=60,
        )
        return completed, root

    def outputs(self, out):
        values = {}
        for line in out.read_text().splitlines():
            key, _, value = line.partition("=")
            values[key] = value
        return values

    def test_the_select_maps_a_release_tag(self):
        completed, out = self.run_select(
            {
                "WORKFLOW_NAME": "Release",
                "HEAD_BRANCH": "v0.44.0",
                "HEAD_SHA": "394a7a440abc1234567890abcdef1234567890ab",
            },
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(
            self.outputs(out), {"tag": "core-v0.44.0", "prerelease": "false"}
        )

    def test_the_select_refuses_a_non_tag_release_run(self):
        for branch in ("main", "v0.44", "0.44.0"):
            with self.subTest(branch=branch):
                completed, _ = self.run_select(
                    {
                        "WORKFLOW_NAME": "Release",
                        "HEAD_BRANCH": branch,
                        "HEAD_SHA": "394a7a440abc1234567890abcdef1234567890ab",
                    },
                )
                self.assertNotEqual(completed.returncode, 0)
                self.assertIn("must come from a vX.Y.Z tag", completed.stdout)

    def test_the_select_maps_a_nightly_run(self):
        completed, out = self.run_select(
            {
                "WORKFLOW_NAME": "CI",
                "HEAD_BRANCH": "main",
                "HEAD_SHA": "394a7a440abc1234567890abcdef1234567890ab",
            },
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        outputs = self.outputs(out)
        self.assertEqual(outputs["prerelease"], "true")
        self.assertRegex(outputs["tag"], r"^nightly-20[0-9]{6}-394a7a440abc$")

    def test_the_select_refuses_a_short_sha(self):
        completed, _ = self.run_select(
            {
                "WORKFLOW_NAME": "CI",
                "HEAD_BRANCH": "main",
                "HEAD_SHA": "xyz",
            },
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("must carry a commit sha", completed.stdout)

    def test_the_select_refuses_an_unexpected_workflow(self):
        completed, _ = self.run_select(
            {
                "WORKFLOW_NAME": "Docs",
                "HEAD_BRANCH": "main",
                "HEAD_SHA": "394a7a440abc1234567890abcdef1234567890ab",
            },
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("unexpected workflow run", completed.stdout)

    def kit_files(self, *versions):
        files = []
        for version, target in versions:
            name = f"pohunek-plugin-kit-{version}-{target}.tar.zst"
            files.append(f"dist/{name}")
        return files

    def test_the_check_accepts_matching_bundles_for_a_release(self):
        files = self.kit_files(
            ("0.44.0", "x86_64-unknown-linux-gnu"),
            ("0.44.0", "aarch64-apple-darwin"),
        )
        completed, root = self.run_body(
            "Check the bundle checksums and version",
            {"TAG": "core-v0.44.0"},
            files=files,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertTrue((root / "dist" / "SHA256SUMS").is_file())

    def test_the_check_accepts_matching_bundles_for_a_nightly(self):
        files = self.kit_files(
            ("0.44.0", "x86_64-unknown-linux-gnu"),
            ("0.44.0", "aarch64-apple-darwin"),
        )
        completed, _ = self.run_body(
            "Check the bundle checksums and version",
            {"TAG": "nightly-20261118-394a7a440abc"},
            files=files,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_the_check_rejects_a_version_mismatch(self):
        files = self.kit_files(
            ("0.44.0", "x86_64-unknown-linux-gnu"),
            ("0.44.0", "aarch64-apple-darwin"),
        )
        completed, _ = self.run_body(
            "Check the bundle checksums and version",
            {"TAG": "core-v0.43.0"},
            files=files,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("does not match the bundle version", completed.stdout + completed.stderr)

    def test_the_check_rejects_disagreeing_bundles(self):
        files = self.kit_files(
            ("0.44.0", "x86_64-unknown-linux-gnu"),
            ("0.43.0", "aarch64-apple-darwin"),
        )
        completed, _ = self.run_body(
            "Check the bundle checksums and version",
            {"TAG": "nightly-20261118-394a7a440abc"},
            files=files,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("no single X.Y.Z version", completed.stdout + completed.stderr)

    def test_the_check_rejects_a_single_bundle(self):
        files = self.kit_files(("0.44.0", "x86_64-unknown-linux-gnu"))
        completed, _ = self.run_body(
            "Check the bundle checksums and version",
            {"TAG": "core-v0.44.0"},
            files=files,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("expected the gnu and darwin kit bundles", completed.stdout + completed.stderr)

    def test_the_check_rejects_a_bad_checksum(self):
        files = self.kit_files(
            ("0.44.0", "x86_64-unknown-linux-gnu"),
            ("0.44.0", "aarch64-apple-darwin"),
        )
        root = Path(tempfile.mkdtemp(prefix="plugin-kit-publish-"))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        for relative in files:
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"fixture")
        (root / "dist" / "kit.tar.zst.sha256").write_text(
            "0" * 64 + "  pohunek-plugin-kit-0.44.0-x86_64-unknown-linux-gnu.tar.zst\n"
        )
        completed = subprocess.run(
            ["bash", "-euo", "pipefail", "-c", self.step("Check the bundle checksums and version")],
            cwd=root,
            env={"PATH": "/usr/bin:/bin", "HOME": str(root), "TAG": "core-v0.44.0"},
            capture_output=True, text=True, timeout=60,
        )
        self.assertNotEqual(completed.returncode, 0)


if __name__ == "__main__":
    unittest.main()
