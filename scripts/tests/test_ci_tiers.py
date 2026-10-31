"""Contract checks for the PR, main, and nightly CI tiers."""

from pathlib import Path
import collections
import fnmatch
import hashlib
import importlib.machinery
import importlib.util
import json
import os
import re
import shutil
import subprocess
import tempfile
import time
import tomllib
import unittest

import hermetic_git  # noqa: F401  (isolates git from the developer's configuration)


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
            "crates/shape-guard/**",
            "crates/daemon/**",
            "crates/cli/**",
            "crates/protocol/**",
            "crates/client/**",
            "crates/knowledge/**",
            "crates/hostcheck/**",
            "crates/netbird/**",
            "crates/overlay/**",
            "crates/overlay-tailscale/**",
            "crates/prompt/**",
            "crates/assistant/**",
            "crates/package/**",
            "scripts/acceptance/**",
            "scripts/dev-bootstrap",
            "scripts/tests/test_dev_bootstrap.py",
            "scripts/test-partitions",
            ".config/nextest.toml",
            ".github/actions/hermes-test-python/**",
            ".github/actions/setup-uv/**",
            "crates/relay-client/**",
            "crates/relay-protocol/**",
            "Cargo.lock",
            ".cargo/**",
            ".github/workflows/ci.yml",
            "rust-toolchain.toml",
            ".github/actions/setup-rust/**",
        },
        # Platform-neutral data whose readers all run on Linux (#1225).
        "main": {"docs/knowledge/**"},
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
            "crates/overlay-tailscale/**",
            "crates/prompt/**",
            "crates/relay-client/**",
            "crates/relay-protocol/**",
        },
    },
    "hermes-compatibility-macos": {
        "filter": "hermes_macos",
        "pr": {
            "compat/hermes/**",
            "runtime-packages/hermes/**",
            "crates/cli/src/hermes_integration/**",
            "crates/cli/tests/support/hermes_provider.rs",
            "crates/worker-protocol/src/hook_schema.rs",
            "crates/xtask/src/hermes*.rs",
            "crates/xtask/src/hermes/**",
            "crates/daemon/src/agent/hermes.rs",
            "scripts/provision-hermes-compat",
            "scripts/tests/provision-hermes-compat.sh",
            "scripts/build-hermes-package",
            "scripts/smoke-hermes-plugin-release",
            "scripts/tests/smoke-hermes-plugin-release.sh",
            "scripts/tests/fixtures/smoke-*",
            "sdk/ts/sdk/test/e2e.test.ts",
            "sdk/ts/sdk/test/hermes-plugin.e2e.test.ts",
            ".github/workflows/ci.yml",
            ".config/nextest.toml",
            "rust-toolchain.toml",
            ".github/actions/setup-rust/**",
            ".github/actions/setup-uv/**",
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
            "crates/overlay-tailscale/**",
            "crates/hostcheck/**",
            "crates/knowledge/**",
            "crates/relay-client/**",
            "crates/relay-protocol/**",
        },
    },
}
# macOS jobs that never run on a pull request, whatever their paths filter
# matches: the main and nightly tiers run them unconditionally.
MACOS_TIER_JOBS = {"macos-tier"}
# macOS jobs that build for other macOS jobs of the run, by the jobs that
# download what they build (issue #1210). Each runs exactly when one of its
# consumers does, so it adds no macOS slot to a run that has none of them.
MACOS_BUILDER_JOBS = {
    "macos-release-build": {"macos-package", "hermes-compatibility-macos"},
}
# The artifact `macos-release-build` uploads: the version-A release daemon
# binaries, tarred so they keep their executable mode.
MACOS_RELEASE_ARTIFACT = "macos-release-daemon"
# The guard that keeps fork and Dependabot pull requests out of the release
# rehearsal: their token cannot hold the scopes the called workflow declares.
SAME_REPOSITORY_PR = (
    "(github.event_name != 'pull_request' || "
    "(github.event.pull_request.head.repo.full_name == github.repository && "
    "github.actor != 'dependabot[bot]'))"
)
# macOS jobs of the reusable workflows `ci.yml` calls, by calling job. They
# take macOS slots in a CI run like the jobs of `ci.yml` itself, so a pull
# request starts them only through the caller's paths filter.
MACOS_CALLED_JOBS = {
    "release-rehearsal-build": {
        "workflow": "release-build.yml",
        "jobs": {"rehearsal-macos"},
        "filter": "release_rehearsal",
    },
    "release-rehearsal": {
        "workflow": "release-evidence.yml",
        "jobs": {"rows"},
        "filter": "release_rehearsal",
    },
    "release-rehearsal-scenarios": {
        "workflow": "release-smoke-scenarios.yml",
        "jobs": {"smoke-scenarios"},
        "filter": "release_rehearsal",
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


# The step that ends a PR or dispatch run when a required job fails (#1075).
FAIL_FAST_STEP = "Cancel the run (fail fast)"
FAIL_FAST_CONDITION = (
    "failure() && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch')"
)


def workflow_jobs(workflow: str = CI) -> dict[str, str]:
    jobs_text = workflow.split("jobs:\n", 1)[1]
    matches = list(re.finditer(r"(?m)^  ([a-z][a-z0-9-]*):\n", jobs_text))
    return {
        match.group(1): jobs_text[
            match.end() : matches[index + 1].start()
            if index + 1 < len(matches)
            else len(jobs_text)
        ]
        for index, match in enumerate(matches)
    }


def runner_matrix_may_use_macos(job: str) -> bool:
    """Whether a job that runs on `matrix.runner` may place a leg on macOS.

    Only the job's `strategy` block is read: a static runner value there that
    names macOS counts, and so does any part of the matrix computed at run
    time (`fromJSON`), whose legs cannot be known from the file.
    """
    if not re.search(r"(?m)^    runs-on: \$\{\{ matrix\.runner \}\}$", job):
        return False
    if not re.search(r"(?m)^    strategy:\n", job):
        return False
    strategy = block(job, "strategy", indent=4)
    return bool(
        re.search(r"(?m)^\s+(?:- )?runner: \[?[^\n]*macos", strategy)
        or "fromJSON(" in strategy
    )


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
PACKAGE_JOBS = {
    "pi-package": "pi",
    "codex-package": "codex",
    "claude-package": "claude",
    "hermes-package": "hermes",
}
PACKAGE_SELECTOR = "package-selection"
PACKAGE_SELECT_COMMAND = 'cargo xtask package affected --base "$BASE_SHA" --json'


def evaluate_job_condition(
    expression: str,
    *,
    event: str,
    tier: str = "",
    changes: dict[str, str] | None = None,
    result: str = "",
    outputs: dict[str, str] | None = None,
) -> bool:
    """`evaluate_condition` for a job that may also wait on other jobs: the
    package selection gives `result` and `outputs`, and every other job of
    `needs` has no result, as when the selection has not narrowed anything."""
    return bool(evaluate_needs_expression(
        expression, event=event, tier=tier, changes=changes,
        results={PACKAGE_SELECTOR: result}, outputs={PACKAGE_SELECTOR: outputs or {}},
    ))


def evaluate_condition(
    expression: str,
    *,
    event: str,
    cancelled: bool = False,
    tier: str = "",
    changes: dict[str, str] | None = None,
    result: str = "",
    outputs: dict[str, str] | None = None,
) -> bool:
    """Evaluate a job's `if:` in Python for the operators it may use.

    Only `!cancelled()`, `github.event_name`, `inputs.tier`, the `changes`
    job's `outputs.<name>`, the selector's `result` and `outputs.<name>`,
    string literals, `==`, `!=`, `&&`, `||` and parentheses are understood;
    anything else fails the translation. A missing output reads as the empty
    string and string comparison ignores case, as in GitHub Actions.
    """
    # Tier selection assumes the rehearsal producer has succeeded; lifecycle
    # outcomes are exercised separately through evaluate_needs_expression.
    python = expression.replace("needs.release-rehearsal-build.result", "'success'")
    python = python.replace("!cancelled()", "(not cancelled)")
    python = python.replace("github.event_name", "event")
    python = python.replace("inputs.tier", "tier")
    python = python.replace(f"needs.{PACKAGE_SELECTOR}.result", "result")
    python = re.sub(
        rf"needs\.{re.escape(PACKAGE_SELECTOR)}\.outputs\.([a-z_]+)",
        r"outputs.get('\1', '')",
        python,
    )
    python = re.sub(r"needs\.changes\.outputs\.([a-z_]+)", r"changes.get('\1', '')", python)
    python = python.replace("&&", " and ").replace("||", " or ")
    python = re.sub(r"'[^']*'", lambda literal: literal.group(0).lower(), python)
    leftover = re.sub(
        r"'[^']*'|\b(?:not|and|or|event|tier|result|cancelled|outputs\.get|changes\.get)\b|==|!=|[()\s,]",
        "",
        python,
    )
    if leftover:
        raise AssertionError(f"untranslated condition syntax {leftover!r} in {expression!r}")
    return bool(eval(python, {"__builtins__": {}}, {
        "event": event.lower(),
        "cancelled": cancelled,
        "tier": tier.lower(),
        "changes": {name: value.lower() for name, value in (changes or {}).items()},
        "result": result.lower(),
        "outputs": {name: value.lower() for name, value in (outputs or {}).items()},
    }))


def evaluate_package_condition(expression: str, event: str, cancelled: bool, result: str, outputs: dict[str, str]) -> bool:
    """Evaluate a package job's `if:`, which reads only the selector."""
    return evaluate_condition(expression, event=event, cancelled=cancelled, result=result, outputs=outputs)


class CiTierWorkflowTests(unittest.TestCase):
    def test_rehearsal_evidence_requires_build_success_and_stops_on_cancel(self):
        condition = job_condition("release-rehearsal")
        self.assertNotIn("always()", condition)
        self.assertIn("!cancelled()", condition)
        self.assertIn("needs.release-rehearsal-build.result == 'success'", condition)
        condition = condition.replace(SAME_REPOSITORY_PR, "'same' == 'same'")
        for build in ("success", "failure", "cancelled", "skipped"):
            for cancelled in (False, True):
                for scenarios in ("success", "failure", "cancelled", "skipped"):
                    with self.subTest(build=build, cancelled=cancelled, scenarios=scenarios):
                        self.assertEqual(
                            bool(evaluate_needs_expression(
                                condition, event="workflow_dispatch", tier="nightly",
                                cancelled=cancelled,
                                results={"release-rehearsal-build": build,
                                         "release-rehearsal-scenarios": scenarios},
                            )),
                            build == "success" and not cancelled,
                        )

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

    def test_superseded_pr_runs_cancel_and_main_runs_drain_without_cross_tier_cancellation(self):
        concurrency = block(CI, "concurrency", indent=0)
        group = re.search(r"(?m)^  group: (.+)$", concurrency)
        self.assertIsNotNone(group)
        self.assertEqual(
            group.group(1),
            "ci-${{ github.ref }}-${{ github.event_name == 'pull_request' && 'pr' "
            "|| github.event_name == 'push' && 'main' || github.run_id }}",
        )
        cancel = re.search(r"(?m)^  cancel-in-progress: \$\{\{ github\.event_name == '([a-z_]+)' \}\}$", concurrency)
        self.assertIsNotNone(cancel, "cancel-in-progress must depend on the event alone")
        # (event, cancels a run in progress): a PR push replaces the PR's run;
        # a push to main waits for the running main run to complete. Schedule
        # and dispatch runs are alone in a run_id group, so nothing cancels them.
        for event, cancels in (
            ("pull_request", True),
            ("push", False),
            ("schedule", False),
            ("workflow_dispatch", False),
        ):
            with self.subTest(event=event):
                self.assertEqual(cancel.group(1) == event, cancels)

    def test_macos_package_runs_on_macos_15_and_nightly_adds_cold_rebuild_and_relay_suite(self):
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

        # One leg on every event: macOS 15 is the tested platform, and the
        # macOS 14.0 deployment target is a build setting, not a runner.
        self.assertIn("    name: macOS package install and upgrade (macos-15)\n", package)
        self.assertIn("    runs-on: macos-15\n", package)
        self.assertNotIn("strategy:", package)
        self.assertNotIn("matrix.", package)
        self.assertNotIn("continue-on-error", package)

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
        self.assertEqual(
            discovered_macos_jobs,
            set(MACOS_PATH_OWNERSHIP) | MACOS_TIER_JOBS | set(MACOS_BUILDER_JOBS),
        )

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

    def test_called_workflows_start_macos_jobs_on_a_pull_request_only_through_their_filter(self):
        jobs = workflow_jobs()
        callers = {
            name: re.search(r"(?m)^    uses: \./\.github/workflows/([a-z0-9-]+\.yml)$", definition)
            for name, definition in jobs.items()
        }
        called = {name: match.group(1) for name, match in callers.items() if match is not None}
        discovered = {}
        for caller, workflow in called.items():
            text = (ROOT / ".github" / "workflows" / workflow).read_text()
            called_jobs = workflow_jobs(text)
            macos = {
                name
                for name, definition in called_jobs.items()
                if re.search(r"(?m)^    runs-on:.*macos", definition)
                or re.search(r"(?m)^        os: \[.*macos.*\]$", definition)
                or runner_matrix_may_use_macos(definition)
            }
            if macos:
                discovered[caller] = {"workflow": workflow, "jobs": macos}
        self.assertEqual(
            discovered,
            {
                caller: {"workflow": entry["workflow"], "jobs": entry["jobs"]}
                for caller, entry in MACOS_CALLED_JOBS.items()
            },
        )

        for caller, entry in MACOS_CALLED_JOBS.items():
            with self.subTest(caller=caller):
                condition = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", jobs[caller])
                self.assertIsNotNone(condition, f"{caller} has no explicit tier condition")
                assert condition is not None
                # A fork or Dependabot pull request never starts the release
                # rehearsal; the scenarios below are a same-repository PR.
                self.assertIn(SAME_REPOSITORY_PR, condition.group(1))
                expression = condition.group(1).replace(SAME_REPOSITORY_PR, "'same' == 'same'")
                for filter_value, expected in (("true", True), ("false", False)):
                    self.assertEqual(
                        evaluate_condition(
                            expression,
                            event="pull_request",
                            changes={entry["filter"]: filter_value},
                        ),
                        expected,
                        f"{caller} on a pull request with {entry['filter']}={filter_value}",
                    )
                # The rehearsal is nightly-tier evidence: push to main and a
                # `main` dispatch never start it, whatever the filter computed
                # against the previous commit, and the nightly tier always does.
                for event, tier, expected in (
                    ("push", "", False),
                    ("workflow_dispatch", "main", False),
                    ("schedule", "", True),
                    ("workflow_dispatch", "nightly", True),
                ):
                    for filter_value in ("true", "false"):
                        self.assertEqual(
                            evaluate_condition(
                                expression,
                                event=event,
                                tier=tier,
                                changes={entry["filter"]: filter_value},
                            ),
                            expected,
                            f"{caller} on {event} {tier} with {entry['filter']}={filter_value}",
                        )
                fork = condition.group(1).replace(SAME_REPOSITORY_PR, "'fork' == 'same'")
                self.assertFalse(evaluate_condition(
                    fork, event="pull_request", changes={entry["filter"]: "true"},
                ))

    def test_main_and_nightly_only_macos_job_has_no_pull_request_branch(self):
        jobs = workflow_jobs()
        for name in MACOS_TIER_JOBS:
            with self.subTest(job=name):
                expression = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", jobs[name])
                self.assertIsNotNone(expression, f"{name} has no explicit tier condition")
                assert expression is not None
                self.assertNotIn("pull_request", expression.group(1))
                self.assertNotIn("needs.", expression.group(1))
                for required in ("github.event_name == 'schedule'", "github.event_name == 'push'"):
                    self.assertIn(required, expression.group(1))
                for tier in ("main", "nightly"):
                    self.assertRegex(
                        expression.group(1),
                        rf"github.event_name == 'workflow_dispatch' && \(.*inputs\.tier == '{tier}'",
                    )

    def test_a_pull_request_starts_a_macos_job_only_for_its_own_filter(self):
        # Every macOS job holds one of the five concurrent hosted macOS slots,
        # so on a pull request nothing but its own paths filter may start it
        # (issue #1050): a core change selecting every runtime package, or a
        # failed selection, must not.
        selections = (
            ("success", {package: "true" for package in PACKAGE_JOBS.values()}),
            ("success", {package: "false" for package in PACKAGE_JOBS.values()}),
            ("failure", {}),
            ("skipped", {}),
        )
        jobs = workflow_jobs()
        for name, ownership in MACOS_PATH_OWNERSHIP.items():
            expression = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", jobs[name])
            self.assertIsNotNone(expression, f"{name} has no explicit tier condition")
            assert expression is not None
            other_filters = {filter_name: "true" for filter_name in filter_names()}
            other_filters[ownership["filter"]] = "false"
            for result, outputs in selections:
                with self.subTest(job=name, selection=result, outputs=outputs):
                    self.assertTrue(evaluate_job_condition(
                        expression.group(1), event="pull_request",
                        changes={ownership["filter"]: "true"}, result=result, outputs=outputs,
                    ))
                    self.assertFalse(evaluate_job_condition(
                        expression.group(1), event="pull_request",
                        changes=other_filters, result=result, outputs=outputs,
                    ))
            for event, tier in (("push", ""), ("schedule", ""), ("workflow_dispatch", "main"), ("workflow_dispatch", "nightly")):
                with self.subTest(job=name, event=event, tier=tier):
                    self.assertTrue(evaluate_job_condition(
                        expression.group(1), event=event, tier=tier, result="skipped",
                    ))
        # A job without a pull-request branch never starts on a pull request,
        # whichever filters match and whatever the package selection produced.
        for name in MACOS_TIER_JOBS:
            expression = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", jobs[name])
            assert expression is not None
            for result, outputs in selections:
                for value in ("true", "false"):
                    changes = {filter_name: value for filter_name in filter_names()}
                    with self.subTest(job=name, selection=result, outputs=outputs, filters=value):
                        self.assertFalse(evaluate_job_condition(
                            expression.group(1), event="pull_request",
                            changes=changes, result=result, outputs=outputs,
                        ))
            for event, tier in (("push", ""), ("schedule", ""), ("workflow_dispatch", "main"), ("workflow_dispatch", "nightly")):
                with self.subTest(job=name, event=event, tier=tier):
                    self.assertTrue(evaluate_job_condition(
                        expression.group(1), event=event, tier=tier, result="skipped",
                    ))


    def test_a_macos_builder_runs_exactly_when_one_of_its_consumers_runs(self):
        jobs = workflow_jobs()
        filters = sorted({MACOS_PATH_OWNERSHIP[consumer]["filter"]
                          for consumers in MACOS_BUILDER_JOBS.values()
                          for consumer in consumers})
        events = (
            ("pull_request", ""), ("push", ""), ("schedule", ""),
            ("workflow_dispatch", "main"), ("workflow_dispatch", "nightly"),
        )
        for builder, consumers in MACOS_BUILDER_JOBS.items():
            condition = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", jobs[builder])
            self.assertIsNotNone(condition, f"{builder} has no explicit tier condition")
            assert condition is not None
            consumer_conditions = {}
            for consumer in consumers:
                match = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", jobs[consumer])
                assert match is not None
                consumer_conditions[consumer] = match.group(1)
            for event, tier in events:
                for values in range(2 ** len(filters)):
                    changes = {
                        name: "true" if values >> index & 1 else "false"
                        for index, name in enumerate(filters)
                    }
                    with self.subTest(builder=builder, event=event, tier=tier, changes=changes):
                        expected = any(
                            evaluate_condition(expression, event=event, tier=tier, changes=changes)
                            for expression in consumer_conditions.values()
                        )
                        self.assertEqual(
                            evaluate_condition(condition.group(1), event=event, tier=tier, changes=changes),
                            expected,
                        )

    def test_consumers_of_the_macos_release_build_download_it_and_never_build_version_a(self):
        jobs = workflow_jobs()
        builder = jobs["macos-release-build"]
        self.assertRegex(builder, r"(?m)^    needs: changes$")
        build = job_steps(builder)["Build the daemon component"]
        self.assertIn('bindir="$(packaging/macos/build-release daemon)"', step_script(build))
        self.assertNotIn("CARGO_LOCKED_FLAG", step_script(build))
        upload = job_steps(builder)["Upload the daemon binaries"]
        self.assertIn("uses: actions/upload-artifact@", upload)
        self.assertIn(f"name: {MACOS_RELEASE_ARTIFACT}\n", upload)

        for consumer in MACOS_BUILDER_JOBS["macos-release-build"]:
            with self.subTest(job=consumer):
                definition = jobs[consumer]
                self.assertRegex(definition, r"(?m)^    needs: \[changes, macos-release-build\]$")
                steps = job_steps(definition)
                download = steps["Download the daemon binaries"]
                self.assertIn("uses: actions/download-artifact@", download)
                self.assertIn(f"name: {MACOS_RELEASE_ARTIFACT}\n", download)
                names = list(steps)
                building = {
                    name for name, step in steps.items()
                    if "packaging/macos/build-release" in step
                }
                # Version B is a different version of the tree, and the
                # nightly cold rebuild must compile to compare; nothing else
                # in a consumer compiles the release daemon.
                allowed = {
                    "macos-package": {
                        "Rebuild from a cold release directory and compare",
                        "Build and package version B",
                    },
                    "hermes-compatibility-macos": set(),
                }[consumer]
                self.assertEqual(building, allowed)
                packaged = [
                    name for name, step in steps.items()
                    if re.search(r"packaging/macos/package --(?:development|stage-release)", step)
                ]
                self.assertTrue(packaged)
                # Packaging version A reads the downloaded binaries.
                first_package = names.index(packaged[0])
                self.assertLess(names.index("Download the daemon binaries"), first_package)

        package_steps = job_steps(jobs["macos-package"])
        cold = package_steps["Rebuild from a cold release directory and compare"]
        self.assertIn("inputs.tier == 'nightly'", cold)
        self.assertNotIn("inputs.tier == 'main'", cold)
        self.assertNotIn("github.event_name == 'push'", cold)
        self.assertIn('cp "${{ steps.build.outputs.bindir }}/$binary" "$first/"', step_script(cold))
        self.assertIn("id: build\n", package_steps["Unpack the daemon binaries"])
        version_b = step_script(package_steps["Build and package version B"])
        self.assertIn('CARGO_LOCKED_FLAG="" packaging/macos/build-release daemon', version_b)

# The reusable workflows the release rehearsal runs (issue #1207). Push to main
# never runs the rehearsal, so a pull request that changes one of their inputs
# must start it through the `release_rehearsal` filter; an input outside the
# filter is caught only by the nightly tier. The scenario workflow is called
# from ci.yml itself and, in a release, from release-evidence.yml (#1259).
REHEARSAL_WORKFLOWS = ("release-build.yml", "release-evidence.yml", "release-smoke-scenarios.yml")
# Inputs the rehearsal reads that the filter deliberately leaves out, with the
# reason. Each would start the rehearsal (about 30 jobs, 7-8 on macOS) on most
# pull requests; a failure only the rehearsal sees in them surfaces in the
# nightly tier, which runs the rehearsal on every schedule.
REHEARSAL_FILTER_EXCEPTIONS = {
    "crates/cli/**": "product crate; pull-request CI builds and tests it",
    "crates/daemon/**": "product crate; pull-request CI builds and tests it",
    "crates/session-worker/**": "product crate; pull-request CI builds and tests it",
    "crates/relay/**": "product crate; pull-request CI builds and tests it",
    "docs/knowledge/**": (
        "read by `cargo xtask docs site`; `docs check` runs the same bundle build "
        "on every pull request, and the site renderer is in crates/xtask, which "
        "the filter lists"
    ),
    "README.md": "copied verbatim into every archive; the guard checks it exists",
    "LICENSE": "copied verbatim into every archive and SDK tarball; the guard checks it exists",
    "docs/features.md": "copied verbatim into every archive; the guard checks it exists",
    "docs/install.md": "copied verbatim into every archive; the guard checks it exists",
    "docs/cli.md": "copied verbatim into every archive; the guard checks it exists",
    "docs/sdk.md": "copied verbatim into every archive; the guard checks it exists",
    "docs/development.md": "copied verbatim into every archive; the guard checks it exists",
    "sdk/ts/protocol/**": "SDK package source; the SDK workspace job typechecks and lints it on every pull request",
    "sdk/ts/sdk/**": "SDK package source; the SDK workspace job typechecks and lints it on every pull request",
    "sdk/ts/testkit/**": "SDK package source; the SDK workspace job typechecks and lints it on every pull request",
}
# Inputs a rehearsal command reads that its text does not name, by the
# command that reads them. `pack-release.ts` packs every SDK workspace package,
# so its directories are added from the tree.
REHEARSAL_COMMAND_INPUTS = {
    "cargo xtask docs site": {"docs/knowledge/**"},
    "bun install --frozen-lockfile": {"package.json", "bun.lock"},
    "sdk/ts/scripts/pack-release.ts": {"LICENSE"},
}
REHEARSAL_PATH_TOKEN = re.compile(
    r"(?<![\w./$-])((?:\.github|scripts|packaging|compat|runtime-packages|sdk|crates|docs)"
    r"/[A-Za-z0-9_.*/-]*[A-Za-z0-9_*])"
)
REHEARSAL_TEST_MODULE = re.compile(r"python3 -m unittest(?: -v)? (scripts(?:\.[a-z0-9_]+)+)")
REHEARSAL_LOCAL_ACTION = re.compile(r"uses: \./(\.github/actions/[a-z0-9-]+)")
REHEARSAL_CARGO_PACKAGE = re.compile(r"\s-p\s+([A-Za-z0-9_-]+)")


def code_lines(text: str) -> list[str]:
    """Shell or YAML lines with continuations joined and comments dropped."""
    text = re.sub(r"\\\n\s*", " ", text)
    return [line for line in text.splitlines() if not line.lstrip().startswith("#")]


def release_mode_only(text: str) -> bool:
    """Whether a job or step header runs only in mode `release` (a tag)."""
    condition = re.search(r"(?m)^\s*if: \$\{\{ (.+) \}\}$", text)
    return condition is not None and re.fullmatch(
        r"inputs\.mode == 'release'(?: && .+)?", condition.group(1)
    ) is not None


def path_references(lines: list[str]) -> set[str]:
    found = set()
    for line in lines:
        found.update(match.group(1) for match in REHEARSAL_PATH_TOKEN.finditer(line))
        found.update(
            match.group(1).replace(".", "/") + ".py" for match in REHEARSAL_TEST_MODULE.finditer(line)
        )
        found.update(match.group(1) + "/**" for match in REHEARSAL_LOCAL_ACTION.finditer(line))
    return found


def glob_covers(pattern: str, path: str) -> bool:
    """Whether a paths-filter glob matches every file an input path names.

    An input is a file, or a glob (`dir/**`, `dir/*`) standing for a tree; a
    glob input is covered only by a `<dir>/**` pattern on it or an ancestor.
    """
    if "*" in path:
        base = path.split("*", 1)[0].rstrip("/")
        return pattern.endswith("/**") and (base + "/").startswith(pattern[:-2])
    regex = "".join(
        ".*" if part == "**" else "[^/]*" if part == "*" else re.escape(part)
        for part in re.split(r"(\*\*|\*)", pattern)
    )
    return re.fullmatch(regex, path) is not None


def rehearsal_inputs() -> dict[str, str]:
    """Every repository path the rehearsal reads, mapped to where it is read.

    The steps of both reusable workflows give the paths they name, the
    `python3 -m unittest` modules they run, the local actions they use and the
    crates of the `cargo -p` packages they build; steps and jobs of mode
    `release` alone are left out. One level down, the scripts those steps run
    give the paths they name, `stage-archive` its `release_files`, and the
    compatibility locks the provisioner scripts the rows run.
    """
    packages = {
        tomllib.loads(manifest.read_text())["package"]["name"]: f"crates/{manifest.parent.name}/**"
        for manifest in (ROOT / "crates").glob("*/Cargo.toml")
    }
    inputs: dict[str, str] = {}
    commands = []
    for workflow in REHEARSAL_WORKFLOWS:
        inputs[f".github/workflows/{workflow}"] = workflow
        for job_name, job in workflow_jobs((ROOT / ".github" / "workflows" / workflow).read_text()).items():
            header, *steps = re.split(r"(?m)^      - ", job)
            if release_mode_only(header):
                continue
            for step in steps:
                if release_mode_only(step):
                    continue
                lines = code_lines(step)
                commands.extend(lines)
                for path in path_references(lines):
                    inputs.setdefault(path, f"{workflow} job {job_name}")
                for line in lines:
                    if "cargo " in line:
                        for package in REHEARSAL_CARGO_PACKAGE.findall(line):
                            inputs.setdefault(packages[package], f"{workflow} job {job_name}: cargo -p {package}")
    for command, paths in REHEARSAL_COMMAND_INPUTS.items():
        if any(command in line for line in commands):
            for path in paths:
                inputs.setdefault(path, command)
    if any("sdk/ts/scripts/pack-release.ts" in line for line in commands):
        for manifest in (ROOT / "sdk" / "ts").glob("*/package.json"):
            inputs.setdefault(f"sdk/ts/{manifest.parent.name}/**", "sdk/ts/scripts/pack-release.ts")
    for path, origin in sorted(inputs.items()):
        script = ROOT / path
        if (
            "*" in path
            or not script.is_file()
            or not path.startswith(("scripts/", "packaging/"))
            or script.suffix not in ("", ".sh", ".py", ".ts")
        ):
            continue
        text = script.read_text()
        for reference in path_references(code_lines(text)):
            inputs.setdefault(reference, f"read by {path}")
        release_files = re.search(r"(?m)^release_files='([^']*)'$", text)
        if release_files:
            for reference in release_files.group(1).split():
                inputs.setdefault(reference, f"release_files of {path}")
    if "scripts/release-workflow/run-row" in inputs:
        for lock in sorted((ROOT / "compat").glob("*/compatibility-lock.json")):
            provisioner = json.loads(lock.read_text()).get("upstream", {}).get("provisioner")
            if provisioner:
                inputs.setdefault(provisioner, f"provisioner of {lock.relative_to(ROOT)}")
    return inputs


class ReleaseRehearsalFilterTests(unittest.TestCase):
    def test_the_filter_covers_every_rehearsal_input_but_the_named_exceptions(self):
        patterns = filter_paths("release_rehearsal")
        inputs = rehearsal_inputs()
        for path, origin in sorted(inputs.items()):
            with self.subTest(input=path, origin=origin):
                covered = any(glob_covers(pattern, path) for pattern in patterns)
                excepted = any(glob_covers(pattern, path) for pattern in REHEARSAL_FILTER_EXCEPTIONS)
                self.assertTrue(
                    covered or excepted,
                    f"{path} ({origin}) is a rehearsal input outside the release_rehearsal "
                    "filter: add it to the filter, or name it in REHEARSAL_FILTER_EXCEPTIONS",
                )
        for exception, reason in REHEARSAL_FILTER_EXCEPTIONS.items():
            with self.subTest(exception=exception):
                self.assertTrue(reason)
                self.assertFalse(
                    any(glob_covers(pattern, exception) for pattern in patterns),
                    f"{exception} is in the filter and needs no exception",
                )
                self.assertTrue(
                    any(glob_covers(exception, path) for path in inputs),
                    f"{exception} is no longer a rehearsal input",
                )

    def test_discovery_reaches_the_inputs_read_one_level_down(self):
        inputs = rehearsal_inputs()
        for path in (
            "scripts/provision-hermes-compat",
            "scripts/smoke-hermes-plugin-release",
            "scripts/tests/test_smoke_archive.py",
            "packaging/smoke-archive",
            "crates/xtask/**",
            "README.md",
            "sdk/ts/sdk/**",
        ):
            with self.subTest(input=path):
                self.assertIn(path, inputs)
        # Steps of mode `release` alone are not rehearsal inputs.
        self.assertNotIn("scripts/build-hermes-package", inputs)

    def test_every_named_file_input_exists(self):
        # A deleted or renamed file the rehearsal copies, such as a
        # `release_files` entry of stage-archive, fails here on the pull
        # request instead of in the nightly rehearsal.
        for path, origin in sorted(rehearsal_inputs().items()):
            if "*" in path:
                continue
            with self.subTest(input=path, origin=origin):
                self.assertTrue((ROOT / path).is_file(), f"{path} ({origin}) does not exist")


def load_test_partitions():
    """The `scripts/test-partitions` module, which has no `.py` suffix."""
    path = str(ROOT / "scripts" / "test-partitions")
    loader = importlib.machinery.SourceFileLoader("test_partitions_script", path)
    spec = importlib.util.spec_from_loader(loader.name, loader)
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


class MacosLanePackageSetTests(unittest.TestCase):
    """The macOS jobs build, lint and run exactly the package sets the runner checks."""

    @classmethod
    def setUpClass(cls):
        cls.partitions = load_test_partitions()

    @staticmethod
    def packages(command: str) -> list[str]:
        return re.findall(r"(?<= -p )[a-z][a-z0-9-]*", command)

    def test_lane_lints_and_archives_exactly_the_lane_packages(self):
        lane = workflow_jobs()["platform-contracts"]
        clippy = [line for line in lane.splitlines() if "cargo clippy --locked -p" in line]
        self.assertEqual(len(clippy), 1, "the lane lints in one clippy step")
        archive = [line for line in lane.splitlines() if "cargo nextest archive --locked --profile macos" in line]
        self.assertEqual(len(archive), 1)
        # nextest runs no doctests; the lane runs them natively for the same set.
        doctests = [line for line in lane.splitlines() if "cargo test --locked --doc -p" in line]
        self.assertEqual(len(doctests), 1, "the lane runs its doctests in one step")
        for command in (clippy[0], archive[0], doctests[0]):
            with self.subTest(command=command):
                packages = self.packages(command)
                self.assertEqual(sorted(packages), sorted(self.partitions.LANE_PACKAGES))
                self.assertEqual(len(set(packages)), len(packages))
        self.assertNotIn("cargo check", lane)
        self.assertEqual(
            [line for line in lane.splitlines() if "cargo test" in line], doctests,
            "doctests are the only cargo test of the lane",
        )
        self.assertIn("check-macos --scope lane", lane)
        self.assertRegex(lane, r"scripts/test-partitions .* run-macos[ \n]")

    def test_tier_archive_holds_every_macos_package_and_runs_only_the_tier(self):
        tier = workflow_jobs()["macos-tier"]
        archive = [line for line in tier.splitlines() if "cargo nextest archive --locked --profile macos" in line]
        self.assertEqual(len(archive), 1)
        packages = self.packages(archive[0])
        self.assertEqual(sorted(packages), sorted(self.partitions.MACOS_PACKAGES))
        self.assertEqual(len(packages), 14)
        self.assertIn("check-macos --scope all", tier)
        self.assertIn("run-macos-tier", tier)
        self.assertNotIn("cargo test", tier)

    def test_hermes_macos_job_runs_its_tests_through_nextest(self):
        hermes = workflow_jobs()["hermes-compatibility-macos"]
        self.assertNotIn("cargo test", hermes)
        steps = job_steps(hermes)
        self.assertIn("tool: cargo-nextest", steps["Install cargo-nextest"])
        harness = steps["Test Hermes harness natively"]
        self.assertIn(
            "cargo nextest run --locked --profile macos -p xtask --lib -E 'test(hermes)'\n",
            harness,
        )
        package = steps["Drive a real pinned Hermes session through the package"]
        self.assertIn(
            "cargo nextest run --locked --profile macos -p pohunek-cli --test all"
            " -E 'test(/^hermes_package::/)' --run-ignored all --test-threads 1\n",
            package,
        )
        # Both runs write the macos profile's one report path, so each copies
        # it under its own name, which the summary and the upload read.
        for step, report in ((harness, "harness"), (package, "package")):
            with self.subTest(report=report):
                script = step_script(step)
                self.assertLess(
                    script.index("rm -f target/nextest/macos/junit.xml || exit 1\n"),
                    script.index("cargo nextest run"),
                    "a report left by an earlier run must not pass as this run's",
                )
                self.assertIn(
                    f'cp target/nextest/macos/junit.xml "$RUNNER_TEMP/hermes-junit/{report}.xml" || status=1\n',
                    step,
                )
                self.assertIn('exit "$status"\n', step)
                self.assertIn(f"$RUNNER_TEMP/hermes-junit/{report}.xml", steps["Summarize flaky tests"])
        self.assertIn("if: always()", steps["Summarize flaky tests"])
        self.assertIn("if: always()", steps["Upload Hermes macOS test reports"])
        self.assertIn("path: ${{ runner.temp }}/hermes-junit/", steps["Upload Hermes macOS test reports"])

    def test_hermes_pty_scenarios_run_one_at_a_time_under_the_macos_profile(self):
        # The scenarios serialize on an in-process mutex, which nextest's
        # process per test cannot share, so a test group stands in for it.
        self.assertIn(
            "static PTY_SCENARIOS: std::sync::Mutex<()>",
            (ROOT / "crates" / "xtask" / "src" / "hermes.rs").read_text(),
        )
        with (ROOT / ".config" / "nextest.toml").open("rb") as config:
            nextest = tomllib.load(config)
        self.assertEqual(nextest["test-groups"]["hermes-pty"], {"max-threads": 1})
        self.assertIn(
            {"filter": "package(=xtask) and test(/^hermes::tests::/)", "test-group": "hermes-pty"},
            nextest["profile"]["macos"]["overrides"],
        )

    def test_macos_lane_archive_and_tier_archive_use_the_checked_selections(self):
        self.assertEqual(
            self.partitions.MACOS_SCOPES["lane"][1],
            self.partitions.LANE_SET,
        )
        self.assertEqual(
            self.partitions.MACOS_SCOPES["all"][1],
            self.partitions.MACOS_SET,
        )


# Every event and tier a ci.yml run can have, crossed with the two filters
# that decide whether the macOS lane and the Hermes macOS job run on a PR.
MACOS_RUN_SCENARIOS = [
    {"event": event, "tier": tier, "changes": {"macos": macos, "hermes_macos": hermes}}
    for event, tier in (
        ("pull_request", ""), ("push", ""), ("schedule", ""),
        ("workflow_dispatch", "main"), ("workflow_dispatch", "nightly"),
    )
    for macos in ("true", "false")
    for hermes in ("true", "false")
]
# The lane also reads the test selection: a narrowed one may leave it out.
MACOS_RUN_SELECTIONS = [
    ("success", {"narrowed": "true", "lane": "true"}),
    ("success", {"narrowed": "true", "lane": "false"}),
    ("success", {"narrowed": "false"}),
    ("failure", {}),
]


class SameOsDuplicateTests(unittest.TestCase):
    """No test runs twice on macOS in one run, and the main tier checks every OS (#1226)."""

    @classmethod
    def setUpClass(cls):
        cls.partitions = load_test_partitions()
        cls.jobs = workflow_jobs()
        cls.hermes = job_condition("hermes-compatibility-macos")

    @staticmethod
    def holds(expression, scenario, selection):
        result, outputs = selection
        return bool(evaluate_needs_expression(
            expression, **scenario,
            results={TEST_SELECTOR: result}, outputs={TEST_SELECTOR: outputs},
        ))

    def test_the_hermes_macos_selection_is_what_the_hermes_job_runs(self):
        steps = job_steps(self.jobs["hermes-compatibility-macos"])
        self.assertIn("-p xtask --lib -E 'test(hermes)'\n", steps["Test Hermes harness natively"])
        self.assertIn(
            "-p pohunek-cli --test all -E 'test(/^hermes_package::/)' --run-ignored all --test-threads 1\n",
            steps["Drive a real pinned Hermes session through the package"],
        )
        self.assertEqual(
            self.partitions.OWNED_ELSEWHERE["hermes-macos"],
            "(package(=xtask) and kind(lib) and test(hermes)) or "
            "(package(=pohunek-cli) and binary(=all) and test(/^hermes_package::/))",
        )

    def test_the_tier_leaves_out_the_harness_and_runs_only_alongside_the_hermes_job(self):
        tier = self.jobs["macos-tier"]
        step = job_steps(tier)["Run macOS tier tests"]
        self.assertIn("run-macos-tier --skip-owned-by hermes-macos\n", step)
        condition = job_condition("macos-tier")
        for scenario in MACOS_RUN_SCENARIOS:
            for selection in MACOS_RUN_SELECTIONS:
                with self.subTest(**scenario, selection=selection):
                    if self.holds(condition, scenario, selection):
                        self.assertTrue(
                            self.holds(self.hermes, scenario, selection),
                            "the tier leaves the harness out only where the Hermes job runs it",
                        )

    def test_the_lane_leaves_out_hermes_package_exactly_when_the_hermes_job_runs(self):
        lane = self.jobs["platform-contracts"]
        step = job_steps(lane)["Run macOS lane tests"]
        skip = re.search(r"(?m)^          SKIP_OWNED_BY: \$\{\{ (.+) \}\}$", step)
        self.assertIsNotNone(skip)
        # Both the narrowed and the whole lane command pass the option.
        self.assertEqual(step.count("scripts/test-partitions "), 2)
        self.assertEqual(step.count(' ${SKIP_OWNED_BY:+--skip-owned-by "$SKIP_OWNED_BY"}\n'), 2)
        self.assertIn("&& 'hermes-macos' || ''", skip.group(1))
        lane_condition = job_condition("platform-contracts")
        ran = 0
        for scenario in MACOS_RUN_SCENARIOS:
            for selection in MACOS_RUN_SELECTIONS:
                if not self.holds(lane_condition, scenario, selection):
                    continue
                ran += 1
                with self.subTest(**scenario, selection=selection):
                    self.assertEqual(
                        self.holds(skip.group(1), scenario, selection),
                        self.holds(self.hermes, scenario, selection),
                    )
        self.assertGreater(ran, 0)

    def test_the_duplicate_check_reads_every_junit_artifact_after_its_jobs(self):
        uploads = {}
        for name, job in self.jobs.items():
            for artifact in re.findall(r"(?m)^          name: (.*junit.*)$", job):
                uploads.setdefault(name, []).append(artifact)
        # The fast-tests matrix names one artifact per shard; a full run starts all.
        shards = json.loads(re.search(r"\|\| '(\[[^']+\])'\)", self.jobs["fast-tests"]).group(1))
        artifacts = set()
        for names in uploads.values():
            for artifact in names:
                if "${{ matrix.shard }}" in artifact:
                    artifacts.update(artifact.replace("${{ matrix.shard }}", shard) for shard in shards)
                else:
                    artifacts.add(artifact)
        config = json.loads((ROOT / "scripts" / "test-duplicates.json").read_text())
        self.assertEqual(set(config["artifacts"]), artifacts)
        check = self.jobs["test-duplicates"]
        needs = set(re.findall(r"(?m)^      - ([a-z0-9-]+)$", block(check, "needs", indent=4)))
        self.assertEqual(needs, set(uploads))
        pattern = re.search(r"(?m)^          pattern: '\{([^}']+)\}'$", check)
        self.assertIsNotNone(pattern, check)
        globs = pattern.group(1).split(",")
        for artifact in artifacts:
            with self.subTest(artifact=artifact):
                self.assertTrue(any(fnmatch.fnmatchcase(artifact, glob) for glob in globs))
        # A called workflow's reports are not this job's to wait for or name.
        called = set(re.findall(r"(?m)^    uses: \./\.github/workflows/([a-z0-9-]+\.yml)$", CI))
        self.assertIn("release-evidence.yml", called)
        foreign = set()
        for workflow in called:
            text = (ROOT / ".github" / "workflows" / workflow).read_text()
            for artifact in re.findall(r"(?m)^\s+name: (.*junit.*)$", text):
                foreign.update(re.sub(r"\$\{\{[^}]*\}\}", value, artifact) for value in ("linux", "macos"))
        self.assertIn("matrix-junit-macos", foreign)
        for artifact in foreign:
            with self.subTest(foreign=artifact):
                self.assertFalse(any(fnmatch.fnmatchcase(artifact, glob) for glob in globs))
        self.assertIn('python3 scripts/test-duplicates "$RUNNER_TEMP/junit"\n', check)
        condition = job_condition("test-duplicates")
        for event, tier, runs in (
            ("pull_request", "", False), ("push", "", True), ("schedule", "", True),
            ("workflow_dispatch", "main", True), ("workflow_dispatch", "nightly", True),
        ):
            with self.subTest(event=event, tier=tier):
                self.assertEqual(evaluate_condition(condition, event=event, tier=tier), runs)
                self.assertFalse(evaluate_condition(condition, event=event, tier=tier, cancelled=True))


class HermesTestInterpreterActionTests(unittest.TestCase):
    """Both native jobs get the Hermes test interpreter from one pinned composite."""

    ACTION = ROOT / ".github" / "actions" / "hermes-test-python" / "action.yml"

    def test_both_macos_jobs_use_the_composite_and_no_inline_copy(self):
        for job in ("platform-contracts", "macos-tier"):
            with self.subTest(job=job):
                steps = job_steps(workflow_jobs()[job])
                step = steps["Require the Hermes test interpreter"]
                self.assertIn("        uses: ./.github/actions/hermes-test-python\n", step)
                others = "".join(body for name, body in steps.items() if name != "Require the Hermes test interpreter")
                self.assertNotIn("POHUNEK_PYTHON_BIN", others)
                self.assertNotIn("uv python install", others)

    def test_the_composite_installs_the_pinned_uv_and_exports_the_interpreter(self):
        action = self.ACTION.read_text()
        self.assertIn("uses: ./.github/actions/setup-uv\n", action)
        self.assertIn("enable-cache: true", action)
        self.assertIn("printf 'POHUNEK_PYTHON_BIN=%s\\n' \"$python_bin\" >> \"$GITHUB_ENV\"", action)

    def test_the_composite_uses_only_commit_pinned_actions_and_is_in_the_lane_filter(self):
        for use in re.findall(r"uses: (\S+)", self.ACTION.read_text()):
            if not use.startswith("./"):
                self.assertRegex(use, r"@[0-9a-f]{40}", use)
        self.assertIn(".github/actions/hermes-test-python/**", filter_paths("macos"))
        self.assertIn(".github/actions/hermes-test-python/**", MACOS_PATH_OWNERSHIP["platform-contracts"]["pr"])


class OutsidePullRequestEvidenceWordingTests(unittest.TestCase):
    """Every instruction that sends a maintainer to a later `main` run names the run that produces the evidence."""

    FILES = (
        "docs/development.md",
        ".agents/instructions/verification.md",
        ".agents/skills/contribute/SKILL.md",
        ".agents/skills/deliver-issue/SKILL.md",
        ".agents/skills/coordinate/SKILL.md",
        ".github/agent-workflow.json",
    )

    def test_no_place_relies_on_the_post_merge_main_run_for_the_rehearsal(self):
        for name in self.FILES:
            with self.subTest(file=name):
                text = " ".join((ROOT / name).read_text().split())
                self.assertNotRegex(text, r"relies on the post-merge .?main.? run")
                self.assertIn("later run on", text)
                self.assertIn("tier=nightly", text)
                self.assertRegex(text, r"(?i)nightly-only jobs|nightly-only")

    def test_the_fork_head_rule_stays(self):
        for name in ("docs/development.md", ".agents/instructions/verification.md", ".github/agent-workflow.json"):
            with self.subTest(file=name):
                text = " ".join((ROOT / name).read_text().split())
                self.assertIn("refs/pull/<N>/head", text)
                self.assertRegex(text, r"(?i)never dispatched|never run through|is never dispatched")


class SetupUvActionTests(unittest.TestCase):
    """uv is installed from one pinned, checksummed composite, never from a version manifest."""

    ACTION = ROOT / ".github" / "actions" / "setup-uv" / "action.yml"
    PINNED = "astral-sh/setup-uv@fac544c07dec837d0ccb6301d7b5580bf5edae39 # v8.2.0"

    def files_with_setup_uv(self):
        found = {}
        for path in [*(ROOT / ".github" / "workflows").glob("*.yml"), *(ROOT / ".github" / "actions").glob("*/action.yml")]:
            for number, line in enumerate(path.read_text().splitlines(), 1):
                if line.lstrip().startswith("uses: astral-sh/setup-uv"):
                    found.setdefault(path, []).append((number, line.strip()))
        return found

    def test_only_the_composite_uses_the_upstream_action(self):
        found = self.files_with_setup_uv()
        self.assertEqual(set(found), {self.ACTION})
        self.assertEqual([line for _, line in found[self.ACTION]], [f"uses: {self.PINNED}"])

    def test_every_install_site_uses_the_composite_and_pins_no_version_itself(self):
        sites = 0
        for path in (ROOT / ".github").rglob("*.yml"):
            text = path.read_text()
            if path == self.ACTION:
                continue
            sites += text.count("uses: ./.github/actions/setup-uv\n")
            self.assertNotIn("0.9.28", text, path)
        self.assertEqual(sites, 7)

    def test_the_composite_pins_one_version_and_a_checksum_per_target(self):
        action = self.ACTION.read_text()
        self.assertEqual(re.findall(r'UV_VERSION: "([^"]+)"', action)[:1], ["0.9.28"])
        for name in ("X86_64_LINUX", "AARCH64_LINUX", "AARCH64_DARWIN", "X86_64_DARWIN"):
            match = re.search(rf"UV_SHA256_{name}: ([0-9a-f]{{64}})\n", action)
            self.assertIsNotNone(match, name)
        self.assertEqual(len(set(re.findall(r"UV_SHA256_\w+: ([0-9a-f]{64})", action))), 4)
        self.assertRegex(action, r"version: \$\{\{ steps\.seed\.outputs\.version \}\}")

    def test_the_download_is_verified_before_extraction_and_the_installed_uv_is_required(self):
        action = self.ACTION.read_text()
        self.assertLess(action.index("| $check)"), action.index("tar -xzf"))
        self.assertIn("--proto '=https'", action)
        for limit in ("--connect-timeout", "--speed-limit", "--speed-time", "--max-time", "--retry-max-time"):
            self.assertIn(limit, action)
        self.assertIn('if [ "$UV_PATH" != "${SEEDED_DIR}/uv" ]', action)

    def test_the_composite_is_in_the_lane_filter(self):
        self.assertIn(".github/actions/setup-uv/**", filter_paths("macos"))
        self.assertIn(".github/actions/setup-uv/**", MACOS_PATH_OWNERSHIP["platform-contracts"]["pr"])


class FailFastTests(unittest.TestCase):
    """A required job that fails cancels the rest of a PR or dispatch run, and nothing else does (#1075)."""

    # The RustSec audit is advisory; the verdict reports after the cancel (it runs under `always()`).
    EXCLUDED = {"audit", "test-selection-verdict"}
    REUSABLE = {
        "upgrade-test",
        "release-rehearsal-build",
        "release-rehearsal-scenarios",
        "release-rehearsal",
        "release-gate",
    }

    @staticmethod
    def permissions(job: str) -> dict[str, str]:
        match = re.search(r"(?m)^    permissions:\n((?:      \S.*\n)+)", job)
        if match is None:
            return {}
        return dict(re.findall(r"(?m)^      ([a-z-]+): (\S+)$", match.group(1)))

    def direct_jobs(self) -> dict[str, str]:
        return {
            name: job
            for name, job in workflow_jobs().items()
            if "\n    steps:\n" in job and name not in self.EXCLUDED and not name.startswith("cancel-after-")
        }

    def test_no_workflow_level_actions_write_exists(self):
        header = CI.split("\njobs:\n", 1)[0]
        self.assertNotIn("actions:", header.split("permissions:", 1)[1].split("\n\n", 1)[0])

    def test_every_required_job_ends_with_the_step_and_its_exact_condition(self):
        direct = self.direct_jobs()
        self.assertGreater(len(direct), 30)
        for name, job in direct.items():
            with self.subTest(job=name):
                steps = list(job_steps(job).items())
                last_name, last = steps[-1]
                self.assertEqual(last_name, FAIL_FAST_STEP)
                self.assertEqual([n for n, _ in steps].count(FAIL_FAST_STEP), 1)
                self.assertIn(f"        if: {FAIL_FAST_CONDITION}\n", last)
                self.assertRegex(last, r"(?m)^        uses: \./(relocated/)?\.github/actions/cancel-run$")
                self.assertEqual(self.permissions(job).get("actions"), "write")
                self.assertNotIn("continue-on-error", job)

    def test_a_job_that_checks_out_into_relocated_uses_the_action_from_there(self):
        for name, job in self.direct_jobs().items():
            relocated = bool(re.search(r"uses: actions/checkout@\S+\n\s+with:\n(?:\s+\S.*\n)*?\s+path: relocated\n", job))
            last = list(job_steps(job).values())[-1]
            with self.subTest(job=name):
                self.assertEqual("uses: ./relocated/" in last, relocated)

    def test_the_advisory_audit_and_the_verdict_do_not_carry_the_step_or_the_scope(self):
        jobs = workflow_jobs()
        for name in self.EXCLUDED:
            with self.subTest(job=name):
                self.assertNotIn(FAIL_FAST_STEP, jobs[name])
                self.assertNotIn("actions", self.permissions(jobs[name]))

    def test_actions_write_is_granted_only_to_jobs_that_carry_the_step_or_watch_a_called_workflow(self):
        direct = set(self.direct_jobs())
        watchers = {f"cancel-after-{name}" for name in self.REUSABLE}
        for name, job in workflow_jobs().items():
            with self.subTest(job=name):
                self.assertEqual("actions" in self.permissions(job), name in direct | watchers)

    def test_each_called_workflow_has_a_watcher_that_fires_only_on_its_failure(self):
        jobs = workflow_jobs()
        called = {name for name, job in jobs.items() if re.search(r"(?m)^    uses: \./\.github/workflows/", job)}
        self.assertEqual(called, self.REUSABLE)
        for name in self.REUSABLE:
            with self.subTest(call=name):
                watcher = jobs[f"cancel-after-{name}"]
                self.assertIn(f"    needs: {name}\n", watcher)
                self.assertIn(
                    f"    if: ${{{{ needs.{name}.result == 'failure' && (github.event_name == 'pull_request' || github.event_name == 'workflow_dispatch') }}}}\n",
                    watcher,
                )
                self.assertEqual(self.permissions(watcher), {"actions": "write"})
                (step,) = job_steps(watcher).values()
                self.assertIn("        continue-on-error: true\n", step)
                self.assertIn('gh run cancel "$RUN_ID"', step)

    def test_the_condition_never_becomes_always_cancelled_or_push_and_schedule_safe(self):
        for word in ("always()", "cancelled()", "push", "schedule"):
            self.assertNotIn(word, FAIL_FAST_CONDITION)
        for name, job in workflow_jobs().items():
            if FAIL_FAST_STEP in job:
                step = job_steps(job)[FAIL_FAST_STEP]
                self.assertNotIn("always()", step, name)

    def test_the_composite_cancels_the_run_and_fails_soft(self):
        action = (ROOT / ".github" / "actions" / "cancel-run" / "action.yml").read_text()
        self.assertIn("      continue-on-error: true\n", action)
        self.assertIn('gh run cancel "$RUN_ID"', action)
        self.assertIn("::error title=Fail fast::", action)
        self.assertIn("RUN_ID: ${{ github.run_id }}", action)
        self.assertEqual(action.count("- name:"), 1)


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
            self.assertIn(
                f"      {package}_baseline_digest: ${{{{ steps.select.outputs.{package}_baseline_digest }}}}\n",
                outputs,
            )
        select = workflow_jobs()[PACKAGE_SELECTOR].split("      - name: Select the runtime packages\n", 1)[1]
        self.assertIn(f"for package in {' '.join(PACKAGE_JOBS.values())}; do", select)

    def test_the_selector_runs_the_xtask_command_on_pull_requests(self):
        selector = workflow_jobs()[PACKAGE_SELECTOR]
        self.assertEqual(self.condition(PACKAGE_SELECTOR), "github.event_name == 'pull_request'")
        self.assertIn(PACKAGE_SELECT_COMMAND, selector)
        self.assertIn("          BASE_SHA: ${{ github.event.pull_request.base.sha }}\n", selector)
        self.assertIn("set -euo pipefail", selector)
        self.assertIn("fetch-depth: 0", selector)
        # The base commit, the token and every step output reach the scripts
        # only through the environment.
        for name, step in job_steps(selector).items():
            if "        run: |\n" in step:
                with self.subTest(step=name):
                    self.assertNotIn("${{", step_script(step))
                    self.assertIn("set -euo pipefail", step_script(step))

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
            ("pull_request", False, "success", {"pi": "true", "codex": "true", "claude": "true", "hermes": "true"}, True),
            # A selection that did not succeed runs every package, whatever
            # outputs it set before failing.
            ("pull_request", False, "failure", {}, True),
            ("pull_request", False, "failure", {"pi": "false", "codex": "false", "claude": "false", "hermes": "false"}, True),
            ("pull_request", False, "skipped", {}, True),
            ("pull_request", False, "cancelled", {"pi": "false", "codex": "false", "claude": "false", "hermes": "false"}, True),
            # A successful selection with a missing or malformed output runs the package.
            ("pull_request", False, "success", {}, True),
            ("pull_request", False, "success", {"pi": "", "codex": "", "claude": "", "hermes": ""}, True),
            ("pull_request", False, "success", {"pi": "no", "codex": "0", "claude": "yes", "hermes": "n"}, True),
            # GitHub compares strings case-insensitively, so any casing of false excludes.
            ("pull_request", False, "success", {"pi": "False", "codex": "FALSE", "claude": "False", "hermes": "fAlSe"}, False),
            ("pull_request", True, "success", {"pi": "true", "codex": "true", "claude": "true", "hermes": "true"}, False),
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



def job_steps(job: str) -> dict[str, str]:
    """Map each named step of a job to its block (the `- name:` line onward)."""
    steps = re.split(r"(?m)^(?=      - )", job.split("    steps:\n", 1)[1])
    named = {}
    for step in steps:
        match = re.match(r"      - name: (.+)\n", step)
        if match:
            named[match.group(1)] = step
    return named


def step_script(step: str) -> str:
    """The `run: |` body of one step block, dedented."""
    match = re.search(r"(?m)^        run: \|\n((?: {10}[^\n]*\n|\n)+)", step)
    if match is None:
        raise AssertionError(f"step has no run block: {step!r}")
    return "".join(line[10:] if line.strip() else line for line in match.group(1).splitlines(True))


# The nextest profile the package jobs run their test binary under.
PACKAGE_PROFILE = "package"


class RuntimePackageReportTests(unittest.TestCase):
    """Package jobs run under the nextest `package` profile and keep its JUnit report."""

    def profile(self) -> dict:
        with (ROOT / ".config" / "nextest.toml").open("rb") as config:
            return tomllib.load(config)["profile"][PACKAGE_PROFILE]

    def report_path(self) -> str:
        return f"target/nextest/{PACKAGE_PROFILE}/{self.profile()['junit']['path']}"

    def test_the_profile_runs_serially_without_retries_and_writes_junit(self):
        profile = self.profile()
        # `ci` is tuned for deterministic tests; the real-upstream tests own
        # their timeout here.
        self.assertEqual(profile.get("inherits"), "default")
        self.assertEqual(profile["test-threads"], 1)
        self.assertIs(profile["fail-fast"], False)
        self.assertEqual(profile["retries"], 0)
        self.assertIn("slow-timeout", profile)
        self.assertTrue(profile["junit"]["path"])

    def test_every_real_upstream_test_timing_out_still_ends_inside_the_job_timeout(self):
        # The job timeout cancels the job before nextest writes the report, so
        # even a run where every real-upstream test hangs must end first.
        timeout = self.profile()["slow-timeout"]
        period = re.fullmatch(r"([0-9]+)s", timeout["period"])
        self.assertIsNotNone(period, timeout)
        assert period is not None
        per_test = int(period.group(1)) * timeout["terminate-after"]
        for job, package in PACKAGE_JOBS.items():
            with self.subTest(job=job):
                source = (ROOT / "crates" / "cli" / "tests" / f"{package}_package.rs").read_text()
                real_upstream = len(re.findall(r'#\[ignore = "needs a real `', source))
                self.assertGreater(real_upstream, 0)
                minutes = re.search(r"(?m)^    timeout-minutes: ([0-9]+)$", workflow_jobs()[job])
                self.assertIsNotNone(minutes)
                assert minutes is not None
                self.assertLess(real_upstream * per_test, int(minutes.group(1)) * 60)

    def test_package_jobs_run_their_binary_under_the_package_profile(self):
        for job, package in PACKAGE_JOBS.items():
            with self.subTest(job=job):
                text = workflow_jobs()[job]
                steps = job_steps(text)
                names = list(steps)
                command = (
                    f"cargo nextest run --locked --profile {PACKAGE_PROFILE} -p pohunek-cli "
                    f"--test all -E 'test(/^{package}_package::/)' --run-ignored all --test-threads 1"
                )
                runs = [name for name, step in steps.items() if command in step]
                self.assertEqual(len(runs), 1, f"{job} runs {command!r} once")
                test_step = steps[runs[0]]
                self.assertNotIn("cargo test", text)
                self.assertLess(names.index("Install cargo-nextest"), names.index(runs[0]))
                self.assertIn("tool: cargo-nextest", steps["Install cargo-nextest"])
                # A report the build cache restored never stands in for this run.
                self.assertLess(test_step.index(f"rm -f {self.report_path()}"), test_step.index(command))

    def test_package_jobs_summarize_and_upload_the_report_whatever_the_result(self):
        for job, package in PACKAGE_JOBS.items():
            with self.subTest(job=job):
                steps = job_steps(workflow_jobs()[job])
                summary = steps["Summarize flaky tests"]
                self.assertIn("        if: always()\n", summary)
                self.assertIn(f"scripts/junit-flaky-summary --label \"{package} package\" {self.report_path()}", summary)
                upload = steps["Upload the package test report"]
                self.assertIn("        if: always()\n", upload)
                self.assertIn("uses: actions/upload-artifact@", upload)
                self.assertIn(f"          name: package-junit-{package}\n", upload)
                self.assertIn(f"          path: {self.report_path()}\n", upload)
                # Distinct from the `nextest-junit-<shard>` reports of the test jobs.
                self.assertNotIn("nextest-junit-", upload)



class RuntimePackageEvidenceTests(unittest.TestCase):
    """Package jobs keep the builder evidence of the archive they test."""

    def test_package_jobs_build_with_evidence_and_upload_it_before_the_tests(self):
        for job, package in PACKAGE_JOBS.items():
            with self.subTest(job=job):
                steps = job_steps(workflow_jobs()[job])
                names = list(steps)
                build = steps["Build the package deterministically"]
                evidence_dir = f'"$RUNNER_TEMP/package-evidence-{package}"'
                self.assertIn(
                    f'cargo xtask package build runtime-packages/{package} --output "$RUNNER_TEMP/{package}.tar.zst" \\\n'
                    f"            --evidence-dir {evidence_dir}\n",
                    build,
                )
                upload = steps["Upload the package evidence"]
                self.assertIn("uses: actions/upload-artifact@", upload)
                self.assertIn(f"          name: package-evidence-{package}\n", upload)
                self.assertIn(f"          path: ${{{{ runner.temp }}}}/package-evidence-{package}\n", upload)
                self.assertIn("          if-no-files-found: error\n", upload)
                # Right after the build and with no condition of its own, so a
                # failing test run still leaves the evidence of its archive.
                self.assertNotRegex(upload, r"(?m)^        if: ")
                self.assertEqual(
                    names.index("Upload the package evidence"),
                    names.index("Build the package deterministically") + 1,
                )
                runs = [name for name, step in steps.items() if f"-E 'test(/^{package}_package::/)'" in step]
                self.assertEqual(len(runs), 1)
                self.assertLess(names.index("Upload the package evidence"), names.index(runs[0]))


# Upper bound for one simulated workflow step; each runs a few `jq` and shell
# builtins against fake `cargo` and `gh`, so a hang is a broken script.
STEP_TIMEOUT_SECONDS = 30

# The selector steps after the toolchain setup, in the order the runner
# executes them, and the step outputs each later step may read.
SELECTOR_STEPS = (
    "Classify the changed files",
    "Read the workspace version",
    "Fetch the release baseline",
    "Verify the release baseline",
    "Select the runtime packages",
)

# Verdicts of `cargo xtask package baseline --json` captured from the real
# release v0.34.2: a checkout of the tag verifies every package with the
# digests of the released archives, and a checkout whose packages changed after
# the release reports `digest_mismatch`. v0.34.2 ships no Hermes package, so
# the Hermes digest is a synthetic, well-formed stand-in for a verified one.
RELEASED_DIGESTS = {
    "claude": "sha256:bc3c11cfee1ab3807996e1053e5898438abac5c9561f2beb577aaddf3cabcbab",
    "codex": "sha256:371c19be10a35d4a424a65376769f9bc5ae13f1039c40e5f2c53336c1d256d66",
    "hermes": "sha256:" + hashlib.sha256(b"synthetic hermes release baseline").hexdigest(),
    "pi": "sha256:446451ead20ef978c19b406949eb01a7d29c9a498b4de8eb76c2b3e5e3c1fd53",
}


def verified(package: str) -> dict:
    return {"verdict": "verified", "reason": None, "digest": RELEASED_DIGESTS[package], "version_not_bumped": False}


def missing(reason: str, version_not_bumped: bool = False) -> dict:
    return {"verdict": "missing", "reason": reason, "digest": None, "version_not_bumped": version_not_bumped}


FAKE_CARGO = """#!/bin/sh
printf '%s\\n' "$*" >> "$FAKE_LOG"
case "$*" in
    "xtask package affected "*) cat "$FAKE_AFFECTED" ;;
    "xtask package baseline "*)
        [ "$FAKE_BASELINE_EXIT" = 0 ] || { echo "package baseline: unreadable checkout" >&2; exit "$FAKE_BASELINE_EXIT"; }
        cat "$FAKE_BASELINE" ;;
    *) echo "unexpected cargo $*" >&2; exit 97 ;;
esac
"""

FAKE_GH = """#!/bin/sh
printf '%s\\n' "$*" >> "$FAKE_LOG"
[ -n "$GH_TOKEN" ] || { echo "gh: no token" >&2; exit 4; }
dir=""
previous=""
for argument in "$@"; do
    [ "$previous" = "--dir" ] && dir=$argument
    previous=$argument
done
[ -d "$dir" ] || { echo "gh: no directory" >&2; exit 5; }
printf 'partial' > "$dir/runtime-catalog.json"
[ "$FAKE_GH_MODE" = fail ] && { echo "release not found" >&2; exit 1; }
# A stalled connection: `exec` keeps the sleeper the process `timeout` kills.
[ "$FAKE_GH_MODE" = hang ] && exec sleep "$FAKE_GH_HANG_SECONDS"
printf 'archive' > "$dir/pohunek-runtime-pi-0.0.0.tar.zst"
exit 0
"""


def resolve_runner_tool(name: str) -> str:
    """Resolve a tool that the workflow step runs on ubuntu-24.04 (GNU coreutils).

    Symlinks the tool into a test bin directory so it's available in the
    restricted PATH. On Linux this finds the real tool; on macOS the test
    fails with a clear message if the tool is absent.
    """
    tool_path = shutil.which(name)
    if tool_path is None:
        raise AssertionError(
            f"{name} is required: the workflow step runs it on ubuntu-24.04 (GNU coreutils)"
        )
    return tool_path


# Seconds a hanging fake `gh` sleeps, and the download limit the hang scenario
# sets: far apart, so only the limit can end the download in time.
FAKE_GH_HANG_SECONDS = 20
HANG_FETCH_TIMEOUT = "1s"


def workflow_env(name: str) -> str:
    """One value of the workflow-level `env:` block."""
    match = re.search(rf"(?m)^  {re.escape(name)}: (\S+)$", CI.split("\njobs:\n", 1)[0])
    if match is None:
        raise AssertionError(f"workflow env {name!r} not found")
    return match.group(1)


def workspace_version() -> str:
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        return tomllib.load(manifest)["workspace"]["package"]["version"]


def step_env(step: str) -> dict[str, str]:
    """The `env:` mapping of one step block, values unresolved."""
    match = re.search(r"(?m)^        env:\n((?:          [A-Z_]+: .+\n)+)", step)
    if match is None:
        return {}
    return dict(re.findall(r"(?m)^          ([A-Z_]+): (.+)$", match.group(1)))


class SelectorRun:
    """Runs the selector steps like the runner does, with fake `cargo` and `gh`."""

    def __init__(
        self,
        test: unittest.TestCase,
        affected: dict,
        baseline,
        baseline_exit: int = 0,
        gh_mode: str = "ok",
        fetch_timeout: str | None = None,
    ):
        tmp = tempfile.TemporaryDirectory()
        test.addCleanup(tmp.cleanup)
        self.home = Path(tmp.name)
        self.runner_temp = self.home / "runner-temp"
        self.runner_temp.mkdir()
        bin_dir = self.home / "bin"
        bin_dir.mkdir()
        for name, body in (("cargo", FAKE_CARGO), ("gh", FAKE_GH)):
            (bin_dir / name).write_text(body)
            (bin_dir / name).chmod(0o755)
        # Link timeout from the host so the workflow step can use it
        timeout_real = resolve_runner_tool("timeout")
        os.symlink(timeout_real, bin_dir / "timeout")
        self.log = self.home / "calls.log"
        self.log.write_text("")
        (self.home / "affected.json").write_text(json.dumps(affected))
        (self.home / "baseline.json").write_text(baseline if isinstance(baseline, str) else json.dumps(baseline))
        self.base_env = {
            "PATH": f"{bin_dir}:/usr/bin:/bin",
            "RUNNER_TEMP": str(self.runner_temp),
            "GITHUB_REPOSITORY": "zajca/pohunek",
            "FAKE_LOG": str(self.log),
            "FAKE_AFFECTED": str(self.home / "affected.json"),
            "FAKE_BASELINE": str(self.home / "baseline.json"),
            "FAKE_BASELINE_EXIT": str(baseline_exit),
            "FAKE_GH_MODE": gh_mode,
            "FAKE_GH_HANG_SECONDS": str(FAKE_GH_HANG_SECONDS),
            # The runner exports workflow-level `env:` to every step.
            "PACKAGE_BASELINE_FETCH_TIMEOUT": fetch_timeout or workflow_env("PACKAGE_BASELINE_FETCH_TIMEOUT"),
        }
        self.step_outputs: dict[str, dict[str, str]] = {}
        self.stderr = ""
        self.result = self.run()
        self.outputs = self.job_outputs()

    def resolve(self, value: str) -> str:
        literal = {
            "${{ github.token }}": "ghs_fake-token",
            "${{ github.event.pull_request.base.sha }}": "0123456789abcdef0123456789abcdef01234567",
        }
        if value in literal:
            return literal[value]
        match = re.fullmatch(r"\$\{\{ steps\.([a-z]+)\.outputs\.([a-z_]+) \}\}", value)
        if match is None:
            raise AssertionError(f"unsupported step environment value {value!r}")
        return self.step_outputs.get(match.group(1), {}).get(match.group(2), "")

    def run(self) -> str:
        selector = workflow_jobs()[PACKAGE_SELECTOR]
        steps = job_steps(selector)
        names = [name for name in steps if name != FAIL_FAST_STEP]
        if tuple(names[-len(SELECTOR_STEPS):]) != SELECTOR_STEPS:
            raise AssertionError(f"selector steps changed: {names}")
        for name in SELECTOR_STEPS:
            step = steps[name]
            output = self.home / f"output-{len(self.step_outputs)}-{name.replace(' ', '-')}"
            output.write_text("")
            env = dict(self.base_env, GITHUB_OUTPUT=str(output))
            env.update({key: self.resolve(value) for key, value in step_env(step).items()})
            completed = subprocess.run(
                ["bash", "-c", step_script(step)],
                env=env,
                cwd=ROOT,
                capture_output=True,
                text=True,
                timeout=STEP_TIMEOUT_SECONDS,
            )
            self.stderr += completed.stdout + completed.stderr
            step_id = re.search(r"(?m)^        id: ([a-z]+)$", step)
            if step_id:
                self.step_outputs[step_id.group(1)] = dict(
                    line.split("=", 1) for line in output.read_text().splitlines() if "=" in line
                )
            if completed.returncode != 0:
                return "failure"
        return "success"

    def job_outputs(self) -> dict[str, str]:
        block_text = block(workflow_jobs()[PACKAGE_SELECTOR], "outputs", indent=4)
        return {
            name: self.step_outputs.get(step, {}).get(key, "")
            for name, step, key in re.findall(
                r"(?m)^      ([a-z_]+): \$\{\{ steps\.([a-z]+)\.outputs\.([a-z_]+) \}\}$", block_text
            )
        }

    def runs(self, job: str) -> bool:
        match = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", workflow_jobs()[job])
        assert match is not None
        return evaluate_package_condition(match.group(1), "pull_request", False, self.result, self.outputs)

    def calls(self) -> list[str]:
        return self.log.read_text().splitlines()


class RuntimePackageBaselineTests(unittest.TestCase):
    """A pull request skips a package only on a signed release baseline (issue #149)."""

    def verdicts(self, packages: dict) -> dict:
        return {"version": workspace_version(), "packages": packages}

    def test_the_selector_reads_the_baseline_only_from_the_release_of_the_workspace_version(self):
        selector = workflow_jobs()[PACKAGE_SELECTOR]
        self.assertRegex(selector, r"(?m)^    permissions:\n      contents: read\n")
        for forbidden in ("actions/download-artifact", "actions/cache", "gh run download", "--run-id", "--latest"):
            with self.subTest(forbidden=forbidden):
                self.assertNotIn(forbidden, selector)
        steps = job_steps(selector)
        self.assertIn(
            'echo "version=$(scripts/release-workflow/version rehearsal "")" >> "$GITHUB_OUTPUT"',
            step_script(steps["Read the workspace version"]),
        )
        fetch = steps["Fetch the release baseline"]
        self.assertEqual(
            step_env(fetch),
            {"GH_TOKEN": "${{ github.token }}", "VERSION": "${{ steps.version.outputs.version }}"},
        )
        script = step_script(fetch)
        self.assertIn(
            'timeout "$PACKAGE_BASELINE_FETCH_TIMEOUT" gh release download "v$VERSION" --repo "$GITHUB_REPOSITORY" --dir "$dir"',
            script,
        )
        # The step holding the token runs `gh` and system tools only, never
        # checkout code: every command is from this list and no path names a
        # file of the work tree.
        allowed = {"set", "mkdir", "timeout", "echo", "rm", "fi"}
        for line in script.splitlines():
            words = line.strip().removeprefix("if ! ").split()
            if not words or words[0] == "--pattern" or re.fullmatch(r"[a-z_]+=\S+", words[0]):
                continue
            with self.subTest(line=line):
                self.assertIn(words[0], allowed)
                if words[0] == "timeout":
                    self.assertEqual(words[2], "gh")
        self.assertNotRegex(script, r"scripts/|cargo|\./|\$GITHUB_WORKSPACE|source |\beval\b")
        # No other step sees the token.
        for name, step in steps.items():
            if name != "Fetch the release baseline":
                with self.subTest(step=name):
                    self.assertNotIn("GH_TOKEN", step)
                    self.assertNotIn("github.token", step)
        # The download limit is a workflow constant, not a step timeout that
        # would turn a stalled download into a failed job.
        self.assertRegex(workflow_env("PACKAGE_BASELINE_FETCH_TIMEOUT"), r"^[0-9]+s$")
        self.assertNotIn("timeout-minutes", selector)

        run = SelectorRun(self, {"core_changed": True, "packages": []}, self.verdicts({p: verified(p) for p in RELEASED_DIGESTS}))
        self.assertEqual(run.result, "success", run.stderr)
        version = workspace_version()
        release_dir = run.runner_temp / "package-baseline"
        self.assertEqual(
            run.calls(),
            [
                "xtask package affected --base 0123456789abcdef0123456789abcdef01234567 --json",
                f"release download v{version} --repo zajca/pohunek --dir {release_dir} "
                "--pattern runtime-catalog.json --pattern attestation-*.json --pattern pohunek-runtime-*.tar.zst",
                f"xtask package baseline --catalog {release_dir}/runtime-catalog.json "
                f"--anchor packaging/runtime-catalog-anchor.json --release-dir {release_dir} "
                f"--version {version} --json",
            ],
        )
        # The assets stay outside the checkout, where they would count as a change.
        self.assertFalse(release_dir.resolve().is_relative_to(ROOT))

    def test_a_package_skips_only_when_unchanged_with_core_unchanged_and_a_verified_baseline(self):
        packages = list(PACKAGE_JOBS.values())
        combinations = [(changed, is_verified) for changed in (False, True) for is_verified in (False, True)]
        for core_changed in (False, True):
            # Each run gives every package a different (changed, verified)
            # pair; four runs give every package every pair.
            for shift in range(len(combinations)):
                state = {
                    package: combinations[(shift + index) % len(combinations)]
                    for index, package in enumerate(packages)
                }
                affected = {
                    "core_changed": core_changed,
                    "packages": [package for package, (changed, _) in state.items() if changed],
                }
                baseline = self.verdicts({
                    package: verified(package) if is_verified else missing("digest_mismatch", package != "pi")
                    for package, (_, is_verified) in state.items()
                })
                run = SelectorRun(self, affected, baseline)
                self.assertEqual(run.result, "success", run.stderr)
                for job, package in PACKAGE_JOBS.items():
                    changed, is_verified = state[package]
                    with self.subTest(core_changed=core_changed, package=package, changed=changed, verified=is_verified):
                        self.assertEqual(run.runs(job), core_changed or changed or not is_verified)
                        self.assertEqual(run.outputs[package], "true" if run.runs(job) else "false")
                        self.assertEqual(
                            run.outputs[f"{package}_baseline_digest"],
                            RELEASED_DIGESTS[package] if is_verified else "",
                        )

    def test_a_failed_download_runs_every_package_without_failing_the_job(self):
        run = SelectorRun(
            self,
            {"core_changed": False, "packages": []},
            self.verdicts({p: missing("catalog_absent") for p in RELEASED_DIGESTS}),
            gh_mode="fail",
        )
        self.assert_no_baseline(run)

    def test_a_stalled_download_is_cut_off_and_runs_every_package_without_failing_the_job(self):
        started = time.monotonic()
        run = SelectorRun(
            self,
            {"core_changed": False, "packages": []},
            self.verdicts({p: missing("catalog_absent") for p in RELEASED_DIGESTS}),
            gh_mode="hang",
            fetch_timeout=HANG_FETCH_TIMEOUT,
        )
        self.assertLess(time.monotonic() - started, FAKE_GH_HANG_SECONDS)
        self.assert_no_baseline(run)

    def assert_no_baseline(self, run: "SelectorRun") -> None:
        self.assertEqual(run.result, "success", run.stderr)
        self.assertIn("::warning::", run.stderr)
        # The partial download is removed, so the verifier sees no release.
        self.assertEqual(list((run.runner_temp / "package-baseline").iterdir()), [])
        for job, package in PACKAGE_JOBS.items():
            with self.subTest(job=job):
                self.assertTrue(run.runs(job))
                self.assertEqual(run.outputs[f"{package}_baseline_digest"], "")

    def test_an_unreadable_checkout_fails_the_selection_and_runs_every_package(self):
        run = SelectorRun(
            self,
            {"core_changed": False, "packages": []},
            "",
            baseline_exit=1,
        )
        self.assertEqual(run.result, "failure")
        self.assertNotIn("select", run.step_outputs)
        for job in PACKAGE_JOBS:
            with self.subTest(job=job):
                self.assertTrue(run.runs(job))

    def test_verdicts_the_selector_cannot_trust_run_the_package(self):
        version = workspace_version()
        unchanged = {"core_changed": False, "packages": []}
        failing = {
            "not json": "verified",
            "another release": {"version": f"{version}.1", "packages": {p: verified(p) for p in RELEASED_DIGESTS}},
            "unknown verdict": self.verdicts({**{p: verified(p) for p in RELEASED_DIGESTS}, "pi": {"verdict": "maybe"}}),
            "no packages": {"version": version},
        }
        for label, baseline in failing.items():
            with self.subTest(label=label):
                run = SelectorRun(self, unchanged, baseline)
                self.assertEqual(run.result, "failure", run.stderr)
                for job in PACKAGE_JOBS:
                    self.assertTrue(run.runs(job))
        # Verdicts that parse but prove nothing for a package select it.
        degraded = {
            "absent package": self.verdicts({p: verified(p) for p in RELEASED_DIGESTS if p != "pi"}),
            "verified without digest": self.verdicts({**{p: verified(p) for p in RELEASED_DIGESTS}, "pi": {"verdict": "verified", "digest": None}}),
            "verified with a malformed digest": self.verdicts(
                {**{p: verified(p) for p in RELEASED_DIGESTS}, "pi": {"verdict": "verified", "digest": "sha256:XYZ"}}
            ),
        }
        for label, baseline in degraded.items():
            with self.subTest(label=label):
                run = SelectorRun(self, unchanged, baseline)
                self.assertEqual(run.result, "success", run.stderr)
                self.assertTrue(run.runs("pi-package"))
                self.assertEqual(run.outputs["pi_baseline_digest"], "")
                for job in PACKAGE_JOBS:
                    if job != "pi-package":
                        self.assertFalse(run.runs(job), job)

    def test_package_conditions_read_only_the_selection_result_and_their_own_output(self):
        for job, package in PACKAGE_JOBS.items():
            with self.subTest(job=job):
                text = workflow_jobs()[job]
                condition = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", text)
                assert condition is not None
                references = set(re.findall(r"needs\.[A-Za-z0-9_.-]+", condition.group(1)))
                self.assertEqual(
                    references,
                    {f"needs.{PACKAGE_SELECTOR}.result", f"needs.{PACKAGE_SELECTOR}.outputs.{package}"},
                )
                # Cache state changes cost only: no job or step condition reads it.
                for line in re.findall(r"(?m)^\s+if: (.+)$", text):
                    self.assertNotRegex(line, r"(?i)cache|hashFiles|steps\.")

    def test_package_jobs_require_the_rebuilt_archive_to_be_the_verified_baseline(self):
        for job, package in PACKAGE_JOBS.items():
            with self.subTest(job=job):
                steps = job_steps(workflow_jobs()[job])
                names = list(steps)
                match = steps["Match the release baseline"]
                # After the evidence upload, so a mismatching archive keeps its
                # evidence, and before anything is tested.
                self.assertEqual(names.index("Match the release baseline"), names.index("Upload the package evidence") + 1)
                runs = [name for name, step in steps.items() if f"-E 'test(/^{package}_package::/)'" in step]
                self.assertLess(names.index("Match the release baseline"), names.index(runs[0]))
                self.assertNotRegex(match, r"(?m)^        if: ")
                self.assertEqual(
                    step_env(match),
                    {
                        "BASELINE_DIGEST": f"${{{{ needs.{PACKAGE_SELECTOR}.result == 'success' && "
                        f"needs.{PACKAGE_SELECTOR}.outputs.{package}_baseline_digest || '' }}}}"
                    },
                )
                script = step_script(match)
                self.assertNotIn("${{", script)

                with tempfile.TemporaryDirectory() as tmp:
                    runner_temp = Path(tmp)
                    archive = runner_temp / f"{package}.tar.zst"
                    archive.write_bytes(b"rebuilt package archive bytes")
                    rebuilt = "sha256:" + hashlib.sha256(archive.read_bytes()).hexdigest()

                    bin_dir = Path(tmp) / "bin"
                    bin_dir.mkdir()
                    # Link sha256sum from the host so the workflow step can use it
                    sha256sum_real = resolve_runner_tool("sha256sum")
                    os.symlink(sha256sum_real, bin_dir / "sha256sum")

                    def check(digest: str) -> subprocess.CompletedProcess:
                        return subprocess.run(
                            ["bash", "-c", script],
                            env={"PATH": f"{bin_dir}:/usr/bin:/bin", "RUNNER_TEMP": str(runner_temp), "BASELINE_DIGEST": digest},
                            capture_output=True,
                            text=True,
                            timeout=STEP_TIMEOUT_SECONDS,
                        )

                    # No verified baseline: nothing to compare.
                    self.assertEqual(check("").returncode, 0)
                    self.assertEqual(check(rebuilt).returncode, 0)
                    mismatch = check(RELEASED_DIGESTS[package])
                    self.assertEqual(mismatch.returncode, 1)
                    self.assertIn("::error::", mismatch.stdout)
                    self.assertIn("not reproducible", mismatch.stdout)
                    self.assertIn(rebuilt, mismatch.stdout)



# The version gate (issue #894): `cargo xtask package bump` against the last
# published release at or below the workspace version.
FAKE_GH_RELEASES = """#!/bin/sh
printf '%s\\n' "$*" >> "$FAKE_LOG"
[ -n "$GH_TOKEN" ] || { echo "gh: no token" >&2; exit 4; }
[ "$FAKE_GH_LIST_MODE" = fail ] && { echo "api unavailable" >&2; exit 1; }
cat "$FAKE_TAGS"
"""

FAKE_CARGO_BUMP = """#!/bin/sh
printf '%s\\n' "$*" >> "$FAKE_LOG"
case "$*" in
    "xtask package bump "*) cat "$FAKE_BUMP"; exit "$FAKE_BUMP_EXIT" ;;
    *) echo "unexpected cargo $*" >&2; exit 97 ;;
esac
"""


def bump_job_step(name: str) -> str:
    return job_steps(workflow_jobs()["package-version-bump"])[name]


def run_bump_step(
    test: unittest.TestCase,
    name: str,
    env: dict[str, str],
    fakes: dict[str, str],
    inputs: dict[str, str],
    cwd: Path | None = None,
) -> subprocess.CompletedProcess:
    """Runs one step script of the version gate with fake tools.

    `inputs` are files written into the scratch directory, which `@HOME@` in
    any env value names. The step runs in `cwd`, the repository root unless
    given. The returned process carries `log` (the fake tools' calls) and
    `outputs` (the step's `GITHUB_OUTPUT` entries).
    """
    tmp = tempfile.TemporaryDirectory()
    test.addCleanup(tmp.cleanup)
    home = Path(tmp.name)
    bin_dir = home / "bin"
    bin_dir.mkdir()
    for tool, body in fakes.items():
        (bin_dir / tool).write_text(body)
        (bin_dir / tool).chmod(0o755)
    # The steps run jq and sort from the runner image; link them into the test bin.
    for tool in ("jq", "sort", "tail"):
        os.symlink(resolve_runner_tool(tool), bin_dir / tool)
    runner_temp = home / "runner-temp"
    runner_temp.mkdir()
    output = home / "github-output"
    output.write_text("")
    log = home / "calls.log"
    log.write_text("")
    for file, text in inputs.items():
        (home / file).write_text(text)
    completed = subprocess.run(
        ["bash", "-c", step_script(bump_job_step(name))],
        env={
            "PATH": f"{bin_dir}:/usr/bin:/bin",
            "RUNNER_TEMP": str(runner_temp),
            "GITHUB_OUTPUT": str(output),
            "GITHUB_REPOSITORY": "zajca/pohunek",
            "PACKAGE_BUMP_RELEASE_LIMIT": workflow_env("PACKAGE_BUMP_RELEASE_LIMIT"),
            "PACKAGE_BASELINE_FETCH_TIMEOUT": workflow_env("PACKAGE_BASELINE_FETCH_TIMEOUT"),
            "FAKE_LOG": str(log),
            **{key: value.replace("@HOME@", str(home)) for key, value in env.items()},
        },
        cwd=cwd or ROOT,
        capture_output=True,
        text=True,
        timeout=STEP_TIMEOUT_SECONDS,
    )
    completed.log = log.read_text()  # type: ignore[attr-defined]
    completed.outputs = dict(  # type: ignore[attr-defined]
        line.split("=", 1) for line in output.read_text().splitlines() if "=" in line
    )
    return completed


class RuntimePackageVersionBumpTests(unittest.TestCase):
    """The version gate compares against the last published release (issue #894)."""

    JOB = "package-version-bump"

    def test_the_gate_runs_on_pull_requests_and_no_other_job_needs_it(self):
        jobs = workflow_jobs()
        condition = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", jobs[self.JOB])
        self.assertIsNotNone(condition)
        assert condition is not None
        self.assertEqual(condition.group(1), "github.event_name == 'pull_request'")
        self.assertIn("    name: package version bump\n", jobs[self.JOB])
        self.assertIn("      contents: read\n", jobs[self.JOB])
        for name, body in jobs.items():
            if name != self.JOB:
                with self.subTest(job=name):
                    self.assertNotRegex(body, rf"(?m)^    needs: .*{self.JOB}")

    def test_only_the_release_listing_and_download_read_the_token(self):
        steps = job_steps(workflow_jobs()[self.JOB])
        with_token = {name for name, step in steps.items() if "GH_TOKEN" in step}
        self.assertEqual(with_token, {"Select the last released version", "Fetch the released baseline"})
        for name, step in steps.items():
            if "        run: |\n" in step:
                with self.subTest(step=name):
                    self.assertNotIn("${{", step_script(step))
                    self.assertIn("set -euo pipefail", step_script(step))

    def test_the_gate_checks_the_released_catalog_with_the_xtask_command(self):
        script = re.sub(r"\s*\\\n\s*", " ", step_script(bump_job_step("Check the version bump")))
        self.assertIn(
            'cargo xtask package bump --catalog "$RELEASE_DIR/runtime-catalog.json" '
            '--anchor packaging/runtime-catalog-anchor.json --release-dir "$RELEASE_DIR" '
            '--version "$VERSION" --json > "$RUNNER_TEMP/package-bump.json"',
            script,
        )
        self.assertRegex(workflow_env("PACKAGE_BUMP_RELEASE_LIMIT"), r"^[1-9][0-9]*$")

    def test_the_last_release_at_or_below_the_workspace_version_is_selected(self):
        # (workspace version, tags gh lists, expected released version)
        scenarios = (
            ("0.34.2", "v0.34.2\nv0.34.1\nv0.33.9\n", "0.34.2"),
            # Release preparation: the workspace version is not released yet.
            ("0.34.3", "v0.34.2\nv0.34.1\n", "0.34.2"),
            # Versions compare numerically, not as text.
            ("0.34.9", "v0.34.10\nv0.34.2\n", "0.34.2"),
            ("0.34.2", "v0.34.10\nv0.34.9\nv0.34.2\n", "0.34.2"),
            # Only plain vX.Y.Z tags count.
            ("0.34.2", "v0.35.0\nv0.34.2-rc.1\nnightly\nv0.34.1\n", "0.34.1"),
            ("0.34.2", "v0.35.0\n", ""),
            ("0.34.2", "", ""),
        )
        for version, tags, expected in scenarios:
            with self.subTest(version=version, tags=tags):
                completed = run_bump_step(
                    self,
                    "Select the last released version",
                    {"GH_TOKEN": "ghs_fake-token", "VERSION": version, "FAKE_TAGS": "@HOME@/tags.txt"},
                    {"gh": FAKE_GH_RELEASES},
                    {"tags.txt": tags},
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertEqual(completed.outputs, {"version": expected})
                self.assertIn("--exclude-drafts --exclude-pre-releases --limit 200 --json tagName", completed.log)

    def test_the_workspace_version_is_read_for_the_gate(self):
        completed = run_bump_step(self, "Read the workspace version", {}, {}, {})
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.outputs, {"version": workspace_version()})

    def test_a_failing_version_script_fails_the_step_instead_of_skipping_the_gate(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        script = Path(tmp.name) / "scripts" / "release-workflow" / "version"
        script.parent.mkdir(parents=True)
        script.write_text("#!/bin/sh\necho 'unsupported workspace version' >&2\nexit 1\n")
        script.chmod(0o755)
        completed = run_bump_step(self, "Read the workspace version", {}, {}, {}, cwd=Path(tmp.name))
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(completed.outputs, {})

    def test_an_unlisted_release_leaves_the_gate_undecided_with_a_warning(self):
        completed = run_bump_step(
            self,
            "Select the last released version",
            {
                "GH_TOKEN": "ghs_fake-token",
                "VERSION": "0.34.2",
                "FAKE_TAGS": "@HOME@/tags.txt",
                "FAKE_GH_LIST_MODE": "fail",
            },
            {"gh": FAKE_GH_RELEASES},
            {"tags.txt": "v0.34.2\n"},
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.outputs, {"version": ""})
        self.assertIn("could not be listed; the version bump gate is undecided", completed.stdout)

    def test_the_gate_fails_on_a_failing_verdict_and_warns_on_an_undecided_one(self):
        for verdict in ("pass", "undecided", "fail"):
            with self.subTest(verdict=verdict):
                exit_code = "2" if verdict == "fail" else "0"
                report = {"version": "0.34.2", "verdict": verdict, "packages": {}}
                completed = run_bump_step(
                    self,
                    "Check the version bump",
                    {
                        "VERSION": "0.34.2",
                        "RELEASE_DIR": "@HOME@/release",
                        "FAKE_BUMP": "@HOME@/bump.json",
                        "FAKE_BUMP_EXIT": exit_code,
                    },
                    {"cargo": FAKE_CARGO_BUMP},
                    {"bump.json": json.dumps(report)},
                )
                self.assertEqual(completed.returncode, int(exit_code), completed.stderr)
                self.assertIn("xtask package bump --catalog", completed.log)
                self.assertEqual("::warning::" in completed.stdout, verdict == "undecided")

    def test_no_published_release_skips_the_check_with_a_warning(self):
        completed = run_bump_step(
            self,
            "Check the version bump",
            {"VERSION": "", "RELEASE_DIR": "", "FAKE_BUMP": "@HOME@/bump.json", "FAKE_BUMP_EXIT": "0"},
            {"cargo": FAKE_CARGO_BUMP},
            {"bump.json": "{}"},
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.log, "")
        self.assertIn("no published release at or below the workspace version", completed.stdout)


TEST_SELECTOR = "test-selection"
TEST_CHECK = "test-selection-check"
TEST_VERDICT = "test-selection-verdict"
# Each narrowed test job, the plan output that skips it, and that output's
# value in a plan that skips it.
NARROWED_TEST_JOBS = {
    "fast-tests": ("fast_shards", "[]"),
    "integration": ("heavy", "false"),
    "integration-relay": ("relay_db", "false"),
}
EVERY_FAST_SHARD = ["core", "daemon", "relay", "cli"]
# The condition under which a job may read the selection and narrow on it.
NARROWED = (
    "github.event_name == 'pull_request' && needs.test-selection.result == 'success' "
    "&& needs.test-selection.outputs.narrowed == 'true'"
)
# A plan output set by a narrowed selection.
NARROWED_PLAN = {"narrowed": "true", "mode": "packages"}
# The jobs the docs-only path of a pull request skips, and the outputs of a
# selection that takes it.
DOCS_ONLY_JOBS = ("test", "darwin-check", "doctests", "build-bins", "sdk", "audit", "shear")
DOCS_ONLY_PLAN = {"narrowed": "true", "mode": "none", "docs_only": "true"}


def evaluate_needs_expression(
    expression: str,
    *,
    event: str,
    cancelled: bool = False,
    tier: str = "",
    changes: dict[str, str] | None = None,
    results: dict[str, str] | None = None,
    outputs: dict[str, dict[str, str]] | None = None,
    env: dict[str, str] | None = None,
    matrix: dict[str, str] | None = None,
):
    """Evaluate an expression over `needs.<job>.result` and `.outputs.<name>`.

    Understands `!cancelled()`, `always()`, `fromJSON`, `github.event_name`,
    `inputs.tier`, `env.<NAME>`, `matrix.<name>`, `needs.<job>.result` and
    `needs.<job>.outputs.<name>`, string and integer literals, `==`, `!=`, `!`,
    `&&`, `||` and parentheses; anything else fails the translation. `&&` and
    `||` return an operand, as in GitHub Actions, so a value expression
    evaluates to its value. A missing result or output reads as the empty
    string, and strings compare ignoring case.
    """
    python = expression.replace("!cancelled()", "(not cancelled)")
    python = python.replace("always()", "(True)")
    python = python.replace("fromJSON(", "fromjson(")
    python = python.replace("github.event_name", "event")
    python = python.replace("inputs.tier", "tier")
    python = re.sub(r"\benv\.([A-Z_]+)", r"envs.get('\1', '')", python)
    python = re.sub(r"\bmatrix\.([a-z_]+)", r"matrix.get('\1', '')", python)
    python = re.sub(r"needs\.changes\.outputs\.([a-z_]+)", r"changes.get('\1', '')", python)
    python = re.sub(
        r"needs\.([a-z][a-z0-9-]*)\.outputs\.([a-z_]+)",
        r"outputs.get('\1', {}).get('\2', '')",
        python,
    )
    python = re.sub(r"needs\.([a-z][a-z0-9-]*)\.result", r"results.get('\1', '')", python)
    python = python.replace("&&", " and ").replace("||", " or ")
    python = re.sub(r"!(?!=)", " not ", python)
    python = re.sub(r"'[^']*'", lambda literal: literal.group(0).lower(), python)
    leftover = re.sub(
        r"'[^']*'|\b(?:not|and|or|True|event|tier|cancelled|fromjson|changes\.get|"
        r"results\.get|outputs\.get|envs\.get|matrix\.get|get|\d+)\b|==|!=|[(){}.,\s]",
        "",
        python,
    )
    if leftover:
        raise AssertionError(f"untranslated expression syntax {leftover!r} in {expression!r}")
    return eval(python, {"__builtins__": {}}, {
        "event": event.lower(),
        "cancelled": cancelled,
        "tier": tier.lower(),
        "fromjson": json.loads,
        # Literals are lowered with the rest of the expression, keys included.
        "envs": {name.lower(): value.lower() for name, value in (env or {}).items()},
        "matrix": {name.lower(): value.lower() for name, value in (matrix or {}).items()},
        "changes": {name: value.lower() for name, value in (changes or {}).items()},
        "results": {name: value.lower() for name, value in (results or {}).items()},
        "outputs": {
            job: {name: value.lower() for name, value in values.items()}
            for job, values in (outputs or {}).items()
        },
    })


def job_needs(job: str) -> list[str]:
    """The `needs` of a job body, as a list."""
    match = re.search(r"(?m)^    needs: (?:\[(.*)\]|(\S+))$", job)
    if match is None:
        return []
    return [name.strip() for name in (match.group(1) or match.group(2)).split(",")]


def job_condition(job: str) -> str:
    match = re.search(r"(?m)^    if: \$\{\{ (.+) \}\}$", workflow_jobs()[job])
    if match is None:
        raise AssertionError(f"{job} has no condition")
    return match.group(1)


def step_condition(step: str) -> str:
    match = re.search(r"(?m)^        if: (?:\$\{\{ (.+) \}\}|(.+))$", step)
    if match is None:
        raise AssertionError(f"step has no condition: {step!r}")
    return match.group(1) or match.group(2)


def step_env_expression(step: str, name: str) -> str:
    """The `${{ }}` expression a step sets environment variable `name` to."""
    match = re.search(rf"(?m)^          {name}: \$\{{\{{ (.+) \}}\}}$", step)
    if match is None:
        raise AssertionError(f"step has no {name} expression: {step!r}")
    return match.group(1)


def ci_rules_paths() -> list[str]:
    """The paths of `CI_RULES` in `cargo xtask affected`."""
    source = (ROOT / "crates" / "xtask" / "src" / "affected" / "mod.rs").read_text()
    rules = source.split("pub(crate) const CI_RULES", 1)[1].split("];", 1)[0]
    return re.findall(r'Pattern::(?:Dir|File)\("([^"]+)"\)', rules)


class TestSelectionWiringTests(unittest.TestCase):
    """The Linux test jobs narrow only on a narrowed selection of a pull request."""

    def runs(self, job, *, event="pull_request", cancelled=False, build="success",
             selection="success", plan=None, relay_changed=True):
        return bool(evaluate_needs_expression(
            job_condition(job), event=event, cancelled=cancelled,
            changes={"relay": "true" if relay_changed else "false"},
            results={"build-tests": build, TEST_SELECTOR: selection},
            outputs={TEST_SELECTOR: plan or {}},
        ))

    def test_the_selection_runs_on_every_event_with_the_merge_base_selector(self):
        job = workflow_jobs()[TEST_SELECTOR]
        self.assertNotRegex(job, r"(?m)^    if:")
        self.assertNotRegex(job, r"(?m)^    needs:")
        depth = re.search(r"(?m)^          fetch-depth: \$\{\{ (.+) \}\}$", job)
        self.assertIsNotNone(depth, job)
        assert depth is not None
        # History for a pull request's merge base; one commit otherwise.
        for event, expected in (("pull_request", 0), ("push", 1), ("schedule", 1),
                                ("workflow_dispatch", 1)):
            with self.subTest(event=event):
                self.assertEqual(evaluate_needs_expression(depth.group(1), event=event), expected)
        steps = job_steps(job)
        select = steps["Select the tests"]
        self.assertIn("          BASE_SHA: ${{ github.event.pull_request.base.sha }}\n", select)
        script = step_script(select)
        self.assertIn('scripts/ci-test-selection --base "$BASE_SHA" --out "$out"', script)
        self.assertIn('scripts/ci-test-selection --full "a $EVENT run runs every test" --out "$out"', script)
        self.assertIn('cat "$out/outputs.env" >> "$GITHUB_OUTPUT"', script)
        self.assertIn(
            'python3 scripts/test-partitions plan --selection "$RUNNER_TEMP/test-selection/selection.json"',
            step_script(steps["Plan the test jobs"]),
        )
        names = list(steps)
        self.assertLess(names.index("Select the tests"),
                        names.index("Decide whether the selection narrows the run"))
        for name, step in steps.items():
            if "        run: |\n" in step:
                with self.subTest(step=name):
                    self.assertNotIn("${{", step_script(step))
                    self.assertIn("set -euo pipefail", step_script(step))
        outputs = block(job, "outputs", indent=4)
        for name in ("mode", "docs_only", "script_tests", "sdk_gates"):
            self.assertIn(f"      {name}: ${{{{ steps.select.outputs.{name} }}}}\n", outputs)
        for name in ("fast_shards", "heavy", "relay_db"):
            self.assertIn(f"      {name}: ${{{{ steps.plan.outputs.{name} }}}}\n", outputs)
        # Only the ci.yml step writes `narrowed`; no head script sets it.
        self.assertIn("      narrowed: ${{ steps.narrow.outputs.narrowed }}\n", outputs)

    def narrow(self, *, event="pull_request", mode="packages", change=None, base=None):
        """Run the narrowing step in a repository whose head adds `change`."""
        script = step_script(
            job_steps(workflow_jobs()[TEST_SELECTOR])["Decide whether the selection narrows the run"]
        )
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory) / "repo"
            repo.mkdir()
            env = {
                **{key: value for key, value in os.environ.items() if not key.startswith("GIT_")},
                "GIT_CONFIG_GLOBAL": os.devnull,
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_AUTHOR_NAME": "test", "GIT_AUTHOR_EMAIL": "test@example.invalid",
                "GIT_COMMITTER_NAME": "test", "GIT_COMMITTER_EMAIL": "test@example.invalid",
                # A commit starts `git maintenance run --auto --detach`, which can still be
                # writing into .git while the fixture repository is removed.
                "GIT_CONFIG_COUNT": "2",
                "GIT_CONFIG_KEY_0": "maintenance.auto", "GIT_CONFIG_VALUE_0": "false",
                "GIT_CONFIG_KEY_1": "gc.auto", "GIT_CONFIG_VALUE_1": "0",
            }

            def git(*args):
                return subprocess.run(["git", *args], cwd=repo, env=env, check=True,
                                      capture_output=True, text=True).stdout.strip()

            git("init", "--quiet", "--initial-branch=main")
            for path in ("crates/daemon/src/lib.rs", "scripts/test-partitions", ".github/workflows/ci.yml",
                         ".config/nextest.toml"):
                (repo / path).parent.mkdir(parents=True, exist_ok=True)
                (repo / path).write_text("base\n")
            git("add", "-A")
            git("commit", "--quiet", "-m", "base")
            base_sha = git("rev-parse", "HEAD")
            for path in [change or "crates/daemon/src/lib.rs"]:
                (repo / path).parent.mkdir(parents=True, exist_ok=True)
                (repo / path).write_text("head\n")
            git("add", "-A")
            git("commit", "--quiet", "-m", "head")
            output = Path(directory) / "output"
            summary = Path(directory) / "summary"
            completed = subprocess.run(
                ["bash", "-c", script], cwd=repo, capture_output=True, text=True,
                env={**env, "EVENT": event, "BASE_SHA": base or base_sha, "MODE": mode,
                     "GITHUB_OUTPUT": str(output), "GITHUB_STEP_SUMMARY": str(summary)},
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            return output.read_text(), summary.read_text()

    def test_a_selection_narrows_only_when_the_test_pipeline_equals_the_merge_base(self):
        self.assertEqual(self.narrow()[0], "narrowed=true\n")
        self.assertEqual(self.narrow(mode="none")[0], "narrowed=true\n")
        for kwargs, reason in (
            ({"event": "push"}, "a push run runs every test"),
            ({"mode": "full"}, "the selection is full"),
            ({"mode": ""}, "the selection is "),
            ({"change": "scripts/test-partitions"}, "changes the test pipeline: scripts/test-partitions"),
            ({"change": ".github/workflows/ci.yml"}, "changes the test pipeline: .github/workflows/ci.yml"),
            ({"change": ".config/nextest.toml"}, "changes the test pipeline: .config/nextest.toml"),
            ({"change": "scripts/ci-test-selection"}, "changes the test pipeline: scripts/ci-test-selection"),
            ({"change": "scripts/json.py"}, "changes the test pipeline: scripts/json.py"),
            ({"base": "0" * 40}, "has no merge base"),
        ):
            with self.subTest(**kwargs):
                output, summary = self.narrow(**kwargs)
                self.assertEqual(output, "narrowed=false\n")
                self.assertIn(reason, summary)

    def test_the_test_pipeline_holds_every_script_the_test_jobs_run(self):
        script = step_script(
            job_steps(workflow_jobs()[TEST_SELECTOR])["Decide whether the selection narrows the run"]
        )
        listed = re.search(r"pipeline=\(\n(.*?)\n\)", script, re.S)
        self.assertIsNotNone(listed, script)
        assert listed is not None
        pipeline = set(listed.group(1).split())
        # A module next to a script shadows the one it imports, so the whole
        # directory is the pipeline, and no script imports from its own one.
        self.assertIn("scripts", pipeline)
        self.assertEqual(workflow_env("PYTHONSAFEPATH"), '"1"')

        def covered(path):
            return any(path == entry or path.startswith(f"{entry}/") for entry in pipeline)

        for path in (".github", ".config/nextest.toml", *ci_rules_paths()):
            with self.subTest(rule=path):
                self.assertTrue(covered(path), path)
        jobs = workflow_jobs()
        for job in (TEST_SELECTOR, TEST_CHECK, TEST_VERDICT, *NARROWED_TEST_JOBS):
            for name, step in job_steps(jobs[job]).items():
                text = step_script(step) if "        run: |\n" in step else step
                for used in re.findall(r"\bscripts/([a-z0-9-]+)\b", text):
                    if used == "tests":
                        continue
                    with self.subTest(job=job, step=name, script=used):
                        self.assertTrue(covered(f"scripts/{used}"), used)
        # Every file scripts/test-partitions reads from the tree.
        partitions = (ROOT / "scripts" / "test-partitions").read_text()
        for read in re.findall(r'ROOT / "([^"]+)"', partitions):
            if read != "target":
                with self.subTest(read=read):
                    self.assertTrue(covered(read), read)

    def test_test_jobs_run_unless_a_narrowed_selection_excludes_them(self):
        for job, (output, skip) in NARROWED_TEST_JOBS.items():
            excluded = {output: skip, **NARROWED_PLAN}
            scenarios = (
                # (event, cancelled, build, selection, plan, runs)
                ("push", False, "success", "success", {output: "true"}, True),
                ("schedule", False, "success", "success", {}, True),
                ("push", False, "success", "failure", {}, True),
                ("pull_request", False, "success", "success", {output: "true", **NARROWED_PLAN}, True),
                # Only a pull request's narrowed selection narrows: a push or
                # a scheduled run with the same outputs runs the job.
                ("push", False, "success", "success", excluded, True),
                ("schedule", False, "success", "success", excluded, True),
                # A selection that is not narrowed runs the job, whatever it plans.
                ("pull_request", False, "success", "success", {output: skip}, True),
                ("pull_request", False, "success", "success", {output: skip, "narrowed": "false"}, True),
                # Only a successful selection narrows, whatever outputs a
                # failed or cancelled one set.
                ("pull_request", False, "success", "failure", excluded, True),
                ("pull_request", False, "success", "cancelled", excluded, True),
                ("pull_request", False, "success", "skipped", {}, True),
                # A narrowed selection without the output runs the job.
                ("pull_request", False, "success", "success", NARROWED_PLAN, True),
                ("pull_request", False, "success", "success", excluded, False),
                # A failed or skipped build never starts a test job.
                ("pull_request", False, "failure", "failure", {}, False),
                ("push", False, "skipped", "success", {}, False),
                ("pull_request", True, "success", "success", {output: "true"}, False),
            )
            for event, cancelled, build, selection, plan, runs in scenarios:
                if job == "integration-relay" and event == "schedule":
                    continue
                with self.subTest(job=job, event=event, cancelled=cancelled, build=build,
                                  selection=selection, plan=plan):
                    self.assertEqual(
                        self.runs(job, event=event, cancelled=cancelled, build=build,
                                  selection=selection, plan=plan),
                        runs,
                    )
        # The relay DB job keeps its paths filter: a selection only narrows it.
        self.assertFalse(self.runs("integration-relay", relay_changed=False,
                                   plan={"relay_db": "true", **NARROWED_PLAN}))
        self.assertTrue(self.runs("integration-relay", event="schedule", relay_changed=False,
                                  plan={"relay_db": "false", **NARROWED_PLAN}))

    def test_the_fast_shards_are_the_planned_ones_or_every_shard(self):
        matrix = block(workflow_jobs()["fast-tests"], "matrix", indent=6)
        expression = re.search(r"(?m)^        shard: \$\{\{ (.+) \}\}$", matrix)
        self.assertIsNotNone(expression, matrix)
        assert expression is not None

        def shards(selection, plan, event="pull_request"):
            return evaluate_needs_expression(
                expression.group(1), event=event,
                results={TEST_SELECTOR: selection}, outputs={TEST_SELECTOR: plan},
            )

        self.assertEqual(shards("success", {"fast_shards": '["cli"]', **NARROWED_PLAN}), ["cli"])
        self.assertEqual(shards("success", {"fast_shards": '["core", "daemon"]', **NARROWED_PLAN}),
                         ["core", "daemon"])
        for selection, plan, event in (
            ("failure", {"fast_shards": '["cli"]', **NARROWED_PLAN}, "pull_request"),
            ("skipped", {}, "pull_request"),
            ("success", {}, "pull_request"),
            ("success", {"fast_shards": "", **NARROWED_PLAN}, "pull_request"),
            ("success", {"fast_shards": '["cli"]'}, "pull_request"),
            ("success", {"fast_shards": '["cli"]', "narrowed": "false"}, "pull_request"),
            ("success", {"fast_shards": '["cli"]', **NARROWED_PLAN}, "push"),
        ):
            with self.subTest(selection=selection, plan=plan, event=event):
                self.assertEqual(shards(selection, plan, event), EVERY_FAST_SHARD)
        self.assertEqual(
            partitions_module().FAST_SHARDS, tuple(EVERY_FAST_SHARD),
            "the fallback must list every fast shard",
        )

    def test_the_selection_check_runs_on_every_narrowed_selection(self):
        job = workflow_jobs()[TEST_CHECK]
        self.assertRegex(job, r"(?m)^    needs: \[build-tests, test-selection\]$")
        self.assertIn(
            'check --selection "$RUNNER_TEMP/test-selection/selection.json"',
            job_steps(job)["Verify the planned shards hold every selected test"],
        )
        for event, build, selection, plan, runs in (
            ("pull_request", "success", "success", NARROWED_PLAN, True),
            # Whatever test jobs the selection starts.
            ("pull_request", "success", "success",
             {"fast_shards": "[]", "heavy": "false", "relay_db": "false", **NARROWED_PLAN}, True),
            ("pull_request", "success", "success", {"narrowed": "false"}, False),
            ("pull_request", "success", "failure", NARROWED_PLAN, False),
            ("push", "success", "success", NARROWED_PLAN, False),
            ("pull_request", "failure", "success", NARROWED_PLAN, False),
        ):
            with self.subTest(event=event, build=build, selection=selection, plan=plan):
                self.assertEqual(self.runs(TEST_CHECK, event=event, build=build,
                                           selection=selection, plan=plan), runs)

    def test_the_verdict_fails_a_test_job_skipped_without_a_narrowed_selection(self):
        job = workflow_jobs()[TEST_VERDICT]
        self.assertEqual(job_condition(TEST_VERDICT), "always()")
        self.assertLessEqual(
            {TEST_SELECTOR, "build-tests", TEST_CHECK, *NARROWED_TEST_JOBS}, set(job_needs(job))
        )
        condition = step_condition(
            job_steps(job)["Fail a test job that failed or was skipped without a narrowed selection"]
        )

        def fails(results, plan=None, selection="success", event="pull_request", narrowed=True):
            every = {"build-tests": "success", "fast-tests": "success", "integration": "success",
                     "integration-relay": "success", TEST_SELECTOR: selection,
                     TEST_CHECK: "success" if narrowed else "skipped"}
            outputs = {**(NARROWED_PLAN if narrowed else {}), **(plan or {})}
            return bool(evaluate_needs_expression(
                condition, event=event, results={**every, **results},
                outputs={TEST_SELECTOR: outputs},
            ))

        self.assertFalse(fails({}))
        self.assertFalse(fails({}, narrowed=False))
        self.assertFalse(fails({}, event="push", narrowed=False))
        self.assertFalse(fails({"integration-relay": "skipped"}))
        self.assertFalse(fails({"fast-tests": "skipped"}, {"fast_shards": "[]"}))
        self.assertFalse(fails({"integration": "skipped"}, {"heavy": "false"}))
        self.assertFalse(fails({"fast-tests": "skipped", "integration": "skipped",
                                "integration-relay": "skipped"},
                               {"fast_shards": "[]", "heavy": "false", "relay_db": "false"}))
        for results, plan, selection, event, narrowed in (
            ({"build-tests": "failure", "fast-tests": "skipped", "integration": "skipped"}, {},
             "success", "pull_request", True),
            ({"build-tests": "cancelled"}, {}, "success", "pull_request", True),
            ({"fast-tests": "failure"}, {}, "success", "pull_request", True),
            ({"fast-tests": "cancelled"}, {"fast_shards": "[]"}, "success", "pull_request", True),
            ({"fast-tests": "skipped"}, {"fast_shards": '["cli"]'}, "success", "pull_request", True),
            ({"fast-tests": "skipped"}, {"fast_shards": "[]"}, "failure", "pull_request", True),
            # A skip is excused only for a narrowed selection of a pull request.
            ({"fast-tests": "skipped"}, {"fast_shards": "[]"}, "success", "pull_request", False),
            ({"fast-tests": "skipped"}, {"fast_shards": "[]"}, "success", "push", True),
            ({"fast-tests": "skipped"}, {"fast_shards": "[]", "mode": "full"}, "success",
             "pull_request", True),
            ({"integration": "skipped"}, {"heavy": "false", "mode": "full"}, "success",
             "pull_request", True),
            ({"integration": "skipped"}, {"heavy": "false"}, "success", "schedule", True),
            ({"integration": "skipped"}, {"heavy": "false"}, "success", "pull_request", False),
            ({"integration": "skipped"}, {"heavy": "true"}, "success", "pull_request", True),
            ({"integration": "skipped"}, {}, "success", "pull_request", True),
            ({"integration": "skipped"}, {"heavy": "false"}, "skipped", "pull_request", True),
            ({"integration": "failure"}, {"heavy": "false"}, "success", "pull_request", True),
            ({"integration-relay": "failure"}, {}, "success", "pull_request", True),
            ({"integration-relay": "cancelled"}, {}, "success", "pull_request", True),
            # The selection check must pass on every narrowed selection.
            ({TEST_CHECK: "skipped"}, {}, "success", "pull_request", True),
            ({TEST_CHECK: "failure"}, {}, "success", "pull_request", True),
            ({TEST_CHECK: "failure"}, {}, "success", "pull_request", False),
            ({TEST_CHECK: "cancelled"}, {}, "success", "push", False),
        ):
            with self.subTest(results=results, plan=plan, selection=selection, event=event,
                              narrowed=narrowed):
                self.assertTrue(fails(results, plan, selection, event, narrowed))

    def test_the_selection_jobs_gate_main_and_nightly(self):
        config = json.loads((ROOT / "scripts" / "ci-evidence.json").read_text())
        for tier in ("main", "nightly"):
            for name in ("test selection", "test selection verdict"):
                with self.subTest(tier=tier, job=name):
                    self.assertIn(name, config["required_jobs"][tier])

    def test_the_test_jobs_read_the_selection_only_when_it_narrows(self):
        self.assertIn('\n  TEST_SELECTION_SPARSE: "false"\n', CI.split("jobs:\n", 1)[0])
        for job in NARROWED_TEST_JOBS:
            steps = job_steps(workflow_jobs()[job])
            names = list(steps)
            with self.subTest(job=job):
                download = steps["Download test selection"]
                self.assertEqual(step_condition(download), NARROWED)
                self.assertIn("          name: test-selection\n", download)
                run = names[next(i for i, name in enumerate(names)
                                 if "--selection" in steps[name] and "run \"$SHARD\"" in steps[name])]
                # The run step passes the selection only when it narrows.
                self.assertEqual(step_env_expression(steps[run], "NARROWED"), NARROWED)
                self.assertIn('if [ "$NARROWED" = true ]; then', step_script(steps[run]))
                sparse_name = "Keep only the selection's files in the tree"
                sparse = names.index(sparse_name)
                # The tree turns sparse after every step that reads the whole
                # checkout and before the tests run.
                self.assertLess(names.index("Download test selection"), sparse)
                self.assertLess(sparse, names.index(run))
                self.assertIn('git sparse-checkout set --no-cone --stdin < "$sparse"',
                              step_script(steps[sparse_name]))
                # G2 runs only behind its switch, and only on a narrowed run.
                for switch, plan, event, expected in (
                    ("false", NARROWED_PLAN, "pull_request", False),
                    ("true", NARROWED_PLAN, "pull_request", True),
                    ("true", {"narrowed": "false"}, "pull_request", False),
                    ("true", NARROWED_PLAN, "push", False),
                ):
                    self.assertEqual(
                        bool(evaluate_needs_expression(
                            step_condition(steps[sparse_name]), event=event,
                            results={TEST_SELECTOR: "success"}, outputs={TEST_SELECTOR: plan},
                            env={"TEST_SELECTION_SPARSE": switch},
                        )),
                        expected,
                        (switch, plan, event),
                    )
        heavy = job_steps(workflow_jobs()["integration"])
        order = list(heavy)
        self.assertLess(order.index("Verify exhaustive disjoint test partitions"),
                        order.index("Keep only the selection's files in the tree"))
        script_tests = step_condition(job_steps(workflow_jobs()["fast-tests"])["Check script regressions"])
        for shard, event, plan, runs in (
            ("core", "pull_request", {"script_tests": "false", **NARROWED_PLAN}, False),
            ("core", "pull_request", {"script_tests": "true", **NARROWED_PLAN}, True),
            # Only a narrowed selection skips them: the scripts the test jobs
            # run are tested whenever the pipeline changed.
            ("core", "pull_request", {"script_tests": "false"}, True),
            ("core", "pull_request", {"script_tests": "false", "narrowed": "false"}, True),
            ("core", "push", {"script_tests": "false", **NARROWED_PLAN}, True),
            ("cli", "pull_request", {"script_tests": "true", **NARROWED_PLAN}, False),
        ):
            with self.subTest(shard=shard, event=event, plan=plan):
                self.assertEqual(bool(evaluate_needs_expression(
                    script_tests, event=event, results={TEST_SELECTOR: "success"},
                    outputs={TEST_SELECTOR: plan}, matrix={"shard": shard},
                )), runs)


class DocsOnlyPathTests(unittest.TestCase):
    """A pull request whose every change is prose skips the build and lint gates."""

    def runs(self, job, *, event="pull_request", cancelled=False, selection="success",
             plan=None, bins="success"):
        return bool(evaluate_needs_expression(
            job_condition(job), event=event, cancelled=cancelled,
            results={TEST_SELECTOR: selection, "build-bins": bins},
            outputs={TEST_SELECTOR: DOCS_ONLY_PLAN if plan is None else plan},
        ))

    def test_the_gate_jobs_run_unless_the_selection_takes_the_docs_only_path(self):
        for job in DOCS_ONLY_JOBS:
            with self.subTest(job=job):
                self.assertIn(TEST_SELECTOR, job_needs(workflow_jobs()[job]))
                self.assertFalse(self.runs(job))
                self.assertFalse(self.runs(job, cancelled=True, plan={}))
                for kwargs in (
                    {"event": "push"}, {"event": "schedule"}, {"event": "workflow_dispatch"},
                    # A failed, cancelled or skipped selection runs every gate.
                    {"selection": "failure"}, {"selection": "cancelled"},
                    {"selection": "skipped"},
                    {"plan": {**DOCS_ONLY_PLAN, "narrowed": "false"}},
                    {"plan": {**DOCS_ONLY_PLAN, "mode": "packages"}},
                    {"plan": {**DOCS_ONLY_PLAN, "mode": "full"}},
                    {"plan": {**DOCS_ONLY_PLAN, "docs_only": "false"}},
                    {"plan": {}},
                ):
                    with self.subTest(**kwargs):
                        self.assertTrue(self.runs(job, **kwargs))

    def test_the_sdk_job_runs_only_on_built_binaries(self):
        self.assertIn("build-bins", job_needs(workflow_jobs()["sdk"]))
        for bins in ("failure", "cancelled", "skipped"):
            with self.subTest(bins=bins):
                self.assertFalse(self.runs("sdk", plan={}, bins=bins))
        self.assertTrue(self.runs("sdk", plan={}))

    def test_the_build_of_the_tests_and_systemd_do_not_wait_for_the_selection(self):
        # Q5 of #1218: wall clock on Rust pull requests beats runner time on
        # docs-only ones.
        for job in ("build-tests", "systemd-supervision"):
            with self.subTest(job=job):
                self.assertNotIn(TEST_SELECTOR, job_needs(workflow_jobs()[job]))

    def test_the_verdict_fails_a_gate_job_skipped_without_the_docs_only_path(self):
        job = workflow_jobs()[TEST_VERDICT]
        condition = step_condition(
            job_steps(job)["Fail a gate job that failed or was skipped without the docs-only path"]
        )

        def fails(results, plan=None, selection="success", event="pull_request"):
            every = {name: "success" for name in DOCS_ONLY_JOBS}
            return bool(evaluate_needs_expression(
                condition, event=event,
                results={**every, TEST_SELECTOR: selection, **results},
                outputs={TEST_SELECTOR: DOCS_ONLY_PLAN if plan is None else plan},
            ))

        skipped = {name: "skipped" for name in DOCS_ONLY_JOBS}
        self.assertFalse(fails({}))
        self.assertFalse(fails({}, plan={}, event="push"))
        self.assertFalse(fails(skipped))
        for name in DOCS_ONLY_JOBS:
            for results, plan, selection, event in (
                ({name: "failure"}, None, "success", "pull_request"),
                ({name: "cancelled"}, None, "success", "pull_request"),
                ({name: "skipped"}, None, "success", "push"),
                ({name: "skipped"}, None, "failure", "pull_request"),
                ({name: "skipped"}, None, "skipped", "pull_request"),
                ({name: "skipped"}, {**DOCS_ONLY_PLAN, "narrowed": "false"}, "success",
                 "pull_request"),
                ({name: "skipped"}, {**DOCS_ONLY_PLAN, "mode": "packages"}, "success",
                 "pull_request"),
                ({name: "skipped"}, {**DOCS_ONLY_PLAN, "docs_only": "false"}, "success",
                 "pull_request"),
                ({name: "skipped"}, {}, "success", "pull_request"),
            ):
                with self.subTest(job=name, results=results, plan=plan, selection=selection,
                                  event=event):
                    self.assertTrue(fails(results, plan, selection, event))

    def test_the_verdict_judges_every_job_that_waits_for_the_selection(self):
        jobs = workflow_jobs()
        verdict = set(job_needs(jobs[TEST_VERDICT]))
        waiting = {TEST_SELECTOR}
        grown = True
        while grown:
            grown = False
            for name, body in jobs.items():
                if name not in waiting and name != TEST_VERDICT and waiting & set(job_needs(body)):
                    waiting.add(name)
                    grown = True
        self.assertEqual(waiting - verdict, set())


LANE_JOB = "platform-contracts"


class LaneSelectionTests(unittest.TestCase):
    """A narrowed selection without lane tests skips the macOS lane (#1225)."""

    def runs(self, *, event="pull_request", tier="", cancelled=False, macos="true",
             selection="success", plan=None):
        return bool(evaluate_needs_expression(
            job_condition(LANE_JOB), event=event, tier=tier, cancelled=cancelled,
            changes={"macos": macos}, results={TEST_SELECTOR: selection},
            outputs={TEST_SELECTOR: {"narrowed": "true", "lane": "false"} if plan is None else plan},
        ))

    def test_the_lane_waits_for_the_selection_and_its_paths_filter(self):
        self.assertEqual(set(job_needs(workflow_jobs()[LANE_JOB])), {"changes", TEST_SELECTOR})

    def test_a_narrowed_selection_without_lane_tests_skips_the_lane(self):
        self.assertFalse(self.runs())
        self.assertFalse(self.runs(macos="false", plan={"narrowed": "true", "lane": "true"}))
        self.assertFalse(self.runs(cancelled=True, plan={}))
        for kwargs in (
            {"plan": {"narrowed": "true", "lane": "true"}},
            # A selector without the lane output, or any selection that is
            # not a narrowed one, runs the whole lane.
            {"plan": {"narrowed": "true"}},
            {"plan": {"narrowed": "false", "lane": "false"}},
            {"selection": "failure"}, {"selection": "cancelled"}, {"selection": "skipped"},
        ):
            with self.subTest(**kwargs):
                self.assertTrue(self.runs(**kwargs))

    def test_every_tier_run_starts_the_lane_whatever_the_selection(self):
        for event, tier in (("push", ""), ("schedule", ""), ("workflow_dispatch", "main"),
                            ("workflow_dispatch", "nightly")):
            for selection in ("success", "failure", "skipped"):
                with self.subTest(event=event, tier=tier, selection=selection):
                    self.assertTrue(self.runs(event=event, tier=tier, macos="",
                                              selection=selection))

    def test_the_lane_tests_read_the_selection_only_when_it_narrows(self):
        steps = job_steps(workflow_jobs()[LANE_JOB])
        self.assertEqual(
            step_condition(steps["Download test selection"]), f"!cancelled() && {NARROWED}"
        )
        run = steps["Run macOS lane tests"]
        self.assertEqual(step_env_expression(run, "NARROWED"), NARROWED)
        script = step_script(run)
        self.assertIn('if [ "$NARROWED" = true ]; then', script)
        self.assertIn('run-macos --selection "$RUNNER_TEMP/test-selection/selection.json"', script)
        # The lint and the doctests stay whole.
        for name in ("Lint macOS lane packages", "Run macOS lane doctests"):
            with self.subTest(step=name):
                self.assertNotIn("selection", steps[name])

    def test_the_knowledge_bundle_does_not_start_the_lane(self):
        self.assertNotIn("docs/knowledge/**", filter_paths("macos"))

    def test_every_lane_trigger_outside_the_crates_runs_the_whole_lane(self):
        # A non-test step of the lane (the contributor path, the APFS checks)
        # reads these inputs, so a change to one must never skip the lane:
        # each is in the test pipeline, which keeps a run from narrowing, or
        # selects every lane test in `cargo xtask affected ci`.
        script = step_script(
            job_steps(workflow_jobs()[TEST_SELECTOR])["Decide whether the selection narrows the run"]
        )
        pipeline = set(re.search(r"pipeline=\(\n(.*?)\n\)", script, re.S).group(1).split())
        # Dependency inputs and the Everything rules of `cargo xtask affected`.
        full_lane = {"Cargo.toml", "Cargo.lock", ".cargo/**", "rust-toolchain.toml"}
        for path in filter_paths("macos"):
            if path.startswith("crates/"):
                continue
            stem = path.removesuffix("/**")
            with self.subTest(path=path):
                self.assertTrue(
                    path in full_lane
                    or any(stem == entry or stem.startswith(f"{entry}/") for entry in pipeline),
                    path,
                )

    def test_the_plan_writes_the_lane_output(self):
        self.assertIn("      lane: ${{ steps.plan.outputs.lane }}\n", workflow_jobs()[TEST_SELECTOR])

    def test_the_verdict_fails_a_lane_skipped_without_a_reason(self):
        job = workflow_jobs()[TEST_VERDICT]
        self.assertLessEqual({"changes", LANE_JOB}, set(job_needs(job)))
        condition = step_condition(
            job_steps(job)["Fail a macOS lane that failed or was skipped without a reason"]
        )

        def fails(lane, event="pull_request", macos="true", changes="success",
                  selection="success", plan=None):
            return bool(evaluate_needs_expression(
                condition, event=event, changes={"macos": macos},
                results={LANE_JOB: lane, "changes": changes, TEST_SELECTOR: selection},
                outputs={TEST_SELECTOR: {"narrowed": "true", "lane": "false"}
                         if plan is None else plan},
            ))

        self.assertFalse(fails("success"))
        self.assertFalse(fails("success", event="push"))
        self.assertFalse(fails("skipped"))
        self.assertFalse(fails("skipped", macos="false", plan={}))
        self.assertFalse(fails("skipped", changes="failure", plan={}))
        for args in (
            {"lane": "failure"}, {"lane": "cancelled"},
            {"lane": "skipped", "event": "push"}, {"lane": "skipped", "event": "schedule"},
            {"lane": "skipped", "event": "workflow_dispatch"},
            # The paths filter excuses a skip on a pull request only.
            {"lane": "skipped", "event": "push", "macos": "false", "plan": {}},
            {"lane": "skipped", "event": "schedule", "changes": "failure", "plan": {}},
            {"lane": "skipped", "plan": {"narrowed": "true", "lane": "true"}},
            {"lane": "skipped", "plan": {"narrowed": "true"}},
            {"lane": "skipped", "plan": {"narrowed": "false", "lane": "false"}},
            {"lane": "skipped", "selection": "failure"},
        ):
            with self.subTest(**args):
                self.assertTrue(fails(**args))


    def test_the_verdict_fails_a_hermes_job_that_did_not_pass_what_the_lane_left_to_it(self):
        job = workflow_jobs()[TEST_VERDICT]
        self.assertLessEqual({"changes", "macos-release-build", "hermes-compatibility-macos"},
                             set(job_needs(job)))
        condition = step_condition(job_steps(job)[
            "Fail a Hermes macOS job that did not pass the tests the lane left to it"])
        lane = job_steps(workflow_jobs()[LANE_JOB])["Run macOS lane tests"]
        skip = re.search(r"(?m)^          SKIP_OWNED_BY: \$\{\{ \((.+)\) && 'hermes-macos' \|\| '' \}\}$", lane)
        self.assertIsNotNone(skip)
        # The step judges the Hermes job under exactly the lane's skip condition.
        self.assertIn(f"({skip.group(1)}) && ", condition)

        def fails(hermes, event="pull_request", hermes_macos="true"):
            return bool(evaluate_needs_expression(
                condition, event=event, changes={"hermes_macos": hermes_macos},
                results={"hermes-compatibility-macos": hermes},
            ))

        self.assertFalse(fails("success"))
        self.assertFalse(fails("success", event="push"))
        # Without Hermes changes the lane runs `hermes_package` itself.
        for result in ("skipped", "failure"):
            with self.subTest(result=result):
                self.assertFalse(fails(result, hermes_macos="false"))
        for result in ("failure", "cancelled", "skipped"):
            for event in ("pull_request", "push", "schedule", "workflow_dispatch"):
                with self.subTest(result=result, event=event):
                    self.assertTrue(fails(result, event=event))


# A step that runs tests from the nextest archive: a test-partitions action
# that starts nextest's runner, not one that only lists tests.
ARCHIVE_RUN = re.compile(r"scripts/test-partitions [^\n]*--archive-file [^\n]*\b(?:run|run-macos|run-macos-tier|audit-sparse)\b")


class ArchiveToolchainTests(unittest.TestCase):
    """Archive test jobs install the pinned toolchain before any test runs (#1256).

    A test that runs cargo would otherwise make rustup install the pinned
    toolchain mid-test, and concurrent installs race on ~/.rustup/downloads.
    """

    def test_every_job_running_archive_tests_installs_the_toolchain_first(self):
        checked = []
        for name, job in workflow_jobs().items():
            if "    steps:\n" not in job:
                continue
            steps = re.split(r"(?m)^(?=      - )", job.split("    steps:\n", 1)[1])
            runs = [index for index, step in enumerate(steps) if ARCHIVE_RUN.search(step)]
            if not runs:
                continue
            checked.append(name)
            setup = [index for index, step in enumerate(steps)
                     if re.search(r"(?m)^        uses: \./(?:relocated/)?\.github/actions/setup-rust$", step)]
            with self.subTest(job=name):
                self.assertTrue(setup, f"{name} runs archive tests without setup-rust")
                self.assertLess(min(setup), min(runs))
        self.assertTrue({"fast-tests", "integration", "integration-relay",
                         "test-selection-audit"} <= set(checked), checked)


class RunnerImageTests(unittest.TestCase):
    """The nextest archive producer and its consumers share one runner image,
    and the jobs that build the shipped binaries stay on the glibc floor (#1731).
    """

    GLIBC_FLOOR = {"release-build", "build-bins", "sdk"}
    IMAGE_EXPRESSION = re.compile(r"\$\{\{[^}]*\b(?:matrix\.(?:os|runner|image)|runner\.(?:image|name))\b")
    FLOOR_WORKFLOWS = (
        "release.yml", "release-build.yml", "release-evidence.yml", "gate-tests.yml", "upgrade.yml",
    )

    @staticmethod
    def runner(job: str) -> str:
        match = re.search(r"(?m)^    runs-on: (\S+)", job)
        if match is None:
            raise AssertionError("job has no runs-on")
        return match.group(1)

    def test_the_archive_producer_and_its_consumers_share_one_image(self):
        jobs = workflow_jobs()
        group = {name for name, job in jobs.items()
                 if re.search(r"(?m)^          name: nextest-archive$", job)}
        self.assertTrue({"build-tests", "fast-tests", "integration"} <= group, group)
        runners = {name: self.runner(jobs[name]) for name in group}
        self.assertEqual(len(set(runners.values())), 1, runners)

    def test_the_jobs_that_build_the_shipped_binaries_stay_on_the_glibc_floor(self):
        jobs = workflow_jobs()
        for name in self.GLIBC_FLOOR:
            with self.subTest(job=name):
                self.assertEqual(self.runner(jobs[name]), "ubuntu-24.04")
        for workflow in self.FLOOR_WORKFLOWS:
            text = (ROOT / ".github" / "workflows" / workflow).read_text()
            with self.subTest(workflow=workflow):
                self.assertEqual(
                    set(re.findall(r"(?m)^\s+(?:runs-on|os): (ubuntu-\S+)", text)),
                    {"ubuntu-24.04"},
                )

    @staticmethod
    def cache_uses(entries):
        """(key, image, saves) of every rust-cache step in `entries`, pairs of
        (image, text). A step without `save-if` saves on a key miss."""
        uses = []
        for image, text in entries:
            for step in text.split("uses: Swatinem/rust-cache")[1:]:
                step = re.split(r"(?m)^\s*- ", step, maxsplit=1)[0]
                key = re.search(r"(?m)^\s+shared-key: (.+?)\s*(?:#.*)?$", step)
                save = re.search(r"(?m)^\s+save-if: (.+)$", step)
                if key:
                    uses.append((key.group(1), image, save is None or save.group(1) != "false"))
        return uses

    @classmethod
    def cache_direction_violations(cls, entries):
        """Keys saved from two images, and keys restored on an older image than
        the one that saves them."""
        savers = collections.defaultdict(set)
        images = collections.defaultdict(set)
        unresolvable = set()
        for key, image, saves in cls.cache_uses(entries):
            # A key with an expression is compared as its template. An
            # expression that picks the image cannot be compared, so it is
            # refused.
            if cls.IMAGE_EXPRESSION.search(key):
                unresolvable.add(key)
            images[key].add(image)
            if saves:
                savers[key].add(image)
        return sorted(unresolvable | {
            key for key, saving in savers.items()
            if len(saving) != 1 or min(images[key]) < max(saving)
        })

    @classmethod
    def cached_jobs_on_an_expression_runner(cls, job_texts):
        """Jobs with a rust-cache step whose `runs-on` is an expression: the
        image is not known, so the cache direction cannot be compared."""
        found = []
        for name, job in job_texts.items():
            runner = re.search(r"(?m)^    runs-on: (.+?)\s*(?:#.*)?$", job)
            if runner and "${{" in runner.group(1) and "Swatinem/rust-cache" in job:
                found.append(name)
        return sorted(found)

    # Jobs whose `runs-on` is an expression and that use the rust-cache, with
    # the image each resolves to: the row matrix and the smoke matrix hold
    # `ubuntu-24.04` as their only Linux runner (checked below), macOS legs
    # carry their own `runner.os` in the key.
    RESOLVED_EXPRESSION_RUNNERS = {"release-evidence.yml:rows", "release-evidence.yml:smoke"}

    def test_a_job_with_an_expression_runner_may_not_use_the_rust_cache(self):
        workflows = ROOT / ".github" / "workflows"
        every_job = {
            f"{path.name}:{name}": job
            for path in sorted(workflows.glob("*.yml"))
            for name, job in workflow_jobs(path.read_text()).items()
        }
        self.assertEqual(
            set(self.cached_jobs_on_an_expression_runner(every_job)),
            self.RESOLVED_EXPRESSION_RUNNERS,
        )
        row_matrix = (ROOT / "scripts" / "release-workflow" / "row-matrix").read_text()
        evidence = (workflows / "release-evidence.yml").read_text()
        smoke = workflow_jobs(evidence)["smoke"]
        for source in (row_matrix, smoke):
            self.assertEqual(set(re.findall(r"ubuntu-[0-9.]+", source)), {"ubuntu-24.04"})
        self.assertEqual(self.cached_jobs_on_an_expression_runner(workflow_jobs()), [])
        cached = "    runs-on: ${{ matrix.os }}\n    steps:\n      - uses: Swatinem/rust-cache@abc\n"
        self.assertEqual(
            self.cached_jobs_on_an_expression_runner({
                "bad": cached,
                "literal": cached.replace("${{ matrix.os }}", "ubuntu-24.04"),
                "nocache": "    runs-on: ${{ matrix.os }}\n    steps: []\n",
            }),
            ["bad"],
        )
        key = "bins # the saver"
        self.assertEqual(
            self.cache_uses([("ubuntu-24.04", f"uses: Swatinem/rust-cache@abc\n        with:\n          shared-key: {key}\n")]),
            [("bins", "ubuntu-24.04", True)],
        )

    def test_a_cache_is_never_restored_on_an_older_image_than_the_one_that_saved_it(self):
        workflows = ROOT / ".github" / "workflows"
        entries = [(self.runner(job), job) for job in workflow_jobs().values()
                   if "runs-on: ubuntu" in job]
        entries += [("ubuntu-24.04", (workflows / name).read_text())
                    for name in self.FLOOR_WORKFLOWS]
        uses = self.cache_uses(entries)
        self.assertTrue({"bins", "systemd", "test-debug"} <= {key for key, _, _ in uses}, uses)
        self.assertEqual(self.cache_direction_violations(entries), [])

    def test_the_cache_direction_check_sees_a_saver_without_save_if(self):
        def step(key, save_if=""):
            return (f"      - uses: Swatinem/rust-cache@abc\n        with:\n"
                    f"          shared-key: {key}\n{save_if}")
        restore = "          save-if: false\n"
        # A key with an expression is kept whole and compared as its template.
        self.assertEqual(self.cache_direction_violations([
            ("ubuntu-26.04", step("bins-${{ matrix.part }}")),
            ("ubuntu-24.04", step("bins-${{ matrix.part }}", restore)),
        ]), ["bins-${{ matrix.part }}"])
        self.assertEqual(self.cache_direction_violations([
            ("ubuntu-24.04", step("bins-${{ matrix.part }}")),
            ("ubuntu-26.04", step("bins-${{ matrix.part }}", restore)),
        ]), [])
        # An expression that selects the image cannot be compared: refused.
        for key in ("bins-${{ matrix.os }}", "bins-${{ runner.image }}"):
            with self.subTest(key=key):
                self.assertEqual(self.cache_direction_violations([
                    ("ubuntu-24.04", step(key)),
                ]), [key])

        self.assertEqual(self.cache_direction_violations([
            ("ubuntu-26.04", step("bins")),
            ("ubuntu-24.04", step("bins", restore)),
        ]), ["bins"])
        self.assertEqual(self.cache_direction_violations([
            ("ubuntu-24.04", step("bins")),
            ("ubuntu-26.04", step("bins", restore)),
        ]), [])
        self.assertEqual(self.cache_direction_violations([
            ("ubuntu-24.04", step("bins")),
            ("ubuntu-26.04", step("bins")),
        ]), ["bins"])

    def test_no_workflow_follows_ubuntu_latest(self):
        for path in sorted((ROOT / ".github" / "workflows").glob("*.yml")):
            with self.subTest(workflow=path.name):
                self.assertIsNone(re.search(r"(?m)^[^#\n]*ubuntu-latest", path.read_text()))


AUDIT_JOB = "test-selection-audit"


class SelectionAuditTests(unittest.TestCase):
    """The G3 audit of #1218 runs each package from its own sparse tree nightly."""

    def test_the_audit_is_one_sequential_job_without_issue_creation(self):
        job = workflow_jobs()[AUDIT_JOB]
        self.assertNotIn("    strategy:\n", job)
        self.assertNotRegex(job, r"(?m)^    uses:")
        self.assertNotRegex(job, r"issues:\s*write|gh\s+issue\s+create|github-script")

    def test_audit_reports_stay_outside_the_same_os_duplicate_check(self):
        # G3 deliberately reruns the ordinary shards in a different checkout.
        # Its reports are audit evidence, not a second owner of those tests.
        job = workflow_jobs()[AUDIT_JOB]
        self.assertIn("          name: test-selection-audit\n", job)
        check = workflow_jobs()["test-duplicates"]
        self.assertNotIn(AUDIT_JOB, job_needs(check))
        pattern = re.search(r"(?m)^          pattern: '\{([^}']+)\}'$", check)
        self.assertIsNotNone(pattern)
        self.assertFalse(any(fnmatch.fnmatchcase("test-selection-audit", glob)
                             for glob in pattern.group(1).split(",")))

    def runs(self, *, event, tier="", cancelled=False, build="success"):
        return bool(evaluate_needs_expression(
            job_condition(AUDIT_JOB), event=event, tier=tier, cancelled=cancelled,
            results={"build-tests": build},
        ))

    def test_the_audit_runs_on_the_nightly_tier_after_a_built_archive(self):
        self.assertEqual(job_needs(workflow_jobs()[AUDIT_JOB]), ["build-tests"])
        self.assertTrue(self.runs(event="schedule"))
        self.assertTrue(self.runs(event="workflow_dispatch", tier="nightly"))
        for kwargs in (
            {"event": "pull_request"}, {"event": "push"},
            {"event": "workflow_dispatch", "tier": "main"},
            {"event": "schedule", "build": "failure"},
            {"event": "schedule", "build": "skipped"},
            {"event": "schedule", "cancelled": True},
        ):
            with self.subTest(**kwargs):
                self.assertFalse(self.runs(**kwargs))

    def test_the_audit_gates_the_nightly_evidence_only(self):
        config = json.loads((ROOT / "scripts" / "ci-evidence.json").read_text())
        name = "test selection audit (G3)"
        self.assertIn(f"    name: {name}\n", workflow_jobs()[AUDIT_JOB])
        self.assertIn(name, config["required_jobs"]["nightly"])
        self.assertNotIn(name, config["required_jobs"]["main"])

    def test_the_audit_runs_the_relay_database_tests_against_the_relay_service(self):
        jobs = workflow_jobs()
        self.assertEqual(block(jobs[AUDIT_JOB], "services", 4),
                         block(jobs["integration-relay"], "services", 4))
        run = job_steps(jobs[AUDIT_JOB])["Run every package from its own sparse tree"]
        self.assertIn(
            "POHUNEK_RELAY_TEST_DATABASE_URL: postgresql://postgres@127.0.0.1:"
            "${{ job.services.postgres.ports['5432'] }}/pohunek_test", run)

    def test_the_audit_runs_the_archive_with_the_selector_of_its_tree(self):
        steps = job_steps(workflow_jobs()[AUDIT_JOB])
        names = list(steps)
        build = steps["Build the selector"]
        self.assertIn("cargo build --locked -p xtask --bin xtask", step_script(build))
        self.assertIn('cp target/debug/xtask "$RUNNER_TEMP/xtask"', step_script(build))
        self.assertIn("          name: nextest-archive\n", steps["Download test archive"])
        run = steps["Run every package from its own sparse tree"]
        self.assertIn(
            'python3 scripts/test-partitions --archive-file '
            '"$RUNNER_TEMP/nextest-archive/nextest-archive.tar.zst" audit-sparse '
            '--selector "$RUNNER_TEMP/xtask" --report-dir "$RUNNER_TEMP/test-selection-audit"',
            step_script(run))
        # The selector leaves target/ before the archive extracts into it.
        self.assertLess(names.index("Build the selector"), names.index("Download test archive"))
        self.assertLess(names.index("Download test archive"),
                        names.index("Run every package from its own sparse tree"))
        for name in ("Summarize the audit", "Upload the audit reports"):
            with self.subTest(step=name):
                self.assertEqual(step_condition(steps[name]), "always()")
                self.assertGreater(names.index(name),
                                   names.index("Run every package from its own sparse tree"))

    def test_the_audit_does_not_wait_for_the_pull_request_sparse_trees(self):
        # G3 is the evidence that turns G2 on, so it runs while G2 is off.
        self.assertNotIn("TEST_SELECTION_SPARSE", workflow_jobs()[AUDIT_JOB])
        self.assertNotIn("test-selection", job_needs(workflow_jobs()[AUDIT_JOB]))


def partitions_module():
    loader = importlib.machinery.SourceFileLoader("partitions_wiring", str(ROOT / "scripts" / "test-partitions"))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


if __name__ == "__main__":
    unittest.main()
