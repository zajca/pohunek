"""Scenarios for scripts/guard-pack (stdlib only).

The real script runs in a throwaway git repository whose tree holds the real
`scripts/changelog` and stand-ins for everything else a gate calls: a private
`cargo`, `gh` and `heavy-run` (named by `$CARGO`, `$GH` and `$HEAVY_RUN`), a
`scripts/release-workflow/version` and a `scripts/test-partitions`. The stand-ins
print the shapes the CI commands print and take their outcome from the
`FAKE_*` variables of the scenario; every call is appended to `$FAKE_CALLS`.
"""

import json
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

import hermetic_git  # noqa: F401  (isolates git from the developer's configuration)

SCRIPTS = Path(__file__).resolve().parents[1]
GUARD_PACK = SCRIPTS / "guard-pack"
CHANGELOG_TOOL = SCRIPTS / "changelog"

LABEL = "no-changelog"
LAYOUT = {
    "repository": "example/app",
    "changelog": "CHANGELOG.md",
    "guard": {"label": LABEL, "triggers": ["docs/cli.md"]},
}
FRAGMENT = "---\nkind: added\n---\nText.\n"

FAKE_CARGO = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    import json, os, sys
    args = sys.argv[1:]
    with open(os.environ["FAKE_CALLS"], "a") as calls:
        calls.write("cargo " + " ".join(args) + "\\n")
    if args[:3] == ["xtask", "package", "affected"]:
        print("{}")
        sys.exit(int(os.environ.get("FAKE_AFFECTED_RC", "0")))
    if args[:3] == ["xtask", "package", "bump"]:
        print(os.environ["FAKE_BUMP_JSON"])
        sys.exit(int(os.environ.get("FAKE_BUMP_RC", "0")))
    if args[:3] == ["xtask", "docs", "check"]:
        if os.environ.get("FAKE_DOCS_RC", "0") != "0":
            sys.exit("error: docs check failed")
        print("docs check ok: all checks passed")
        sys.exit(0)
    if args[:2] == ["nextest", "archive"]:
        if os.environ.get("FAKE_ARCHIVE_RC", "0") != "0":
            sys.exit("error: could not compile the archive")
        open(args[args.index("--archive-file") + 1], "w").close()
        sys.exit(0)
    sys.exit(f"unexpected cargo call {args}")
    """
)

FAKE_GH = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    import os, sys
    args = sys.argv[1:]
    with open(os.environ["FAKE_CALLS"], "a") as calls:
        calls.write("gh " + " ".join(args) + "\\n")
    if args[:2] == ["release", "list"]:
        if os.environ.get("FAKE_GH_LIST_RC", "0") != "0":
            sys.exit("error: HTTP 502")
        print(os.environ.get("FAKE_GH_LIST", ""))
        sys.exit(0)
    if args[:2] == ["release", "download"]:
        if os.environ.get("FAKE_GH_DOWNLOAD_RC", "0") != "0":
            sys.exit("error: no assets match the file pattern")
        directory = args[args.index("--dir") + 1]
        open(os.path.join(directory, "runtime-catalog.json"), "w").write("{}")
        sys.exit(0)
    sys.exit(f"unexpected gh call {args}")
    """
)

HEAVY_RUN = textwrap.dedent(
    """\
    #!/bin/sh
    echo "heavy-run $*" >> "$FAKE_CALLS"
    exec "$@"
    """
)

VERSION = textwrap.dedent(
    """\
    #!/bin/sh
    echo "${FAKE_VERSION:-1.2.3}"
    """
)

