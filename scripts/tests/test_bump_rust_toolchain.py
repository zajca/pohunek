"""Integration scenarios for the `scripts/bump-rust-toolchain` CLI (stdlib only).

Each test runs the actual script as a child process of a disposable
repository root (the script copy, `rust-toolchain.toml`, workflows naming a
dated nightly, and the composite action that only validates one) and checks
the files it leaves behind byte for byte. No network, toolchain, or real
repository file is touched.
"""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

REPO_SCRIPT = Path(__file__).resolve().parents[1] / "bump-rust-toolchain"

TOOLCHAIN = (
    "# The exact Rust toolchain for CI and local development.\n"
    "[toolchain]\n"
    'channel = "1.99.0"\n'
    'components = ["clippy", "rustfmt"]\n'
    'profile = "minimal"\n'
)
CI_WORKFLOW = (
    "jobs:\n"
    "  udeps:\n"
    "    steps:\n"
    "      - uses: ./.github/actions/setup-rust\n"
    "        with:\n"
    "          toolchain: nightly-2026-10-11\n"
    "      # nightly-2026-10-11 named in a comment is not a toolchain input\n"
    "      - run: cargo udeps --workspace\n"
    "  lint:\n"
    "    steps:\n"
    "      - uses: ./.github/actions/setup-rust\n"
    "        with:\n"
    "          components: clippy\n"
)
SWEEP_WORKFLOW = (
    "jobs:\n"
    "  sweep:\n"
    "    steps:\n"
    "      - uses: ./.github/actions/setup-rust\n"
    "        with:\n"
    "          toolchain: 'nightly-2026-09-30'  # quoted literal\n"
)
ACTION = (
    "inputs:\n"
    "  toolchain:\n"
    "    description: empty, or a dated nightly-YYYY-MM-DD\n"
    "runs:\n"
    "  steps:\n"
    "    - run: '[[ \"$T\" =~ ^nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]]'\n"
)


