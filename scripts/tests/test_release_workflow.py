"""Structural checks on release.yml: write-token jobs run no repository code.

A job holding `contents: write` may only download an artifact, check it, and
attach it. The rules below are deliberately narrow so that adding any other
step to such a job fails the test.
"""

from pathlib import Path
import re
import unittest

import yaml

WORKFLOW = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "release.yml"

# A `run:` step in a write-token job may only start commands from this set.
ALLOWED_RUN_COMMANDS = {"set", "cd", "sha256sum", "shasum", "test", "ls", "for", "do", "done"}


def load_jobs() -> dict:
    return yaml.safe_load(WORKFLOW.read_text())["jobs"]


def write_jobs(jobs: dict) -> dict:
    return {
        name: job
        for name, job in jobs.items()
        if (job.get("permissions") or {}).get("contents") == "write"
    }


def run_commands(script: str) -> list[str]:
    """First word of every logical line of a shell script, ignoring comments."""
    words = []
    for line in script.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        words.append(re.split(r"\s+", line)[0])
    return words


class WriteTokenJobTests(unittest.TestCase):
    def test_write_token_jobs_exist(self):
        self.assertTrue(write_jobs(load_jobs()))

    def test_workflow_default_token_is_read_only(self):
        data = yaml.safe_load(WORKFLOW.read_text())
        self.assertEqual(data["permissions"], {"contents": "read"})

    def test_write_token_jobs_never_check_out_the_repository(self):
        for name, job in write_jobs(load_jobs()).items():
            for step in job["steps"]:
                self.assertFalse(
                    str(step.get("uses", "")).startswith("actions/checkout"),
                    f"{name} holds a write token and checks out the repository",
                )

    def test_write_token_jobs_run_only_checksum_and_count_commands(self):
        for name, job in write_jobs(load_jobs()).items():
            for step in job["steps"]:
                if "run" not in step:
                    continue
                for word in run_commands(step["run"]):
                    self.assertIn(
                        word,
                        ALLOWED_RUN_COMMANDS,
                        f"{name} step {step.get('name')!r} runs `{word}` with a write token",
                    )

    def test_write_token_jobs_use_only_pinned_actions(self):
        for name, job in write_jobs(load_jobs()).items():
            for step in job["steps"]:
                if "uses" in step:
                    self.assertRegex(
                        step["uses"],
                        r"@[0-9a-f]{40}$",
                        f"{name} uses an unpinned action {step['uses']}",
                    )

    def test_build_job_cannot_write(self):
        build = load_jobs()["build"]
        self.assertEqual(build["permissions"], {"contents": "read"})
        for step in build["steps"]:
            self.assertNotIn("action-gh-release", str(step.get("uses", "")))

    def test_publish_job_mirrors_the_build_matrix(self):
        jobs = load_jobs()
        pairs = lambda job: {  # noqa: E731
            (e["component"], e["target"]) for e in job["strategy"]["matrix"]["include"]
        }
        self.assertEqual(pairs(jobs["publish"]), pairs(jobs["build"]))
        self.assertEqual(jobs["publish"]["needs"], ["build"])


if __name__ == "__main__":
    unittest.main()
