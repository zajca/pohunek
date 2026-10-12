"""Integration scenarios for the `scripts/dev-bootstrap` CLI (stdlib only).

Each test runs the actual script as a child process of a disposable
repository root (the script copy, its `Cargo.toml`, `rust-toolchain.toml`
and `.config/nextest.toml`; `.bun-version` and `package.json` for the sdk
scenarios) with executable fake tools on `PATH`. The fake
`cargo` mirrors the real one's subcommand resolution: it prefers
`$CARGO_HOME/bin/cargo-<sub>` unless that directory already sits later on
`PATH`, exactly the order `dev-bootstrap` relies on. No host tool, network
or real PATH entry is used, and no test monkeypatches production code.
"""

import json
import os
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

# The bun release `.bun-version` and `packageManager` pin in the copy.
BUN_PIN = "1.3.11"
SHELLCHECK_VERSION = (
    "ShellCheck - shell script analysis tool\nversion: 0.10.0\n"
    "license: GNU General Public License, version 3\n"
    "website: https://www.shellcheck.net\n"
)
ZIG_VERSION = "0.13.0\n"
# A distinctive fake database URL; it must never reach any output.
FAKE_DATABASE_URL = "postgresql://sentinel-user:s3cr3t-Pa55@db.invalid:5432/never-print-me"

