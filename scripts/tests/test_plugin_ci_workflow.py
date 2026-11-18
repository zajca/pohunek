"""Structural checks on the reusable plugin CI workflow (`plugin-ci.yml`).

The workflow runs the plugin kit (lint, double build with byte compare,
verify, detect-check, smoke, version-bump check) per row and platform in a
single job with a read-only token. The checks pin its security model:

- one job only, holding exactly `contents: read` and no other scope;
- no secret is minted or forwarded (the read-only `github.token` reaches
  only the `gh` CLI through the environment, never a secret input);
- no step restores a cache;
- Linux runs on `ubuntu-24.04`, never `ubuntu-latest`;
- every third-party action is pinned to a full commit sha;
- the trigger is `workflow_call` with the documented inputs and defaults;
- the matrix runs every row of `rows` against every target of `platforms`;
- the `release` row verifies the attested daemon archive of `core-tag`;
- the `previous-minor` row resolves the newest release of the previous
  minor with a version compare, never a lexical one;
- detect-check runs only when `corpus-dir` is set;
- the optional `smoke-script` runs inside the generic smoke, on the same
  live isolated root, with its socket path and its session id exported.
"""

import re
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
        self.assertIn("runs-on: ubuntu-24.04", self.text)
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

    def test_the_smoke_script_runs_inside_the_smoke_with_its_socket(self):
        smoke = self.text.partition("Smoke the package and the smoke script")[2]
        self.assertIn("--script", smoke)
        self.assertIn('SMOKE_SCRIPT', smoke)
        self.assertNotIn("POHUNEK_SOCKET", smoke)


if __name__ == "__main__":
    unittest.main()
