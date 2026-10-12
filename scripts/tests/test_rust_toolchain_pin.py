"""Scenarios for the Rust toolchain pin: the scan gate and the setup-rust action.

The gate scenarios run the real `scripts/check-rust-toolchain-pin` as a
process against fixture repository trees in a temporary directory, plus once
against this repository. The action scenarios run the resolve step of the real
`.github/actions/setup-rust/action.yml` (extracted from the file, executed by
bash) against a fixture checkout, with the GitHub output and env files the
runner would provide. Stdlib only.
"""

from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check-rust-toolchain-pin"
ACTION = ROOT / ".github" / "actions" / "setup-rust" / "action.yml"
ACTION_RELATIVE = Path(".github") / "actions" / "setup-rust" / "action.yml"
WORKFLOW_RELATIVE = Path(".github") / "workflows" / "ci.yml"

EXIT_CLEAN = 0
EXIT_VIOLATIONS = 1
EXIT_CONFIG = 2
# Mirrors MAX_REPORTED_VIOLATIONS in the script: the report is bounded.
MAX_REPORTED_VIOLATIONS = 50
# Upper bound for one process run; both are local and finish in well under 1 s.
RUN_TIMEOUT_SECONDS = 60

PIN = """[toolchain]
channel = "1.99.0"
components = ["clippy", "rustfmt"]
profile = "minimal"
"""

CLEAN_WORKFLOW = """name: CI
on: [push]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install Rust toolchain
        uses: ./.github/actions/setup-rust
        with:
          components: rustfmt, clippy
      - run: cargo clippy --workspace
  udeps:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      # toolchain: stable in a comment is not a step input
      - name: Install nightly toolchain
        uses: ./.github/actions/setup-rust
        with:
          toolchain: "nightly-2026-10-11"  # dated, so not floating
      - run: cargo udeps --workspace
"""


def write_tree(root, pin=PIN, workflow=CLEAN_WORKFLOW):
    """A repository tree with the real setup-rust action and one workflow."""
    action = root / ACTION_RELATIVE
    action.parent.mkdir(parents=True)
    action.write_text(ACTION.read_text())
    ci = root / WORKFLOW_RELATIVE
    ci.parent.mkdir(parents=True)
    ci.write_text(workflow)
    if pin is not None:
        (root / "rust-toolchain.toml").write_text(pin)


def line_of(text, needle):
    """1-based line number of the only line containing `needle`."""
    numbers = [n for n, line in enumerate(text.splitlines(), start=1) if needle in line]
    if len(numbers) != 1:
        raise AssertionError(f"{needle!r} occurs on lines {numbers}")
    return numbers[0]


class CheckRustToolchainPinTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name) / "repo"
        self.root.mkdir()

    def run_check(self, *args):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *args],
            capture_output=True,
            text=True,
            timeout=RUN_TIMEOUT_SECONDS,
        )

    def check_tree(self, **tree):
        write_tree(self.root, **tree)
        return self.run_check(str(self.root))

    def assert_violation(self, result, path, line, fragment):
        self.assertEqual(result.returncode, EXIT_VIOLATIONS, result.stdout + result.stderr)
        expected = f"{path}:{line}: "
        matching = [l for l in result.stdout.splitlines() if l.startswith(expected)]
        self.assertTrue(matching, f"no {expected!r} line in:\n{result.stdout}")
        self.assertIn(fragment, matching[0])

    def workflow_with(self, old, new):
        self.assertIn(old, CLEAN_WORKFLOW)
        return CLEAN_WORKFLOW.replace(old, new, 1)

    def test_real_repository_tree_is_clean(self):
        result = self.run_check()
        self.assertEqual(result.returncode, EXIT_CLEAN, result.stdout + result.stderr)
        self.assertIn("ok", result.stdout)

    def test_accepts_dated_nightly_input_and_the_composite_action(self):
        result = self.check_tree()
        self.assertEqual(result.returncode, EXIT_CLEAN, result.stdout + result.stderr)
        self.assertEqual(result.stdout.strip(), "check-rust-toolchain-pin: ok (2 workflow and action files)")

    def test_rejects_stable_toolchain_input(self):
        workflow = self.workflow_with(
            "          components: rustfmt, clippy\n",
            "          toolchain: stable\n          components: rustfmt, clippy\n",
        )
        result = self.check_tree(workflow=workflow)
        self.assert_violation(
            result, WORKFLOW_RELATIVE.as_posix(), line_of(workflow, "          toolchain: stable"), "'stable'"
        )

    def test_rejects_undated_and_malformed_nightly(self):
        for value in ("nightly", "'nightly'", "nightly-2026-10", "nightly-2026-13-40", "beta", "1.99"):
            with self.subTest(value=value):
                workflow = self.workflow_with(
                    'toolchain: "nightly-2026-10-11"  # dated, so not floating',
                    f"toolchain: {value}",
                )
                with tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    write_tree(root, workflow=workflow)
                    result = self.run_check(str(root))
                self.assert_violation(
                    result,
                    WORKFLOW_RELATIVE.as_posix(),
                    line_of(workflow, f"toolchain: {value}"),
                    "not a dated nightly-YYYY-MM-DD",
                )

    def test_rejects_toolchain_expression_outside_the_action(self):
        workflow = self.workflow_with(
            'toolchain: "nightly-2026-10-11"  # dated, so not floating',
            "toolchain: ${{ matrix.toolchain }}",
        )
        result = self.check_tree(workflow=workflow)
        self.assert_violation(
            result, WORKFLOW_RELATIVE.as_posix(), line_of(workflow, "matrix.toolchain"), "hides the channel"
        )

    def test_rejects_direct_installer_use_outside_the_composite_action(self):
        workflow = self.workflow_with(
            "        uses: ./.github/actions/setup-rust\n        with:\n          components",
            "        uses: dtolnay/rust-toolchain@02cb101ec7c40f2c49e1d9714d64511d8e1b74de # v1\n"
            "        with:\n          components",
        )
        result = self.check_tree(workflow=workflow)
        self.assert_violation(
            result,
            WORKFLOW_RELATIVE.as_posix(),
            line_of(workflow, "dtolnay/rust-toolchain@"),
            "installs Rust outside ./.github/actions/setup-rust",
        )

    def test_rejects_composite_action_pinned_by_branch(self):
        write_tree(self.root)
        action = self.root / ACTION_RELATIVE
        text = action.read_text()
        self.assertIn("dtolnay/rust-toolchain@02cb101ec7c40f2c49e1d9714d64511d8e1b74de", text)
        action.write_text(
            text.replace("dtolnay/rust-toolchain@02cb101ec7c40f2c49e1d9714d64511d8e1b74de", "dtolnay/rust-toolchain@stable")
        )
        result = self.run_check(str(self.root))
        self.assert_violation(
            result,
            ACTION_RELATIVE.as_posix(),
            line_of(action.read_text(), "dtolnay/rust-toolchain@stable"),
            "not pinned to a commit SHA",
        )

    def check_workflow_line(self, old, new):
        """Run the check on the clean workflow with one line replaced."""
        workflow = self.workflow_with(old, new)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_tree(root, workflow=workflow)
            return workflow, self.run_check(str(root))

    def test_rejects_floating_toolchain_selected_in_a_command(self):
        for command, channel in (
            ("cargo +stable clippy", "stable"),
            ("rustup default nightly", "nightly"),
            ("rustup run beta cargo test", "beta"),
            ("rustup update stable", "stable"),
            ("rustup toolchain add beta", "beta"),
            ("rustup toolchain install --profile minimal nightly", "nightly"),
            ("rustup component add clippy --toolchain nightly", "nightly"),
            ("rustup target add --toolchain=stable wasm32-wasip1", "stable"),
            ("rustup set default-toolchain stable", "stable"),
            ("rustup override set 1.98.0", "1.98.0"),
            ("RUSTUP_TOOLCHAIN=beta cargo build", "beta"),
            ("cd sub && ~/.cargo/bin/cargo +nightly-2026-10 udeps", "nightly-2026-10"),
        ):
            with self.subTest(command=command):
                workflow, result = self.check_workflow_line(
                    "      - run: cargo clippy --workspace\n", f"      - run: {command}\n"
                )
                self.assert_violation(
                    result,
                    WORKFLOW_RELATIVE.as_posix(),
                    line_of(workflow, command),
                    f"{channel!r} is neither the rust-toolchain.toml channel nor a dated nightly",
                )

    def test_rejects_toolchain_selector_hidden_in_a_variable(self):
        workflow, result = self.check_workflow_line(
            "      - run: cargo clippy --workspace\n", "      - run: cargo +$TOOLCHAIN clippy\n"
        )
        self.assert_violation(
            result, WORKFLOW_RELATIVE.as_posix(), line_of(workflow, "+$TOOLCHAIN"), "hides the channel"
        )

    def test_accepts_commands_on_the_pin_or_a_dated_nightly(self):
        for command in (
            "cargo +1.99.0 clippy",
            "rustup toolchain install nightly-2026-10-11 --profile minimal",
            "rustup component add clippy --toolchain nightly-2026-10-11",
            "rustup toolchain install",
            "rustup target add aarch64-apple-darwin",
            "RUSTUP_TOOLCHAIN=nightly-2026-10-11 cargo udeps",
            "rustup show active-toolchain",
        ):
            with self.subTest(command=command):
                _, result = self.check_workflow_line(
                    "      - run: cargo clippy --workspace\n", f"      - run: {command}\n"
                )
                self.assertEqual(result.returncode, EXIT_CLEAN, result.stdout + result.stderr)

    def test_rejects_any_other_rust_installer_action(self):
        for reference in (
            "actions-rust-lang/setup-rust-toolchain@v1",
            "dtolnay/rust-toolchain@stable",
            "actions-rs/toolchain@16499b5e05bf2e26879000db0c1d13f7e13fa3af",
            "./.github/actions/setup-rust-nightly",
        ):
            with self.subTest(reference=reference):
                workflow, result = self.check_workflow_line(
                    "      - uses: actions/checkout@v4\n      - name: Install Rust toolchain\n",
                    f"      - uses: actions/checkout@v4\n      - name: Install Rust toolchain\n"
                    f"      - {{ uses: {reference} }}\n",
                )
                self.assert_violation(
                    result, WORKFLOW_RELATIVE.as_posix(), line_of(workflow, reference), "installs Rust outside"
                )

    def test_flow_mapping_toolchain_is_checked(self):
        workflow, result = self.check_workflow_line(
            "        with:\n          components: rustfmt, clippy\n",
            "        with: { components: clippy, toolchain: stable }\n",
        )
        self.assert_violation(
            result, WORKFLOW_RELATIVE.as_posix(), line_of(workflow, "{ components: clippy, toolchain: stable }"), "'stable'"
        )
        _, result = self.check_workflow_line(
            "        with:\n          components: rustfmt, clippy\n",
            "        with: { toolchain: nightly-2026-10-11, components: clippy }\n",
        )
        self.assertEqual(result.returncode, EXIT_CLEAN, result.stdout + result.stderr)

    def test_accepts_the_local_action_from_a_subdirectory_checkout(self):
        _, result = self.check_workflow_line(
            "        uses: ./.github/actions/setup-rust\n        with:\n          components",
            "        uses: ./relocated/.github/actions/setup-rust\n        with:\n          components",
        )
        self.assertEqual(result.returncode, EXIT_CLEAN, result.stdout + result.stderr)

    def test_rejects_missing_pin_file(self):
        result = self.check_tree(pin=None)
        self.assert_violation(result, "rust-toolchain.toml", 1, "missing")

    def test_rejects_floating_or_partial_pin_channel(self):
        for channel in ("stable", "1.99", "nightly-2026-10-11"):
            with self.subTest(channel=channel):
                pin = PIN.replace('channel = "1.99.0"', f'channel = "{channel}"')
                with tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    write_tree(root, pin=pin)
                    result = self.run_check(str(root))
                self.assert_violation(result, "rust-toolchain.toml", line_of(pin, "channel ="), "floats")

    def test_rejects_pin_lines_the_action_cannot_read(self):
        for old, new, message in (
            ('channel = "1.99.0"', 'channel = "1.99.0"  # pin', "channel must be exactly"),
            ('channel = "1.99.0"', "channel = '1.99.0'", "channel must be exactly"),
            ('components = ["clippy", "rustfmt"]', 'components = ["clippy", "rustfmt"]  # lint', "components must be"),
            ('components = ["clippy", "rustfmt"]', 'components = ["clippy", "rustfmt",]', "components must be"),
            ('components = ["clippy", "rustfmt"]', "components = ['clippy']", "components must be"),
            ('components = ["clippy", "rustfmt"]', 'components = [\n  "clippy",\n]', "components must be"),
        ):
            with self.subTest(line=new):
                pin = PIN.replace(old, new)
                with tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    write_tree(root, pin=pin)
                    result = self.run_check(str(root))
                self.assert_violation(result, "rust-toolchain.toml", line_of(pin, new.split("\n")[0]), message)

    def test_rejects_pin_without_channel(self):
        result = self.check_tree(pin='[toolchain]\nprofile = "minimal"\n')
        self.assert_violation(result, "rust-toolchain.toml", 1, "no [toolchain] channel")

    def test_unreadable_input_is_a_configuration_error(self):
        result = self.run_check(str(self.root))
        self.assertEqual(result.returncode, EXIT_CONFIG, result.stdout + result.stderr)
        self.assertIn("no workflow directory", result.stderr)

        write_tree(self.root)
        (self.root / WORKFLOW_RELATIVE).write_bytes(b"\xff\xfe not utf-8")
        result = self.run_check(str(self.root))
        self.assertEqual(result.returncode, EXIT_CONFIG, result.stdout + result.stderr)
        self.assertIn("cannot read", result.stderr)

    def test_report_is_bounded(self):
        extra = MAX_REPORTED_VIOLATIONS + 7
        workflow = CLEAN_WORKFLOW + "".join("      - run: cargo +stable build\n" for _ in range(extra))
        result = self.check_tree(workflow=workflow)
        self.assertEqual(result.returncode, EXIT_VIOLATIONS)
        lines = result.stdout.splitlines()
        self.assertEqual(len(lines), MAX_REPORTED_VIOLATIONS + 1)
        self.assertEqual(lines[-1], f"... and {extra - MAX_REPORTED_VIOLATIONS} more")
        self.assertIn(f"{extra} violation(s)", result.stderr)


