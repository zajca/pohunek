#!/usr/bin/env python3
"""Executable tests for scripts/harness-milestone (GitHub-issue-driven starts).

Every run is exercised through a temporary git fixture plus fake `gh` and
fake `lh-harness` boundary processes: the fake processes only record what
they were called with (argv lines) and emit canned output, so no real
harness, model, or network call is made. Negative paths must leave the git
state exactly as it was: no worktree, no branch, no worktrees directory, no
gh invocation. The production script has no test bypass: fixtures place the
stand-in checkout on a writable volume the disk guard accepts.
"""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent.parent
SCRIPT = REPO / "scripts" / "harness-milestone"

# Rationale: the production worktree guard forbids /tmp and $HOME parents.
# Fixtures therefore live under /var/tmp (writable, but outside both), so
# the real script runs unmodified — production carries no test-only bypass.
FIXTURE_BASE_PARENT = Path("/var/tmp")
FIXTURE_BASE_PARENT_TAG = "FIXTURE_BASE_PARENT=/var/tmp (not /tmp, not $HOME)"

FIXTURE_ISSUE_NUMBER = 5
CANONICAL_URL = "https://github.com/zajca/pohunek/issues/5"
FIXTURE_ISSUE_JSON = json.dumps(
    {
        "number": 5,
        "url": CANONICAL_URL,
        "state": "OPEN",
        "title": "Hermes milestone",
    }
)
DEFAULT_CONFIG = {
    "repository": "zajca/pohunek",
    "defaultProject": {
        "owner": "zajca",
        "number": 1,
        "url": "https://github.com/users/zajca/projects/1",
    },
    "statusOptions": ["Todo", "In Progress", "Done"],
    "statusSemantics": {
        "Todo": "work not started",
        "In Progress": "work underway",
        "Done": "verified landing",
    },
}

FAKE_LH = """#!/bin/sh
if [ "${1:-}" = "--version" ]; then
    printf '%s\\n' "${FAKE_LH_VERSION:-lh-harness 0.1.7}"
    exit 0
fi
if [ "${1:-}" = "run" ]; then
    shift
    for arg in "$@"; do
        printf '%s\\n' "$arg"
    done > "$FAKE_LH_LOG"
    exit 0
fi
echo "fake lh-harness: unexpected argv: $*" >&2
exit 123
"""

FAKE_GH = """#!/bin/sh
printf '%s\\n' "$*" >> "$FAKE_GH_LOG"
if [ -n "${FAKE_GH_FAIL:-}" ]; then
    printf '%s\\n' "$FAKE_GH_FAIL" >&2
    exit "${FAKE_GH_EXIT:-1}"
fi
printf '%s\\n' "$FAKE_GH_JSON"
"""

DEFAULT_TASK_TEXT = (
    "# Task: Implement the pohunek milestone specified in the given GitHub "
    "issue\n\nFake harness workflow text for fixture tests.\n"
)


