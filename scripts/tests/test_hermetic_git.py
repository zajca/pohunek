"""Script tests run git without the developer's Git configuration (#1779).

The scenario gives a child process a hostile global, XDG and system Git
configuration and runs the git steps the script tests run: init, commit, an
annotated tag and a remote URL lookup. With `hermetic_git` imported they
succeed and none of the hostile settings applies; without it the same steps
fail, which shows the configuration is hostile.
"""

import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest

import hermetic_git  # noqa: F401  (isolates git from the developer's configuration)

TESTS = Path(__file__).resolve().parent
# Modules whose `git` is a stand-in script, not the real program.
FAKE_GIT = {"test_measure_dev_loop.py"}
RUNS_GIT = re.compile(r"""["']git["']|\bgit -C|\bgit init""")

STEPS = textwrap.dedent(
    """
    import os, subprocess, sys
    {prelude}
    def git(*args):
        return subprocess.run(["git", "-c", "maintenance.auto=false", "-c", "gc.auto=0", *args], check=True, capture_output=True, text=True).stdout.strip()

    git("init", "-q", "repo")
    os.chdir("repo")
    open("file", "w").write("content\\n")
    open("noise.log", "w").write("content\\n")
    git("add", ".")
    git("commit", "-q", "-m", "fixture")
    git("tag", "-a", "-m", "fixture", "v1")
    git("remote", "add", "origin", "https://example.invalid/repo.git")
    print(git("ls-remote", "--get-url", "origin"))
    print(" ".join(git("ls-files").split()))
    """
)


def hostile_environment(root):
    """An environment whose every user- and system-level Git file is hostile."""
    hooks = root / "hooks"
    hooks.mkdir()
    marker = root / "hook-ran"
    hook = hooks / "pre-commit"
    hook.write_text(f"#!/bin/sh\ntouch {marker}\nexit 1\n")
    hook.chmod(0o755)
    hostile = (
        "[commit]\n\tgpgsign = true\n[tag]\n\tgpgsign = true\n"
        "[gpg]\n\tformat = ssh\n[gpg \"ssh\"]\n\tprogram = /bin/false\n"
        "[user]\n\tsigningkey = /nonexistent/key.pub\n"
        "[url \"ssh://git@invalid.example/\"]\n\tinsteadOf = https://\n"
        f"[core]\n\thooksPath = {hooks}\n"
        f"[init]\n\ttemplateDir = {root / 'missing-template'}\n"
    )
    files = {name: root / name for name in ("global", "system")}
    for path in files.values():
        path.write_text(hostile)
    xdg = root / "xdg"
    (xdg / "git").mkdir(parents=True)
    (xdg / "git" / "config").write_text(hostile)
    (xdg / "git" / "ignore").write_text("*.log\n")
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("GIT_")
    }
    environment.update(
        HOME=str(root),
        XDG_CONFIG_HOME=str(xdg),
        GIT_CONFIG_GLOBAL=str(files["global"]),
        GIT_CONFIG_SYSTEM=str(files["system"]),
        PYTHONPATH=str(TESTS),
    )
    return environment, marker


class HermeticGitTests(unittest.TestCase):
    def run_steps(self, prelude):
        root = Path(tempfile.mkdtemp(prefix="hg-"))
        self.addCleanup(lambda: subprocess.run(["rm", "-rf", str(root)], check=False))
        environment, marker = hostile_environment(root)
        work = root / "work"
        work.mkdir()
        result = subprocess.run(
            [sys.executable, "-c", STEPS.format(prelude=prelude)],
            cwd=work, env=environment, capture_output=True, text=True,
        )
        return result, marker

    def test_a_hostile_git_configuration_does_not_reach_a_script_test(self):
        result, marker = self.run_steps("import hermetic_git")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines()[0], "https://example.invalid/repo.git")
        self.assertEqual(result.stdout.splitlines()[1], "file noise.log", "a personal ignore file applied")
        self.assertFalse(marker.exists(), "the global core.hooksPath hook ran")

    def test_the_same_steps_fail_without_the_isolation(self):
        result, _ = self.run_steps("")
        self.assertNotEqual(result.returncode, 0, "the fixture configuration is not hostile")

    def test_every_test_module_that_runs_git_isolates_it(self):
        for path in sorted(TESTS.glob("test_*.py")):
            if path.name in FAKE_GIT:
                continue
            source = path.read_text()
            if not RUNS_GIT.search(source):
                continue
            with self.subTest(module=path.name):
                self.assertRegex(
                    source, re.compile(r"^import hermetic_git\b", re.MULTILINE),
                    f"{path.name} runs git without importing hermetic_git",
                )


if __name__ == "__main__":
    unittest.main()
