"""Regression checks for the local tool check (stdlib only).

No test looks at the host PATH or runs a real tool: `which`, the version
runner are injected, and the nextest config is a
temporary file.
"""

import importlib.machinery
import importlib.util
import io
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "dev-bootstrap"
LOADER = importlib.machinery.SourceFileLoader("dev_bootstrap", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
dev_bootstrap = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(dev_bootstrap)

PATH_DIR = "/fake/path"
CARGO_HOME = "/fake/cargo-home"
CARGO_BIN = f"{CARGO_HOME}/bin"

# Real `--version` output shapes of every checked tool.
CURRENT = {
    "rustc": "rustc 1.98.1 (48a229cea 2026-09-01)\n",
    "cargo-nextest": "cargo-nextest 0.9.145 (00af4550e 2026-09-16)\n"
                     "release: 0.9.145\nhost: x86_64-unknown-linux-gnu\n",
    "python3": "Python 3.13.1\n",
    "bacon": "bacon 3.25.0\n",
    "hyperfine": "hyperfine 1.20.0\n",
    "mold": "mold 2.42.1 (9b376bc6a9899d4a16b41777de1f013989459fbc; "
            "compatible with GNU ld)\n",
}


class FakeHost:
    """Injected lookups over an in-memory set of installed tools.

    `on_path` and `in_cargo_bin` map tool names to their `--version` output.
    """

    def __init__(self, on_path=None, in_cargo_bin=None, cargo=True):
        self.on_path = dict(CURRENT if on_path is None else on_path)
        self.in_cargo_bin = dict(in_cargo_bin or {})
        self.cargo = cargo
        # Cargo prefers $CARGO_HOME/bin unless it already sits later on PATH.
        self.cargo_prefers_path = False
        self.version_calls = []

    def which(self, name, path=None):
        if name == "cargo" and path is None:
            return f"{PATH_DIR}/cargo" if self.cargo else None
        if path is None:
            return f"{PATH_DIR}/{name}" if name in self.on_path else None
        if path == CARGO_BIN and name in self.in_cargo_bin:
            return f"{CARGO_BIN}/{name}"
        return None

    def runner(self, argv):
        self.version_calls.append(argv)
        directory, name = argv[0].rsplit("/", 1)
        if name == "cargo":
            # Cargo's own order: `$CARGO_HOME/bin` before `PATH`.
            sub = f"cargo-{argv[1]}"
            order = (self.on_path, self.in_cargo_bin) if self.cargo_prefers_path else (self.in_cargo_bin, self.on_path)
            for source in order:
                if sub in source:
                    output = source[sub]
                    if isinstance(output, Exception):
                        raise output
                    return output
            raise subprocess.CalledProcessError(101, argv)
        source = self.on_path if directory == PATH_DIR else self.in_cargo_bin
        output = source[name]
        if isinstance(output, Exception):
            raise output
        return output



class DevBootstrapCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)
        self.write_nextest_config(
            '# comment\nnextest-version = { required = "0.9.115" }\n'
        )
        self.write_manifest('[workspace.package]\nrust-version = "1.96"\n')

    def write_manifest(self, text):
        (self.root / "Cargo.toml").write_text(text)

    def write_nextest_config(self, text):
        config = self.root / ".config" / "nextest.toml"
        config.parent.mkdir(parents=True, exist_ok=True)
        config.write_text(text)

    def run_main(self, host, *args, system="Linux"):
        out = io.StringIO()
        status = dev_bootstrap.main(
            list(args),
            which=host.which,
            runner=host.runner,
                        root=self.root,
            environ={"CARGO_HOME": CARGO_HOME},
            out=out,
            system=system,
        )
        return status, out.getvalue()

