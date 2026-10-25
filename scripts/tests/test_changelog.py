"""Behavioural scenarios for scripts/changelog, run through its command line.

The fixtures under scripts/tests/fixtures/changelog are the changelog contract
(section 7 of the ecosystem contract, issue #1588): every implementation must
reproduce their outputs byte for byte. The invalid-input cases of the same
contract are written here as fragment bytes and must each be refused by
`check`, and by `fold`, which must then leave the tree untouched.
"""

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "scripts" / "changelog"
FIXTURES = Path(__file__).resolve().parent / "fixtures" / "changelog"

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
