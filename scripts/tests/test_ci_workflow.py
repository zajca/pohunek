"""Structural checks for the release-build gating in ci.yml (stdlib only)."""

from pathlib import Path
import re
import unittest

WORKFLOW = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "ci.yml"

REQUIRED_RELEASE_PATHS = {
    "Cargo.toml",
    "crates/**/Cargo.toml",
    "Cargo.lock",
    ".cargo/**",
    ".github/workflows/ci.yml",
    ".github/workflows/release.yml",
    "scripts/release",
    "packaging/**",
    "rust-toolchain",
    "rust-toolchain.toml",
}
FULL_RUN_EVENTS = ("schedule", "push", "workflow_dispatch")


def job_block(text: str, job: str) -> str:
    """Return the text of one top-level job (two-space indented key)."""
    match = re.search(
        rf"^  {re.escape(job)}:\n(.*?)(?=^  \S[^\n]*:\n|\Z)", text, re.S | re.M
    )
    if match is None:
        raise AssertionError(f"job {job!r} not found in {WORKFLOW}")
    return match.group(1)


def release_filter_paths(text: str) -> set[str]:
    """Return the patterns listed under the `release:` paths-filter key."""
    match = re.search(
        r"^            release:\n((?:              - '[^\n]*'\n|            #[^\n]*\n)+)",
        text,
        re.M,
    )
    if match is None:
        raise AssertionError("`release` paths filter not found")
    return set(re.findall(r"- '([^']+)'", match.group(1)))


class ReleaseGatingTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text()

    def test_release_filter_covers_release_inputs(self):
        missing = REQUIRED_RELEASE_PATHS - release_filter_paths(self.text)
        self.assertEqual(missing, set())

    def test_changes_job_exposes_release_output(self):
        self.assertIn(
            "release: ${{ steps.filter.outputs.release }}", job_block(self.text, "changes")
        )

    def test_release_build_runs_on_full_events_and_filter(self):
        block = job_block(self.text, "release-build")
        condition = re.search(r"^    if: (.*)$", block, re.M)
        self.assertIsNotNone(condition, "release-build has no `if:`")
        for event in FULL_RUN_EVENTS:
            self.assertIn(f"github.event_name == '{event}'", condition.group(1))
        self.assertIn("needs.changes.outputs.release == 'true'", condition.group(1))
        self.assertRegex(block, r"(?m)^    needs: changes$")
        self.assertIn("cargo build --workspace --release", block)

    def test_release_build_owns_release_cache_saver(self):
        block = job_block(self.text, "release-build")
        self.assertIn("shared-key: release", block)
        self.assertIn("save-if: ${{ github.ref == 'refs/heads/main' }}", block)

    def test_doctests_run_on_every_pull_request(self):
        block = job_block(self.text, "doctests")
        self.assertNotRegex(block, r"(?m)^    if:")
        self.assertIn("cargo test --doc --workspace --all-features", block)
        self.assertNotIn("--release", block)

    def test_no_job_depends_on_removed_release_check(self):
        self.assertNotIn("release-check", self.text)


if __name__ == "__main__":
    unittest.main()
