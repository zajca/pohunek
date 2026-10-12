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
            "rust-toolchain.toml",
            ".github/actions/setup-rust/**",
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
            "rust-toolchain.toml",
            ".github/actions/setup-rust/**",
            ".github/workflows/release.yml",
        },
        "main": {
            "docs/knowledge/**",
            "crates/xtask/**",
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
            "crates/cli/tests/hermes_package.rs",
            "crates/cli/tests/support/hermes_provider.rs",
            "crates/worker-protocol/src/hook_schema.rs",
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
            "rust-toolchain.toml",
            ".github/actions/setup-rust/**",
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
            "rust-toolchain.toml",
            ".github/actions/setup-rust/**",
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


# A toolchain change must run every Rust job, so every filter that selects
# one lists the pin and the action that installs it.
TOOLCHAIN_INPUTS = {"rust-toolchain.toml", ".github/actions/setup-rust/**"}


def filter_names() -> list[str]:
    filters = re.search(r"(?ms)^          filters: \|\n(.*?)(?=^\S|^  [a-z])", CI)
    if filters is None:
        raise AssertionError("paths filter block not found")
    return re.findall(r"(?m)^            ([a-z_]+):\n", filters.group(1))


# Runtime package jobs and the package each one drives (issue #149). The
# selector job decides which of them a pull request runs; nothing else may.
PACKAGE_JOBS = {"pi-package": "pi", "codex-package": "codex", "claude-package": "claude"}
PACKAGE_SELECTOR = "package-selection"
PACKAGE_SELECT_COMMAND = 'cargo xtask package affected --base "$BASE_SHA" --json'


def evaluate_package_condition(expression: str, event: str, cancelled: bool, result: str, outputs: dict[str, str]) -> bool:
    """Evaluate a package job's `if:` in Python for the operators it may use.

    Only `!cancelled()`, `github.event_name`, the selector's `result` and
    `outputs.<name>`, string literals, `==`, `!=`, `&&`, `||` and parentheses
    are understood; anything else fails the translation. A missing output reads
    as the empty string and string comparison ignores case, as in GitHub Actions.
    """
    python = expression.replace("!cancelled()", "(not cancelled)")
    python = python.replace("github.event_name", "event")
    python = python.replace(f"needs.{PACKAGE_SELECTOR}.result", "result")
    python = re.sub(
        rf"needs\.{re.escape(PACKAGE_SELECTOR)}\.outputs\.([a-z_]+)",
        r"outputs.get('\1', '')",
        python,
    )
    python = python.replace("&&", " and ").replace("||", " or ")
    python = re.sub(r"'[^']*'", lambda literal: literal.group(0).lower(), python)
    leftover = re.sub(r"'[^']*'|\b(?:not|and|or|event|result|cancelled|outputs\.get)\b|==|!=|[()\s,]", "", python)
    if leftover:
        raise AssertionError(f"untranslated condition syntax {leftover!r} in {expression!r}")
    return bool(eval(python, {"__builtins__": {}}, {
        "event": event.lower(),
        "cancelled": cancelled,
        "result": result.lower(),
        "outputs": {name: value.lower() for name, value in outputs.items()},
    }))


class CiTierWorkflowTests(unittest.TestCase):
    def test_every_paths_filter_reacts_to_a_toolchain_change(self):
        for name in filter_names():
            with self.subTest(filter=name):
                self.assertLessEqual(TOOLCHAIN_INPUTS, filter_paths(name))

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
        self.assertIn(
            "continue-on-error: ${{ matrix.runner == 'macos-14' && github.event_name != 'schedule' && !(github.event_name == 'workflow_dispatch' && inputs.tier == 'nightly') }}",
            package,
        )

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


class RuntimePackageSelectionTests(unittest.TestCase):
    """Package jobs follow the fail-closed selector, never hand-kept path lists."""

    def condition(self, job: str) -> str:
        match = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", workflow_jobs()[job])
        self.assertIsNotNone(match, f"{job} has no condition")
        assert match is not None
        return match.group(1)

    def test_every_official_package_has_a_job_and_a_selector_output(self):
        official = {
            entry.name for entry in (ROOT / "runtime-packages").iterdir() if entry.is_dir()
        }
        self.assertEqual(set(PACKAGE_JOBS.values()), official)
        outputs = block(workflow_jobs()[PACKAGE_SELECTOR], "outputs", indent=4)
        for package in official:
            self.assertIn(f"      {package}: ${{{{ steps.select.outputs.{package} }}}}\n", outputs)
        select = workflow_jobs()[PACKAGE_SELECTOR].split("      - name: Select the runtime packages\n", 1)[1]
        self.assertIn(f"for package in {' '.join(PACKAGE_JOBS.values())}; do", select)

    def test_the_selector_runs_the_xtask_command_on_pull_requests(self):
        selector = workflow_jobs()[PACKAGE_SELECTOR]
        self.assertEqual(self.condition(PACKAGE_SELECTOR), "github.event_name == 'pull_request'")
        self.assertIn(PACKAGE_SELECT_COMMAND, selector)
        self.assertIn("          BASE_SHA: ${{ github.event.pull_request.base.sha }}\n", selector)
        self.assertIn("set -euo pipefail", selector)
        self.assertIn("fetch-depth: 0", selector)
        # The base commit reaches the script only through the environment.
        script = selector.split("        run: |\n", 1)[1]
        self.assertNotIn("${{", script)

    def test_package_jobs_depend_on_the_selector_and_carry_no_path_lists(self):
        jobs = workflow_jobs()
        changes = jobs["changes"]
        for job, package in PACKAGE_JOBS.items():
            with self.subTest(job=job):
                self.assertRegex(jobs[job], rf"(?m)^    needs: {PACKAGE_SELECTOR}$")
                self.assertNotIn("needs.changes", jobs[job])
                self.assertNotIn(f"{package}_package", changes)
                with self.assertRaises(AssertionError):
                    filter_paths(f"{package}_package")

    def test_package_jobs_run_unless_a_successful_selection_excludes_them(self):
        scenarios = (
            # (event, cancelled, selector result, outputs, runs)
            ("push", False, "skipped", {}, True),
            ("schedule", False, "skipped", {}, True),
            ("workflow_dispatch", False, "skipped", {}, True),
            ("pull_request", False, "success", {"pi": "true", "codex": "true", "claude": "true"}, True),
            # A selection that did not succeed runs every package, whatever
            # outputs it set before failing.
            ("pull_request", False, "failure", {}, True),
            ("pull_request", False, "failure", {"pi": "false", "codex": "false", "claude": "false"}, True),
            ("pull_request", False, "skipped", {}, True),
            ("pull_request", False, "cancelled", {"pi": "false", "codex": "false", "claude": "false"}, True),
            # A successful selection with a missing or malformed output runs the package.
            ("pull_request", False, "success", {}, True),
            ("pull_request", False, "success", {"pi": "", "codex": "", "claude": ""}, True),
            ("pull_request", False, "success", {"pi": "no", "codex": "0", "claude": "yes"}, True),
            # GitHub compares strings case-insensitively, so any casing of false excludes.
            ("pull_request", False, "success", {"pi": "False", "codex": "FALSE", "claude": "False"}, False),
            ("pull_request", True, "success", {"pi": "true", "codex": "true", "claude": "true"}, False),
        )
        for job, package in PACKAGE_JOBS.items():
            expression = self.condition(job)
            for event, cancelled, result, outputs, runs in scenarios:
                with self.subTest(job=job, event=event, cancelled=cancelled, result=result, outputs=outputs):
                    self.assertEqual(
                        evaluate_package_condition(expression, event, cancelled, result, outputs), runs
                    )
            with self.subTest(job=job, excluded=True):
                others = {name: "true" for name in PACKAGE_JOBS.values()}
                others[package] = "false"
                self.assertFalse(evaluate_package_condition(expression, "pull_request", False, "success", others))
                for other in set(PACKAGE_JOBS.values()) - {package}:
                    only_other = {name: "false" for name in PACKAGE_JOBS.values()}
                    only_other[other] = "true"
                    self.assertFalse(
                        evaluate_package_condition(expression, "pull_request", False, "success", only_other)
                    )


if __name__ == "__main__":
    unittest.main()