class BumpRustToolchainTests(unittest.TestCase):
    """Run the real `bump-rust-toolchain` script against a disposable tree."""

    def setUp(self):
        self.make_tree()

    def make_tree(self):
        """A fresh disposable repository; each call starts a new one."""
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name) / "repo"
        workflows = self.root / ".github" / "workflows"
        workflows.mkdir(parents=True)
        (self.root / ".github" / "actions" / "setup-rust").mkdir(parents=True)
        (self.root / "scripts").mkdir()
        script = self.root / "scripts" / "bump-rust-toolchain"
        script.write_text(REPO_SCRIPT.read_text())
        script.chmod(0o755)
        self.toolchain = self.root / "rust-toolchain.toml"
        self.ci = workflows / "ci.yml"
        self.sweep = workflows / "sweep.yml"
        self.action = self.root / ".github" / "actions" / "setup-rust" / "action.yml"
        self.toolchain.write_text(TOOLCHAIN)
        self.ci.write_text(CI_WORKFLOW)
        self.sweep.write_text(SWEEP_WORKFLOW)
        self.action.write_text(ACTION)

    def run_script(self, *args):
        completed = subprocess.run(
            [sys.executable, str(self.root / "scripts" / "bump-rust-toolchain"), *args],
            cwd=self.root,
            env={"PATH": "/usr/bin:/bin"},
            capture_output=True,
            text=True,
            timeout=60,
        )
        return completed.returncode, completed.stdout, completed.stderr

    def snapshot(self):
        return {
            path: path.read_bytes()
            for path in (self.toolchain, self.ci, self.sweep, self.action)
        }

    def test_bump_stable_rewrites_only_the_channel(self):
        status, out, err = self.run_script("--stable", "1.100.0")
        self.assertEqual(status, 0, out + err)
        self.assertEqual(
            self.toolchain.read_text(),
            TOOLCHAIN.replace('"1.99.0"', '"1.100.0"'),
        )
        self.assertEqual(self.ci.read_text(), CI_WORKFLOW)
        self.assertEqual(self.sweep.read_text(), SWEEP_WORKFLOW)
        self.assertIn("rust-toolchain.toml: channel 1.99.0 -> 1.100.0", out)
        self.assertIn("rustup toolchain install   # in the repository root", out)
        self.assertIn("cargo clippy --workspace --all-targets --all-features -- -D warnings", out)
        self.assertIn("scripts/check-rust-toolchain-pin", out)
        self.assertIn("cargo xtask docs check", out)
        self.assertNotIn("udeps", out)

    def test_bump_nightly_rewrites_every_dated_toolchain_input(self):
        status, out, err = self.run_script("--nightly", "2026-11-02")
        self.assertEqual(status, 0, out + err)
        self.assertEqual(
            self.ci.read_text(),
            CI_WORKFLOW.replace(
                "toolchain: nightly-2026-10-11", "toolchain: nightly-2026-11-02"
            ),
        )
        # The comment mention is not an input and stays as it was.
        self.assertIn("# nightly-2026-10-11 named in a comment", self.ci.read_text())
        self.assertEqual(
            self.sweep.read_text(),
            SWEEP_WORKFLOW.replace("nightly-2026-09-30", "nightly-2026-11-02"),
        )
        self.assertEqual(self.toolchain.read_text(), TOOLCHAIN)
        self.assertEqual(self.action.read_text(), ACTION)
        self.assertIn(
            ".github/workflows/ci.yml:6: nightly-2026-10-11 -> nightly-2026-11-02", out
        )
        self.assertIn(
            ".github/workflows/sweep.yml:6: nightly-2026-09-30 -> nightly-2026-11-02", out
        )
        self.assertIn("rustup toolchain install nightly-2026-11-02", out)
        self.assertIn(
            "cargo +nightly-2026-11-02 udeps --workspace --all-targets --all-features", out
        )

    def test_channel_line_is_written_in_the_exact_form_the_ci_action_reads(self):
        # Trailing text after the quote (a comment) is dropped: the CI action
        # accepts only `channel = "X.Y.Z"` with nothing after the quote.
        self.toolchain.write_text(TOOLCHAIN.replace(
            'channel = "1.99.0"\n', '  channel = "1.99.0"  # the CI pin\n'
        ))
        status, out, err = self.run_script("--stable", "1.100.0")
        self.assertEqual(status, 0, out + err)
        self.assertEqual(
            self.toolchain.read_text(),
            TOOLCHAIN.replace('channel = "1.99.0"', 'channel = "1.100.0"'),
        )
        self.assertIn("channel line rewritten as exactly `channel = \"1.100.0\"`", out)

        # The same release with a non-exact line is a change, not a no-op.
        self.toolchain.write_text(TOOLCHAIN.replace(
            'channel = "1.99.0"\n', 'channel = "1.99.0" # pin\n'
        ))
        status, out, err = self.run_script("--stable", "1.99.0")
        self.assertEqual(status, 0, out + err)
        self.assertNotIn("no change", out)
        self.assertEqual(self.toolchain.read_text(), TOOLCHAIN)
        status, out, err = self.run_script("--stable", "1.99.0")
        self.assertIn("no change", out)

    def test_bump_both_together(self):
        status, out, err = self.run_script("--stable", "1.100.1", "--nightly", "2026-11-02")
        self.assertEqual(status, 0, out + err)
        self.assertIn('channel = "1.100.1"', self.toolchain.read_text())
        self.assertIn("toolchain: nightly-2026-11-02", self.ci.read_text())
        self.assertIn("nightly-2026-11-02", self.sweep.read_text())

    def test_invalid_formats_are_rejected_without_writing(self):
        before = self.snapshot()
        cases = [
            (),
            ("--stable", "1.100"),
            ("--stable", "stable"),
            ("--stable", "1.100.0-beta.1"),
            ("--stable", "01.100.0"),
            ("--nightly", "2026-13-40"),
            ("--nightly", "2026-02-30"),
            ("--nightly", "nightly-2026-10-11"),
            ("--nightly", "20261011"),
            # A valid stable is not written when the nightly is invalid.
            ("--stable", "1.100.0", "--nightly", "2026-1-2"),
        ]
        for args in cases:
            with self.subTest(args=args):
                status, out, err = self.run_script(*args)
                self.assertEqual(status, 2, out + err)
                self.assertTrue(err.strip(), "a rejection explains itself")
                self.assertEqual(self.snapshot(), before)

    def test_repository_inputs_that_cannot_be_rewritten_leave_the_tree_untouched(self):
        def drop_nightly_inputs():
            self.ci.write_text(CI_WORKFLOW.replace("toolchain: nightly-2026-10-11", "toolchain: ''"))
            self.sweep.unlink()

        def drop_channel():
            self.toolchain.write_text('[toolchain]\nprofile = "minimal"\n')

        def drop_toolchain_file():
            self.toolchain.unlink()

        cases = {
            # A valid stable bump is not written when no workflow names a nightly.
            "no nightly input": (
                drop_nightly_inputs,
                ("--stable", "1.100.0", "--nightly", "2026-11-02"),
                "no `toolchain: nightly-YYYY-MM-DD` input",
            ),
            "no channel line": (
                drop_channel, ("--stable", "1.100.0"), "expected exactly one `channel",
            ),
            "missing toolchain file": (
                drop_toolchain_file, ("--stable", "1.100.0"), "cannot read",
            ),
        }
        for case, (break_tree, args, message) in cases.items():
            with self.subTest(case):
                self.make_tree()
                break_tree()
                before = {
                    path: path.read_bytes()
                    for path in (self.toolchain, self.ci, self.sweep, self.action)
                    if path.exists()
                }
                status, out, err = self.run_script(*args)
                self.assertEqual(status, 2, out + err)
                self.assertIn(message, err)
                self.assertEqual({path: path.read_bytes() for path in before}, before)

    def test_second_run_reports_no_change(self):
        args = ("--stable", "1.100.0", "--nightly", "2026-11-02")
        status, out, err = self.run_script(*args)
        self.assertEqual(status, 0, out + err)
        bumped = self.snapshot()
        status, out, err = self.run_script(*args)
        self.assertEqual(status, 0, out + err)
        self.assertIn("no change", out)
        self.assertNotIn("->", out)
        self.assertEqual(self.snapshot(), bumped)

    def test_file_mode_is_kept_and_no_temp_files_remain(self):
        self.toolchain.chmod(0o640)
        status, out, err = self.run_script("--stable", "1.100.0")
        self.assertEqual(status, 0, out + err)
        self.assertEqual(self.toolchain.stat().st_mode & 0o777, 0o640)
        leftovers = [p.name for p in self.root.iterdir() if p.name.startswith(".rust-toolchain")]
        self.assertEqual(leftovers, [])


if __name__ == "__main__":
    unittest.main()