class ParseVersionTests(unittest.TestCase):
    def test_real_outputs(self):
        cases = {
            CURRENT["cargo-nextest"]: (0, 9, 145),
            CURRENT["python3"]: (3, 13, 1),
            CURRENT["bacon"]: (3, 25, 0),
            CURRENT["hyperfine"]: (1, 20, 0),
            CURRENT["mold"]: (2, 42, 1),
        }
        for text, expected in cases.items():
            with self.subTest(text=text):
                self.assertEqual(dev_bootstrap.parse_version(text), expected)

    def test_two_part_version_pads_patch(self):
        self.assertEqual(dev_bootstrap.parse_version("bacon 3.4"), (3, 4, 0))

    def test_prerelease_suffix_keeps_numeric_core(self):
        self.assertEqual(
            dev_bootstrap.parse_version("cargo-nextest 0.9.150-rc.1"),
            (0, 9, 150),
        )

    def test_prerelease_of_the_minimum_is_below_it(self):
        minimum = dev_bootstrap.parse_version("0.9.115")
        for text, below in (
            ("cargo-nextest 0.9.115-rc.1", True),
            ("cargo-nextest 0.9.115", False),
            ("cargo-nextest 0.9.116-rc.1", False),
            ("cargo-nextest 0.9.114", True),
        ):
            with self.subTest(text=text):
                self.assertEqual(
                    dev_bootstrap.below_minimum(
                        dev_bootstrap.parse_version(text),
                        dev_bootstrap.is_prerelease(text),
                        minimum,
                    ),
                    below,
                )

    def test_leading_blank_lines_are_skipped(self):
        self.assertEqual(
            dev_bootstrap.parse_version("\n\nhyperfine 1.9.0\n"), (1, 9, 0)
        )

    def test_hash_on_a_later_line_is_ignored(self):
        self.assertIsNone(
            dev_bootstrap.parse_version("bacon unknown\ncommit 1.2.3")
        )

    def test_unparseable_and_empty(self):
        for text in ("bacon 3", "", "   \n", "mold (dev build)"):
            with self.subTest(text=text):
                self.assertIsNone(dev_bootstrap.parse_version(text))

    def test_numeric_comparison_not_lexical(self):
        self.assertLess((0, 9, 99), (0, 9, 115))
        self.assertLess(
            dev_bootstrap.parse_version("cargo-nextest 0.9.99"),
            dev_bootstrap.parse_version("0.9.115"),
        )


class NextestConfigTests(DevBootstrapCase):
    def test_minimum_read_from_table(self):
        self.write_nextest_config(
            'nextest-version = { required = "0.9.200", recommended = "1.0.0" }\n'
        )
        self.assertEqual(dev_bootstrap.nextest_min_version(self.root),
                         (0, 9, 200))

    def test_minimum_read_from_bare_string(self):
        self.write_nextest_config('nextest-version = "0.9.120"\n')
        self.assertEqual(dev_bootstrap.nextest_min_version(self.root),
                         (0, 9, 120))

    def test_config_minimum_drives_the_check(self):
        self.write_nextest_config(
            'nextest-version = { required = "0.9.200" }\n'
        )
        status, output = self.run_main(FakeHost())
        self.assertEqual(status, 1)
        self.assertIn("cargo-nextest 0.9.145 is older than 0.9.200", output)

    def test_missing_requirement_is_a_config_error(self):
        self.write_nextest_config("[profile.default]\ntest-threads = 4\n")
        status, output = self.run_main(FakeHost())
        self.assertEqual(status, 2)
        self.assertIn("declares no nextest-version requirement", output)

    def test_missing_file_is_a_config_error(self):
        (self.root / ".config" / "nextest.toml").unlink()
        status, output = self.run_main(FakeHost())
        self.assertEqual(status, 2)
        self.assertIn("cannot read", output)

    def test_repository_config_parses(self):
        root = Path(__file__).resolve().parents[2]
        self.assertIsInstance(dev_bootstrap.nextest_min_version(root), tuple)


