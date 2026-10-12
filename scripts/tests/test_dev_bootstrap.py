"""Integration scenarios for the `scripts/dev-bootstrap` CLI (stdlib only).

Each test runs the actual script as a child process of a disposable
repository root (the script copy, its `Cargo.toml`, `rust-toolchain.toml`
and `.config/nextest.toml`) with executable fake tools on `PATH`. The fake
`cargo` mirrors the real one's subcommand resolution: it prefers
`$CARGO_HOME/bin/cargo-<sub>` unless that directory already sits later on
`PATH`, exactly the order `dev-bootstrap` relies on. No host tool, network
or real PATH entry is used, and no test monkeypatches production code.
"""

import platform
from pathlib import Path
import sys
import subprocess
import tempfile
import unittest

REPO_SCRIPT = Path(__file__).resolve().parents[1] / "dev-bootstrap"

# "Install home" passed as CARGO_HOME; fake cargo resolves subcommands from
# its `bin/` directory first, per the real Cargo order.
CARGO_HOME = "cargo-home"
CARGO_BIN = f"{CARGO_HOME}/bin"

MSRV_MANIFEST = "[workspace.package]\nrust-version = \"1.96\"\n"
NEXTEST_CONFIG = 'nextest-version = { required = "0.9.115" }\n'
# Pins the release the fake `rustc` reports, so the pin row passes by default.
TOOLCHAIN_PIN = "1.98.1"


def toolchain_file(channel):
    return (
        "# CI and local toolchain pin.\n"
        f'[toolchain]\nchannel = "{channel}"\n'
        'components = ["clippy", "rustfmt"]\nprofile = "minimal"\n'
    )


# Version outputs shaped like the real tools'.
RUSTC_VERSION = "rustc 1.98.1 (48a229cea 2026-09-01)\n"
NEXTEST_VERSION = (
    "cargo-nextest 0.9.145 (00af4550e 2026-09-16)\n"
    "release: 0.9.145\nhost: x86_64-unknown-linux-gnu\n"
)
PYTHON_VERSION = "Python 3.13.1\n"
BACON_VERSION = "bacon 3.25.0\n"
HYPERFINE_VERSION = "hyperfine 1.20.0\n"
MOLD_VERSION = (
    "mold 2.42.1 (9b376bc6a9899d4a16b41777de1f013989459fbc; "
    "compatible with GNU ld)\n"
)

IS_LINUX = platform.system() == "Linux"


def sh_quote(text):
    # Single-quote for `sh` word arguments the fake tools embed.
    return "'" + text.replace("'", "'\\''") + "'"


