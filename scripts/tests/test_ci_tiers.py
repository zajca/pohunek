"""Contract checks for the PR, main, and nightly CI tiers."""

from pathlib import Path
import json
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
CI = (ROOT / ".github" / "workflows" / "ci.yml").read_text()

# Reviewed against each macOS job's commands, build inputs, fixtures and the
# dependencies those commands exercise. Paths under ``pr`` trigger that job
# on pull requests. Paths under ``main`` are covered by the unconditional main
# and nightly tiers; add new step inputs here when reviewing a job change.
MACOS_PATH_OWNERSHIP = {
    "platform-contracts": {
        "filter": "macos",
        "pr": {
            "Cargo.toml",
            "crates/platform/**",
            "crates/paths/**",
            "crates/worker-protocol/**",
            "crates/session-worker/**",
            "crates/terminal/**",
            "crates/logging/**",
            "crates/service-config/**",
            "crates/test-support/**",
            "crates/daemon/**",
            "crates/cli/**",
            "crates/protocol/**",
            "crates/client/**",
            "crates/knowledge/**",
            "crates/hostcheck/**",
            "crates/netbird/**",
            "crates/overlay/**",
            "crates/prompt/**",
            "scripts/acceptance/**",
            "crates/relay-client/**",
            "crates/relay-protocol/**",
            "docs/knowledge/**",
            "Cargo.lock",
            ".cargo/**",
            ".github/workflows/ci.yml",
        },
        "main": set(),
    },
    "macos-package": {
        "filter": "macos_package",
        "pr": {
            "packaging/**",
            "scripts/acceptance/macos-package-install",
            "scripts/acceptance/launchd_lifetime_evidence.py",
            "crates/platform/src/supervisor/launchd/**",
            ".github/workflows/ci.yml",
            ".github/workflows/release.yml",
        },
        "main": {
            "docs/knowledge/**",
            "crates/xtask/**",
            "rust-toolchain*",
            "compat/**",
            "README.md",
            "LICENSE",
            "docs/features.md",
            "docs/install.md",
            "docs/cli.md",
            "docs/sdk.md",
            "docs/development.md",
            "Cargo.toml",
            "Cargo.lock",
            ".cargo/**",
            "crates/cli/**",
            "crates/daemon/**",
            "crates/session-worker/**",
            "crates/platform/**",
            "crates/paths/**",
            "crates/service-config/**",
            "crates/worker-protocol/**",
            "crates/logging/**",
            "crates/terminal/**",
            "crates/protocol/**",
            "crates/client/**",
            "crates/knowledge/**",
            "crates/hostcheck/**",
            "crates/netbird/**",
            "crates/overlay/**",
            "crates/prompt/**",
            "crates/relay-client/**",
            "crates/relay-protocol/**",
        },
    },
    "hermes-compatibility-macos": {
        "filter": "hermes_macos",
        "pr": {
            "compat/hermes/**",
            "crates/cli/src/hermes_integration/**",
            "crates/xtask/src/hermes*.rs",
            "crates/daemon/src/agent/hermes.rs",
            "scripts/provision-hermes-compat",
            "scripts/tests/provision-hermes-compat.sh",
            "scripts/smoke-hermes-plugin-release",
            "scripts/tests/smoke-hermes-plugin-release.sh",
            "scripts/tests/fixtures/smoke-*",
            "sdk/ts/sdk/test/e2e.test.ts",
            "sdk/ts/sdk/test/hermes-plugin.e2e.test.ts",
            ".github/workflows/ci.yml",
        },
        "main": {
            "Cargo.toml",
            "Cargo.lock",
            "crates/cli/Cargo.toml",
            "crates/cli/src/**",
            "crates/paths/**",
            "crates/protocol/**",
            "crates/xtask/Cargo.toml",
            "crates/xtask/src/**",
            "packaging/**",
        },
    },
    "sdk-macos": {
        "filter": "sdk_macos",
        "pr": {
            "sdk/ts/**",
            "package.json",
            "bun.lock",
            ".bun-version",
            ".github/workflows/ci.yml",
        },
        "main": {
            "tsconfig*.json",
            "eslint.config.js",
            "Cargo.toml",
            "Cargo.lock",
            ".cargo/**",
            "crates/paths/**",
            "crates/protocol/**",
            "crates/platform/**",
            "crates/daemon/**",
            "crates/session-worker/**",
            "crates/worker-protocol/**",
            "crates/terminal/**",
            "crates/logging/**",
            "crates/service-config/**",
            "crates/cli/**",
            "crates/client/**",
            "crates/netbird/**",
            "crates/overlay/**",
            "crates/hostcheck/**",
            "crates/knowledge/**",
            "crates/relay-client/**",
            "crates/relay-protocol/**",
        },
    },
}


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