# The partition script's scope table and a `check-macos` that fails while the
# tree holds a file named `unselected-test`, with the reason shape of the real
# `validate_macos_coverage`.
PARTITIONS = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    import os, sys
    MACOS_SCOPES = {"all": (["pkg-a", "pkg-b"], "all"), "lane": (["pkg-a"], "lane")}
    if __name__ == "__main__":
        if os.path.exists("unselected-test"):
            sys.exit(
                "error: macOS partition coverage failed: absent packages=[], empty buckets=[], "
                "missing=['pkg-a::bin tests::an_ignored_test'], extra=[], overlap={}"
            )
        print("macOS coverage OK")
    """
)


class GuardPackScenarios(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.repo = self.root / "repo"
        self.bin = self.root / "bin"
        self.calls = self.root / "calls.log"
        self.repo.mkdir()
        self.bin.mkdir()
        self.calls.write_text("")
        for name, text in (("cargo", FAKE_CARGO), ("gh", FAKE_GH), ("heavy-run", HEAVY_RUN)):
            self.executable(self.bin / name, text)
        self.write(".github/changelog.json", json.dumps(LAYOUT))
        self.write("CHANGELOG.md", "# Changelog\n\nUser-facing changes of example/app.\n")
        self.write("changelog.d/.gitkeep", "")
        self.write("docs/cli.md", "cli\n")
        self.write("src/lib.rs", "// code\n")
        self.write(".gitignore", "/target\n")
        self.write("scripts/changelog", CHANGELOG_TOOL.read_text())
        self.write("scripts/release-workflow/version", VERSION)
        self.write("scripts/test-partitions", PARTITIONS)
        for relative in ("scripts/changelog", "scripts/release-workflow/version", "scripts/test-partitions"):
            (self.repo / relative).chmod(0o755)
        self.git("init", "-q", "-b", "main")
        self.git("add", "-A")
        self.commit("initial")
        self.git("checkout", "-q", "-b", "pr")
        self.environment = dict(
            os.environ,
            CARGO=str(self.bin / "cargo"),
            GH=str(self.bin / "gh"),
            HEAVY_RUN=str(self.bin / "heavy-run"),
            FAKE_CALLS=str(self.calls),
            GUARD_PACK_PLATFORM="darwin",
            FAKE_GH_LIST="v1.2.3",
            FAKE_BUMP_JSON=json.dumps({"version": "1.2.3", "verdict": "pass", "packages": {"pi": {"verdict": "pass", "reason": None}}}),
        )

    @staticmethod
    def executable(path, text):
        path.write_text(text)
        path.chmod(0o755)

    def write(self, relative, text):
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def git(self, *args):
        # A commit must not start a detached `maintenance.auto` / `gc.auto` run that
        # outlives the fixture directory (#1251).
        subprocess.run(
            ["git", "-c", "maintenance.auto=false", "-c", "gc.auto=0", "-C", str(self.repo), *args],
            check=True, capture_output=True,
        )

    def commit(self, message):
        self.git("add", "-A")
        self.git("-c", "commit.gpgsign=false", "commit", "-q", "-m", message)

    def guard_pack(self, *extra, **environment):
        return subprocess.run(
            [sys.executable, str(GUARD_PACK), "--root", str(self.repo), "--base", "main", *extra],
            capture_output=True, text=True, check=False, env=dict(self.environment, **environment),
        )

    def gate_lines(self, result):
        """The gate lines by name, and the summary line."""
        lines = result.stdout.splitlines()
        gates = {line.split(":", 1)[0]: line for line in lines[:-1]}
        return gates, lines[-1]

    def calls_made(self):
        return self.calls.read_text().splitlines()


class EveryGateRunsAsItsOwnProcess(GuardPackScenarios):
    def test_a_clean_change_passes_every_gate_and_skips_the_test_selection(self):
        self.write("src/lib.rs", "// changed\n")
        self.write("changelog.d/9-thing.md", FRAGMENT)
        self.commit("change")
        result = self.guard_pack()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        gates, summary = self.gate_lines(result)
        self.assertEqual(
            list(gates),
            ["changelog-check", "changelog-guard", "package-affected", "package-bump", "test-selection", "docs-check"],
        )
        for name in ("changelog-check", "changelog-guard", "package-affected", "package-bump", "docs-check"):
            self.assertIn(f"{name}: PASS", gates[name])
        self.assertIn("test-selection: SKIP (no ignored test added)", gates["test-selection"])
        self.assertEqual(summary, "guard-pack: 5 PASS, 0 FAIL, 0 UNDECIDED, 1 SKIP")
        self.assertFalse([call for call in self.calls_made() if call.startswith("heavy-run")])

    def test_one_failing_gate_makes_the_exit_code_one_and_the_others_still_run(self):
        self.write("src/lib.rs", "// changed\n")
        self.commit("change")
        result = self.guard_pack(FAKE_DOCS_RC="1")
        self.assertEqual(result.returncode, 1)
        gates, summary = self.gate_lines(result)
        self.assertIn("docs-check: FAIL", gates["docs-check"])
        self.assertIn("error: docs check failed", gates["docs-check"])
        self.assertIn("package-bump: PASS", gates["package-bump"])
        self.assertEqual(summary, "guard-pack: 4 PASS, 1 FAIL, 0 UNDECIDED, 1 SKIP")

    def test_an_uncommitted_change_is_refused(self):
        self.write("src/lib.rs", "// uncommitted\n")
        result = self.guard_pack()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not clean", result.stderr)
        self.assertEqual(result.stdout, "")

    def test_a_head_that_is_not_checked_out_is_refused(self):
        self.write("src/lib.rs", "// changed\n")
        self.commit("change")
        result = self.guard_pack("--head", "main")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not the checked-out commit", result.stderr)
        self.assertEqual(result.stdout, "")


class ChangelogGates(GuardPackScenarios):
    def test_a_documented_behavior_change_without_a_fragment_fails_and_names_the_label(self):
        self.write("docs/cli.md", "cli, changed\n")
        self.commit("cli change")
        result = self.guard_pack()
        self.assertEqual(result.returncode, 1)
        gates, _summary = self.gate_lines(result)
        self.assertIn("changelog-guard: FAIL (no fragment; the no-changelog label would exempt", gates["changelog-guard"])

    def test_the_exemption_label_passes_the_guard(self):
        self.write("docs/cli.md", "cli, changed\n")
        self.commit("cli change")
        result = self.guard_pack("--labels-json", json.dumps([LABEL]))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_an_invalid_fragment_fails_the_check(self):
        self.write("changelog.d/9-thing.md", "no header\n")
        self.commit("bad fragment")
        result = self.guard_pack()
        gates, _summary = self.gate_lines(result)
        self.assertIn("changelog-check: FAIL", gates["changelog-check"])


class PackageBumpGate(GuardPackScenarios):
    def change(self):
        self.write("src/lib.rs", "// changed\n")
        self.commit("change")

    def test_a_package_that_is_not_bumped_fails_with_the_code_ci_reports(self):
        self.change()
        report = {"version": "1.2.3", "verdict": "fail", "packages": {
            "pi": {"verdict": "fail", "reason": "version_not_bumped"},
            "codex": {"verdict": "pass", "reason": None},
        }}
        result = self.guard_pack(FAKE_BUMP_JSON=json.dumps(report), FAKE_BUMP_RC="1")
        self.assertEqual(result.returncode, 1)
        gates, _summary = self.gate_lines(result)
        self.assertIn("package-bump: FAIL (v1.2.3: pi: version_not_bumped", gates["package-bump"])
        self.assertNotIn("codex", gates["package-bump"])

    def test_an_absent_catalog_is_undecided_and_the_last_line_lists_it(self):
        self.change()
        report = {"version": "1.2.3", "verdict": "undecided", "packages": {"pi": {"verdict": "undecided", "reason": "catalog_absent"}}}
        result = self.guard_pack(FAKE_BUMP_JSON=json.dumps(report))
        self.assertEqual(result.returncode, 0)
        gates, summary = self.gate_lines(result)
        self.assertIn("package-bump: UNDECIDED (CI-only:", gates["package-bump"])
        self.assertNotIn("PASS", gates["package-bump"])
        self.assertTrue(summary.startswith("guard-pack: 4 PASS, 0 FAIL, 1 UNDECIDED, 1 SKIP; UNDECIDED, CI enforces these: package-bump (CI-only:"), summary)

    def test_a_failing_release_listing_or_download_is_undecided_never_pass(self):
        self.change()
        for variable in ("FAKE_GH_LIST_RC", "FAKE_GH_DOWNLOAD_RC"):
            with self.subTest(variable):
                result = self.guard_pack(**{variable: "1"})
                self.assertEqual(result.returncode, 0)
                gates, summary = self.gate_lines(result)
                self.assertIn("package-bump: UNDECIDED (CI-only:", gates["package-bump"])
                self.assertIn("package-bump (CI-only:", summary)
        self.assertFalse([call for call in self.calls_made() if call.startswith("cargo xtask package bump")])

    def test_a_workspace_version_below_every_release_is_undecided(self):
        self.change()
        result = self.guard_pack(FAKE_GH_LIST="v2.0.0\nnightly-20261119", FAKE_VERSION="1.2.3")
        gates, _summary = self.gate_lines(result)
        self.assertIn("package-bump: UNDECIDED (CI-only: no published release at or below the workspace version 1.2.3", gates["package-bump"])

    def test_the_baseline_is_the_highest_release_at_or_below_the_workspace_version_by_number(self):
        self.change()
        listing = "v1.2.8\nv1.2.10\nv1.3.0\nbeta-1\nv1.2.9-rc1"
        result = self.guard_pack(FAKE_GH_LIST=listing, FAKE_VERSION="1.2.11")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        bump = [call for call in self.calls_made() if call.startswith("cargo xtask package bump")]
        self.assertEqual(len(bump), 1)
        self.assertIn("--version 1.2.10 ", bump[0] + " ")
        download = [call for call in self.calls_made() if call.startswith("gh release download")]
        self.assertIn("release download v1.2.10 --repo example/app", download[0])

    def test_an_unreadable_workspace_version_fails_the_gate(self):
        self.change()
        result = self.guard_pack(FAKE_VERSION="not-a-version")
        self.assertEqual(result.returncode, 1)
        gates, _summary = self.gate_lines(result)
        self.assertIn("package-bump: FAIL (the workspace version cannot be read", gates["package-bump"])


class TestSelectionGate(GuardPackScenarios):
    def add_ignored_test(self, unselected):
        self.write("src/lib.rs", "// code\n#[ignore = \"slow\"]\nfn an_ignored_test() {}\n")
        if unselected:
            self.write("unselected-test", "")
        self.commit("add an ignored test")

    def test_an_ignored_test_that_no_lane_selects_fails_and_names_it(self):
        self.add_ignored_test(unselected=True)
        result = self.guard_pack()
        self.assertEqual(result.returncode, 1)
        gates, _summary = self.gate_lines(result)
        self.assertIn("test-selection: FAIL (missing=['pkg-a::bin tests::an_ignored_test']", gates["test-selection"])
        heavy = [call for call in self.calls_made() if call.startswith("heavy-run")]
        self.assertEqual(len(heavy), 2)
        self.assertIn("nextest archive --locked --profile macos -p pkg-a -p pkg-b --archive-file", heavy[0])
        self.assertIn("check-macos --scope all", heavy[1])

    def test_a_host_other_than_macos_is_undecided_and_builds_nothing(self):
        self.add_ignored_test(unselected=True)
        result = self.guard_pack(GUARD_PACK_PLATFORM="linux")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        gates, summary = self.gate_lines(result)
        self.assertIn("test-selection: UNDECIDED (CI-only: the macOS test partition needs a macOS host", gates["test-selection"])
        self.assertIn("test-selection (CI-only:", summary)
        self.assertFalse([call for call in self.calls_made() if call.startswith("heavy-run")])

    def test_an_ignored_test_that_a_lane_selects_passes(self):
        self.add_ignored_test(unselected=False)
        result = self.guard_pack()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        gates, _summary = self.gate_lines(result)
        self.assertIn("test-selection: PASS", gates["test-selection"])

    def test_an_archive_that_does_not_build_fails_the_gate(self):
        self.add_ignored_test(unselected=False)
        result = self.guard_pack(FAKE_ARCHIVE_RC="1")
        self.assertEqual(result.returncode, 1)
        gates, _summary = self.gate_lines(result)
        self.assertIn("test-selection: FAIL (the macOS archive did not build: error: could not compile the archive", gates["test-selection"])

    def test_an_ignore_that_the_diff_removes_does_not_run_the_gate(self):
        self.write("src/lib.rs", "// code\n#[ignore]\nfn an_ignored_test() {}\n")
        self.commit("ignored test on main")
        self.git("checkout", "-q", "main")
        self.git("merge", "-q", "--ff-only", "pr")
        self.git("checkout", "-q", "-b", "fix")
        self.write("src/lib.rs", "// code\nfn an_ignored_test() {}\n")
        self.commit("enable the test")
        result = self.guard_pack()
        gates, _summary = self.gate_lines(result)
        self.assertIn("test-selection: SKIP", gates["test-selection"])


if __name__ == "__main__":
    unittest.main()
