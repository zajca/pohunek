"""Every step that can wait on a network or a local archive has an upper bound (#1811),
and every fail-fast cancel names the job it came from (#1823). Stdlib only."""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = sorted((ROOT / ".github" / "workflows").glob("*.yml"))
SETUP_MOLD_ACTION = ROOT / ".github" / "actions" / "setup-mold" / "action.yml"
CANCEL_ACTION = ROOT / ".github" / "actions" / "cancel-run" / "action.yml"
CANCEL_ANNOTATION = "::error title=Fail fast::"


def steps_of(text: str) -> list[str]:
    """Split a workflow into step blocks (a `- ` item at six spaces of indent)."""
    return re.split(r"(?m)^(?=      - )", text)[1:]


class StepBoundTests(unittest.TestCase):
    def test_every_setup_mold_call_has_a_step_timeout(self):
        calls = 0
        for workflow in WORKFLOWS:
            for step in steps_of(workflow.read_text()):
                if "uses: ./.github/actions/setup-mold" in step:
                    calls += 1
                    with self.subTest(workflow=workflow.name):
                        self.assertRegex(step, r"(?m)^        timeout-minutes: \d+$")
        self.assertGreater(calls, 15)

    def test_no_workflow_installs_mold_through_an_unbounded_apt_run(self):
        for workflow in WORKFLOWS:
            self.assertNotRegex(workflow.read_text(), r"apt-get[^\n]*install[^\n]*\bmold\b", workflow.name)

    def test_every_apt_step_has_a_step_timeout_and_network_timeouts(self):
        for workflow in WORKFLOWS:
            for step in steps_of(workflow.read_text()):
                if "apt-get" in step:
                    with self.subTest(workflow=workflow.name):
                        self.assertRegex(step, r"(?m)^        timeout-minutes: \d+$")
                        self.assertIn("Acquire::http::Timeout=", step)

    def test_the_composite_bounds_its_local_steps(self):
        text = SETUP_MOLD_ACTION.read_text()
        self.assertIn("LOCAL_STEP_TIMEOUT_SECONDS", text)
        for command in ("sha256sum -c -", "tar -xzf", '"$dest/bin/mold" --version'):
            self.assertRegex(text, rf'timeout "\$LOCAL_STEP_TIMEOUT_SECONDS" {re.escape(command)}')


class MoldPinTests(unittest.TestCase):
    def test_every_workflow_carries_the_same_mold_pin(self):
        pins = {}
        for workflow in WORKFLOWS:
            text = workflow.read_text()
            if "./.github/actions/setup-mold" in text:
                version = re.search(r'(?m)^  MOLD_VERSION: "([^"]+)"$', text)
                sha = re.search(r"(?m)^  MOLD_SHA256: ([0-9a-f]{64})$", text)
                self.assertTrue(version and sha, workflow.name)
                pins[workflow.name] = (version.group(1), sha.group(1))
        self.assertGreaterEqual(len(pins), 5)
        self.assertEqual(len(set(pins.values())), 1, pins)


class CancelAttributionTests(unittest.TestCase):
    def cancel_blocks(self) -> list[tuple[str, str]]:
        blocks = [(CANCEL_ACTION.name, CANCEL_ACTION.read_text())]
        for workflow in WORKFLOWS:
            for step in steps_of(workflow.read_text()):
                if "gh run cancel" in step:
                    blocks.append((workflow.name, step))
        return blocks

    def test_every_cancel_names_its_job(self):
        blocks = self.cancel_blocks()
        self.assertGreater(len(blocks), 10)
        for name, block in blocks:
            with self.subTest(source=name):
                self.assertIn(CANCEL_ANNOTATION, block)
                self.assertIn("FAILED_JOB:", block)

    def test_cancels_of_a_job_link_the_exact_job(self):
        # A matrix shares one job id across its legs; the check run id tells them apart.
        for name, block in self.cancel_blocks():
            if "FAILED_JOB: ${{ github.job }}" in block:
                with self.subTest(source=name):
                    self.assertIn("JOB_ID: ${{ job.check_run_id }}", block)
                    self.assertIn("/job/${JOB_ID}", block)


if __name__ == "__main__":
    unittest.main()