def resolve_script():
    """The `run: |` body of the action's `resolve` step, dedented."""
    match = re.search(
        r"^ {6}id: resolve\n(?:.*\n)*? {6}run: \|\n((?: {8}[^\n]*\n|\n)+)",
        ACTION.read_text(),
        re.M,
    )
    if match is None:
        raise AssertionError(f"resolve step not found in {ACTION}")
    return "".join(line[8:] if line.strip() else line for line in match.group(1).splitlines(True))


class SetupRustResolveTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.home = Path(tmp.name)
        # A subdirectory checkout, like the `relocated/` test jobs: the action
        # must read the pin of the tree it belongs to.
        self.checkout = self.home / "workspace" / "relocated"
        self.checkout.mkdir(parents=True)
        write_tree(self.checkout)
        self.output = self.home / "github_output"
        self.env_file = self.home / "github_env"

    def resolve(self, toolchain="", components="", targets=""):
        self.output.write_text("")
        self.env_file.write_text("")
        result = subprocess.run(
            ["bash", "-c", resolve_script()],
            env={
                "PATH": "/usr/bin:/bin",
                "GITHUB_ACTION_PATH": str(self.checkout / ACTION_RELATIVE.parent),
                "GITHUB_OUTPUT": str(self.output),
                "GITHUB_ENV": str(self.env_file),
                "INPUT_TOOLCHAIN": toolchain,
                "INPUT_COMPONENTS": components,
                "INPUT_TARGETS": targets,
            },
            cwd=self.home,
            capture_output=True,
            text=True,
            timeout=RUN_TIMEOUT_SECONDS,
        )
        outputs = dict(
            line.split("=", 1) for line in self.output.read_text().splitlines() if "=" in line
        )
        return result, outputs, self.env_file.read_text()

    def test_empty_toolchain_installs_the_pin_with_its_components(self):
        result, outputs, env = self.resolve(components="rustfmt, clippy", targets="aarch64-apple-darwin")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs["toolchain"], "1.99.0")
        self.assertEqual(outputs["components"], "clippy,rustfmt")
        self.assertEqual(outputs["targets"], "aarch64-apple-darwin")
        self.assertEqual(Path(outputs["repo-root"]), self.checkout.resolve())
        self.assertEqual(env, "")

    def test_dated_nightly_is_exported_over_the_directory_override(self):
        result, outputs, env = self.resolve(toolchain="nightly-2026-10-11")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs["toolchain"], "nightly-2026-10-11")
        self.assertEqual(outputs["components"], "")
        self.assertEqual(env, "RUSTUP_TOOLCHAIN=nightly-2026-10-11\n")

    def test_floating_toolchain_input_fails(self):
        for value in ("stable", "nightly", "1.99.0", "nightly-2026-10-11\nstable"):
            with self.subTest(value=value):
                result, outputs, env = self.resolve(toolchain=value)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("::error::setup-rust:", result.stderr)
                self.assertNotIn("toolchain", outputs)
                self.assertEqual(env, "")

    def test_unsafe_component_list_fails(self):
        result, outputs, _ = self.resolve(components="clippy; rm -rf /")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a comma-separated list", result.stderr)
        self.assertNotIn("toolchain", outputs)

    def test_gate_accepts_exactly_the_pin_files_the_action_reads(self):
        variants = {
            "canonical": PIN,
            "no components": PIN.replace('components = ["clippy", "rustfmt"]\n', ""),
            "empty components": PIN.replace('["clippy", "rustfmt"]', "[]"),
            "spaced": PIN.replace('channel = "1.99.0"', '  channel   =   "1.99.0"  '),
            "channel comment": PIN.replace('"1.99.0"', '"1.99.0" # pin'),
            "single quotes": PIN.replace('"1.99.0"', "'1.99.0'"),
            "components comment": PIN.replace('"rustfmt"]', '"rustfmt"] # lint'),
            "trailing comma": PIN.replace('"rustfmt"]', '"rustfmt",]'),
            "multi-line components": PIN.replace('["clippy", "rustfmt"]', '[\n  "clippy",\n]'),
            "floating channel": PIN.replace('"1.99.0"', '"stable"'),
        }
        for name, pin in variants.items():
            with self.subTest(variant=name):
                (self.checkout / "rust-toolchain.toml").write_text(pin)
                action, _, _ = self.resolve()
                gate = subprocess.run(
                    [sys.executable, str(SCRIPT), str(self.checkout)],
                    capture_output=True,
                    text=True,
                    timeout=RUN_TIMEOUT_SECONDS,
                )
                self.assertIn(gate.returncode, (EXIT_CLEAN, EXIT_VIOLATIONS), gate.stderr)
                self.assertEqual(
                    gate.returncode == EXIT_CLEAN,
                    action.returncode == 0,
                    f"gate:\n{gate.stdout}{gate.stderr}\naction:\n{action.stderr}",
                )

    def test_floating_or_missing_pin_fails(self):
        pin = self.checkout / "rust-toolchain.toml"
        pin.write_text(PIN.replace('"1.99.0"', '"stable"'))
        result, _, _ = self.resolve()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is not an exact X.Y.Z release", result.stderr)

        pin.unlink()
        result, _, _ = self.resolve()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing", result.stderr)


if __name__ == "__main__":
    unittest.main()