class CheckTests(DevBootstrapCase):
    def test_mold_is_checked_only_on_linux(self):
        on_path = {name: output for name, output in CURRENT.items() if name != "mold"}
        status, output = self.run_main(FakeHost(on_path=on_path), "--strict", system="Darwin")
        self.assertEqual(status, 0, output)
        self.assertNotIn("mold", output)

        status, output = self.run_main(FakeHost(on_path=on_path), "--strict", system="Linux")
        self.assertEqual(status, 1, output)
        self.assertIn("mold", output)

    def test_everything_current_passes(self):
        status, output = self.run_main(FakeHost())
        self.assertEqual(status, 0)
        self.assertIn("ok       cargo-nextest 0.9.145 >= 0.9.115 (required)",
                      output)
        self.assertIn("ok       mold 2.42.1 >= 2.30.0 (optional)", output)

    def test_missing_required_tool_fails_with_install_command(self):
        tools = dict(CURRENT)
        del tools["cargo-nextest"]
        status, output = self.run_main(FakeHost(on_path=tools))
        self.assertEqual(status, 1)
        self.assertIn("FAIL     cargo-nextest is missing (needs >= 0.9.115)",
                      output)
        self.assertIn("fix: cargo install --locked cargo-nextest", output)
        self.assertIn("failing: cargo-nextest", output)

    def test_too_old_required_tool_fails(self):
        tools = dict(CURRENT, python3="Python 3.10.12\n")
        status, output = self.run_main(FakeHost(on_path=tools))
        self.assertEqual(status, 1)
        self.assertIn("python3 3.10.12 is older than 3.11.0 (required)",
                      output)
        self.assertIn("sudo apt-get install python3", output)

    def test_missing_optional_tool_warns_without_strict(self):
        tools = dict(CURRENT)
        del tools["hyperfine"]
        del tools["mold"]
        status, output = self.run_main(FakeHost(on_path=tools))
        self.assertEqual(status, 0)
        self.assertIn("warn     hyperfine is missing", output)
        self.assertIn("fix: cargo install --locked hyperfine", output)
        self.assertIn("warn     mold is missing", output)

    def test_missing_optional_tool_fails_under_strict(self):
        tools = dict(CURRENT)
        del tools["bacon"]
        status, output = self.run_main(FakeHost(on_path=tools), "--strict")
        self.assertEqual(status, 1)
        self.assertIn("FAIL     bacon is missing (needs >= 3.13.0) (optional)",
                      output)

    def test_too_old_optional_tool_fails_only_under_strict(self):
        tools = dict(CURRENT, bacon="bacon 3.12.1\n")
        self.assertEqual(self.run_main(FakeHost(on_path=tools))[0], 0)
        status, output = self.run_main(FakeHost(on_path=tools), "--strict")
        self.assertEqual(status, 1)
        self.assertIn("bacon 3.12.1 is older than 3.13.0", output)

    def test_cargo_subcommand_found_in_cargo_home_off_path(self):
        tools = dict(CURRENT)
        nextest = tools.pop("cargo-nextest")
        host = FakeHost(on_path=tools, in_cargo_bin={"cargo-nextest": nextest})
        status, output = self.run_main(host)
        self.assertEqual(status, 0)
        self.assertIn([f"{PATH_DIR}/cargo", "nextest", "--version"], host.version_calls)

    def test_cargo_subcommand_version_is_the_one_cargo_runs(self):
        # Cargo prefers $CARGO_HOME/bin over PATH: an old home-bin copy fails
        # the check even when a current one is on PATH, and vice versa.
        old = "cargo-nextest 0.9.100\n"
        host = FakeHost(in_cargo_bin={"cargo-nextest": old})
        status, output = self.run_main(host)
        self.assertEqual(status, 1, output)
        self.assertIn("cargo-nextest 0.9.100 is older than", output)

        tools = dict(CURRENT)
        tools["cargo-nextest"] = old
        host = FakeHost(on_path=tools, in_cargo_bin={"cargo-nextest": CURRENT["cargo-nextest"]})
        status, output = self.run_main(host)
        self.assertEqual(status, 0, output)

    def test_current_cargo_home_nextest_shadowed_on_path_is_a_path_problem(self):
        # Cargo keeps the PATH position of $CARGO_HOME/bin when it is on PATH:
        # `cargo nextest` then runs the old PATH copy.
        tools = dict(CURRENT)
        tools["cargo-nextest"] = "cargo-nextest 0.9.100\n"
        host = FakeHost(on_path=tools, in_cargo_bin={"cargo-nextest": CURRENT["cargo-nextest"]})
        host.cargo_prefers_path = True
        status, output = self.run_main(host)
        self.assertEqual(status, 1, output)
        self.assertIn("cargo-nextest 0.9.145 is shadowed by an earlier copy on PATH", output)
        self.assertIn(f'export PATH={CARGO_BIN}:"$PATH"', output)

    def test_cargo_failure_that_is_not_an_old_version_keeps_its_report(self):
        # e.g. a Cargo alias shadowing `nextest`: not a PATH-order problem.
        tools = dict(CURRENT)
        tools["cargo-nextest"] = "garbage without a version\n"
        host = FakeHost(on_path=tools, in_cargo_bin={"cargo-nextest": CURRENT["cargo-nextest"]})
        host.cargo_prefers_path = True
        status, output = self.run_main(host)
        self.assertEqual(status, 1, output)
        self.assertIn("cargo-nextest version unknown", output)
        self.assertNotIn("shadowed", output)

    def test_stale_cargo_home_nextest_gets_an_install_pinned_there(self):
        host = FakeHost(in_cargo_bin={"cargo-nextest": "cargo-nextest 0.9.100\n"})
        status, output = self.run_main(host)
        self.assertEqual(status, 1, output)
        self.assertIn("cargo install --root /fake/cargo-home --locked cargo-nextest", output)

    def test_two_stale_nextest_copies_get_install_and_path_fix(self):
        tools = dict(CURRENT)
        tools["cargo-nextest"] = "cargo-nextest 0.9.90\n"
        host = FakeHost(on_path=tools, in_cargo_bin={"cargo-nextest": "cargo-nextest 0.9.100\n"})
        host.cargo_prefers_path = True
        status, output = self.run_main(host)
        self.assertEqual(status, 1, output)
        self.assertIn(
            "cargo install --root /fake/cargo-home --locked cargo-nextest; then put it first: "
            f'export PATH={CARGO_BIN}:"$PATH"',
            output,
        )

    def test_two_equally_stale_nextest_copies_get_install_and_path_fix(self):
        tools = dict(CURRENT)
        tools["cargo-nextest"] = "cargo-nextest 0.9.100\n"
        host = FakeHost(on_path=tools, in_cargo_bin={"cargo-nextest": "cargo-nextest 0.9.100\n"})
        host.cargo_prefers_path = True
        status, output = self.run_main(host)
        self.assertEqual(status, 1, output)
        self.assertIn("; then put it first: ", output)

    def test_cargo_subcommand_without_cargo_is_a_failure(self):
        status, output = self.run_main(FakeHost(cargo=False))
        self.assertEqual(status, 1, output)
        self.assertIn("`cargo` is not on PATH", output)

    def test_current_cargo_home_copy_shadowed_by_old_path_copy(self):
        tools = dict(CURRENT)
        tools["bacon"] = "bacon 2.0.0\n"
        host = FakeHost(on_path=tools, in_cargo_bin={"bacon": CURRENT["bacon"]})
        status, output = self.run_main(host, "--strict")
        self.assertEqual(status, 1, output)
        self.assertIn("bacon 3.25.0 is shadowed by", output)
        self.assertIn(f'export PATH={CARGO_BIN}:"$PATH"', output)

    def test_missing_cargo_is_reported_before_a_missing_subcommand(self):
        tools = dict(CURRENT)
        tools.pop("cargo-nextest")
        host = FakeHost(on_path=tools, cargo=False)
        status, output = self.run_main(host)
        self.assertEqual(status, 1, output)
        self.assertIn("`cargo` is not on PATH", output)
        self.assertIn("https://rustup.rs", output)
        self.assertNotIn("cargo install --locked cargo-nextest", output)

    def test_outdated_copy_only_in_cargo_home_keeps_its_install_fix(self):
        tools = dict(CURRENT)
        tools.pop("bacon")
        host = FakeHost(on_path=tools, in_cargo_bin={"bacon": "bacon 2.0.0\n"})
        status, output = self.run_main(host, "--strict")
        self.assertEqual(status, 1, output)
        self.assertIn("bacon 2.0.0 is older than", output)
        self.assertIn("cargo install --root /fake/cargo-home --locked bacon", output)
        self.assertNotIn("not on PATH", output)

    def test_rustc_is_checked_against_the_workspace_msrv(self):
        for output, expected in (
            ("rustc 1.95.0 (abc 2026-05-01)\n", 1),
            (CURRENT["rustc"], 0),
        ):
            with self.subTest(output=output):
                tools = dict(CURRENT)
                tools["rustc"] = output
                status, report = self.run_main(FakeHost(on_path=tools))
                self.assertEqual(status, expected, report)
        tools = dict(CURRENT)
        tools.pop("rustc")
        status, report = self.run_main(FakeHost(on_path=tools))
        self.assertEqual(status, 1, report)
        self.assertIn("rustc is missing (needs >= 1.96.0)", report)

    def test_missing_msrv_is_a_configuration_error(self):
        self.write_manifest("[workspace]\n")
        status, output = self.run_main(FakeHost())
        self.assertEqual(status, 2, output)
        self.assertIn("rust-version", output)

    def test_path_fix_quotes_shell_metacharacters(self):
        fix = dev_bootstrap.path_fix('/home/a $(b)`c`"d/bin')
        self.assertEqual(fix, "export PATH='/home/a $(b)`c`\"d/bin':\"$PATH\"")

    def test_binary_only_in_cargo_home_is_reported_off_path(self):
        tools = dict(CURRENT)
        bacon = tools.pop("bacon")
        host = FakeHost(on_path=tools, in_cargo_bin={"bacon": bacon})
        status, output = self.run_main(host, "--strict")
        self.assertEqual(status, 1)
        self.assertIn("bacon is not on PATH", output)
        self.assertIn(f'export PATH={CARGO_BIN}:"$PATH"', output)

    def test_unrunnable_tool_is_a_failure_not_a_pass(self):
        tools = dict(
            CURRENT,
            python3=subprocess.CalledProcessError(1, ["python3"]),
        )
        status, output = self.run_main(FakeHost(on_path=tools))
        self.assertEqual(status, 1)
        self.assertIn("python3 version unknown", output)

    def test_unparseable_output_is_a_failure(self):
        tools = dict(CURRENT, **{"cargo-nextest": "cargo-nextest dev\n"})
        status, output = self.run_main(FakeHost(on_path=tools))
        self.assertEqual(status, 1)
        self.assertIn("unparseable version output 'cargo-nextest dev'",
                      output)


if __name__ == "__main__":
    unittest.main()
