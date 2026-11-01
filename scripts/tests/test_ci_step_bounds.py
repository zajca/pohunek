"""Every step that can wait on a network or a local archive has an upper bound (#1811),
and every fail-fast cancel names the job it came from (#1823). Stdlib only."""

import os
import re
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = sorted((ROOT / ".github" / "workflows").glob("*.yml"))
SETUP_MOLD_ACTION = ROOT / ".github" / "actions" / "setup-mold" / "action.yml"
CANCEL_ACTION = ROOT / ".github" / "actions" / "cancel-run" / "action.yml"
CANCEL_SCRIPT = ROOT / ".github" / "actions" / "cancel-run" / "cancel-run.sh"


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
    def test_the_composite_passes_the_exact_job_to_the_script(self):
        # A matrix shares one job id across its legs; the check run id tells them apart.
        action = CANCEL_ACTION.read_text()
        self.assertIn("FAILED_JOB: ${{ github.job }}", action)
        self.assertIn("JOB_ID: ${{ job.check_run_id }}", action)
        self.assertRegex(action, r"(?m)^        LOOKUP_TIMEOUT_SECONDS: \d+$")
        self.assertIn("${GITHUB_ACTION_PATH}/cancel-run.sh", action)

    def test_no_job_cancels_the_run_inline(self):
        # Only the watchers of called workflows cancel inline: they have no job of their own to resolve.
        for workflow in WORKFLOWS:
            for step in steps_of(workflow.read_text()):
                if "gh run cancel" in step:
                    with self.subTest(workflow=workflow.name):
                        self.assertIn("(a called workflow) failed first", step)

    def run_script(
        self, jobs_api_json: str | None, job_id: str = "99", hang: bool = False, timeout: str = "20"
    ) -> tuple[str, str, str]:
        """Run cancel-run.sh with a fake `gh`; return (stdout, summary, gh calls)."""
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            bin_dir = tmp_path / "bin"
            bin_dir.mkdir()
            calls = tmp_path / "calls"
            reply = tmp_path / "reply.json"
            if jobs_api_json is not None:
                reply.write_text(jobs_api_json)
            gh = bin_dir / "gh"
            gh.write_text(
                "#!/bin/sh\n"
                f'echo "$*" >> "{calls}"\n'
                'if [ "$1" = api ]; then\n'
                + (f'  read -r _ < "{tmp_path / "never"}"\n' if hang else "")
                + f'  [ -f "{reply}" ] || exit 1\n'
                f'  cat "{reply}"\n'
                "fi\n"
            )
            gh.chmod(0o755)
            if hang:
                os.mkfifo(tmp_path / "never")
            summary = tmp_path / "summary.md"
            env = {
                "PATH": f"{bin_dir}:{os.environ['PATH']}",
                "GH_TOKEN": "x",
                "RUN_ID": "7",
                "REPOSITORY": "o/r",
                "FAILED_JOB": "fast-tests",
                "JOB_ID": job_id,
                "LOOKUP_TIMEOUT_SECONDS": timeout,
                "GITHUB_STEP_SUMMARY": str(summary),
            }
            result = subprocess.run(["bash", str(CANCEL_SCRIPT)], env=env, capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            return result.stdout, summary.read_text(), calls.read_text()

    def test_the_script_names_the_display_name_the_failed_step_and_cancels(self):
        info = '{"name": "tests (cli, fast)", "steps": [{"name": "Set up job", "conclusion": "success"}, {"name": "Run fast shard", "conclusion": "failure"}]}'
        out, summary, calls = self.run_script(info)
        expected = "job 'tests (cli, fast)' (fast-tests, https://github.com/o/r/actions/runs/7/job/99) failed first in step 'Run fast shard'"
        self.assertIn(f"::error title=Fail fast::{expected}", out)
        self.assertIn(expected, summary)
        self.assertIn("run cancel 7 --repo o/r", calls)
        self.assertIn("api repos/o/r/actions/jobs/99", calls)

    def test_the_script_still_cancels_and_falls_back_to_the_job_id_when_the_lookup_fails(self):
        out, summary, calls = self.run_script(None)
        self.assertIn("job 'fast-tests' (fast-tests, https://github.com/o/r/actions/runs/7/job/99) failed first;", out)
        self.assertNotIn("in step", out)
        self.assertIn("failed first", summary)
        self.assertIn("run cancel 7 --repo o/r", calls)


    def test_an_empty_job_id_skips_the_lookup_and_the_job_link_but_cancels(self):
        out, summary, calls = self.run_script('{"name": "unused"}', job_id="")
        expected = "job 'fast-tests' (fast-tests, https://github.com/o/r/actions/runs/7) failed first"
        self.assertIn(f"::error title=Fail fast::{expected}", out)
        self.assertIn(expected, summary)
        self.assertNotIn("/job/", out)
        self.assertNotIn("api ", calls)
        self.assertIn("run cancel 7 --repo o/r", calls)

    def test_a_lookup_that_outlives_its_bound_falls_back_and_cancels(self):
        # A zero bound fires the watchdog at once; the fake `gh api` blocks on a fifo nobody writes.
        out, _, calls = self.run_script('{"name": "unused"}', hang=True, timeout="0")
        self.assertIn("::error title=Fail fast::job 'fast-tests' (fast-tests, https://github.com/o/r/actions/runs/7/job/99) failed first;", out)
        self.assertIn("run cancel 7 --repo o/r", calls)


if __name__ == "__main__":
    unittest.main()
