"""Static checks of the workflow that tells the Homebrew tap about a release.

release.yml holds no secret, so the dispatch token lives in this separate
workflow. These tests pin what keeps that safe: it runs only after a successful
tag release, checks out nothing, reads the tag through `env`, matches it before
use, skips pre-releases, and uses the token in exactly one pinned action.
"""

from pathlib import Path
import re
import unittest

WORKFLOWS = Path(__file__).resolve().parents[2] / ".github" / "workflows"
NOTIFY = (WORKFLOWS / "notify-tap.yml").read_text()
RELEASE = (WORKFLOWS / "release.yml").read_text()

PINNED = re.compile(r"@[0-9a-f]{40}(?: # v[0-9.]+)?$")
TOKEN = "secrets.TAP_DISPATCH_PAT"


class NotifyTapWorkflowTest(unittest.TestCase):
    def test_it_runs_after_the_release_workflow_completes(self):
        self.assertIn("workflow_run:\n    workflows: [Release]\n    types: [completed]", NOTIFY)
        self.assertRegex(RELEASE, r"(?m)^name: Release$")
        header = NOTIFY.split("jobs:")[0]
        for trigger in ("pull_request", "pull_request_target", "push:", "workflow_dispatch"):
            self.assertNotIn(trigger, header)

    def test_only_a_successful_tag_push_run_notifies(self):
        self.assertIn("github.event.workflow_run.conclusion == 'success'", NOTIFY)
        self.assertIn("github.event.workflow_run.event == 'push'", NOTIFY)

    def test_permissions_are_read_only(self):
        self.assertIn("\npermissions: {}\n", NOTIFY)
        self.assertIn("    permissions:\n      contents: read\n", NOTIFY)
        self.assertNotIn("write", NOTIFY.replace("workflow_run", ""))

    def test_nothing_is_checked_out(self):
        self.assertNotIn("actions/checkout", NOTIFY)
        self.assertNotIn("persist-credentials", NOTIFY)

    def test_every_action_is_pinned_to_a_commit(self):
        uses = re.findall(r"(?m)^\s+uses: (\S.*)$", NOTIFY)
        self.assertEqual(len(uses), 1)
        for value in uses:
            self.assertRegex(value, PINNED)

    def test_the_token_is_used_once_in_the_dispatch_step_only(self):
        self.assertEqual(NOTIFY.count(TOKEN), 1)
        step = NOTIFY.split("- name: Dispatch the bump to the tap")[1]
        self.assertIn("token: ${{ " + TOKEN + " }}", step)
        self.assertIn("repository: zajca/homebrew-pohunek", step)
        self.assertIn("event-type: pohunek-work-release", step)

    def test_the_release_workflow_stays_free_of_the_dispatch_secret(self):
        self.assertNotIn("TAP_DISPATCH_PAT", RELEASE)

    def test_the_tag_is_read_through_env_and_matched_before_use(self):
        run = NOTIFY.split("run: |")[1].split("- name: Dispatch")[0]
        self.assertNotIn("${{", run)
        self.assertIn("TAG: ${{ github.event.workflow_run.head_branch }}", NOTIFY)
        self.assertIn("'^[0-9]+\\.[0-9]+\\.[0-9]+$'", run)

    def test_the_release_must_be_published_and_stable(self):
        self.assertIn("--json isDraft,isPrerelease", NOTIFY)
        self.assertIn('"false true")', NOTIFY)
        self.assertIn("formula=pohunek", NOTIFY)


if __name__ == "__main__":
    unittest.main()
