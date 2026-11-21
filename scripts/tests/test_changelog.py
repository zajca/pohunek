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


class IssueLink(ChangelogScenarios):
    """A folded entry carries its issue link once, whether or not the fragment ends with it."""

    LINK = "([#7](https://github.com/example/app/issues/7))"

    def folded_section(self, *fragments: tuple[str, str]) -> str:
        root = self.copy_input("single")
        for name in self.fragments(root):
            (root / "changelog.d" / name).unlink()
        for name, text in fragments:
            (root / "changelog.d" / name).write_text(f"---\nkind: added\n---\n{text}\n")
        fold = run(root, "fold", "0.37.0", "--date", "2026-10-24")
        self.assertEqual(fold.returncode, 0, fold.stderr)
        return (root / "CHANGELOG.md").read_text()

    def test_a_fragment_with_and_without_its_own_link_ends_with_it_once(self):
        section = self.folded_section(
            ("7-bare.md", "Bare entry."),
            ("7-linked.md", f"Linked entry. {self.LINK}"),
            ("7-wrapped.md", f"Wrapped entry.\n{self.LINK}"),
        )
        self.assertIn(f"- Bare entry. {self.LINK}\n", section)
        self.assertIn(f"- Linked entry. {self.LINK}\n", section)
        self.assertIn(f"- Wrapped entry. {self.LINK}\n", section)
        self.assertEqual(section.count(self.LINK), 3)

    def test_a_fragment_citing_several_issues_ends_with_its_own_link_once(self):
        other = "[#8](https://github.com/example/app/issues/8)"
        own = self.LINK[1:-1]
        section = self.folded_section(("7-both.md", f"Entry. ({other}, {own})"))
        self.assertIn(f"- Entry. ({other}, {own})\n", section)

    def test_an_own_link_in_any_position_is_not_repeated(self):
        other = "[#8](https://github.com/example/app/issues/8)"
        own = self.LINK[1:-1]
        cases = {
            "7-first.md": f"Entry. ({own}, {other})",
            "7-bare.md": f"Entry. {own}",
            "7-period.md": f"Entry. ({own}).",
            "7-middle.md": f"Entry {own} continues.",
        }
        section = self.folded_section(*((name, text) for name, text in cases.items()))
        for name, text in cases.items():
            with self.subTest(fragment=name):
                self.assertIn(f"- {text}\n", section)
        self.assertEqual(section.count(own), len(cases))

    def test_a_link_to_another_issue_does_not_replace_the_entry_link(self):
        other = "([#8](https://github.com/example/app/issues/8))"
        section = self.folded_section(("7-other.md", f"Entry. {other}"))
        self.assertIn(f"- Entry. {other} {self.LINK}\n", section)

    def test_a_link_to_an_issue_sharing_the_number_prefix_does_not_replace_the_entry_link(self):
        other = "([#70](https://github.com/example/app/issues/70))"
        section = self.folded_section(("7-prefix.md", f"Entry. {other}"))
        self.assertIn(f"- Entry. {other} {self.LINK}\n", section)

    def test_notes_post_of_a_linked_fragment_has_no_doubled_link(self):
        root = self.copy_input("single")
        for name in self.fragments(root):
            (root / "changelog.d" / name).unlink()
        (root / "changelog.d" / "7-linked.md").write_text(f"---\nkind: added\n---\nLinked entry. {self.LINK}\n")
        self.assertEqual(run(root, "fold", "0.37.0", "--date", "2026-10-24").returncode, 0)
        post = run(root, "notes", "0.37.0", "--post")
        self.assertEqual(post.returncode, 0, post.stderr)
        self.assertEqual(post.stdout.decode().count(self.LINK), 1)


class ReleasePost(ChangelogScenarios):
    """`notes --post`: the skeleton a person turns into the release post.

    The website keeps only what sits between the markers, so the skeleton must
    hold exactly one marker pair around the whole post and the full section.
    """

    def folded(self, fixture: str, *fold_args: str) -> Path:
        root = self.copy_input(fixture)
        fold = run(root, "fold", *fold_args)
        self.assertEqual(fold.returncode, 0, fold.stderr)
        return root

    def assert_one_marked_block(self, output: bytes) -> list[bytes]:
        lines = output.split(b"\n")
        self.assertEqual(lines.count(b"<!-- pohunek-changelog:start -->"), 1)
        self.assertEqual(lines.count(b"<!-- pohunek-changelog:end -->"), 1)
        self.assertEqual(lines[0], b"<!-- pohunek-changelog:start -->")
        self.assertEqual(lines[-2:], [b"<!-- pohunek-changelog:end -->", b""])
        return lines[1:-2]

    def test_post_matches_the_contract_fixture(self):
        root = self.folded("single", "0.37.0", "--date", "2026-10-24")
        post = run(root, "notes", "0.37.0", "--post")
        self.assertEqual(post.returncode, 0, post.stderr)
        self.assertEqual(post.stdout, (FIXTURES / "single" / "expected-0.37.0" / "post.md").read_bytes())
        self.assert_one_marked_block(post.stdout)

    def test_post_of_a_release_without_user_facing_changes(self):
        root = self.folded("single", "0.37.0", "--date", "2026-10-24")
        fold = run(root, "fold", "0.37.1", "--date", "2026-10-25")
        self.assertEqual(fold.returncode, 0, fold.stderr)
        post = run(root, "notes", "0.37.1", "--post")
        self.assertEqual(post.returncode, 0, post.stderr)
        block = self.assert_one_marked_block(post.stdout)
        self.assertEqual(block[-3:], [b"### Full changelog", b"", b"No user-facing changes."])

    def test_post_follows_the_surface_of_a_multi_surface_repository(self):
        root = self.folded("multi", "0.8.0", "--surface", "plugin", "--date", "2026-10-24")
        post = run(root, "notes", "0.8.0", "--surface", "plugin", "--post")
        self.assertEqual(post.returncode, 0, post.stderr)
        block = b"\n".join(self.assert_one_marked_block(post.stdout))
        self.assertIn(b"\n#### Changed\n\n- Both read the host list from `hosts.toml`.", block)
        self.assertIn(b"\n#### Fixed\n\n- `pohunek-work setup` keeps", block)

    def test_post_keeps_the_release_rules(self):
        root = self.copy_input("single")
        result = run(root, "notes", "0.36.0", "--post")
        self.assertEqual(result.returncode, 1)
        self.assertIn(b"unfolded fragments remain", result.stderr)
        self.assertEqual(result.stdout, b"")


if __name__ == "__main__":
    unittest.main()