class DevBootstrapSubprocessTests(unittest.TestCase):
    """Run the real `dev-bootstrap` script against a disposable install world."""

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.home = Path(tmp.name)
        # Everything the script reads lives in the copy; the production
        # script is never run from the real repository.
        self.root = self.home / "repo"
        (self.root / ".config").mkdir(parents=True)
        (self.root / "scripts").mkdir()
        (self.root / "scripts" / "dev-bootstrap").write_text(REPO_SCRIPT.read_text())
        (self.root / "Cargo.toml").write_text(MSRV_MANIFEST)
        (self.root / "rust-toolchain.toml").write_text(toolchain_file(TOOLCHAIN_PIN))
        (self.root / ".config" / "nextest.toml").write_text(NEXTEST_CONFIG)
        self.fakes = self.home / "fakes"
        self.fakes.mkdir()
        self.cargo_bin = self.home / CARGO_BIN
        (self.cargo_bin).mkdir(parents=True)
        self.write_cargo()
        self.write_tool("rustc", RUSTC_VERSION)
        self.write_tool("python3", PYTHON_VERSION)

    # -- fake-world helpers -------------------------------------------

    def write_tool(self, name, output, directory=None, bad=False, body=None):
        """Write an executable fake tool that prints `output` (or garbage).

        `body` replaces the default print entirely, for fakes that exit
        nonzero or fail loudly.
        """
        path = (directory or self.fakes) / name
        if body is None:
            body = "printf %s garbage" if bad else f"printf %s {sh_quote(output)}"
        path.write_text(f"#!/bin/sh\n{body}\n")
        path.chmod(0o755)
        return path

    def cargo_script(self):
        # Mirrors the real resolution: `$CARGO_HOME/bin/cargo-<sub>` wins
        # over PATH unless that directory is already on PATH (then PATH
        # order decides, potentially shadowing the home copy).
        return (
            '#!/bin/sh\n'
            'sub=$1\ncargo_home=${CARGO_HOME:-$HOME/.cargo}\n'
            'home_bin=$cargo_home/bin\ntool=cargo-$sub\n'
            'prefers_home=1\n'
            'case ":$PATH:" in *":$home_bin:"*) prefers_home=0 ;; esac\n'
            'if [ "$prefers_home" = 1 ] && [ -x "$home_bin/$tool" ]; then\n'
            '  exec "$home_bin/$tool" "$@"\n'
            'fi\n'
            'exec "$tool" "$@"\n'
        )

    def write_all_tools(self):
        # Every optional tool present and current, so `--strict` judges only
        # the rows a scenario changes.
        self.write_tool("cargo-nextest", NEXTEST_VERSION)
        self.write_tool("bacon", BACON_VERSION)
        self.write_tool("hyperfine", HYPERFINE_VERSION)
        if IS_LINUX:
            self.write_tool("mold", MOLD_VERSION)

    def pin(self, channel):
        (self.root / "rust-toolchain.toml").write_text(toolchain_file(channel))

    def write_cargo(self):
        # `cargo` itself must be invocable on PATH, as in a real install.
        (self.fakes / "cargo").write_text(self.cargo_script())
        (self.fakes / "cargo").chmod(0o755)

    def env(self, cargo_home=None):
        return {
            "PATH": str(self.fakes),
            "CARGO_HOME": str(cargo_home if cargo_home is not None
                              else self.home / CARGO_HOME),
            "HOME": str(self.home),
        }

    def run_script(self, *args, path_entries=None, cargo_home=None, cwd=None,
                   extra_env=None):
        path = ":".join(map(str, path_entries)) if path_entries else str(self.fakes)
        completed = subprocess.run(
            [sys.executable, str(self.root / "scripts" / "dev-bootstrap"), *args],
            cwd=cwd or self.root,
            env={**self.env(cargo_home), "PATH": path, **(extra_env or {})},
            capture_output=True,
            text=True,
            timeout=60,
        )
        return completed.returncode, completed.stdout

    # -- the report over a working install ----------------------------

    def test_report_header_and_everything_current_exits_zero(self):
        self.write_tool("cargo-nextest", NEXTEST_VERSION)
        self.write_tool("bacon", BACON_VERSION)
        self.write_tool("hyperfine", HYPERFINE_VERSION)
        self.write_tool("mold", MOLD_VERSION)
        status, output = self.run_script()
        self.assertEqual(status, 0, output)
        self.assertIn("dev-bootstrap: local tool check", output)
        self.assertIn(
            "ok       cargo-nextest 0.9.145 >= 0.9.115 (required)", output
        )
        self.assertIn("all checked tools that count are ready", output)

    # -- required tools -----------------------------------------------

    def test_missing_required_tool_fails_with_install_fix(self):
        status, output = self.run_script()
        self.assertEqual(status, 1, output)
        self.assertIn("FAIL     cargo-nextest is missing (needs >= 0.9.115)", output)
        self.assertIn(
            f"fix: cargo install --root {self.home / CARGO_HOME} "
            "--locked cargo-nextest",
            output,
        )
        self.assertIn("failing: cargo-nextest", output)

    def test_too_old_required_tool_fails(self):
        self.write_tool("cargo-nextest", NEXTEST_VERSION)
        self.write_tool("python3", "Python 3.10.12\n")
        status, output = self.run_script()
        self.assertEqual(status, 1, output)
        self.assertIn(
            "python3 3.10.12 is older than 3.11.0 (required)", output
        )
        self.assertIn("sudo apt-get install python3", output)
        self.assertIn("failing: python3", output)

    def test_prerelease_of_the_required_minimum_fails(self):
        self.write_tool("cargo-nextest", "cargo-nextest 0.9.115-rc.1\n")
        status, output = self.run_script()
        self.assertEqual(status, 1, output)
        self.assertIn(
            "cargo-nextest 0.9.115 is older than 0.9.115", output
        )

    def test_configured_minimum_drives_the_check(self):
        self.write_tool("cargo-nextest", NEXTEST_VERSION)
        (self.root / ".config" / "nextest.toml").write_text(
            'nextest-version = { required = "0.9.200" }\n'
        )
        status, output = self.run_script()
        self.assertEqual(status, 1, output)
        self.assertIn(
            "cargo-nextest 0.9.145 is older than 0.9.200 (required)", output
        )

    # -- optional tools ------------------------------------------------

    def test_missing_optional_and_required_tools_fail_together(self):
        status, output = self.run_script()
        self.assertEqual(status, 1, output)
        self.assertIn("failing: cargo-nextest", output)
        self.assertIn("warn     bacon is missing", output)
        if IS_LINUX:
            self.assertIn("warn     mold is missing", output)

    def test_mold_is_checked_only_on_linux(self):
        self.write_tool("cargo-nextest", NEXTEST_VERSION)
        self.write_tool("bacon", BACON_VERSION)
        self.write_tool("hyperfine", HYPERFINE_VERSION)
        status, output = self.run_script("--strict")
        self.assertEqual(status, 1 if IS_LINUX else 0, output)
        if IS_LINUX:
            self.assertIn("FAIL     mold is missing", output)
        else:
            self.assertNotIn("mold", output)

    def test_missing_optional_tool_fails_only_under_strict(self):
        self.write_tool("cargo-nextest", NEXTEST_VERSION)
        self.write_tool("hyperfine", HYPERFINE_VERSION)
        if IS_LINUX:
            self.write_tool("mold", MOLD_VERSION)
        status, output = self.run_script()
        self.assertEqual(status, 0, output)
        self.assertIn("warn     bacon is missing", output)
        self.assertIn(f"fix: cargo install --root {self.home / CARGO_HOME} "
                      "--locked bacon", output)

        status, output = self.run_script("--strict")
        self.assertEqual(status, 1, output)
        self.assertIn("FAIL     bacon is missing (needs >= 3.13.0) (optional)", output)

    def test_too_old_optional_tool_fails_only_under_strict(self):
        self.write_tool("cargo-nextest", NEXTEST_VERSION)
        self.write_tool("bacon", "bacon 3.12.1\n")
        self.write_tool("hyperfine", HYPERFINE_VERSION)
        if IS_LINUX:
            self.write_tool("mold", MOLD_VERSION)
        status, output = self.run_script()
        self.assertEqual(status, 0, output)
        self.assertIn("bacon 3.12.1 is older than 3.13.0", output)

        status, output = self.run_script("--strict")
        self.assertEqual(status, 1, output)

    # -- cargo subcommand precedence (fake cargo mirrors real Cargo) ----

    def test_cargo_runs_the_cargo_home_copy_even_when_path_is_current(self):
        # Cargo prefers $CARGO_HOME/bin: an old home copy fails the check
        # even when a current one is on PATH, and the fix installs there.
        self.write_tool("cargo-nextest", NEXTEST_VERSION)
        self.write_tool("cargo-nextest", "cargo-nextest 0.9.100\n",
                        directory=self.cargo_bin)
        status, output = self.run_script()
        self.assertEqual(status, 1, output)
        self.assertIn("cargo-nextest 0.9.100 is older than", output)
        self.assertIn(
            f"fix: cargo install --root {self.home / CARGO_HOME} "
            "--locked cargo-nextest",
            output,
        )

    def test_subcommand_found_in_cargo_home_only_reports_ok(self):
        self.write_tool("cargo-nextest", NEXTEST_VERSION,
                        directory=self.cargo_bin)
        status, output = self.run_script()
        self.assertEqual(status, 0, output)
        self.assertIn("ok       cargo-nextest 0.9.145 >= 0.9.115 (required)", output)

    def test_cargo_home_copy_shadowed_by_stale_path_copy_is_a_path_problem(self):
        # The install root ($CARGO_HOME/bin) is already on PATH, later than
        # a stale copy, so reordering PATH (not reinstalling) is what the
        # report asks for.
        self.write_tool("cargo-nextest", "cargo-nextest 0.9.100\n")
        self.write_tool("cargo-nextest", NEXTEST_VERSION,
                        directory=self.cargo_bin)
        status, output = self.run_script(path_entries=[self.fakes, self.cargo_bin])
        self.assertEqual(status, 1, output)
        self.assertIn(
            "cargo-nextest 0.9.145 is shadowed by an earlier copy on PATH "
            "(required)",
            output,
        )
        self.assertIn(f'export PATH={self.cargo_bin}:"$PATH"', output)
        # The PATH fix is what reorders the shadow, so no stale-copy install
        # chain appears: the home copy is already current.
        self.assertIn("fix: found at", output)

    def test_stale_path_copy_gets_install_and_path_fix(self):
        # No copy in $CARGO_HOME/bin yet: the report chains both fixes,
        # because the new install could still be shadowed by the stale copy.
        self.write_tool("cargo-nextest", "cargo-nextest 0.9.90\n")
        status, output = self.run_script()
        self.assertEqual(status, 1, output)
        self.assertIn(
            f"fix: cargo install --root {self.home / CARGO_HOME} --locked "
            f"cargo-nextest; then put it first: export PATH={self.cargo_bin}:\"$PATH\"",
            output,
        )

    def test_binaries_found_in_cargo_home_are_reported_off_path(self):
        self.write_tool("cargo-nextest", NEXTEST_VERSION)
        self.write_tool("bacon", BACON_VERSION, directory=self.cargo_bin)
        status, output = self.run_script("--strict")
        self.assertEqual(status, 1, output)
        self.assertIn("bacon is not on PATH", output)
        self.assertIn(f'export PATH={self.cargo_bin}:"$PATH"', output)

    def test_failing_tool_reports_its_diagnostic_first(self):
        # A tool that runs but exits nonzero: its own diagnostic line is
        # quoted, and reinstalling is offered only as the last resort.
        self.write_tool("rustc", None, body=(
            "printf %s 'error: no suitable target found (rustc)\n'; exit 1"
        ))
        status, output = self.run_script()
        self.assertEqual(status, 1, output)
        self.assertIn("rustc version unknown", output)
        self.assertIn("error: no suitable target found (rustc)", output)
        self.assertIn("resolve the error above;", output)
        self.assertIn("if the binary itself is broken:", output)

    def test_path_fix_quotes_shell_metacharacters_safely(self):
        # CARGO_HOME with shell metacharacters: the suggested PATH fix
        # single-quotes the directory, so pasting it is safe.
        self.write_tool("cargo-nextest", NEXTEST_VERSION)
        # Place a current bacon only in the metacharacter-heavy CARGO_HOME.
        metachars = self.home / "a $(b)`c`\"d"
        (metachars / "bin").mkdir(parents=True)
        self.write_tool("bacon", BACON_VERSION, directory=metachars / "bin")
        status, output = self.run_script("--strict", cargo_home=metachars)
        self.assertEqual(status, 1, output)
        self.assertIn("bacon is not on PATH", output)
        expected = f'export PATH={sh_quote(str(metachars / "bin"))}:"$PATH"'
        self.assertIn(f"put it first: {expected}", output)
        # The suggested export really is valid shell that accepts the dir.
        shell_home = self.home / "shell-check"
        shell_home.mkdir()
        script = shell_home / "fix.sh"
        script.write_text(f"#!/bin/sh\n{expected}\nprintf %s \"$PATH\"\n")
        script.chmod(0o755)
        checked = subprocess.run(
            [str(script)], capture_output=True, text=True, timeout=60,
        )
        self.assertEqual(checked.returncode, 0, checked.stderr)
        # The quoted directory survived as one token (no command
        # substitution or quoting error), and $PATH stayed evaluated.
        new_path = checked.stdout.strip()
        self.assertEqual(new_path.split(":", 1)[0], str(metachars / "bin"))
        self.assertIn(":", new_path)

    def test_unparseable_version_output_is_a_failure(self):
        self.write_tool("cargo-nextest", "cargo-nextest dev\n", bad=True)
        status, output = self.run_script()
        self.assertEqual(status, 1, output)
        self.assertIn("cargo-nextest version unknown", output)
        self.assertIn("unparseable version output 'garbage'", output)
        self.assertNotIn("shadowed", output)

    def test_missing_cargo_is_reported_instead_of_the_subcommand(self):
        (self.fakes / "cargo").unlink()
        status, output = self.run_script()
        self.assertEqual(status, 1, output)
        self.assertIn("`cargo` is not on PATH", output)
        self.assertIn("https://rustup.rs", output)
        self.assertNotIn("cargo install --locked", output)

    # -- the rust-toolchain.toml pin ------------------------------------

    def test_rustc_matching_the_pin_passes_strict(self):
        self.write_all_tools()
        status, output = self.run_script("--strict")
        self.assertEqual(status, 0, output)
        self.assertIn(
            f"ok       rustc {TOOLCHAIN_PIN} matches the rust-toolchain.toml pin (pin)",
            output,
        )

    def test_rustc_differing_from_the_pin_warns_and_fails_only_under_strict(self):
        # A distribution rustc first on PATH, the rustup proxy behind it in
        # $CARGO_HOME/bin: the fix reorders PATH and installs the pin.
        self.write_all_tools()
        self.pin("1.99.0")
        self.write_tool("rustc", "rustc 1.99.0 (b940084d7 2026-09-28)\n",
                        directory=self.cargo_bin)
        status, output = self.run_script()
        self.assertEqual(status, 0, output)
        self.assertIn(
            f"warn     rustc 1.98.1 at {self.fakes / 'rustc'} differs from the "
            "rust-toolchain.toml pin 1.99.0 (pin)",
            output,
        )
        self.assertIn(
            f"fix: {self.fakes / 'rustc'} is not the rustup proxy; put the rustup "
            "proxies first so rustup selects the pin from rust-toolchain.toml: "
            f'export PATH={self.cargo_bin}:"$PATH"; then run `rustup toolchain '
            "install` in the repository root to install the pin",
            output,
        )
        # The MSRV floor still holds for the same rustc.
        self.assertIn("ok       rustc 1.98.1 >= 1.96.0 (required)", output)

        status, output = self.run_script("--strict")
        self.assertEqual(status, 1, output)
        self.assertIn("FAIL     rustc 1.98.1 at", output)
        self.assertIn("dev-bootstrap: failing: rustc-pin\n", output)

    def test_pin_mismatch_without_rustup_points_at_rustup(self):
        self.write_all_tools()
        self.pin("1.99.0")
        status, output = self.run_script()
        self.assertEqual(status, 0, output)
        self.assertIn("install rustup (https://rustup.rs)", output)
        self.assertIn(f'export PATH={self.cargo_bin}:"$PATH"', output)

    def test_pin_mismatch_through_the_proxy_names_the_override(self):
        # The proxy is first on PATH and still selects another toolchain:
        # only an override can win over rust-toolchain.toml.
        self.write_all_tools()
        self.write_tool("rustc", "rustc 1.97.0 (aaaaaaaaa 2026-07-07)\n",
                        directory=self.cargo_bin)
        entries = [self.cargo_bin, self.fakes]
        status, output = self.run_script(path_entries=entries)
        self.assertEqual(status, 0, output)
        self.assertIn("rustc 1.97.0 at", output)
        self.assertIn("`rustup override unset` in the repository root", output)

        status, output = self.run_script(
            path_entries=entries, extra_env={"RUSTUP_TOOLCHAIN": "1.97.0"}
        )
        self.assertEqual(status, 0, output)
        self.assertIn("RUSTUP_TOOLCHAIN is set and wins over rust-toolchain.toml", output)

    def test_prerelease_of_the_pin_is_a_mismatch(self):
        self.write_all_tools()
        self.write_tool("rustc", f"rustc {TOOLCHAIN_PIN}-beta.3 (ccccccccc 2026-08-20)\n")
        status, output = self.run_script("--strict")
        self.assertEqual(status, 1, output)
        self.assertIn(
            f"rustc {TOOLCHAIN_PIN} (pre-release) at {self.fakes / 'rustc'} differs",
            output,
        )

    def test_pin_is_not_compared_while_the_rustc_row_fails(self):
        self.write_all_tools()
        self.write_tool("rustc", "rustc 1.95.0 (ddddddddd 2026-05-01)\n")
        status, output = self.run_script("--strict")
        self.assertEqual(status, 1, output)
        self.assertIn("rustc 1.95.0 is older than 1.96.0 (required)", output)
        self.assertIn(
            f"n/a      rust-toolchain.toml pin {TOOLCHAIN_PIN} not compared", output
        )
        self.assertIn("dev-bootstrap: failing: rustc\n", output)

    def test_version_commands_run_in_the_root_without_rustup_auto_install(self):
        # A rustup proxy resolves rust-toolchain.toml from its working
        # directory and downloads a missing pin unless RUSTUP_AUTO_INSTALL=0;
        # these fakes fail unless both hold, even when the script is started
        # from outside the repository.
        guard = (
            '[ "$RUSTUP_AUTO_INSTALL" = 0 ] && [ -f rust-toolchain.toml ] || '
            "{ echo 'would resolve or install a toolchain' >&2; exit 1; }; "
        )
        self.write_all_tools()
        self.write_tool("rustc", None, body=guard + f"printf %s {sh_quote(RUSTC_VERSION)}")
        self.write_tool("cargo-nextest", None,
                        body=guard + f"printf %s {sh_quote(NEXTEST_VERSION)}")
        status, output = self.run_script("--strict", cwd=self.home)
        self.assertEqual(status, 0, output)
        self.assertIn(f"ok       rustc {TOOLCHAIN_PIN} matches", output)
        self.assertIn("ok       cargo-nextest 0.9.145", output)

    def test_pinned_toolchain_not_installed_names_rustup_install(self):
        # The rustup proxies come first and the pin is not installed yet (the
        # state right after a bump): with auto-install off, every proxy
        # refuses, and the fix installs the toolchain instead of the tools.
        self.write_all_tools()
        refusal = (
            "printf %s 'error: toolchain '\\''1.98.1-x86_64-unknown-linux-gnu'\\'' "
            "is not installed\nhelp: run `rustup toolchain install` to install it\n' >&2; "
            "exit 1"
        )
        self.write_tool("rustc", None, directory=self.cargo_bin, body=refusal)
        self.write_tool("cargo", None, directory=self.cargo_bin, body=refusal)
        status, output = self.run_script(path_entries=[self.cargo_bin, self.fakes])
        self.assertEqual(status, 1, output)
        for tool in ("rustc", "cargo-nextest"):
            self.assertIn(
                f"FAIL     {tool} cannot run: rustup toolchain "
                "1.98.1-x86_64-unknown-linux-gnu is not installed (required)",
                output,
            )
        self.assertEqual(
            output.count("fix: run `rustup toolchain install` in the repository root"), 2,
            output,
        )
        self.assertNotIn("cargo install", output)
        self.assertIn(f"n/a      rust-toolchain.toml pin {TOOLCHAIN_PIN} not compared", output)
        self.assertIn("dev-bootstrap: failing: rustc, cargo-nextest\n", output)

    def test_missing_or_malformed_pin_is_a_configuration_error(self):
        cases = {
            "missing": (None, "cannot read"),
            "invalid toml": ("[toolchain\n", "cannot read"),
            "no channel": ('[toolchain]\nprofile = "minimal"\n',
                           "declares no [toolchain] channel"),
            "floating stable": (toolchain_file("stable"),
                                "channel 'stable' is not an exact X.Y.Z release"),
            "minor only": (toolchain_file("1.99"),
                           "channel '1.99' is not an exact X.Y.Z release"),
            "dated nightly": (toolchain_file("nightly-2026-10-11"),
                              "is not an exact X.Y.Z release"),
        }
        pin_file = self.root / "rust-toolchain.toml"
        for case, (content, message) in cases.items():
            with self.subTest(case):
                if content is None:
                    pin_file.unlink(missing_ok=True)
                else:
                    pin_file.write_text(content)
                status, output = self.run_script()
                self.assertEqual(status, 2, output)
                self.assertIn("rust-toolchain.toml", output)
                self.assertIn(message, output)
                self.assertNotIn("local tool check", output)

    # -- malformed configuration ---------------------------------------

    def test_missing_nextest_config_is_an_configuration_error(self):
        (self.root / ".config" / "nextest.toml").unlink()
        status, output = self.run_script()
        self.assertEqual(status, 2, output)
        self.assertIn("cannot read", output)
        self.assertIn("nextest.toml", output)

    def test_nextest_config_without_requirement_is_a_configuration_error(self):
        (self.root / ".config" / "nextest.toml").write_text(
            "[profile.default]\ntest-threads = 4\n"
        )
        status, output = self.run_script()
        self.assertEqual(status, 2, output)
        self.assertIn("declares no nextest-version requirement", output)

    def test_missing_msrv_is_a_configuration_error(self):
        (self.root / "Cargo.toml").write_text("[workspace]\n")
        status, output = self.run_script()
        self.assertEqual(status, 2, output)
        self.assertIn("rust-version", output)


if __name__ == "__main__":
    unittest.main()
