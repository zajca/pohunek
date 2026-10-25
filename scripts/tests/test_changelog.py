"""Behavioural scenarios for scripts/changelog, run through its command line.

The fixtures under scripts/tests/fixtures/changelog are the changelog contract
(section 7 of the ecosystem contract, issue #1588): every implementation must
reproduce their outputs byte for byte. The invalid-input cases of the same
contract are written here as fragment bytes and must each be refused by
`check`, and by `fold`, which must then leave the tree untouched.
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "scripts" / "changelog"
FIXTURES = Path(__file__).resolve().parent / "fixtures" / "changelog"
CHECK_WORKFLOW = ROOT / ".github" / "workflows" / "changelog.yml"
CHECKOUT = "actions/checkout@11d5960a326750d5838078e36cf38b85af677262"

VALID_BODY = "---\nkind: added\n---\nText.\n"


def run(root: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(TOOL), "--root", str(root), *args],
        capture_output=True,
        check=False,
    )


class ChangelogScenarios(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)

    def copy_input(self, fixture: str, name: str | None = None) -> Path:
        root = self.tmp / (name or fixture)
        shutil.copytree(FIXTURES / fixture / "input", root)
        return root

    def fragments(self, root: Path) -> list[str]:
        return sorted(p.name for p in (root / "changelog.d").iterdir() if not p.name.startswith("."))


class ContractFixtures(ChangelogScenarios):
    def test_single_product_fold_and_notes_match_the_contract(self):
        root = self.copy_input("single")
        expected = FIXTURES / "single"

        check = run(root, "check")
        self.assertEqual(check.returncode, 0, check.stderr)

        fold = run(root, "fold", "0.37.0", "--date", "2026-10-24")
        self.assertEqual(fold.returncode, 0, fold.stderr)
        self.assertEqual(
            (root / "CHANGELOG.md").read_bytes(),
            (expected / "expected-0.37.0" / "CHANGELOG.md").read_bytes(),
        )
        self.assertEqual(self.fragments(root), [])
        notes = run(root, "notes", "0.37.0")
        self.assertEqual(notes.returncode, 0, notes.stderr)
        self.assertEqual(notes.stdout, (expected / "expected-0.37.0" / "notes.md").read_bytes())

        fold = run(root, "fold", "0.37.1", "--date", "2026-10-25")
        self.assertEqual(fold.returncode, 0, fold.stderr)
        self.assertEqual(
            (root / "CHANGELOG.md").read_bytes(),
            (expected / "expected-0.37.1" / "CHANGELOG.md").read_bytes(),
        )
        notes = run(root, "notes", "0.37.1")
        self.assertEqual(notes.returncode, 0, notes.stderr)
        self.assertEqual(notes.stdout, (expected / "expected-0.37.1" / "notes.md").read_bytes())

    def test_multi_surface_fold_releases_one_surface_and_keeps_the_others(self):
        root = self.copy_input("multi")
        expected = FIXTURES / "multi" / "expected-plugin-0.8.0"
        untouched_web = (root / "web" / "CHANGELOG.md").read_bytes()

        fold = run(root, "fold", "0.8.0", "--surface", "plugin", "--date", "2026-10-24")
        self.assertEqual(fold.returncode, 0, fold.stderr)
        self.assertEqual(
            (root / "plugin" / "CHANGELOG.md").read_bytes(),
            (expected / "plugin" / "CHANGELOG.md").read_bytes(),
        )
        self.assertEqual(
            (root / "web" / "CHANGELOG.md").read_bytes(),
            untouched_web,
        )
        self.assertEqual(self.fragments(root), ["31-shared-hosts.md", "32-web-dark-mode.md"])
        for name in self.fragments(root):
            with self.subTest(fragment=name):
                self.assertEqual(
                    (root / "changelog.d" / name).read_bytes(),
                    (expected / "changelog.d" / name).read_bytes(),
                )

    def test_the_repository_tree_passes_check(self):
        result = run(ROOT, "check")
        self.assertEqual(result.returncode, 0, result.stderr)


class InvalidFragments(ChangelogScenarios):
    # One case per invalid input the contract lists. Each file is written
    # into a copy of the single-product input, which has a valid changelog.
    CASES = {
        "no opening rule": ("12-a.md", b"kind: added\n---\nText.\n"),
        "no closing rule": ("12-a.md", b"---\nkind: added\nText.\n"),
        "unknown key": ("12-a.md", b"---\nkind: added\ncolour: red\n---\nText.\n"),
        "duplicate key": ("12-a.md", b"---\nkind: added\nkind: fixed\n---\nText.\n"),
        "missing kind": ("12-a.md", b"---\nbreaking: true\n---\nText.\n"),
        "unknown kind": ("12-a.md", b"---\nkind: improved\n---\nText.\n"),
        "breaking other than true or false": ("12-a.md", b"---\nkind: added\nbreaking: yes\n---\nText.\n"),
        "empty body": ("12-a.md", b"---\nkind: added\n---\n\n"),
        "blank line in the body": ("12-a.md", b"---\nkind: added\n---\nFirst.\n\nSecond.\n"),
        "body line starting with a heading marker": ("12-a.md", b"---\nkind: added\n---\n# Heading\n"),
        "file name without a slug": ("x-foo.md", VALID_BODY.encode()),
        "file name with a leading zero": ("012-foo.md", VALID_BODY.encode()),
        "file name with upper case": ("12-Foo.md", VALID_BODY.encode()),
        "file name with an underscore": ("12_foo.md", VALID_BODY.encode()),
        "surfaces in a single-product repository": (
            "12-a.md",
            b"---\nkind: added\nsurfaces: web\n---\nText.\n",
        ),
        "CRLF line endings": ("12-a.md", b"---\r\nkind: added\r\n---\r\nText.\r\n"),
        "byte order mark": ("12-a.md", b"\xef\xbb\xbf" + VALID_BODY.encode()),
    }

    def test_check_refuses_each_invalid_fragment(self):
        for case, (name, content) in self.CASES.items():
            with self.subTest(case=case):
                root = self.copy_input("single", name=case.replace(" ", "-"))
                (root / "changelog.d" / name).write_bytes(content)
                result = run(root, "check")
                self.assertEqual(result.returncode, 1)
                self.assertIn(b"changelog: changelog.d/", result.stderr)

    def test_fold_writes_nothing_when_a_fragment_is_invalid(self):
        root = self.copy_input("single")
        (root / "changelog.d" / "12-a.md").write_bytes(b"---\nkind: improved\n---\nText.\n")
        before = (root / "CHANGELOG.md").read_bytes()
        fragments_before = self.fragments(root)

        result = run(root, "fold", "0.37.0", "--date", "2026-10-24")

        self.assertEqual(result.returncode, 1)
        self.assertEqual((root / "CHANGELOG.md").read_bytes(), before)
        self.assertEqual(self.fragments(root), fragments_before)

    def test_multi_surface_declarations_are_checked(self):
        cases = {
            "missing surfaces": b"---\nkind: added\n---\nText.\n",
            "unknown surface": b"---\nkind: added\nsurfaces: mobile\n---\nText.\n",
            "duplicate surface": b"---\nkind: added\nsurfaces: web, web\n---\nText.\n",
        }
        for case, content in cases.items():
            with self.subTest(case=case):
                root = self.copy_input("multi", name=case.replace(" ", "-"))
                (root / "changelog.d" / "40-case.md").write_bytes(content)
                result = run(root, "check")
                self.assertEqual(result.returncode, 1)
                self.assertIn(b"changelog: changelog.d/40-case.md", result.stderr)


def run_script_lines(text: str) -> list[str]:
    """The lines of every `run:` script of a workflow, inline or block."""
    kept, indent = [], None
    for line in text.splitlines():
        if indent is not None:
            if not line.strip() or len(line) - len(line.lstrip()) > indent:
                kept.append(line)
                continue
            indent = None
        if re.fullmatch(r"\s+(- )?run: \|", line):
            indent = len(line) - len(line.lstrip())
        elif re.fullmatch(r"\s+(- )?run: .+", line):
            kept.append(line)
    return kept


class CheckWorkflow(unittest.TestCase):
    """The pull-request check: a read-only `pull_request` run that trusts no event value in a script."""

    def setUp(self):
        self.text = CHECK_WORKFLOW.read_text()

    def test_it_runs_on_pull_requests_and_the_label_events_only(self):
        self.assertIn(
            "on:\n  pull_request:\n    types: [opened, synchronize, reopened, labeled, unlabeled, ready_for_review]\n",
            self.text,
        )
        self.assertNotRegex(self.text, r"(?m)^\s*pull_request_target:")

    def test_the_token_is_read_only_and_the_checkout_is_pinned(self):
        self.assertIn("\npermissions: {}\n", self.text)
        self.assertEqual(self.text.count("      contents: read\n"), 1)
        self.assertEqual(re.findall(r"uses: (\S+)", self.text), [CHECKOUT])
        self.assertIn("persist-credentials: false", self.text)

    def test_scripts_read_event_values_only_through_the_environment(self):
        scripts = "\n".join(run_script_lines(self.text))
        self.assertNotIn("${{", scripts)
        self.assertIn("python3 scripts/changelog check", scripts)
        self.assertIn('"$BASE_SHA...$HEAD_SHA"', scripts)
        self.assertIn('"$NO_CHANGELOG" = true', scripts)


class FragmentRequirement(unittest.TestCase):
    """The pull-request check's fragment rule, run as the workflow runs it in a real history."""

    def script(self) -> str:
        text = CHECK_WORKFLOW.read_text()
        block = text.split("      - name: Require a fragment or the no-changelog label\n", 1)[1]
        body = block.split("        run: |\n", 1)[1].split("\n\n", 1)[0]
        return "\n".join(line[10:] for line in body.splitlines())

    def repo_with(self, *paths: str) -> tuple[Path, str, str]:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        env = {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_SYSTEM": "/dev/null",
               "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
               "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid",
               "PATH": os.environ["PATH"]}
        self.env = env

        def git(*args: str) -> str:
            return subprocess.run(["git", "-C", str(root), *args], check=True, env=env,
                                  capture_output=True, text=True).stdout.strip()

        git("init", "-q", "-b", "main")
        (root / "README.md").write_text("base\n")
        git("add", "-A")
        git("-c", "commit.gpgsign=false", "commit", "-q", "-m", "base")
        base = git("rev-parse", "HEAD")
        for path in paths:
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_text("---\nkind: added\n---\nText.\n")
        git("add", "-A")
        git("-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", "head")
        return root, base, git("rev-parse", "HEAD")

    def run_rule(self, paths: tuple[str, ...], no_changelog: str = "false") -> int:
        root, base, head = self.repo_with(*paths)
        env = dict(self.env, BASE_SHA=base, HEAD_SHA=head, NO_CHANGELOG=no_changelog)
        result = subprocess.run(["bash", "-c", self.script()], cwd=root, env=env,
                                capture_output=True, text=True, check=False)
        return result.returncode

    def test_a_valid_fragment_satisfies_the_rule(self):
        self.assertEqual(self.run_rule(("changelog.d/12-tailscale-overlay.md",)), 0)

    def test_a_dotfile_does_not_satisfy_the_rule(self):
        self.assertEqual(self.run_rule(("changelog.d/.hidden.md",)), 1)

    def test_a_name_the_tool_rejects_does_not_satisfy_the_rule(self):
        self.assertEqual(self.run_rule(("changelog.d/x-foo.md",)), 1)
        self.assertEqual(self.run_rule(("changelog.d/12-Foo.md",)), 1)

    def test_the_label_waives_the_rule(self):
        self.assertEqual(self.run_rule((), no_changelog="true"), 0)


