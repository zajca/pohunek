"""Contract checks for the PR, main, and nightly CI tiers."""

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
CI = (ROOT / ".github" / "workflows" / "ci.yml").read_text()


def block(text: str, name: str, indent: int = 2) -> str:
    """Return one canonical YAML mapping block, ending at the next sibling."""
    prefix = " " * indent
    match = re.search(
        rf"(?m)^{prefix}{re.escape(name)}:\n(.*?)(?=^{prefix}[A-Za-z0-9_-]+:|\Z)",
        text,
        re.S,
    )
    if match is None:
        raise AssertionError(f"YAML block {name!r} not found")
    return match.group(1)


def job(name: str) -> str:
    return block(CI.split("jobs:\n", 1)[1], name)


def filter_paths(name: str) -> set[str]:
    match = re.search(
        rf"(?m)^            {re.escape(name)}:\n((?:              - '[^']+'\n)+)",
        CI,
    )
    if match is None:
        raise AssertionError(f"paths filter {name!r} not found")
    return set(re.findall(r"              - '([^']+)'", match.group(1)))


class CiTierWorkflowTests(unittest.TestCase):
    def test_nightly_runs_daily_and_manual_dispatch_requires_a_tier(self):
        schedule = block(CI.split("on:\n", 1)[1], "schedule", indent=2)
        self.assertIn("- cron: '0 5 * * *'", schedule)

        dispatch = block(CI.split("on:\n", 1)[1], "workflow_dispatch", indent=2)
        tier = block(dispatch, "inputs", indent=4)
        tier = block(tier, "tier", indent=6)
        self.assertIn("required: true", tier)
        self.assertIn("type: choice", tier)
        self.assertRegex(tier, r"(?m)^\s+- main$")
        self.assertRegex(tier, r"(?m)^\s+- nightly$")

    def test_superseded_pr_and_main_runs_cancel_without_cross_tier_cancellation(self):
        concurrency = block(CI, "concurrency", indent=0)
        self.assertIn("github.ref", concurrency)
        self.assertIn("github.event_name == 'pull_request'", concurrency)
        self.assertIn("github.event_name == 'push'", concurrency)
        self.assertIn("github.run_id", concurrency)
        self.assertIn("cancel-in-progress: true", concurrency)

    def test_nightly_only_runs_macOS_14_and_cold_rebuild(self):
        package = job("macos-package")
        self.assertIn("github.event_name == 'schedule'", package)
        self.assertIn("inputs.tier == 'nightly'", package)
        self.assertIn("matrix.runner == 'macos-15'", package)
        self.assertIn("inputs.tier == 'main'", package)
        self.assertIn("needs.changes.outputs.macos_package == 'true'", package)

        cold_rebuild = re.search(
            r"(?ms)^      - name: Rebuild from a cold release directory and compare\n(.*?)(?=^      - name:|\Z)",
            package,
        )
        self.assertIsNotNone(cold_rebuild)
        assert cold_rebuild is not None
        self.assertIn("github.event_name == 'schedule'", cold_rebuild.group(1))
        self.assertIn("inputs.tier == 'nightly'", cold_rebuild.group(1))

        relay = job("integration-relay")
        self.assertIn("github.event_name == 'schedule'", relay)
        self.assertIn("inputs.tier == 'nightly'", relay)
        self.assertIn("needs.changes.outputs.relay == 'true'", relay)

    def test_macos_pr_filters_are_narrow_and_ci_workflow_forces_the_jobs(self):
        package = filter_paths("macos_package")
        self.assertTrue(
            {
                "packaging/**",
                "scripts/acceptance/macos-package-install",
                "scripts/acceptance/launchd_lifetime_evidence.py",
                "crates/platform/src/supervisor/launchd/**",
                ".github/workflows/release.yml",
                ".github/workflows/ci.yml",
            }.issubset(package)
        )
        self.assertNotIn("crates/daemon/**", package)
        self.assertNotIn("crates/cli/**", package)

        hermes = filter_paths("hermes_macos")
        self.assertTrue(
            {
                "compat/hermes/**",
                "crates/cli/src/hermes_integration/**",
                "crates/xtask/src/hermes*.rs",
                "crates/daemon/src/agent/hermes.rs",
                "scripts/provision-hermes-compat",
                ".github/workflows/ci.yml",
            }.issubset(hermes)
        )
        self.assertIn("crates/cli/src/**", filter_paths("hermes"))

        sdk = filter_paths("sdk_macos")
        self.assertTrue(
            {
                "sdk/ts/**",
                "package.json",
                "bun.lock",
                ".bun-version",
                ".github/workflows/ci.yml",
            }.issubset(sdk)
        )
        self.assertNotIn("crates/daemon/**", sdk)

    def test_every_macOS_job_has_a_tier_condition_and_pr_filter(self):
        checks = {
            "platform-contracts": ("outputs.macos == 'true'",),
            "macos-package": ("outputs.macos_package == 'true'",),
            "hermes-compatibility-macos": ("outputs.hermes_macos == 'true'",),
            "sdk-macos": ("outputs.sdk_macos == 'true'",),
        }
        for name, pr_filters in checks.items():
            with self.subTest(job=name):
                definition = job(name)
                condition = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", definition)
                self.assertIsNotNone(condition, f"{name} has no explicit tier condition")
                assert condition is not None
                for filter_name in pr_filters:
                    self.assertIn(filter_name, condition.group(1))
                self.assertIn("github.event_name == 'schedule'", condition.group(1))
                self.assertIn("github.event_name == 'push'", condition.group(1))
                self.assertIn("github.event_name == 'workflow_dispatch'", condition.group(1))


if __name__ == "__main__":
    unittest.main()
