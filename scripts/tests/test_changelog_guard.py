"""Scenarios for `scripts/changelog guard`: a documented behavior change needs a fragment.

The tests run the real script and real git in a throwaway repository; a pull
request is a branch off `main`, and `guard` compares it with the merge base.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

TOOL = Path(__file__).resolve().parents[1] / "changelog"

LABEL = "no-changelog"
LAYOUT = {
    "repository": "example/app",
    "changelog": "CHANGELOG.md",
    "guard": {"label": LABEL, "triggers": ["docs/knowledge/log.d/*.md", "docs/cli.md", "docs/public-api.md"]},
}
CHANGELOG = "# Changelog\n\nUser-facing changes of example/app.\n"
FRAGMENT = "---\nkind: added\n---\nText.\n"
LOG_FRAGMENT = "## Unreleased (2026-10-29, thing, #7)\n\n- Thing.\n"


class GuardScenarios(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.repo = Path(tmp.name)
        self.env = dict(
            {key: value for key, value in os.environ.items() if not key.startswith("GIT_")},
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_CONFIG_SYSTEM=os.devnull,
            GIT_AUTHOR_NAME="test",
            GIT_AUTHOR_EMAIL="test@example.invalid",
            GIT_COMMITTER_NAME="test",
            GIT_COMMITTER_EMAIL="test@example.invalid",
            GIT_CONFIG_COUNT="2",
            GIT_CONFIG_KEY_0="maintenance.auto",
            GIT_CONFIG_VALUE_0="false",
            GIT_CONFIG_KEY_1="gc.auto",
            GIT_CONFIG_VALUE_1="0",
        )
        self.write(".github/changelog.json", json.dumps(LAYOUT))
        self.write("CHANGELOG.md", CHANGELOG)
        self.write("docs/cli.md", "cli\n")
        self.write("docs/public-api.md", "api\n")
        self.write("src/lib.rs", "// code\n")
        self.write("changelog.d/.gitkeep", "")
        self.git("init", "-q", "-b", "main")
        self.commit("initial")
        self.git("checkout", "-q", "-b", "pr")

    def write(self, relative: str, text: str) -> None:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def git(self, *args: str) -> None:
        subprocess.run(["git", "-C", str(self.repo), *args], check=True, env=self.env, capture_output=True)

    def commit(self, message: str) -> None:
        self.git("add", "-A")
        self.git("-c", "commit.gpgsign=false", "commit", "-q", "-m", message)

    def guard(self, *labels: str, base: str = "main") -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(TOOL), "--root", str(self.repo), "guard", "--base", base, "--labels-json", json.dumps(list(labels))],
            capture_output=True, text=True, check=False, env=self.env,
        )

    def assert_passes(self, result: subprocess.CompletedProcess) -> None:
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("changelog guard: ok", result.stdout)


class DocumentedBehaviorChange(GuardScenarios):
    def test_a_change_to_no_documented_behavior_passes(self):
        self.write("src/lib.rs", "// changed\n")
        self.commit("code only")
        self.assert_passes(self.guard())

    def test_a_knowledge_log_fragment_without_a_changelog_fragment_fails_and_says_how_to_fix_it(self):
        self.write("docs/knowledge/log.d/7-thing.md", LOG_FRAGMENT)
        self.commit("behavior change")
        result = self.guard()
        self.assertEqual(result.returncode, 1)
        self.assertIn("docs/knowledge/log.d/7-thing.md", result.stderr)
        self.assertIn("changelog.d/<issue>-<slug>.md", result.stderr)
        self.assertIn(LABEL, result.stderr)

    def test_an_edit_of_the_cli_reference_without_a_fragment_fails(self):
        self.write("docs/cli.md", "cli, changed\n")
        self.commit("cli change")
        result = self.guard()
        self.assertEqual(result.returncode, 1)
        self.assertIn("docs/cli.md", result.stderr)

    def test_an_edit_of_the_public_api_reference_without_a_fragment_fails(self):
        self.write("docs/public-api.md", "api, changed\n")
        self.commit("api change")
        result = self.guard()
        self.assertEqual(result.returncode, 1)
        self.assertIn("docs/public-api.md", result.stderr)

    def test_an_added_changelog_fragment_satisfies_the_guard(self):
        self.write("docs/knowledge/log.d/7-thing.md", LOG_FRAGMENT)
        self.write("changelog.d/7-thing.md", FRAGMENT)
        self.commit("behavior change with its fragment")
        self.assert_passes(self.guard())

    def merge_to_main(self):
        self.git("checkout", "-q", "main")
        self.git("merge", "-q", "--ff-only", "pr")

    def test_a_renamed_changelog_fragment_does_not_count_as_a_new_entry(self):
        self.write("changelog.d/7-old-name.md", FRAGMENT)
        self.commit("fragment on main")
        self.merge_to_main()
        self.git("checkout", "-q", "-b", "pr2")
        self.git("mv", "changelog.d/7-old-name.md", "changelog.d/7-new-name.md")
        self.write("docs/cli.md", "cli, changed\n")
        self.commit("rename and change")
        self.assertEqual(self.guard().returncode, 1)

    def test_an_edited_existing_changelog_fragment_does_not_count_as_a_new_entry(self):
        self.write("changelog.d/7-old.md", FRAGMENT)
        self.commit("fragment on main")
        self.merge_to_main()
        self.git("checkout", "-q", "-b", "pr2")
        self.write("changelog.d/7-old.md", FRAGMENT + "More text.\n")
        self.write("docs/cli.md", "cli, changed\n")
        self.commit("edit the fragment and the docs")
        self.assertEqual(self.guard().returncode, 1)

    def test_a_file_renamed_into_a_trigger_path_is_a_trigger(self):
        self.write("notes/thing.md", "A longer note so that git detects the rename. " * 20)
        self.commit("note on main")
        self.merge_to_main()
        self.git("checkout", "-q", "-b", "pr2")
        (self.repo / "docs/knowledge/log.d").mkdir(parents=True)
        self.git("mv", "notes/thing.md", "docs/knowledge/log.d/7-thing.md")
        self.commit("move the note into the knowledge log")
        result = self.guard()
        self.assertEqual(result.returncode, 1)
        self.assertIn("docs/knowledge/log.d/7-thing.md", result.stderr)

    def test_a_file_renamed_into_changelog_d_counts_as_a_new_entry(self):
        self.write("notes/thing.md", FRAGMENT * 5)
        self.commit("note on main")
        self.merge_to_main()
        self.git("checkout", "-q", "-b", "pr2")
        self.git("mv", "notes/thing.md", "changelog.d/7-thing.md")
        self.write("docs/cli.md", "cli, changed\n")
        self.commit("move the note in as a fragment")
        self.assert_passes(self.guard())

    def test_a_deleted_fragment_does_not_satisfy_the_guard(self):
        self.write("changelog.d/7-old.md", FRAGMENT)
        self.commit("fragment on main")
        self.merge_to_main()
        self.git("checkout", "-q", "-b", "pr2")
        self.git("rm", "-q", "changelog.d/7-old.md")
        self.write("docs/cli.md", "cli, changed\n")
        self.commit("delete the fragment, change the docs")
        self.assertEqual(self.guard().returncode, 1)

    def test_a_changed_changelog_satisfies_the_guard(self):
        self.write("docs/cli.md", "cli, changed\n")
        self.write("CHANGELOG.md", CHANGELOG + "\n## 1.0.0\n\n### Added\n\n- Thing.\n")
        self.commit("changelog written by hand")
        self.assert_passes(self.guard())

    def test_the_exemption_label_satisfies_the_guard(self):
        self.write("docs/knowledge/log.d/7-thing.md", LOG_FRAGMENT)
        self.commit("behavior change")
        self.assert_passes(self.guard(LABEL))

    def test_a_label_that_only_starts_with_the_exemption_does_not_exempt(self):
        self.write("docs/knowledge/log.d/7-thing.md", LOG_FRAGMENT)
        self.commit("behavior change")
        self.assertEqual(self.guard(LABEL + "-x", "other").returncode, 1)

    def test_a_release_commit_that_removes_log_fragments_is_not_a_trigger(self):
        self.write("docs/knowledge/log.d/7-thing.md", LOG_FRAGMENT)
        self.commit("fragment on main")
        self.merge_to_main()
        self.git("checkout", "-q", "-b", "release")
        self.git("rm", "-q", "docs/knowledge/log.d/7-thing.md")
        self.commit("Release v1.0.0")
        self.assert_passes(self.guard())


class RefusedInput(GuardScenarios):
    def test_a_repository_without_a_guard_declaration_is_refused(self):
        layout = {key: value for key, value in LAYOUT.items() if key != "guard"}
        self.write(".github/changelog.json", json.dumps(layout))
        result = self.guard()
        self.assertEqual(result.returncode, 1)
        self.assertIn("declare guard", result.stderr)

    def test_a_guard_without_triggers_is_refused(self):
        layout = dict(LAYOUT, guard={"label": LABEL, "triggers": []})
        self.write(".github/changelog.json", json.dumps(layout))
        result = self.guard()
        self.assertEqual(result.returncode, 1)
        self.assertIn("guard.triggers", result.stderr)

    def test_a_guard_without_a_label_is_refused(self):
        layout = dict(LAYOUT, guard={"triggers": ["docs/cli.md"]})
        self.write(".github/changelog.json", json.dumps(layout))
        self.assertEqual(self.guard().returncode, 1)

    def test_an_unknown_base_revision_fails_instead_of_passing(self):
        result = self.guard(base="0123456789abcdef0123456789abcdef01234567")
        self.assertEqual(result.returncode, 1)
        self.assertIn("cannot diff", result.stderr)

    def test_labels_that_are_not_a_json_array_of_names_are_refused(self):
        result = subprocess.run(
            [sys.executable, str(TOOL), "--root", str(self.repo), "guard", "--base", "main", "--labels-json", '{"a": 1}'],
            capture_output=True, text=True, check=False, env=self.env,
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("--labels-json", result.stderr)


if __name__ == "__main__":
    unittest.main()
