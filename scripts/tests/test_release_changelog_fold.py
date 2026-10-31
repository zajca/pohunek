"""Release changelog fold: `scripts/release` folds the fragments in the release commit.

The tests run the real script, the real `scripts/changelog` and real git in a
throwaway repository. Only the two external tools are stubbed: `cargo` (the
knowledge log and the docs check are cargo xtask commands, and the docs check
can be made to fail) and `ci-evidence` (it queries GitHub). Nothing is pushed.
"""

import datetime
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

import hermetic_git  # noqa: F401  (isolates git from the developer's configuration)

SCRIPTS = Path(__file__).resolve().parents[1]
RELEASE = SCRIPTS / "release"
CHANGELOG_TOOL = SCRIPTS / "changelog"

CARGO_TOML = '[workspace.package]\nversion = "0.1.0"\n'
CHANGELOG = "# Changelog\n\nUser-facing changes of example/app.\n\n## 0.1.0 - 2026-10-01\n\n### Added\n\n- First.\n"
LAYOUT = '{\n  "repository": "example/app",\n  "changelog": "CHANGELOG.md"\n}\n'
CARGO_STUB = """#!/bin/sh
if [ "$1" = xtask ] && [ "$2" = docs ] && [ "$3" = check ] && [ -n "$POHUNEK_TEST_DOCS_CHECK_FAILS" ]; then
  exit 1
fi
exit 0
"""
CI_EVIDENCE_STUB = "raise SystemExit(0)\n"


class ReleaseChangelogFoldTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="release-changelog-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.repo = self.tmp / "repo"
        (self.repo / "scripts").mkdir(parents=True)
        shutil.copy(RELEASE, self.repo / "scripts" / "release")
        shutil.copy(CHANGELOG_TOOL, self.repo / "scripts" / "changelog")
        (self.repo / "scripts" / "ci-evidence").write_text(CI_EVIDENCE_STUB)
        self.write("Cargo.toml", CARGO_TOML)
        self.write("Cargo.lock", "# stub lock\n")
        self.write("CHANGELOG.md", CHANGELOG)
        self.write(".github/changelog.json", LAYOUT)
        self.write("changelog.d/.gitkeep", "")
        stub_bin = self.tmp / "bin"
        stub_bin.mkdir()
        cargo = stub_bin / "cargo"
        cargo.write_text(CARGO_STUB)
        cargo.chmod(0o755)
        self.env = dict(os.environ, PATH=f"{stub_bin}:{os.environ['PATH']}")
        self.env["GIT_CONFIG_GLOBAL"] = os.devnull
        self.env["GIT_CONFIG_SYSTEM"] = os.devnull
        for role in ("AUTHOR", "COMMITTER"):
            self.env[f"GIT_{role}_NAME"] = "test"
            self.env[f"GIT_{role}_EMAIL"] = "test@example.invalid"
        self.env.update({
            "GIT_CONFIG_COUNT": "2",
            "GIT_CONFIG_KEY_0": "maintenance.auto", "GIT_CONFIG_VALUE_0": "false",
            "GIT_CONFIG_KEY_1": "gc.auto", "GIT_CONFIG_VALUE_1": "0",
        })
        self.git("init", "-q", "-b", "main")
        self.git("remote", "add", "origin", "https://github.com/example/app.git")
        self.git("add", "-A")
        self.git("-c", "commit.gpgsign=false", "commit", "-q", "-m", "initial")
        self.git("tag", "-a", "v0.1.0", "-m", "v0.1.0")

    def add_fragment(self):
        self.write("changelog.d/12-tailscale.md", "---\nkind: added\n---\nTailscale carries the connection.\n")

    def write(self, relative, text):
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def git(self, *args) -> str:
        return subprocess.run(
            ["git", "-C", str(self.repo), *args], check=True, env=self.env,
            capture_output=True, text=True,
        ).stdout

    def commit_all(self, message):
        self.git("add", "-A")
        self.git("-c", "commit.gpgsign=false", "commit", "-q", "-m", message)

    def release(self, *args, docs_check_fails=False):
        env = dict(self.env)
        if docs_check_fails:
            env["POHUNEK_TEST_DOCS_CHECK_FAILS"] = "1"
        return subprocess.run(
            ["sh", str(self.repo / "scripts" / "release"), *args],
            cwd=self.repo, env=env, capture_output=True, text=True, check=False,
        )

    def test_dry_run_prints_the_folded_section_and_changes_nothing(self):
        self.add_fragment()
        self.commit_all("add a fragment")

        result = self.release("0.2.0", "--dry-run", "--no-push")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("<!-- pohunek-changelog:start -->", result.stdout)
        self.assertIn(
            "### Added\n\n- Tailscale carries the connection. "
            "([#12](https://github.com/example/app/issues/12))",
            result.stdout,
        )
        self.assertEqual(self.git("status", "--porcelain"), "")
        self.assertTrue((self.repo / "changelog.d" / "12-tailscale.md").exists())

    def test_dry_run_refuses_an_invalid_fragment(self):
        self.write("changelog.d/13-broken.md", "---\nkind: improved\n---\nText.\n")
        self.commit_all("add an invalid fragment")

        result = self.release("0.2.0", "--dry-run", "--no-push")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unknown kind 'improved'", result.stderr)

    def test_release_folds_the_fragments_into_the_release_commit(self):
        self.add_fragment()
        self.commit_all("add a fragment")

        result = self.release("0.2.0", "--no-push")

        self.assertEqual(result.returncode, 0, result.stderr)
        today = datetime.datetime.now(datetime.timezone.utc).date().isoformat()
        changelog = self.git("show", "HEAD:CHANGELOG.md")
        self.assertIn(f"## 0.2.0 - {today}\n\n### Added\n\n- Tailscale carries", changelog)
        self.assertEqual(self.git("ls-tree", "-r", "--name-only", "HEAD", "--", "changelog.d").split(), ["changelog.d/.gitkeep"])
        self.assertEqual(self.git("rev-parse", "refs/tags/v0.2.0^{commit}").strip(), self.git("rev-parse", "HEAD").strip())
        self.assertEqual(self.git("status", "--porcelain"), "")

    def test_a_candidate_that_still_holds_fragments_is_refused(self):
        self.add_fragment()
        self.write("Cargo.toml", CARGO_TOML.replace("0.1.0", "0.2.0"))
        self.commit_all("bump to 0.2.0 without folding")

        result = self.release("0.2.0", "--no-push")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unfolded fragments remain", result.stderr)
        self.assertEqual(self.git("tag", "--list", "v0.2.0"), "")

    def test_a_failed_release_restores_the_changelog_and_its_fragments(self):
        self.add_fragment()
        self.commit_all("add a fragment")

        result = self.release("0.2.0", "--no-push", docs_check_fails=True)

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("a release step failed", result.stderr)
        self.assertEqual(self.git("status", "--porcelain"), "")
        self.assertEqual((self.repo / "CHANGELOG.md").read_text(), CHANGELOG)
        self.assertTrue((self.repo / "changelog.d" / "12-tailscale.md").exists())
        self.assertEqual((self.repo / "Cargo.toml").read_text(), CARGO_TOML)


if __name__ == "__main__":
    unittest.main()