class HarnessMilestoneCase(unittest.TestCase):
    def setUp(self):
        self.base = Path(
            tempfile.mkdtemp(prefix="pohunek-harness-milestone-", dir=str(FIXTURE_BASE_PARENT))
        )
        self.addCleanup(self._cleanup)
        self.tools = self.base / "bin"
        self.tools.mkdir(parents=True)
        for name, body in (("gh", FAKE_GH), ("lh-harness", FAKE_LH)):
            path = self.tools / name
            path.write_text(body)
            path.chmod(0o755)
        self.gh_log = self.base / "gh.log"
        self.lh_log = self.base / "lh.log"
        self.repo = self.base / "primary"
        self.repo.mkdir(parents=True)
        self._git("init", "-q", "-b", "main")
        self._git("config", "user.email", "test@example.invalid")
        self._git("config", "user.name", "Test")
        self._git("config", "commit.gpgsign", "false")
        self.worktrees = self.base / "pohunek-worktrees"
        self._write_config(json.dumps(DEFAULT_CONFIG))
        self.default_task_path = (
            self.repo / ".lh-harness" / "workflows" / "milestone-build.md"
        )
        self.default_task = self._write_task(DEFAULT_TASK_TEXT)
        (self.repo / ".github" / "marker.txt").write_text("fixture\n")
        self._git("add", ".")
        self._git("commit", "-q", "-m", "fixture")

    def _cleanup(self):
        shutil.rmtree(self.base, ignore_errors=True)

    def _git(self, *args):
        subprocess.run(
            ["git", "-C", str(self.repo), *args],
            check=True,
            capture_output=True,
        )

    def _write_config(self, text):
        path = self.repo / ".github" / "agent-workflow.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text + "\n")

    def _write_task(self, text):
        self.default_task_path.parent.mkdir(parents=True, exist_ok=True)
        self.default_task_path.write_text(text)
        return str(self.default_task_path)

    def _env(self):
        env = dict(os.environ)
        env["PATH"] = str(self.tools) + os.pathsep + env.get("PATH", "")
        env["FAKE_GH_LOG"] = str(self.gh_log)
        env["FAKE_LH_LOG"] = str(self.lh_log)
        env["FAKE_GH_JSON"] = FIXTURE_ISSUE_JSON
        env["POHUNEK_HARNESS_ALLOW_UNSANDBOXED"] = "1"
        return env

    def _run(self, *args, env=None, expect=0):
        if env is None:
            env = self._env()
        proc = subprocess.run(
            [str(SCRIPT), *args],
            env=env,
            # The script derives the primary checkout from `git rev-parse`
            # relative to the caller, so run the fixture-backed executable
            # with the fixture repository as its working directory.
            cwd=str(self.repo),
            capture_output=True,
            text=True,
            timeout=120,
        )
        if expect is not None:
            self.assertEqual(
                proc.returncode,
                expect,
                f"stdout={proc.stdout!r} stderr={proc.stderr!r}",
            )
        return proc

    def _argv(self):
        return self.lh_log.read_text().splitlines()

    def _gh_calls(self):
        if not self.gh_log.exists():
            return []
        return self.gh_log.read_text().splitlines()

    def _branch_exists(self, name):
        out = subprocess.run(
            ["git", "-C", str(self.repo), "branch", "--list", name],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        return bool(out.strip())

    def _worktree_list(self):
        return subprocess.run(
            ["git", "-C", str(self.repo), "worktree", "list", "--porcelain"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout

    def assert_no_git_mutation(self, before_list):
        self.assertEqual(
            self._worktree_list(),
            before_list,
            "pre-failure git state must be untouched",
        )
        self.assertFalse(
            self._branch_exists("zajca/x"),
            "failed preflight must not create a branch",
        )
        self.assertFalse(
            (self.worktrees / "x").exists(),
            "failed preflight must not create a worktree directory",
        )
        self.assertFalse(
            self.worktrees.exists(),
            "failed preflight must not create the worktrees root",
        )
        # (no assertion on gh calls here: scenarios whose failing step IS the
        # gh fetch do call gh once; tests that demand rejection before gh
        # assert _gh_calls() == [] explicitly.)

    def _delivered_text(self):
        argv = self._argv()
        self.assertEqual(argv[0], "--task", f"unexpected argv: {argv}")
        self.assertTrue(argv[1].startswith("@"), f"unexpected task arg: {argv[1]}")
        return Path(argv[1][1:]).read_text()

    # Happy paths ---------------------------------------------------------

    def test_valid_issue_number_starts_run_with_identity_in_task_text(self):
        self._run("hermes-m1", "--issue", str(FIXTURE_ISSUE_NUMBER))
        delivered = self._delivered_text()
        self.assertIn("Target issue for this run: " + CANONICAL_URL, delivered)
        self.assertIn(CANONICAL_URL, delivered)
        self.assertTrue(self._branch_exists("zajca/hermes-m1"))
        # No NEXT.md dependency: the fixture has none and none is created.
        self.assertFalse((self.repo / "NEXT.md").exists())
        self.assertFalse(
            (self.worktrees / "hermes-m1" / "NEXT.md").exists()
        )

    def test_valid_issue_url_delivers_same_canonical_identity(self):
        self._run("hermes-m2", "--issue", CANONICAL_URL)
        delivered = self._delivered_text()
        self.assertIn("Target issue for this run: " + CANONICAL_URL, delivered)
        # gh was pointed at the canonical URL of the configured repository,
        # not the raw user input and not a different repo.
        gh_first = self._gh_calls()[0]
        self.assertIn(CANONICAL_URL, gh_first)
        self.assertIn("zajca/pohunek", gh_first)

    def test_default_task_text_bootstraps_the_workflow_with_no_next(self):
        # No NEXT.md exists anywhere in the fixture; the run succeeds anyway.
        self.assertFalse((self.repo / "NEXT.md").exists())
        self._run("hermes-m3", "--issue", str(FIXTURE_ISSUE_NUMBER))
        delivered = self._delivered_text()
        self.assertIn(".lh-harness/workflows/milestone-build.md", delivered)
        self.assertIn("no NEXT.md exists", delivered)

    def test_custom_task_content_survives_verbatim_after_identity(self):
        task_path = self._write_task("CUSTOM-TASK-MARKER: do the thing\n")
        self._run("hermes-m4", "--issue", str(FIXTURE_ISSUE_NUMBER), "--task", task_path)
        argv = self._argv()
        delivered = Path(argv[1][1:]).read_text()
        head, _, tail = delivered.partition("\n---\n")
        self.assertIn("Target issue for this run: " + CANONICAL_URL, head)
        self.assertEqual(tail, "CUSTOM-TASK-MARKER: do the thing\n")

    def test_custom_task_path_and_max_rounds_reach_harness(self):
        task_path = self._write_task("RELATIVE-TASK-8\n")
        self._run(
            "hermes-m8",
            "--issue",
            str(FIXTURE_ISSUE_NUMBER),
            "--task",
            task_path,
            "--max-rounds",
            "8",
        )
        argv = self._argv()
        self.assertEqual(argv[argv.index("--max-rounds") + 1], "8")
        self.assertIn("RELATIVE-TASK-8\n", self._delivered_text())

    def test_worktree_paths_and_timeouts_reach_harness(self):
        self._run("hermes-m9", "--issue", str(FIXTURE_ISSUE_NUMBER), "--max-rounds", "9")
        argv = self._argv()
        self.assertEqual(
            argv[argv.index("--workspace") + 1], str(self.worktrees / "hermes-m9")
        )
        self.assertEqual(argv[argv.index("--manager-timeout") + 1], "600")
        self.assertEqual(argv[argv.index("--auditor-timeout") + 1], "2400")

    def test_gh_receives_canonical_issue_url_with_configured_repo(self):
        self._run("hermes-m10", "--issue", CANONICAL_URL)
        gh_first = self._gh_calls()[0]
        self.assertIn(CANONICAL_URL, gh_first)
        self.assertIn("--repo", gh_first)
        self.assertIn("zajca/pohunek", gh_first)
        self.assertIn("--json", gh_first)

    def test_disk_guard_allows_only_non_tmp_and_non_home_fixture_root(self):
        # The fixture root itself is the disk-guard guarantee in action: the
        # real script, with no test override, accepted this fixture location
        # (under %s) while it refuses /tmp and $HOME bases below.
        self.assertTrue(
            str(self.repo).startswith(str(FIXTURE_BASE_PARENT)),
            FIXTURE_BASE_PARENT_TAG,
        )
        self.assertFalse(
            str(self.repo).startswith("/tmp/"), FIXTURE_BASE_PARENT_TAG
        )
        self.assertNotIn("POHUNEK_HM_TEST_WORKTREES_ROOT", self._env())

    # Negative paths: every failure must leave git state untouched ----------

    def test_missing_issue_argument_is_rejected_before_gh(self):
        before = self._worktree_list()
        proc = self._run("x", expect=None)
        self.assertEqual(proc.returncode, 2)
        self.assertEqual(self._gh_calls(), [])
        self.assert_no_git_mutation(before)

    def test_malformed_issue_is_rejected_before_gh(self):
        before = self._worktree_list()
        proc = self._run("x", "--issue", "not-a-number-or-url", expect=None)
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(self._gh_calls(), [])
        self.assert_no_git_mutation(before)

    def test_cross_repo_issue_url_is_rejected_before_gh(self):
        before = self._worktree_list()
        proc = self._run(
            "x",
            "--issue",
            "https://github.com/other/pohunek/issues/1",
            expect=None,
        )
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(self._gh_calls(), [])
        self.assert_no_git_mutation(before)

    def test_gh_failure_blocks_the_run(self):
        before = self._worktree_list()
        env = self._env()
        env["FAKE_GH_FAIL"] = "gh: Not Found PRIVATE_DIAGNOSTIC_MARKER"
        proc = self._run("x", "--issue", "1", env=env, expect=None)
        self.assertEqual(proc.returncode, 1)
        self.assertIn("could not fetch issue", proc.stderr)
        self.assertNotIn("PRIVATE_DIAGNOSTIC_MARKER", proc.stdout + proc.stderr)
        self.assert_no_git_mutation(before)

    def test_non_github_url_schema_is_malformed_for_this_tracker(self):
        before = self._worktree_list()
        proc = self._run(
            "x", "--issue", "https://gitlab.com/zajca/pohunek/-/issues/1", expect=None
        )
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(self._gh_calls(), [])
        self.assert_no_git_mutation(before)

    def test_closed_issue_blocks_the_run(self):
        before = self._worktree_list()
        env = self._env()
        env["FAKE_GH_JSON"] = json.dumps(
            {
                "number": 1,
                "url": "https://github.com/zajca/pohunek/issues/1",
                "state": "CLOSED",
                "title": "x",
            }
        )
        proc = self._run("x", "--issue", "1", env=env, expect=None)
        self.assertEqual(proc.returncode, 1)
        self.assertIn("not open", proc.stderr)
        self.assert_no_git_mutation(before)

    def test_fetched_issue_from_wrong_repo_blocks_the_run(self):
        before = self._worktree_list()
        env = self._env()
        env["FAKE_GH_JSON"] = json.dumps(
            {
                "number": 1,
                "url": "https://github.com/other/pohunek/issues/1",
                "state": "OPEN",
                "title": "x",
            }
        )
        proc = self._run("x", "--issue", "1", env=env, expect=None)
        self.assertEqual(proc.returncode, 1)
        self.assert_no_git_mutation(before)

    def test_fetched_issue_number_mismatch_blocks_the_run(self):
        before = self._worktree_list()
        env = self._env()
        env["FAKE_GH_JSON"] = json.dumps(
            {
                "number": 7,
                "url": "https://github.com/zajca/pohunek/issues/7",
                "state": "OPEN",
                "title": "x",
            }
        )
        proc = self._run("x", "--issue", "1", env=env, expect=None)
        self.assertEqual(proc.returncode, 1)
        self.assert_no_git_mutation(before)

    def test_bad_config_blocks_the_run(self):
        before = self._worktree_list()
        self._write_config(json.dumps({"repository": "zajca/pohunek"}))
        proc = self._run("x", "--issue", "1", expect=None)
        self.assertEqual(proc.returncode, 1)
        self.assert_no_git_mutation(before)

    def test_unparseable_config_blocks_the_run(self):
        before = self._worktree_list()
        self._write_config("{ not json")
        proc = self._run("x", "--issue", "1", expect=None)
        self.assertEqual(proc.returncode, 1)
        self.assert_no_git_mutation(before)

    def test_missing_config_blocks_the_run(self):
        before = self._worktree_list()
        (self.repo / ".github" / "agent-workflow.json").unlink()
        proc = self._run("x", "--issue", "1", expect=None)
        self.assertEqual(proc.returncode, 1)
        self.assert_no_git_mutation(before)

    def test_missing_default_task_blocks_the_run(self):
        before = self._worktree_list()
        self.default_task_path.unlink()
        proc = self._run("x", "--issue", "1", expect=None)
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(self._gh_calls(), [])
        self.assert_no_git_mutation(before)

    def test_missing_custom_task_blocks_the_run(self):
        before = self._worktree_list()
        proc = self._run(
            "x",
            "--issue",
            "1",
            "--task",
            str(self.base / "no-such-task.md"),
            expect=None,
        )
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(self._gh_calls(), [])
        self.assert_no_git_mutation(before)

    def test_unreadable_task_blocks_before_gh_and_git(self):
        self.default_task_path.chmod(0)
        try:
            if os.access(self.default_task_path, os.R_OK):
                self.skipTest("current account can read mode-000 files")
            for extra_args in ((), ("--task", str(self.default_task_path))):
                with self.subTest(extra_args=extra_args):
                    before = self._worktree_list()
                    proc = self._run("x", "--issue", "1", *extra_args, expect=None)
                    self.assertEqual(proc.returncode, 1)
                    self.assertIn("unreadable", proc.stderr)
                    self.assertEqual(self._gh_calls(), [])
                    self.assert_no_git_mutation(before)
        finally:
            self.default_task_path.chmod(0o644)

    def test_missing_pinned_version_blocks_the_run(self):
        before = self._worktree_list()
        env = self._env()
        env["FAKE_LH_VERSION"] = "lh-harness 0.1.8"
        proc = self._run("x", "--issue", "1", env=env, expect=None)
        self.assertEqual(proc.returncode, 1)
        self.assertIn("0.1.7", proc.stderr)
        self.assert_no_git_mutation(before)

    def test_missing_unsandboxed_opt_in_blocks_the_run(self):
        before = self._worktree_list()
        env = self._env()
        env.pop("POHUNEK_HARNESS_ALLOW_UNSANDBOXED", None)
        proc = self._run("x", "--issue", "1", env=env, expect=None)
        self.assertEqual(proc.returncode, 1)
        self.assert_no_git_mutation(before)

    def test_disk_guard_refuses_a_tmp_fixture_root(self):
        # From a /tmp-placed stand-in the real (override-free) script must
        # refuse to start before creating anything.
        tmp_base = Path(tempfile.mkdtemp(prefix="pohunek-hm-guard-"))
        self.addCleanup(shutil.rmtree, tmp_base, ignore_errors=True)
        repo = tmp_base / "primary"
        repo.mkdir()
        subprocess.run(["git", "-C", str(repo), "init", "-q", "-b", "main"], check=True, capture_output=True)
        subprocess.run(["git", "-C", str(repo), "config", "user.email", "t@e.invalid"], check=True, capture_output=True)
        subprocess.run(["git", "-C", str(repo), "config", "user.name", "t"], check=True, capture_output=True)
        (repo / ".github").mkdir()
        (repo / ".github" / "agent-workflow.json").write_text(json.dumps(DEFAULT_CONFIG))
        task = repo / ".lh-harness" / "workflows" / "milestone-build.md"
        task.parent.mkdir(parents=True)
        task.write_text(DEFAULT_TASK_TEXT)
        subprocess.run(["git", "-C", str(repo), "add", "."], check=True, capture_output=True)
        subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", "f"], check=True, capture_output=True)
        before = subprocess.run(
            ["git", "-C", str(repo), "worktree", "list", "--porcelain"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        proc = subprocess.run(
            [str(SCRIPT), "x", "--issue", "1"],
            env=self._env(),
            cwd=str(repo),
            capture_output=True,
            text=True,
            timeout=120,
        )
        self.assertEqual(proc.returncode, 1)
        self.assertIn("must not be /tmp", proc.stderr)
        self.assertEqual(self._gh_calls(), [])
        after = subprocess.run(
            ["git", "-C", str(repo), "worktree", "list", "--porcelain"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        self.assertEqual(after, before)
        self.assertFalse((tmp_base / "pohunek-worktrees").exists())

    def test_max_rounds_0_blocks_the_run(self):
        before = self._worktree_list()
        proc = self._run("x", "--issue", "1", "--max-rounds", "0", expect=None)
        self.assertEqual(proc.returncode, 2)
        self.assertFalse(self._branch_exists("zajca/x"))
        self.assertFalse((self.worktrees / "x").exists())
        self.assertFalse(self.worktrees.exists())

    def test_slug_validation_blocks_nonconforming_slugs(self):
        for slug in ("Bad", "a/b", "a b", "with.dot"):
            proc = self._run(slug, "--issue", "1", expect=None)
            self.assertEqual(proc.returncode, 2, f"slug {slug!r}")
            self.assertFalse(self._branch_exists("zajca/" + slug))
            self.assertFalse((self.worktrees / slug).exists())
            self.assertFalse(self.worktrees.exists())

    # Config boundary cases: malformed inputs reject before gh and git ------

    def _run_with_config(self, config, expect=1):
        before = self._worktree_list()
        self._write_config(json.dumps(config))
        proc = self._run("x", "--issue", "1", expect=expect)
        return before, proc

    def assert_config_rejected(self, config):
        before, proc = self._run_with_config(config)
        self.assertEqual(self._gh_calls(), [])
        self.assert_no_git_mutation(before)

    def test_config_repository_slash_only_owner_is_rejected(self):
        self.assert_config_rejected({**DEFAULT_CONFIG, "repository": "/pohunek"})

    def test_config_repository_with_whitespace_is_rejected(self):
        self.assert_config_rejected(
            {**DEFAULT_CONFIG, "repository": "zaj ca/pohunek"}
        )

    def test_config_repository_with_two_slashes_is_rejected(self):
        self.assert_config_rejected(
            {**DEFAULT_CONFIG, "repository": "zajca/pohunek/extra"}
        )

    def test_config_repository_newline_in_name_is_rejected(self):
        self.assert_config_rejected(
            {**DEFAULT_CONFIG, "repository": "zajca/pohunek\nx"}
        )

    def test_config_project_number_boolean_is_rejected(self):
        self.assert_config_rejected(
            {
                **DEFAULT_CONFIG,
                "defaultProject": {
                    "owner": "zajca",
                    "number": True,
                    "url": "https://github.com/users/zajca/projects/1",
                },
            }
        )

    def test_config_project_number_zero_is_rejected(self):
        self.assert_config_rejected(
            {
                **DEFAULT_CONFIG,
                "defaultProject": {
                    "owner": "zajca",
                    "number": 0,
                    "url": "https://github.com/users/zajca/projects/0",
                },
            }
        )

    def test_config_project_number_string_is_rejected(self):
        self.assert_config_rejected(
            {
                **DEFAULT_CONFIG,
                "defaultProject": {
                    "owner": "zajca",
                    "number": "1",
                    "url": "https://github.com/users/zajca/projects/1",
                },
            }
        )

    def test_config_project_unrelated_url_is_rejected(self):
        self.assert_config_rejected(
            {
                **DEFAULT_CONFIG,
                "defaultProject": {
                    "owner": "zajca",
                    "number": 1,
                    "url": "https://example.com/projects/1",
                },
            }
        )

    def test_config_project_url_mismatching_owner_is_rejected(self):
        self.assert_config_rejected(
            {
                **DEFAULT_CONFIG,
                "defaultProject": {
                    "owner": "other",
                    "number": 1,
                    "url": "https://github.com/users/zajca/projects/1",
                },
            }
        )

    def test_config_project_url_mismatching_number_is_rejected(self):
        self.assert_config_rejected(
            {
                **DEFAULT_CONFIG,
                "defaultProject": {
                    "owner": "zajca",
                    "number": 7,
                    "url": "https://github.com/users/zajca/projects/1",
                },
            }
        )