class ReleaseRules(ChangelogScenarios):
    def test_fold_refuses_a_version_that_already_has_a_section(self):
        root = self.copy_input("single")
        result = run(root, "fold", "0.36.0", "--date", "2026-10-24")
        self.assertEqual(result.returncode, 1)
        self.assertIn(b"already exists", result.stderr)

    def test_notes_refuse_fragments_that_were_not_folded(self):
        root = self.copy_input("single")
        result = run(root, "notes", "0.36.0")
        self.assertEqual(result.returncode, 1)
        self.assertIn(b"unfolded fragments remain", result.stderr)

    def test_notes_refuse_a_version_without_a_section(self):
        root = self.copy_input("single")
        for fragment in (root / "changelog.d").iterdir():
            fragment.unlink()
        result = run(root, "notes", "0.99.0")
        self.assertEqual(result.returncode, 1)
        self.assertIn(b"no section for 0.99.0", result.stderr)

    def test_fold_never_creates_a_missing_changelog(self):
        root = self.copy_input("single")
        (root / "CHANGELOG.md").unlink()
        result = run(root, "fold", "0.37.0", "--date", "2026-10-24")
        self.assertEqual(result.returncode, 1)
        self.assertFalse((root / "CHANGELOG.md").exists())

    def test_the_surface_argument_follows_the_repository_layout(self):
        single = self.copy_input("single")
        self.assertEqual(run(single, "fold", "0.37.0", "--surface", "web").returncode, 1)
        multi = self.copy_input("multi")
        self.assertEqual(run(multi, "fold", "0.8.0", "--date", "2026-10-24").returncode, 1)
        self.assertEqual(run(multi, "fold", "0.8.0", "--surface", "mobile").returncode, 1)

    def test_only_release_and_prerelease_versions_are_accepted(self):
        root = self.copy_input("single")
        for version in ("0.37", "v0.37.0", "0.37.0-rc.1", "0.37.0-pre"):
            with self.subTest(version=version):
                self.assertEqual(run(root, "fold", version, "--date", "2026-10-24").returncode, 1)
        result = run(root, "fold", "0.38.0-pre.3", "--date", "2026-10-24")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_the_date_must_be_a_real_calendar_date(self):
        root = self.copy_input("single")
        self.assertEqual(run(root, "fold", "0.37.0", "--date", "2026-13-01").returncode, 1)
        self.assertEqual(run(root, "fold", "0.37.0", "--date", "2026-10-24T00:00").returncode, 1)


if __name__ == "__main__":
    unittest.main()
