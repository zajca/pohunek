"""Regression checks for the macOS packaging tooling (stdlib only).

The tools run on a real Mac, so these tests cover what any host can prove: the
shell scripts parse as POSIX sh, the Mach-O audit judges recorded `otool`,
`lipo`, and `strings` output exactly as its header documents (the real tools
are replaced by shims reading sidecar files), and the deployment target is one
value everywhere.
"""

import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
MACOS = ROOT / "packaging" / "macos"
AUDIT = MACOS / "audit-macho"
SCRIPTS = [
    MACOS / "audit-macho",
    MACOS / "build-release",
    MACOS / "package",
    ROOT / "scripts" / "acceptance" / "macos-package-install",
]

MACHO_MAGIC = bytes.fromhex("cffaedfe")

# Reads <file>.<suffix> instead of inspecting the file, as the Xcode tools do.
SHIM = """#!/bin/sh
set -eu
tool=$(basename "$0")
case "$tool $1" in
  "otool -l") exec cat "$2.otool-l" ;;
  "otool -L") exec cat "$2.otool-L" ;;
  "lipo -archs") exec cat "$2.archs" ;;
  "strings -a") [ ! -f "$2.strings-fails" ] || exit 1; exec cat "$2.strings" ;;
esac
echo "unexpected shim call: $tool $*" >&2
exit 99
"""

LOAD_COMMANDS = """Load command 1
      cmd LC_BUILD_VERSION
  cmdsize 32
 platform 1
    minos {minos}
      sdk 15.0
"""

LIBRARIES = """{name}:
\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1351.0.0)
\t/System/Library/Frameworks/Security.framework/Versions/A/Security (compatibility version 1.0.0, current version 61901.0.0)
"""


class AuditTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="pohunek-audit-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.tools = self.root / "tools"
        self.tools.mkdir()
        for name in ("otool", "lipo", "strings"):
            shim = self.tools / name
            shim.write_text(SHIM)
            shim.chmod(0o755)
        self.tree = self.root / "tree"
        self.tree.mkdir()

    def binary(self, name, *, archs="arm64", minos="14.0", libraries=None, rpath=None,
               strings="clean\n", magic=MACHO_MAGIC):
        path = self.tree / name
        path.write_bytes(magic + b"\0" * 12)
        load = LOAD_COMMANDS.format(minos=minos)
        if rpath:
            load += "Load command 2\n      cmd LC_RPATH\n  cmdsize 32\n     path {} (offset 12)\n".format(rpath)
        (self.tree / (name + ".otool-l")).write_text(load)
        (self.tree / (name + ".otool-L")).write_text(
            libraries if libraries is not None else LIBRARIES.format(name=name)
        )
        (self.tree / (name + ".archs")).write_text(archs + "\n")
        (self.tree / (name + ".strings")).write_text(strings)
        return path

    def audit(self, *args):
        env = dict(os.environ, PATH="{}:{}".format(self.tools, os.environ["PATH"]))
        return subprocess.run(
            [str(AUDIT), *map(str, args)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

    def test_a_native_arm64_binary_for_the_pinned_target_passes(self):
        path = self.binary("pohunek")
        result = self.audit(path)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 Mach-O file(s) passed", result.stdout)

    def test_a_directory_is_walked_and_non_macho_files_are_ignored(self):
        self.binary("pohunek")
        self.binary("pohunekd", minos="13.3")
        (self.tree / "README.md").write_text("text\n")
        result = self.audit(self.tree)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("2 Mach-O file(s) passed", result.stdout)

    def test_a_missing_or_empty_target_fails(self):
        self.assertNotEqual(self.audit(self.root / "absent").returncode, 0)
        (self.tree / "README.md").write_text("text\n")
        result = self.audit(self.tree)
        self.assertEqual(result.returncode, 1)
        self.assertIn("no Mach-O file found", result.stderr)

    def test_other_architectures_and_fat_images_fail(self):
        for archs in ("x86_64", "arm64 x86_64", "arm64e"):
            path = self.binary("a-" + archs.replace(" ", "-"), archs=archs)
            result = self.audit(path)
            self.assertEqual(result.returncode, 1, archs)
            self.assertIn("expected exactly arm64", result.stderr)
        fat = self.binary("fat", magic=bytes.fromhex("cafebabe"))
        result = self.audit(fat)
        self.assertEqual(result.returncode, 1)
        self.assertIn("universal (fat) binary", result.stderr)

    def test_a_newer_deployment_target_fails_and_a_missing_one_fails(self):
        result = self.audit(self.binary("new", minos="15.0"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("deployment target 15.0 is newer than 14.0", result.stderr)
        result = self.audit("--minimum-os", "15.0", self.binary("ok", minos="15.0"))
        self.assertEqual(result.returncode, 0, result.stderr)
        none = self.binary("none")
        (self.tree / "none.otool-l").write_text("Load command 1\n      cmd LC_SEGMENT_64\n")
        result = self.audit(none)
        self.assertEqual(result.returncode, 1)
        self.assertIn("no macOS deployment target", result.stderr)

    def test_minor_versions_compare_numerically(self):
        self.assertEqual(self.audit(self.binary("a", minos="14.10")).returncode, 1)
        self.assertEqual(self.audit("--minimum-os", "14.10", self.binary("b", minos="14.9")).returncode, 0)

    def test_libraries_outside_the_system_fail(self):
        for library in (
            "/opt/homebrew/opt/openssl/lib/libssl.3.dylib",
            "/usr/local/lib/libfoo.dylib",
            "@rpath/libbar.dylib",
            "@executable_path/libbaz.dylib",
        ):
            libraries = LIBRARIES.format(name="x") + "\t{} (compatibility version 1.0.0)\n".format(library)
            result = self.audit(self.binary("lib", libraries=libraries))
            self.assertEqual(result.returncode, 1, library)
            self.assertIn("links " + library, result.stderr)

    def test_a_runtime_search_path_fails(self):
        result = self.audit(self.binary("rpath", rpath="/Users/dev/build/lib"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("runtime search path", result.stderr)

    def test_build_machine_paths_fail(self):
        for text in (
            "panic at /Users/runner/work/pohunek/src/main.rs",
            "/home/runner/.cargo/registry/src/x",
            "/usr/local/Cellar/openssl/3.0/lib",
        ):
            result = self.audit(self.binary("leak", strings="ok\n" + text + "\n"))
            self.assertEqual(result.returncode, 1, text)
            self.assertIn("build-machine path", result.stderr)

    def test_legitimate_homebrew_and_volume_names_pass(self):
        result = self.audit(self.binary("path", strings="/opt/homebrew/bin\n/usr/local/bin\n/Volumes\n"))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_extra_forbidden_strings_apply(self):
        path = self.binary("extra", strings="/build/checkout/crates/x.rs\n")
        self.assertEqual(self.audit(path).returncode, 0)
        result = self.audit("--forbid-string", "/build/checkout", path)
        self.assertEqual(result.returncode, 1)
        self.assertIn("/build/checkout", result.stderr)

    def test_every_problem_is_reported_and_counted(self):
        path = self.binary("bad", archs="x86_64", minos="16.0", rpath="/tmp/x", strings="/Users/runner/x\n")
        result = self.audit(path)
        self.assertEqual(result.returncode, 1)
        for text in ("expected exactly arm64", "newer than", "runtime search path", "build-machine path"):
            self.assertIn(text, result.stderr)
        self.assertIn("4 problem(s)", result.stderr)
        self.assertNotIn("ok ", result.stdout)

    def test_a_required_program_must_itself_be_a_macho_file(self):
        self.binary("pohunek")
        (self.tree / "pohunekd").write_text("#!/bin/sh\n")
        result = self.audit("--require", "pohunek", "--require", "pohunekd", self.tree)
        self.assertEqual(result.returncode, 1)
        self.assertIn("pohunekd: the required program is missing or not a Mach-O file", result.stderr)
        self.assertNotIn("pohunek: the required", result.stderr.replace("pohunekd", ""))
        ok = self.audit("--require", "pohunek", self.tree)
        self.assertEqual(ok.returncode, 0, ok.stderr)
        self.assertEqual(self.audit("--require", "absent", self.tree).returncode, 1)
        self.assertEqual(self.audit("--require", "pohunek", self.tree / "pohunek").returncode, 2)

    def test_a_binary_for_another_apple_platform_fails(self):
        path = self.binary("ios")
        (self.tree / "ios.otool-l").write_text(LOAD_COMMANDS.format(minos="14.0").replace("platform 1", "platform 2"))
        result = self.audit(path)
        self.assertEqual(result.returncode, 1)
        self.assertIn("built for platform '2', not macOS", result.stderr)
        named = self.binary("named")
        (self.tree / "named.otool-l").write_text(LOAD_COMMANDS.format(minos="14.0").replace("platform 1", "platform MACOS"))
        self.assertEqual(self.audit(named).returncode, 0)

    def test_a_failing_strings_tool_fails_the_audit(self):
        path = self.binary("pohunek")
        (self.tree / "pohunek.strings-fails").write_text("")
        result = self.audit(path)
        self.assertEqual(result.returncode, 1)
        self.assertIn("build-machine path check did not run", result.stderr)
        self.assertNotIn("ok ", result.stdout)

    def test_bad_arguments_are_refused(self):
        for args in (["--minimum-os"], ["--forbid-string", ""], ["--minimum-os", "x", "."], ["--nope"], []):
            self.assertEqual(self.audit(*args).returncode, 2, args)


class BuildReleaseTest(unittest.TestCase):
    def test_a_relative_target_directory_is_reported_below_the_repository_root(self):
        tools = Path(tempfile.mkdtemp(prefix="pohunek-build-"))
        self.addCleanup(shutil.rmtree, tools, ignore_errors=True)
        for name, text in (
            ("uname", '#!/bin/sh\ncase "$1" in -s) echo Darwin ;; -m) echo arm64 ;; esac\n'),
            ("rustc", "#!/bin/sh\necho /fake/sysroot\n"),
            ("cargo", "#!/bin/sh\nexit 0\n"),
        ):
            path = tools / name
            path.write_text(text)
            path.chmod(0o755)
        env = dict(os.environ, PATH="%s:%s" % (tools, os.environ["PATH"]), CARGO_TARGET_DIR="out/target")
        result = subprocess.run(
            [str(MACOS / "build-release"), "cli"],
            cwd=ROOT / "packaging",
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "%s/out/target/aarch64-apple-darwin/release" % ROOT)


class BuildFlagsTest(unittest.TestCase):
    def test_remap_flags_survive_spaces_and_keep_the_callers_flags(self):
        root = Path(tempfile.mkdtemp(prefix="pohunek build "))
        self.addCleanup(shutil.rmtree, root, ignore_errors=True)
        tools = root / "tools"
        tools.mkdir()
        record = root / "flags"
        for name, text in (
            ("uname", '#!/bin/sh\ncase "$1" in -s) echo Darwin ;; -m) echo arm64 ;; esac\n'),
            ("rustc", "#!/bin/sh\necho '/fake sysroot'\n"),
            ("cargo", '#!/bin/sh\nprintf "%s" "$CARGO_ENCODED_RUSTFLAGS" > "$RECORD"\n[ -z "${RUSTFLAGS:-}" ]\n'),
        ):
            path = tools / name
            path.write_text(text)
            path.chmod(0o755)
        env = dict(
            os.environ,
            PATH="%s:%s" % (tools, os.environ["PATH"]),
            RUSTFLAGS="-D warnings",
            RECORD=str(record),
            CARGO_HOME="/cargo home",
        )
        result = subprocess.run(
            [str(MACOS / "build-release"), "cli"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        flags = record.read_text().split("\x1f")
        self.assertEqual(flags[:2], ["-D", "warnings"])
        self.assertIn("--remap-path-prefix=%s=/build/pohunek" % ROOT, flags)
        self.assertIn("--remap-path-prefix=/cargo home=/build/cargo", flags)
        self.assertIn("--remap-path-prefix=/fake sysroot=/build/rust-sysroot", flags)


class ToolingTest(unittest.TestCase):
    def test_scripts_parse_as_posix_sh_and_are_executable(self):
        for script in SCRIPTS:
            self.assertTrue(os.stat(script).st_mode & stat.S_IXUSR, script)
            result = subprocess.run(["sh", "-n", str(script)], stderr=subprocess.PIPE, text=True)
            self.assertEqual(result.returncode, 0, "{}: {}".format(script, result.stderr))
            self.assertTrue(script.read_text().startswith("#!/bin/sh\n"), script)

    def test_the_deployment_target_is_one_value(self):
        target = (MACOS / "DEPLOYMENT_TARGET").read_text().strip()
        self.assertRegex(target, r"^\d+\.\d+$")
        text = (ROOT / ".github/workflows/ci.yml").read_text()
        for value in re.findall(r'MACOSX_DEPLOYMENT_TARGET[:=] ?"?([0-9.]+)', text):
            self.assertEqual(value, target)
        for doc in ("AGENTS.md", "docs/design/macos-support-rfc.md"):
            self.assertIn("MACOSX_DEPLOYMENT_TARGET={}".format(target), (ROOT / doc).read_text(), doc)

    def test_the_development_package_is_never_a_release_name(self):
        text = (MACOS / "package").read_text()
        self.assertIn("-unsigned-development", text)
        self.assertIn("unsigned-development", text.split("write-manifest", 1)[1])


if __name__ == "__main__":
    unittest.main()