SYSTEM = platform.system()
IS_LINUX = SYSTEM == "Linux"
IS_DARWIN = SYSTEM == "Darwin"
ROW_KEYS = {
    "path", "name", "category", "status", "version", "minimum", "location",
    "detail", "fix",
}
CHECK_KEYS = {"path", "kind", "command", "note"}
GATES_SKILL = ".agents/skills/gates/SKILL.md"
VERIFICATION_DOC = ".agents/instructions/verification.md"


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
        status, stdout, _ = self.run_full(
            *args, path_entries=path_entries, cargo_home=cargo_home, cwd=cwd,
            extra_env=extra_env,
        )
        return status, stdout

    def run_full(self, *args, path_entries=None, cargo_home=None, cwd=None,
                 extra_env=None):
        """Exit status, stdout and stderr of one run."""
        path = ":".join(map(str, path_entries)) if path_entries else str(self.fakes)
        completed = subprocess.run(
            [sys.executable, str(self.root / "scripts" / "dev-bootstrap"), *args],
            cwd=cwd or self.root,
            env={**self.env(cargo_home), "PATH": path, **(extra_env or {})},
            capture_output=True,
            text=True,
            timeout=60,
        )
        return completed.returncode, completed.stdout, completed.stderr

    def run_json(self, *args, **kwargs):
        """Exit status and the one JSON document stdout must hold."""
        status, stdout, stderr = self.run_full("--json", *args, **kwargs)
        # json.loads rejects anything besides one document (plus whitespace).
        return status, json.loads(stdout), stderr

    def write_bun_config(self, bun_version=BUN_PIN, package_manager=f"bun@{BUN_PIN}"):
        (self.root / ".bun-version").write_text(f"{bun_version}\n")
        (self.root / "package.json").write_text(json.dumps({
            "name": "pohunek-workspace",
            "private": True,
            "packageManager": package_manager,
        }, indent=2) + "\n")

    def write_rustup(self, *targets):
        # `rustup target list --installed` prints one target per line; the
        # fake refuses unless run in the repository root with auto-install
        # off, as every rustup call of the script must be.
        listing = "".join(f"{target}\n" for target in targets)
        self.write_tool("rustup", None, body=(
            '[ "$RUSTUP_AUTO_INSTALL" = 0 ] && [ -f rust-toolchain.toml ] || '
            "{ echo 'would resolve or install a toolchain' >&2; exit 1; }; "
            '[ "$1 $2 $3" = "target list --installed" ] || exit 64; '
            f"printf %s {sh_quote(listing)}"
        ))

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

    # -- the default invocation ----------------------------------------

    def test_default_invocation_prints_the_rust_report_without_headings(self):
        # The whole report, byte for byte: no --path checks the rust path and
        # prints its rows without path headings.
        self.write_all_tools()
        status, stdout, stderr = self.run_full()
        self.assertEqual(status, 0, stdout)
        self.assertEqual(stderr, "")
        mold = "  ok       mold 2.42.1 >= 2.30.0 (optional)\n" if IS_LINUX else ""
        self.assertEqual(
            stdout,
            "dev-bootstrap: local tool check (required tools only fail the run)\n"
            "  ok       rustc 1.98.1 >= 1.96.0 (required)\n"
            f"  ok       rustc {TOOLCHAIN_PIN} matches the rust-toolchain.toml pin (pin)\n"
            "  ok       cargo-nextest 0.9.145 >= 0.9.115 (required)\n"
            "  ok       python3 3.13.1 >= 3.11.0 (required)\n"
            "  ok       bacon 3.25.0 >= 3.13.0 (optional)\n"
            "  ok       hyperfine 1.20.0 >= 1.9.0 (optional)\n"
            f"{mold}"
            "dev-bootstrap: all checked tools that count are ready\n",
        )

    def test_explicit_rust_path_reports_the_same_rows_under_a_heading(self):
        self.write_all_tools()
        _, default = self.run_script()
        status, explicit = self.run_script("--path", "rust")
        self.assertEqual(status, 0, explicit)
        self.assertIn(
            "dev-bootstrap: local tool check (paths: rust; required tools only "
            "fail the run)\npath rust:\n",
            explicit,
        )
        rows = [line for line in default.splitlines() if line.startswith("  ")]
        # The explicit report adds the path's checks after its rows.
        explicit_rows = explicit.split("\n  checks:\n", 1)[0].splitlines()
        explicit_rows = [line for line in explicit_rows if line.startswith("  ")]
        self.assertEqual(rows, explicit_rows)

    # -- the --json contract -------------------------------------------

    def test_json_document_shape_and_version(self):
        self.write_all_tools()
        status, doc, stderr = self.run_json()
        self.assertEqual(status, 0, doc)
        self.assertEqual(stderr, "")
        self.assertEqual(doc["schema"], "pohunek.dev-bootstrap")
        self.assertEqual(doc["version"], 1)
        self.assertEqual(doc["paths"], ["rust"])
        self.assertIs(doc["strict"], False)
        self.assertEqual(doc["os"], SYSTEM)
        self.assertEqual(doc["result"], "ready")
        self.assertEqual(doc["failing"], [])
        self.assertIsNone(doc["error"])
        self.assertIsNone(doc["message"])
        self.assertEqual(
            set(doc),
            {"schema", "version", "paths", "strict", "os", "rows", "checks",
             "failing", "result", "error", "message"},
        )
        names = ["rustc", "rustc-pin", "cargo-nextest", "python3", "bacon", "hyperfine"]
        if IS_LINUX:
            names.append("mold")
        self.assertEqual([row["name"] for row in doc["rows"]], names)
        for row in doc["rows"]:
            self.assertEqual(set(row), ROW_KEYS)
            self.assertEqual(row["path"], "rust")
            self.assertEqual(row["status"], "ok")
            self.assertIsNone(row["fix"])
        rows = {row["name"]: row for row in doc["rows"]}
        self.assertEqual(rows["cargo-nextest"]["version"], "0.9.145")
        self.assertEqual(rows["cargo-nextest"]["minimum"], "0.9.115")
        self.assertEqual(rows["cargo-nextest"]["category"], "required")
        self.assertEqual(rows["bacon"]["category"], "optional")
        self.assertEqual(rows["rustc-pin"]["category"], "pin")
        self.assertEqual(rows["rustc-pin"]["minimum"], TOOLCHAIN_PIN)
        self.assertEqual(rows["rustc"]["location"], str(self.fakes / "rustc"))
        # `location` is a real path or null: Cargo chooses the subcommand
        # binary, so that row names the command in `detail` instead.
        self.assertIsNone(rows["cargo-nextest"]["location"])
        self.assertIn(f"`{self.fakes / 'cargo'} nextest`", rows["cargo-nextest"]["detail"])
        for row in doc["rows"]:
            if row["location"] is not None:
                self.assertTrue(os.path.isabs(row["location"]), row)
                self.assertTrue(os.path.exists(row["location"]), row)

    def test_json_not_ready_lists_failing_rows_with_their_fix(self):
        status, doc, _ = self.run_json("--strict")
        self.assertEqual(status, 1, doc)
        self.assertIs(doc["strict"], True)
        self.assertEqual(doc["result"], "not-ready")
        self.assertIn("cargo-nextest", doc["failing"])
        self.assertIn("bacon", doc["failing"])
        nextest = next(row for row in doc["rows"] if row["name"] == "cargo-nextest")
        self.assertEqual(nextest["status"], "missing")
        self.assertIsNone(nextest["version"])
        self.assertIn("cargo install --root", nextest["fix"])

    def test_json_reports_prerelease_versions_and_the_uncompared_pin(self):
        self.write_all_tools()
        self.write_tool("rustc", "rustc 1.96.0-beta.2 (eeeeeeeee 2026-04-01)\n")
        status, doc, _ = self.run_json()
        self.assertEqual(status, 1, doc)
        rows = {row["name"]: row for row in doc["rows"]}
        self.assertEqual(rows["rustc"]["status"], "too-old")
        self.assertEqual(rows["rustc"]["version"], "1.96.0-beta.2")
        self.assertEqual(rows["rustc-pin"]["status"], "not-compared")
        self.assertEqual(
            rows["rustc-pin"]["fix"], "fix the rustc row; the pin is compared once it passes"
        )
        self.assertEqual(doc["failing"], ["rustc"])

    def test_every_non_ok_row_carries_a_fix(self):
        # Nothing but rustc and python3 on PATH, every path selected, strict.
        self.write_bun_config()
        status, doc, _ = self.run_json(
            "--strict", "--path", "rust", "--path", "sdk", "--path", "scripts",
            "--path", "macos-lint", "--path", "relay-db", "--path", "native",
        )
        self.assertEqual(status, 1, doc)
        self.assertEqual(
            doc["paths"], ["rust", "sdk", "scripts", "macos-lint", "relay-db", "native"]
        )
        names = [row["name"] for row in doc["rows"]]
        self.assertEqual(len(names), len(set(names)), names)
        for row in doc["rows"]:
            with self.subTest(row["name"]):
                if row["status"] in ("ok", "not-applicable"):
                    self.assertIsNone(row["fix"])
                else:
                    self.assertTrue(row["fix"], row)
                if row["name"] in doc["failing"]:
                    self.assertNotIn(row["status"],
                                     ("ok", "not-compared", "ci-only", "not-applicable"))
        if IS_LINUX:
            expected = [
                "cargo-nextest", "bacon", "hyperfine", "mold", "bun", "dash",
                "shellcheck", "aarch64-apple-darwin", "zig",
                "POHUNEK_RELAY_TEST_DATABASE_URL", "systemd-user-manager",
            ]
        elif IS_DARWIN:
            expected = [
                "cargo-nextest", "bacon", "hyperfine", "bun", "shellcheck",
                "POHUNEK_RELAY_TEST_DATABASE_URL", "launchd-gui-domain",
            ]
        else:
            self.skipTest("the expected rows cover Linux and macOS hosts")
        self.assertEqual(doc["failing"], expected)

    # -- path selection --------------------------------------------------

    def test_unknown_path_is_a_usage_error(self):
        status, doc, stderr = self.run_json("--path", "rust", "--path", "gui")
        self.assertEqual(status, 2, doc)
        self.assertEqual(stderr, "")
        self.assertEqual(doc["result"], "error")
        self.assertEqual(doc["error"], "unsupported-path")
        self.assertIn("'gui'", doc["message"])
        self.assertIn("rust, sdk, scripts, macos-lint, relay-db, native", doc["message"])
        self.assertEqual(doc["rows"], [])
        self.assertEqual(doc["schema"], "pohunek.dev-bootstrap")
        self.assertEqual(doc["version"], 1)

        # Without --json every exit-2 message of the script goes to stdout,
        # where configuration errors are reported too.
        status, stdout, stderr = self.run_full("--path", "gui")
        self.assertEqual(status, 2, stdout)
        self.assertEqual(stderr, "")
        self.assertIn("dev-bootstrap: unsupported --path 'gui'", stdout)
        self.assertNotIn("local tool check", stdout)

    def test_malformed_flag_keeps_the_argparse_usage_error(self):
        status, stdout, stderr = self.run_full("--json", "--path")
        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertIn("usage: dev-bootstrap", stderr)

    def test_configuration_error_in_json_mode(self):
        (self.root / ".config" / "nextest.toml").unlink()
        status, doc, _ = self.run_json()
        self.assertEqual(status, 2, doc)
        self.assertEqual(doc["result"], "error")
        self.assertEqual(doc["error"], "config-error")
        self.assertIn("nextest.toml", doc["message"])

    def test_malformed_msrv_table_is_a_configuration_error(self):
        for manifest in ("workspace = 1\n", "[workspace]\npackage = 2\n"):
            with self.subTest(manifest):
                (self.root / "Cargo.toml").write_text(manifest)
                status, doc, _ = self.run_json()
                self.assertEqual(status, 2, doc)
                self.assertEqual(doc["error"], "config-error")
                self.assertIn("declares no [workspace.package] rust-version",
                              doc["message"])

                status, output = self.run_script()
                self.assertEqual(status, 2, output)
                self.assertIn("rust-version", output)

    def test_undecodable_tool_output_still_yields_one_document(self):
        # Invalid UTF-8 from a version command is quoted with replacement
        # characters instead of aborting the run.
        self.write_all_tools()
        self.write_tool("bacon", None, body="printf '\\377\\376 bacon \\377\\n'")
        status, doc, _ = self.run_json()
        self.assertEqual(status, 0, doc)
        bacon = next(row for row in doc["rows"] if row["name"] == "bacon")
        self.assertEqual(bacon["status"], "unknown-version")
        self.assertIn("unparseable version output", bacon["detail"])

        status, output = self.run_script()
        self.assertEqual(status, 0, output)
        self.assertIn("bacon version unknown", output)

    def test_a_shared_tool_is_reported_once_under_the_first_path(self):
        self.write_all_tools()
        _, doc, _ = self.run_json("--path", "rust", "--path", "scripts")
        python = [row for row in doc["rows"] if row["name"] == "python3"]
        self.assertEqual(len(python), 1, doc["rows"])
        self.assertEqual(python[0]["path"], "rust")

        _, doc, _ = self.run_json("--path", "scripts", "--path", "rust", "--path", "scripts")
        self.assertEqual(doc["paths"], ["scripts", "rust"])
        python = [row for row in doc["rows"] if row["name"] == "python3"]
        self.assertEqual(len(python), 1, doc["rows"])
        self.assertEqual(python[0]["path"], "scripts")

    def test_a_path_reads_only_its_own_configuration(self):
        # No Cargo.toml and no bun pin files: the scripts path still runs.
        (self.root / "Cargo.toml").unlink()
        status, doc, _ = self.run_json("--path", "scripts")
        self.assertEqual(doc["result"], "ready", doc)
        self.assertEqual(status, 0)

    # -- sdk: bun --------------------------------------------------------

    def test_sdk_bun_at_the_pin_passes_strict(self):
        self.write_bun_config()
        self.write_tool("bun", f"{BUN_PIN}\n")
        status, output = self.run_script("--strict", "--path", "sdk")
        self.assertEqual(status, 0, output)
        self.assertIn("path sdk:\n", output)
        self.assertIn(f"ok       bun {BUN_PIN} >= {BUN_PIN} (required)", output)
        self.assertIn(f"ok       bun {BUN_PIN} matches the .bun-version pin (pin)", output)

        status, doc, _ = self.run_json("--strict", "--path", "sdk")
        self.assertEqual(status, 0, doc)
        self.assertEqual([(row["name"], row["status"]) for row in doc["rows"]],
                         [("bun", "ok"), ("bun-pin", "ok")])

    def test_sdk_missing_bun_fails_with_the_pinned_installer(self):
        self.write_bun_config()
        status, doc, _ = self.run_json("--path", "sdk")
        self.assertEqual(status, 1, doc)
        rows = {row["name"]: row for row in doc["rows"]}
        self.assertEqual(rows["bun"]["status"], "missing")
        self.assertIn(
            f"curl -fsSL https://bun.com/install | bash -s bun-v{BUN_PIN}", rows["bun"]["fix"]
        )
        self.assertEqual(rows["bun-pin"]["status"], "not-compared")
        self.assertEqual(doc["failing"], ["bun"])

    def test_sdk_too_old_bun_fails(self):
        self.write_bun_config()
        self.write_tool("bun", "1.2.20\n")
        status, output = self.run_script("--path", "sdk")
        self.assertEqual(status, 1, output)
        self.assertIn(f"FAIL     bun 1.2.20 is older than {BUN_PIN} (required)", output)
        self.assertIn("bun upgrade", output)
        self.assertIn("dev-bootstrap: failing: bun\n", output)

    def test_sdk_bun_other_than_the_pin_fails_only_under_strict(self):
        self.write_bun_config()
        self.write_tool("bun", "1.4.2\n")
        status, output = self.run_script("--path", "sdk")
        self.assertEqual(status, 0, output)
        self.assertIn(
            f"warn     bun 1.4.2 at {self.fakes / 'bun'} differs from the "
            f".bun-version pin {BUN_PIN} (pin)",
            output,
        )
        self.assertIn(f"bash -s bun-v{BUN_PIN}", output)

        status, doc, _ = self.run_json("--strict", "--path", "sdk")
        self.assertEqual(status, 1, doc)
        pin = next(row for row in doc["rows"] if row["name"] == "bun-pin")
        self.assertEqual(pin["status"], "pin-mismatch")
        self.assertEqual(pin["version"], "1.4.2")
        self.assertEqual(pin["minimum"], BUN_PIN)
        self.assertEqual(doc["failing"], ["bun-pin"])

    def test_sdk_pin_disagreement_is_a_configuration_error(self):
        cases = {
            "disagreeing packageManager": (BUN_PIN, "bun@1.3.10", "make them agree"),
            "floating bun-version": ("latest", f"bun@{BUN_PIN}", "not an exact X.Y.Z"),
            "other package manager": (BUN_PIN, "pnpm@9.0.0", "is not bun@X.Y.Z"),
        }
        self.write_tool("bun", f"{BUN_PIN}\n")
        for case, (bun_version, manager, message) in cases.items():
            with self.subTest(case):
                self.write_bun_config(bun_version, manager)
                status, doc, _ = self.run_json("--path", "sdk")
                self.assertEqual(status, 2, doc)
                self.assertEqual(doc["result"], "error")
                self.assertEqual(doc["error"], "config-error")
                self.assertIn(message, doc["message"])

                status, output = self.run_script("--path", "sdk")
                self.assertEqual(status, 2, output)
                self.assertIn(message, output)
                self.assertNotIn("local tool check", output)

        # An integrity suffix on packageManager is part of the field's syntax.
        self.write_bun_config(BUN_PIN, f"bun@{BUN_PIN}+sha512.abc123")
        status, doc, _ = self.run_json("--path", "sdk")
        self.assertEqual(status, 0, doc)

    def test_sdk_missing_bun_version_file_is_a_configuration_error(self):
        status, doc, _ = self.run_json("--path", "sdk")
        self.assertEqual(status, 2, doc)
        self.assertEqual(doc["error"], "config-error")
        self.assertIn(".bun-version", doc["message"])

    # -- scripts ---------------------------------------------------------

    def test_scripts_path_never_runs_dash_and_reads_the_shellcheck_version(self):
        # dash has no version flag: the fake records any invocation and
        # fails, so an ok row proves presence alone decided it.
        marker = self.home / "dash-was-run"
        self.write_tool("dash", None, body=f"touch {sh_quote(str(marker))}; exit 2")
        self.write_tool("shellcheck", SHELLCHECK_VERSION)
        status, doc, _ = self.run_json("--strict", "--path", "scripts")
        self.assertEqual(status, 0, doc)
        self.assertFalse(marker.exists())
        rows = {row["name"]: row for row in doc["rows"]}
        self.assertEqual(rows["shellcheck"]["status"], "ok")
        self.assertEqual(rows["shellcheck"]["version"], "0.10.0")
        self.assertIsNone(rows["shellcheck"]["minimum"])
        self.assertEqual(rows["shellcheck"]["category"], "optional")
        if IS_LINUX:
            self.assertEqual(rows["dash"]["status"], "ok")
            self.assertIsNone(rows["dash"]["version"])
            self.assertEqual(rows["dash"]["location"], str(self.fakes / "dash"))
        else:
            self.assertNotIn("dash", rows)

    def test_scripts_path_reports_missing_optional_helpers(self):
        status, output = self.run_script("--path", "scripts")
        self.assertEqual(status, 0, output)
        self.assertIn("warn     shellcheck is missing (optional)", output)
        if IS_LINUX:
            self.assertIn("warn     dash is missing (optional)", output)
        status, output = self.run_script("--strict", "--path", "scripts")
        self.assertEqual(status, 1, output)

    # -- macos-lint --------------------------------------------------------

    def test_macos_lint_with_the_target_installed(self):
        self.write_rustup("x86_64-unknown-linux-gnu", "aarch64-apple-darwin")
        self.write_tool("zig", ZIG_VERSION)
        status, doc, _ = self.run_json("--path", "macos-lint", cwd=self.home)
        self.assertEqual(status, 0, doc)
        rows = {row["name"]: row for row in doc["rows"]}
        if IS_DARWIN:
            for name in ("aarch64-apple-darwin", "zig"):
                self.assertEqual(rows[name]["status"], "not-applicable")
                self.assertIsNone(rows[name]["fix"])
            return
        self.assertEqual(rows["aarch64-apple-darwin"]["status"], "ok")
        self.assertEqual(rows["aarch64-apple-darwin"]["location"],
                         str(self.fakes / "rustup"))
        self.assertEqual(rows["zig"]["status"], "ok")
        self.assertEqual(rows["zig"]["version"], "0.13.0")

    def test_macos_lint_with_the_target_or_zig_missing(self):
        self.write_rustup("x86_64-unknown-linux-gnu")
        status, output = self.run_script("--path", "macos-lint")
        if IS_DARWIN:
            self.assertEqual(status, 0, output)
            self.assertIn("n/a      zig: not needed on this OS", output)
            return
        self.assertEqual(status, 1, output)
        self.assertIn(
            "FAIL     aarch64-apple-darwin is missing: rustup lists the target "
            "as not installed (required)",
            output,
        )
        self.assertIn("fix: run `rustup target add aarch64-apple-darwin` in the "
                      "repository root", output)
        self.assertIn("FAIL     zig is missing (required)", output)
        self.assertIn("dev-bootstrap: failing: aarch64-apple-darwin, zig\n", output)

    def test_macos_lint_without_rustup_points_at_rustup(self):
        self.write_tool("zig", ZIG_VERSION)
        status, doc, _ = self.run_json("--path", "macos-lint")
        if IS_DARWIN:
            self.assertEqual(status, 0, doc)
            return
        self.assertEqual(status, 1, doc)
        target = doc["rows"][0]
        self.assertEqual(target["status"], "missing")
        self.assertIn("install rustup (https://rustup.rs)", target["fix"])
        self.assertIn("rustup target add aarch64-apple-darwin", target["fix"])

    @unittest.skipIf(IS_DARWIN, "macOS does not check the cross target")
    def test_macos_lint_target_with_the_pinned_toolchain_not_installed(self):
        self.write_tool("zig", ZIG_VERSION)
        self.write_tool("rustup", None, body=(
            "printf %s 'error: toolchain '\\''1.99.0-x86_64-unknown-linux-gnu'\\'' "
            "is not installed\nhelp: run `rustup toolchain install` to install it\n' >&2; "
            "exit 1"
        ))
        status, doc, _ = self.run_json("--path", "macos-lint")
        self.assertEqual(status, 1, doc)
        target = doc["rows"][0]
        self.assertEqual(target["status"], "toolchain-not-installed")
        self.assertEqual(target["detail"], "1.99.0-x86_64-unknown-linux-gnu")
        self.assertIn("run `rustup toolchain install` in the repository root", target["fix"])
        self.assertEqual(doc["failing"], ["aarch64-apple-darwin"])

    @unittest.skipIf(IS_DARWIN, "macOS does not check the cross target")
    def test_macos_lint_target_query_failure_quotes_the_error(self):
        self.write_tool("zig", ZIG_VERSION)
        self.write_tool("rustup", None, body=(
            "echo 'error: rustup home is not writable (rustup)' >&2; exit 1"
        ))
        status, output = self.run_script("--path", "macos-lint")
        self.assertEqual(status, 1, output)
        self.assertIn("FAIL     aarch64-apple-darwin version unknown:", output)
        self.assertIn("error: rustup home is not writable (rustup)", output)
        self.assertIn(
            "fix: resolve the error above: `rustup target list --installed` must "
            "succeed in the repository root",
            output,
        )

    # -- relay-db ------------------------------------------------------------

    def test_relay_db_variable_is_reported_as_set_without_its_value(self):
        env = {"POHUNEK_RELAY_TEST_DATABASE_URL": FAKE_DATABASE_URL}
        for args in (("--path", "relay-db"), ("--json", "--path", "relay-db"),
                     ("--json", "--strict", "--path", "relay-db", "--path", "rust")):
            with self.subTest(args):
                status, stdout, stderr = self.run_full(*args, extra_env=env)
                for fragment in (FAKE_DATABASE_URL, "sentinel-user", "s3cr3t-Pa55",
                                 "db.invalid", "never-print-me"):
                    self.assertNotIn(fragment, stdout)
                    self.assertNotIn(fragment, stderr)
                self.assertIn("POHUNEK_RELAY_TEST_DATABASE_URL", stdout)
        status, doc, _ = self.run_json("--path", "relay-db", extra_env=env)
        self.assertEqual(status, 0, doc)
        row = doc["rows"][0]
        self.assertEqual(row["status"], "ok")
        self.assertEqual(row["detail"], "set")
        self.assertIsNone(row["version"])
        self.assertIsNone(row["location"])

    def test_relay_db_value_never_reaches_a_child_diagnostic(self):
        # A failing tool that echoes the variable: its diagnostic is quoted
        # into the row, so the script must not hand the value to children.
        self.write_bun_config()
        echo = 'echo "db=$POHUNEK_RELAY_TEST_DATABASE_URL" >&2; exit 1'
        self.write_tool("bun", None, body=echo)
        self.write_all_tools()
        self.write_tool("bacon", None, body=echo)
        env = {"POHUNEK_RELAY_TEST_DATABASE_URL": FAKE_DATABASE_URL}
        for args in (("--path", "sdk", "--path", "relay-db"),
                     ("--json", "--path", "sdk", "--path", "relay-db"),
                     ("--json", "--strict", "--path", "rust")):
            with self.subTest(args):
                status, stdout, stderr = self.run_full(*args, extra_env=env)
                self.assertIn("db=", stdout)
                for fragment in ("sentinel-user", "s3cr3t-Pa55", "never-print-me"):
                    self.assertNotIn(fragment, stdout)
                    self.assertNotIn(fragment, stderr)

    def test_relay_db_variable_unset_or_empty_fails_with_the_fix(self):
        for env in ({}, {"POHUNEK_RELAY_TEST_DATABASE_URL": ""}):
            with self.subTest(env):
                status, doc, _ = self.run_json("--path", "relay-db", extra_env=env)
                self.assertEqual(status, 1, doc)
                row = doc["rows"][0]
                self.assertEqual(row["status"], "unset")
                self.assertEqual(row["category"], "required")
                self.assertIn(".agents/instructions/verification.md", row["fix"])
                self.assertIn("tests (relay DB, PostgreSQL)", row["fix"])
                self.assertEqual(doc["failing"], ["POHUNEK_RELAY_TEST_DATABASE_URL"])

    # -- native ----------------------------------------------------------------

    def test_native_suites_this_host_cannot_run_are_never_ok(self):
        # No systemctl or launchctl on PATH.
        status, doc, _ = self.run_json("--path", "native")
        self.assertEqual(status, 1, doc)
        rows = {row["name"]: row for row in doc["rows"]}
        self.assertNotIn("ok", [row["status"] for row in doc["rows"]])
        if IS_LINUX:
            self.assertNotIn("POHUNEK_SYSTEMD_E2E", rows)
            self.assertEqual(rows["systemd-user-manager"]["status"], "unavailable")
            self.assertIn("systemctl is not on PATH", rows["systemd-user-manager"]["detail"])
            self.assertIn("real systemd supervision", rows["systemd-user-manager"]["fix"])
            ci = rows["launchd-suite"]
            self.assertIn("platform contracts (arm64)", ci["fix"])
        elif IS_DARWIN:
            self.assertEqual(rows["launchd-gui-domain"]["status"], "unavailable")
            ci = rows["systemd-suites"]
            self.assertIn("real systemd supervision", ci["fix"])
        else:
            self.skipTest("native rows cover Linux and macOS hosts")
        self.assertEqual((ci["status"], ci["category"]), ("ci-only", "ci-only"))
        self.assertNotIn(ci["name"], doc["failing"])

    @unittest.skipUnless(IS_LINUX, "the real-systemd rows exist on Linux only")
    def test_native_systemd_user_manager_states(self):
        cases = {
            # (state printed, exit status) -> row status
            "running": ("running", 0, "ok"),
            "degraded": ("degraded", 1, "ok"),
            "offline": ("offline", 1, "unavailable"),
        }
        for case, (state, code, expected) in cases.items():
            with self.subTest(case):
                self.write_tool("systemctl", None, body=(
                    '[ "$1 $2" = "--user is-system-running" ] || exit 64; '
                    f"echo {state}; exit {code}"
                ))
                # The per-command opt-in is not a host capability: without it
                # a usable user manager still makes the path ready.
                status, doc, _ = self.run_json("--path", "native")
                rows = {row["name"]: row for row in doc["rows"]}
                manager = rows["systemd-user-manager"]
                self.assertEqual(manager["status"], expected, doc)
                self.assertEqual([row["name"] for row in doc["rows"]],
                                 ["systemd-user-manager", "launchd-suite"])
                self.assertEqual(rows["launchd-suite"]["status"], "ci-only")
                self.assertEqual(status, 0 if expected == "ok" else 1, doc)
                if expected == "ok":
                    self.assertIn("POHUNEK_SYSTEMD_E2E=1", manager["detail"])
                    self.assertIn("--ignored --test-threads 1", manager["detail"])
                else:
                    self.assertIn("'offline'", manager["detail"])

        status, output = self.run_script("--path", "native")
        self.assertEqual(status, 1, output)
        self.assertIn(
            "ci-only  launchd-suite: the launchd suites (crates/platform/tests/launchd.rs, "
            "crates/daemon/tests/supervised_lifecycle.rs, crates/cli/tests/service_launchd.rs "
            "and crates/cli/tests/service_upgrade.rs) run only on macOS (ci-only)",
            output,
        )

    # -- focused checks and final-gate pointers ---------------------------------

    def checks_of(self, path):
        """The `checks` of a `--json --path <path>` run, with the document."""
        self.write_bun_config()
        _, doc, _ = self.run_json("--path", path)
        return doc["checks"], doc

    def test_every_path_lists_focused_checks_and_points_to_the_final_gates(self):
        # Commands copied from the verification guide and the gates skill;
        # each path's list is complete on its own.
        expected_focused = {
            "rust": ["cargo xtask affected", "cargo t -p <crate>"],
            "sdk": ["bun install --frozen-lockfile", "bun run typecheck",
                    "bun run lint", "bun test", "bun test sdk/ts/scripts"],
            "scripts": [
                "python3 -m unittest discover -s scripts/tests -p 'test_*.py'",
                "python3 -m unittest discover -s scripts/tests -p test_<name>.py",
            ],
            "macos-lint": ["scripts/darwin-check"],
            "relay-db": ["python3 scripts/test-partitions run relay-db"],
        }
        for path in ("rust", "sdk", "scripts", "macos-lint", "relay-db", "native"):
            with self.subTest(path):
                checks, doc = self.checks_of(path)
                self.assertEqual(doc["version"], 1)
                for check in checks:
                    self.assertEqual(set(check), CHECK_KEYS)
                    self.assertEqual(check["path"], path)
                    self.assertIn(check["kind"], ("focused", "final", "ci-only"))
                    self.assertTrue(check["note"], check)
                    # Only a focused check is a command; a pointer or a CI
                    # job is named in the note.
                    if check["kind"] == "focused":
                        self.assertTrue(check["command"], check)
                    else:
                        self.assertIsNone(check["command"], check)
                kinds = [check["kind"] for check in checks]
                # Focused first, then the final pointers, then CI evidence.
                self.assertEqual(kinds, sorted(kinds, key=["focused", "final", "ci-only"].index))
                finals = [check["note"] for check in checks if check["kind"] == "final"]
                self.assertIn("Core gate set", finals[0])
                self.assertIn(GATES_SKILL, finals[0])
                self.assertIn(VERIFICATION_DOC, finals[0])
                focused = [check["command"] for check in checks if check["kind"] == "focused"]
                if path in expected_focused:
                    self.assertEqual(focused, expected_focused[path])
                if path == "sdk":
                    self.assertEqual(len(finals), 2)
                    self.assertIn("When the change touches the SDK workspace", finals[1])
                else:
                    self.assertEqual(len(finals), 1)

    def test_ci_only_checks_name_the_ci_job_that_is_the_evidence(self):
        # The other OS's native suites are a `ci-only` row, never repeated
        # as a check.
        expected = {
            "rust": [],
            "scripts": [],
            "macos-lint": ["platform contracts (arm64)"],
            "relay-db": ["tests (relay DB, PostgreSQL)"],
            "native": [],
        }
        if IS_LINUX:
            expected["sdk"] = ["SDK workspace (macOS arm64)"]
        elif IS_DARWIN:
            expected["sdk"] = []
        for path, jobs in expected.items():
            with self.subTest(path):
                checks, _ = self.checks_of(path)
                ci = [check["note"] for check in checks if check["kind"] == "ci-only"]
                self.assertEqual(len(ci), len(jobs), ci)
                for note, job in zip(ci, jobs):
                    self.assertTrue(note.startswith(f"CI job `{job}`"), note)

    def test_native_focused_checks_are_the_suites_this_os_runs(self):
        # Mirrors the CI job of this OS: build the binaries the suites spawn,
        # then every suite that job runs against the real supervisor.
        checks, doc = self.checks_of("native")
        focused = [check for check in checks if check["kind"] == "focused"]
        build = "cargo build --locked -p pohunek-daemon -p pohunek-session-worker -p pohunek-cli --bins"
        if IS_LINUX:
            self.assertEqual(
                [check["command"] for check in focused],
                [build, "POHUNEK_SYSTEMD_E2E=1 cargo test <suite> -- --ignored --test-threads 1"],
            )
            # The note names each suite the way `real systemd supervision`
            # selects it.
            for suite in ("-p pohunek-platform --test systemd",
                          "-p pohunek-daemon --test systemd_durable_worker",
                          "-p pohunek-cli --test service_systemd",
                          "-p pohunek-daemon --test supervised_lifecycle",
                          "-p pohunek-cli --test service_upgrade"):
                self.assertIn(f"`{suite}`", focused[1]["note"])
            # The other OS's suites are reported once, as the ci-only row.
            self.assertEqual(
                [(row["name"], row["status"]) for row in doc["rows"] if row["category"] == "ci-only"],
                [("launchd-suite", "ci-only")],
            )
        elif IS_DARWIN:
            self.assertEqual(
                [check["command"] for check in focused],
                [build, "cargo test --locked -p pohunek-platform",
                 "cargo test --locked -p pohunek-daemon", "cargo test --locked -p pohunek-cli"],
            )
            self.assertIn("crates/platform/tests/launchd.rs", focused[1]["note"])
            self.assertIn("crates/daemon/tests/supervised_lifecycle.rs", focused[2]["note"])
            self.assertIn("crates/cli/tests/service_launchd.rs", focused[3]["note"])
            self.assertIn("crates/cli/tests/service_upgrade.rs", focused[3]["note"])
        else:
            self.assertEqual(focused, [])

    def test_checks_are_listed_for_every_selected_path_whatever_the_result(self):
        # Nothing but rustc and python3 on PATH: not ready, checks still listed,
        # and a pointer shared by two paths appears under each.
        status, doc, _ = self.run_json("--path", "rust", "--path", "relay-db")
        self.assertEqual(status, 1, doc)
        self.assertEqual(doc["result"], "not-ready")
        self.assertEqual(
            [check["path"] for check in doc["checks"]],
            ["rust", "rust", "rust", "relay-db", "relay-db", "relay-db"],
        )
        finals = [check["path"] for check in doc["checks"] if check["kind"] == "final"]
        self.assertEqual(finals, ["rust", "relay-db"])

    def test_human_report_lists_checks_only_for_an_explicit_path(self):
        self.write_all_tools()
        status, default = self.run_script()
        self.assertEqual(status, 0, default)
        self.assertNotIn("checks:", default)
        self.assertNotIn("cargo xtask affected", default)

        status, explicit = self.run_script("--path", "rust", "--path", "relay-db")
        self.assertEqual(status, 1, explicit)
        # Each path: heading, rows, then its checks; the summary line stays last.
        rust_block = explicit.split("path rust:\n", 1)[1].split("path relay-db:\n", 1)[0]
        self.assertIn(
            "  checks:\n"
            "    focused  cargo xtask affected\n"
            "             `cargo ta`: ",
            rust_block,
        )
        self.assertIn("\n    focused  cargo t -p <crate>\n", rust_block)
        self.assertIn(
            '\n    final    the "Core gate set" in .agents/skills/gates/SKILL.md',
            rust_block,
        )
        self.assertLess(rust_block.index("ok       cargo-nextest"),
                        rust_block.index("  checks:"))
        relay_block = explicit.split("path relay-db:\n", 1)[1]
        self.assertIn(
            "    focused  python3 scripts/test-partitions run relay-db\n", relay_block
        )
        self.assertIn(
            "    ci-only  CI job `tests (relay DB, PostgreSQL)`: ", relay_block
        )
        self.assertTrue(
            explicit.endswith("dev-bootstrap: failing: POHUNEK_RELAY_TEST_DATABASE_URL\n"),
            explicit,
        )

    def test_error_documents_carry_no_checks(self):
        status, doc, _ = self.run_json("--path", "rust", "--path", "gui")
        self.assertEqual((status, doc["error"]), (2, "unsupported-path"), doc)
        self.assertEqual(doc["checks"], [])

        (self.root / ".config" / "nextest.toml").unlink()
        status, doc, _ = self.run_json("--path", "rust")
        self.assertEqual((status, doc["error"]), (2, "config-error"), doc)
        self.assertEqual(doc["checks"], [])


if __name__ == "__main__":
    unittest.main()