def workflow_jobs() -> dict[str, str]:
    jobs_text = CI.split("jobs:\n", 1)[1]
    matches = list(re.finditer(r"(?m)^  ([a-z][a-z0-9-]*):\n", jobs_text))
    return {
        match.group(1): jobs_text[
            match.end() : matches[index + 1].start()
            if index + 1 < len(matches)
            else len(jobs_text)
        ]
        for index, match in enumerate(matches)
    }


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
        triggers = CI.split("on:\n", 1)[1]
        schedule = block(triggers, "schedule", indent=2)
        self.assertIn("- cron: '0 5 * * *'", schedule)

        dispatch = block(triggers, "workflow_dispatch", indent=2)
        tier = block(block(dispatch, "inputs", indent=4), "tier", indent=6)
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

    def test_nightly_only_runs_macOS_14_cold_rebuild_and_relay_database_suite(self):
        jobs = workflow_jobs()
        package = jobs["macos-package"]
        condition = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", package)
        self.assertIsNotNone(condition)
        assert condition is not None
        self.assertNotIn("matrix.", condition.group(1))
        self.assertIn("github.event_name == 'schedule'", condition.group(1))
        self.assertIn("inputs.tier == 'nightly'", condition.group(1))
        self.assertIn("inputs.tier == 'main'", condition.group(1))
        self.assertIn("github.event_name == 'push'", condition.group(1))
        self.assertIn("needs.changes.outputs.macos_package == 'true'", condition.group(1))

        strategy = block(package, "strategy", indent=4)
        matrix = re.search(r"(?m)^      matrix: \$\{\{ (.+) \}\}$", strategy)
        self.assertIsNotNone(matrix)
        assert matrix is not None
        self.assertEqual(
            matrix.group(1),
            "fromJSON((github.event_name == 'schedule' || "
            "(github.event_name == 'workflow_dispatch' && inputs.tier == 'nightly')) "
            "&& '{\"runner\":[\"macos-14\",\"macos-15\"]}' "
            "|| '{\"runner\":[\"macos-15\"]}')",
        )
        matrix_values = [
            json.loads(payload)["runner"]
            for payload in re.findall(r"'({\"runner\":\[.*?\]})'", matrix.group(1))
        ]
        self.assertEqual(matrix_values, [["macos-14", "macos-15"], ["macos-15"]])
        self.assertIn("continue-on-error: ${{ matrix.runner == 'macos-14' }}", package)

        scenarios = (
            ("schedule", None, ["macos-14", "macos-15"]),
            ("workflow_dispatch", "nightly", ["macos-14", "macos-15"]),
            ("workflow_dispatch", "main", ["macos-15"]),
            ("push", None, ["macos-15"]),
            ("pull_request", None, ["macos-15"]),
        )
        for event, tier, runners in scenarios:
            with self.subTest(event=event, tier=tier):
                nightly = event == "schedule" or (event == "workflow_dispatch" and tier == "nightly")
                selected = matrix_values[0 if nightly else 1]
                self.assertEqual(selected, runners)

        cold_rebuild = re.search(
            r"(?ms)^      - name: Rebuild from a cold release directory and compare\n"
            r"(.*?)(?=^      - name:|\Z)",
            package,
        )
        self.assertIsNotNone(cold_rebuild)
        assert cold_rebuild is not None
        self.assertIn("github.event_name == 'schedule'", cold_rebuild.group(1))
        self.assertIn("inputs.tier == 'nightly'", cold_rebuild.group(1))

        relay = jobs["integration-relay"]
        self.assertIn("github.event_name == 'schedule'", relay)
        self.assertIn("inputs.tier == 'nightly'", relay)
        self.assertIn("needs.changes.outputs.relay == 'true'", relay)

    def test_reviewed_macos_input_inventory_has_pr_or_main_tier_coverage(self):
        jobs = workflow_jobs()
        discovered_macos_jobs = {
            name
            for name, definition in jobs.items()
            if re.search(r"(?m)^    runs-on:.*macos", definition)
            or re.search(r"(?m)^    runs-on: \$\{\{ matrix\.runner \}\}", definition)
        }
        self.assertEqual(discovered_macos_jobs, set(MACOS_PATH_OWNERSHIP))

        for name, ownership in MACOS_PATH_OWNERSHIP.items():
            with self.subTest(job=name):
                definition = jobs[name]
                condition = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", definition)
                self.assertIsNotNone(condition, f"{name} has no explicit tier condition")
                assert condition is not None
                expression = condition.group(1)
                for required in (
                    "github.event_name == 'schedule'",
                    "github.event_name == 'push'",
                    "github.event_name == 'pull_request'",
                    f"needs.changes.outputs.{ownership['filter']} == 'true'",
                ):
                    self.assertIn(required, expression)
                for tier in ("main", "nightly"):
                    self.assertRegex(
                        expression,
                        rf"github.event_name == 'workflow_dispatch' && "
                        rf"(?:\([^)]*inputs\.tier == '{tier}'[^)]*\)|inputs\.tier == '{tier}')",
                    )

                pr_paths = set(ownership["pr"])
                main_paths = set(ownership["main"])
                self.assertTrue(pr_paths, "every macOS job must own explicit PR inputs")
                self.assertFalse(pr_paths & main_paths, "each input must have one documented tier owner")
                self.assertEqual(filter_paths(ownership["filter"]), pr_paths)
                self.assertIn(".github/workflows/ci.yml", pr_paths)

                # Main and nightly execute the job unconditionally, covering
                # reviewed build dependencies intentionally omitted from PR filters.
                if main_paths:
                    self.assertTrue(
                        all(path not in pr_paths for path in main_paths),
                        "main-tier-only inputs must stay out of the PR filter inventory",
                    )


if __name__ == "__main__":
    unittest.main()
