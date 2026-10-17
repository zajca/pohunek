"""Script test fixture repositories run git without background maintenance.

A `git commit` starts `git maintenance run --auto --detach`. The detached
process can still be writing into `.git` when a test removes its fixture
repository, and the removal then fails with "Directory not empty" (#1251).
"""

import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

TESTS = Path(__file__).resolve().parent
# The settings a fixture turns off, as configuration keys, -c options or
# `git config` arguments; the first scenario shows they stop the maintenance.
DISABLED = ("maintenance.auto", "gc.auto")
# A test module that creates a real repository runs `git init`.
CREATES_REPOSITORY = re.compile(r"[\"'](?:-C[\"'].*?)?init[\"']")


def traced_commit(*config):
    """The GIT_TRACE output of one commit in a fresh repository with `config`."""
    with tempfile.TemporaryDirectory() as directory:
        env = {
            **{key: value for key, value in os.environ.items() if not key.startswith("GIT_")},
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "test", "GIT_AUTHOR_EMAIL": "test@example.invalid",
            "GIT_COMMITTER_NAME": "test", "GIT_COMMITTER_EMAIL": "test@example.invalid",
            "GIT_CONFIG_COUNT": str(len(config)),
        }
        for index, (key, value) in enumerate(config):
            env[f"GIT_CONFIG_KEY_{index}"] = key
            env[f"GIT_CONFIG_VALUE_{index}"] = value

        def git(*args, trace=False):
            return subprocess.run(
                ["git", *args], cwd=directory, check=True, capture_output=True, text=True,
                env={**env, "GIT_TRACE": "1"} if trace else env,
            )

        git("init", "--quiet")
        Path(directory, "file").write_text("content\n")
        git("add", "file")
        return git("commit", "--quiet", "-m", "fixture", trace=True).stderr


class GitFixtureMaintenanceTests(unittest.TestCase):
    def test_the_fixture_settings_keep_a_commit_from_starting_maintenance(self):
        # The control runs the maintenance in the foreground, so it has ended
        # before its own repository is removed.
        if "maintenance run" not in traced_commit(("maintenance.autoDetach", "false")):
            self.skipTest("this git starts no maintenance after a commit")
        trace = traced_commit(("maintenance.auto", "false"), ("gc.auto", "0"))
        self.assertNotIn("maintenance run", trace)
        self.assertNotIn("gc --auto", trace)

    def test_every_test_module_that_creates_a_repository_disables_maintenance(self):
        for path in sorted(TESTS.glob("test_*.py")):
            if path == Path(__file__).resolve():
                continue
            source = path.read_text()
            if "git" not in source or not CREATES_REPOSITORY.search(source):
                continue
            with self.subTest(module=path.name):
                missing = [setting for setting in DISABLED if setting not in source]
                self.assertEqual(missing, [], f"{path.name} creates a repository without them")


if __name__ == "__main__":
    unittest.main()
