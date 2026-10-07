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


ARCHIVE_CONSUMERS = ("fast-tests", "integration", "integration-relay")


class TestArchiveTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text()

    def test_build_tests_archives_the_whole_workspace_once(self):
        block = job_block(self.text, "build-tests")
        self.assertIn(
            "cargo nextest archive --profile ci --workspace --all-features --archive-file", block
        )
        self.assertIn("name: nextest-archive", block)
        self.assertIn("if-no-files-found: error", block)
        self.assertNotRegex(block, r"(?m)^    needs:")
        self.assertNotRegex(block, r"(?m)^    if:")

    def test_build_tests_is_the_only_test_debug_saver(self):
        saver = "save-if: ${{ github.ref == 'refs/heads/main' }}"
        blocks = {
            name: job_block(self.text, name)
            for name in re.findall(r"(?m)^  ([a-z][a-z0-9-]*):\n    (?:name|needs|if):", self.text)
        }
        savers = [
            name for name, block in blocks.items()
            if "shared-key: test-debug" in block and saver in block
        ]
        self.assertEqual(savers, ["build-tests"])

    def test_archive_consumers_run_from_the_archive_without_a_toolchain(self):
        for job in ARCHIVE_CONSUMERS:
            with self.subTest(job=job):
                block = job_block(self.text, job)
                self.assertRegex(block, r"(?m)^    needs: (?:build-tests|\[.*\bbuild-tests\b.*\])$")
                self.assertIn("name: nextest-archive", block)
                self.assertIn("--archive-file", block)
                forbidden = ["rust-cache", "setup-mold", "cargo build"]
                if job != "integration":
                    forbidden.append("rust-toolchain")
                for token in forbidden:
                    self.assertNotIn(token, block)
                self.assertNotIn("RUSTFLAGS", block)

    def test_archive_consumers_run_at_a_path_other_than_the_build_path(self):
        for job in ARCHIVE_CONSUMERS:
            with self.subTest(job=job):
                block = job_block(self.text, job)
                self.assertRegex(block, r"uses: actions/checkout@v4\n\s+with:\n\s+path: relocated\n")
                self.assertRegex(block, r"defaults:\n\s+run:\n\s+working-directory: relocated\n")
        self.assertNotIn("path: relocated", job_block(self.text, "build-tests"))

    def test_heavy_has_the_toolchain_and_sources_the_dependency_policy_test_needs(self):
        block = job_block(self.text, "integration")
        self.assertIn("rust-toolchain", block)
        self.assertIn("components: clippy", block)
        self.assertIn("cargo fetch --locked", block)

    def test_heavy_checks_partitions_against_the_archive(self):
        block = job_block(self.text, "integration")
        self.assertRegex(block, r"test-partitions --archive-file \S+ check")
        self.assertRegex(block, r"test-partitions --archive-file \S+ run heavy")

    def test_relay_db_keeps_its_changes_gate(self):
        block = job_block(self.text, "integration-relay")
        condition = re.search(r"^    if: (.*)$", block, re.M)
        self.assertIsNotNone(condition)
        self.assertIn("needs.changes.outputs.relay == 'true'", condition.group(1))
        self.assertRegex(block, r"needs: \[changes, build-tests\]")

    def test_default_feature_binaries_still_come_from_build_bins(self):
        block = job_block(self.text, "build-bins")
        self.assertIn("cargo build --locked -p pohunek-daemon -p pohunek-session-worker -p pohunek-cli", block)
        self.assertIn("name: pohunek-bins", block)
        self.assertIn("build-bins", job_block(self.text, "hermes-compatibility"))


if __name__ == "__main__":
    unittest.main()
