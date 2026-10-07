"""Regression checks for the macOS ad-hoc signing tooling (stdlib only).

The tools need a Mac, so these tests run them against a shim of `codesign`
that records its arguments and prints canned output. They prove the tooling's
own behavior: every Mach-O file is ad-hoc signed with the documented flags and
identifier, the verifier accepts only ad-hoc signatures and rejects unsigned,
broken, and certificate-signed files, and the release packaging step audits,
signs, verifies, and archives in order with `signing adhoc` in the manifest.
The release workflow checks pin the job layout that keeps credentials out of
every macOS job.
"""

import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
MACOS = ROOT / "packaging" / "macos"

MACHO = bytes.fromhex("cffaedfe") + b"\0" * 12

ADHOC_DETAILS = """Executable=/x/pohunek
Identifier=io.github.zajca.pohunek.pohunek
Format=Mach-O thin (arm64)
CodeDirectory v=20400 size=500 flags=0x2(adhoc) hashes=10+2 location=embedded
Signature=adhoc
TeamIdentifier=not set
"""

DEVELOPER_ID_DETAILS = """Executable=/x/pohunek
Identifier=io.github.zajca.pohunek.pohunek
Format=Mach-O thin (arm64)
CodeDirectory v=20500 size=900 flags=0x10000(runtime) hashes=20+7 location=embedded
Authority=Developer ID Application: Example (ABCDE12345)
Authority=Developer ID Certification Authority
Authority=Apple Root CA
Timestamp=Oct 1, 2026 at 10:00:00
TeamIdentifier=ABCDE12345
"""

# Records every call as one line; prints canned output chosen by environment
# variables the test sets.
CODESIGN = """#!/bin/sh
printf 'codesign %s\\n' "$*" >> "$SHIM_LOG"
for arg in "$@"; do
  if [ "$arg" = -dvv ]; then
    cat "$SHIM_CODESIGN_DETAILS" >&2
    exit "${SHIM_CODESIGN_DESCRIBE_STATUS:-0}"
  fi
done
case " $* " in
  *" --verify "*) exit "${SHIM_CODESIGN_VERIFY_STATUS:-0}" ;;
esac
exit 0
"""


def write_shim(directory, name, text):
    path = directory / name
    path.write_text(text)
    path.chmod(0o755)


class Base(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="pohunek-signing-"))
        self.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.tools = self.root / "tools"
        self.tools.mkdir()
        write_shim(self.tools, "codesign", CODESIGN)
        self.log = self.root / "shim.log"
        self.log.write_text("")
        self.details = self.root / "details.txt"
        self.details.write_text(ADHOC_DETAILS)
        self.staging = self.root / "staging"
        self.staging.mkdir()

    def env(self, **extra):
        env = {
            "PATH": "{}:{}".format(self.tools, os.environ["PATH"]),
            "SHIM_LOG": str(self.log),
            "SHIM_CODESIGN_DETAILS": str(self.details),
            "TMPDIR": str(self.root),
        }
        env.update(extra)
        return env

    def run_tool(self, name, *args, env=None):
        full = dict(self.env(), **(env or {}))
        return subprocess.run(
            [str(MACOS / name), *map(str, args)],
            env=full,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

    def calls(self, prefix=""):
        return [line for line in self.log.read_text().splitlines() if line.startswith(prefix)]

    def macho(self, relative):
        path = self.staging / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(MACHO)
        path.chmod(0o755)
        return path


class SignTest(Base):
    def test_every_macho_is_ad_hoc_signed_with_a_stable_identifier(self):
        self.macho("pohunek")
        self.macho("pohunekd")
        (self.staging / "README.md").write_text("text")
        result = self.run_tool("sign", self.staging)
        self.assertEqual(result.returncode, 0, result.stderr)
        signs = [c for c in self.calls("codesign") if "--sign" in c]
        self.assertEqual(len(signs), 2)
        for name in ("pohunek", "pohunekd"):
            line = next(c for c in signs if c.endswith("/" + name))
            self.assertIn("--force", line)
            self.assertIn("--sign - ", line)
            self.assertIn("--identifier io.github.zajca.pohunek." + name, line)
            # No certificate, keychain, hardened runtime, or timestamp.
            for flag in ("--keychain", "--options", "--timestamp", "--entitlements"):
                self.assertNotIn(flag, line)
        verifies = [c for c in self.calls("codesign") if "--verify --strict" in c]
        self.assertEqual(len(verifies), 2)

    def test_an_app_bundle_is_refused(self):
        self.macho("Example.app/Contents/MacOS/example")
        result = self.run_tool("sign", self.staging)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("app bundles", result.stderr)
        self.assertEqual(self.calls("codesign"), [])

    def test_nothing_to_sign_fails(self):
        empty = self.root / "empty"
        empty.mkdir()
        result = self.run_tool("sign", empty)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("nothing to sign", result.stderr)

    def test_a_failed_verification_fails_the_signing(self):
        self.macho("pohunek")
        result = self.run_tool("sign", self.staging, env={"SHIM_CODESIGN_VERIFY_STATUS": "1"})
        self.assertNotEqual(result.returncode, 0)


class VerifySignedTest(Base):
    def verify(self, *args, **env):
        return self.run_tool("verify-signed", *args, env=env)

    def test_an_ad_hoc_signed_tree_passes_with_strict_verification(self):
        self.macho("pohunek")
        self.macho("pohunekd")
        result = self.verify("--adhoc", self.staging)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("2 ad-hoc signed file(s) verified", result.stdout)
        self.assertEqual(len([c for c in self.calls("codesign") if "--verify --strict" in c]), 2)

    def test_a_developer_id_signature_is_rejected(self):
        self.macho("pohunek")
        self.details.write_text(DEVELOPER_ID_DETAILS)
        result = self.verify("--adhoc", self.staging)
        self.assertEqual(result.returncode, 1)
        self.assertIn("not ad-hoc", result.stderr)

    def test_an_unsigned_file_is_rejected(self):
        self.macho("pohunek")
        result = self.verify("--adhoc", self.staging, SHIM_CODESIGN_VERIFY_STATUS="1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("does not verify", result.stderr)

    def test_a_signature_that_codesign_cannot_describe_is_rejected(self):
        self.macho("pohunek")
        result = self.verify("--adhoc", self.staging, SHIM_CODESIGN_DESCRIBE_STATUS="1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("cannot describe", result.stderr)

    def test_a_signature_without_the_adhoc_marker_is_rejected(self):
        self.macho("pohunek")
        self.details.write_text(ADHOC_DETAILS.replace("Signature=adhoc\n", ""))
        result = self.verify("--adhoc", self.staging)
        self.assertEqual(result.returncode, 1)
        self.assertIn("not ad-hoc signed", result.stderr)

    def test_one_bad_file_fails_the_tree_and_every_problem_is_reported(self):
        self.macho("pohunek")
        self.macho("pohunekd")
        result = self.verify("--adhoc", self.staging, SHIM_CODESIGN_VERIFY_STATUS="1")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr.count("FAIL "), 2)

    def test_the_mode_switch_is_required(self):
        self.macho("pohunek")
        self.assertEqual(self.verify(self.staging).returncode, 2)
        for old in ("--notarized", "--team-id"):
            self.assertEqual(self.verify(old, self.staging).returncode, 2, old)

    def test_a_tree_without_a_macho_file_is_not_a_pass(self):
        empty = self.root / "empty"
        empty.mkdir()
        (empty / "README.md").write_text("text")
        result = self.verify("--adhoc", empty)
        self.assertEqual(result.returncode, 1)
        self.assertIn("no Mach-O file", result.stderr)


class PackageReleaseTest(Base):
    NAME = "pohunek-cli-1.2.3-aarch64-apple-darwin"

    def audit_tools(self):
        # The audit reads the tree through otool, lipo, and strings, which the
        # shims below answer for every file.
        audit_tools = self.root / "audit-tools"
        audit_tools.mkdir(exist_ok=True)
        for tool in ("otool", "lipo", "strings"):
            write_shim(
                audit_tools,
                tool,
                "#!/bin/sh\n"
                "case \"$1\" in\n"
                "  -archs) echo arm64 ;;\n"
                "  -l) printf 'Load command 1\\n      cmd LC_BUILD_VERSION\\n platform 1\\n    minos 14.0\\n' ;;\n"
                "  -L) printf 'x:\\n\\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0)\\n' ;;\n"
                "  -a) echo clean ;;\n"
                "esac\n",
            )
        return audit_tools

    def release_env(self):
        return {
            "SOURCE_DATE_EPOCH": "1700000000",
            "PATH": "%s:%s:%s" % (self.audit_tools(), self.tools, os.environ["PATH"]),
        }

    def test_a_development_tree_or_a_misnamed_tree_is_never_packaged_as_a_release(self):
        for name in (
            "pohunek-daemon-1.2.3-aarch64-apple-darwin-unsigned-development",
            "pohunek-cli-1.2.3-aarch64-apple-darwin",
        ):
            staging = self.root / name
            staging.mkdir()
            result = self.run_tool("package", "--adhoc-release", "daemon", "1.2.3", staging)
            self.assertEqual(result.returncode, 1, name)
            self.assertIn(
                "development staging" if name.endswith("development") else "unexpected staging directory name",
                result.stderr,
            )
        self.assertEqual(self.calls(), [])

    def test_the_release_step_audits_signs_verifies_and_archives_in_order(self):
        staging = self.root / self.NAME
        program = staging / "pohunek"
        program.parent.mkdir()
        program.write_bytes(MACHO)
        program.chmod(0o755)
        (staging / "README.md").write_text("text\n")
        result = self.run_tool("package", "--adhoc-release", "cli", "1.2.3", staging, env=self.release_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        archive = Path(result.stdout.strip())
        self.assertEqual(archive, self.root / (self.NAME + ".tar.gz"))
        self.assertTrue(archive.is_file())
        checksum = (self.root / (self.NAME + ".tar.gz.sha256")).read_text()
        self.assertTrue(checksum.strip().endswith(self.NAME + ".tar.gz"))
        manifest = (staging / "MANIFEST").read_text()
        self.assertIn("signing adhoc\n", manifest)
        self.assertIn("minimum-macos 14.0\n", manifest)
        self.assertIn("component cli\n", manifest)
        calls = self.calls()
        first_sign = next(i for i, c in enumerate(calls) if c.startswith("codesign") and "--sign" in c)
        verify = next(i for i, c in enumerate(calls) if c.startswith("codesign") and "-dvv" in c)
        self.assertLess(first_sign, verify)

    def test_a_tree_that_fails_verification_is_not_archived(self):
        staging = self.root / self.NAME
        program = staging / "pohunek"
        program.parent.mkdir()
        program.write_bytes(MACHO)
        program.chmod(0o755)
        self.details.write_text(DEVELOPER_ID_DETAILS)
        result = self.run_tool("package", "--adhoc-release", "cli", "1.2.3", staging, env=self.release_env())
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((staging / "MANIFEST").exists())
        self.assertFalse((self.root / (self.NAME + ".tar.gz")).exists())

    def test_a_staged_tree_with_a_symlink_is_refused_before_anything_is_signed(self):
        staging = self.root / self.NAME
        staging.mkdir()
        (staging / "link").symlink_to("/etc/passwd")
        result = self.run_tool("package", "--adhoc-release", "cli", "1.2.3", staging)
        self.assertEqual(result.returncode, 1)
        self.assertIn("symbolic link or special file", result.stderr)
        self.assertEqual(self.calls(), [])

    def test_modes_and_components_are_validated(self):
        for args in (
            ("--sideload", "daemon", "1.2.3", self.root, self.root, self.root),
            ("--development", "relay", "1.2.3", self.root, self.root, self.root),
            ("--development", "daemon"),
            ("--release", "daemon", "1.2.3", self.root, self.root, self.root),
            ("--sign-release", "daemon", "1.2.3", self.root),
        ):
            result = self.run_tool("package", *args)
            self.assertEqual(result.returncode, 1, args)


class ReleaseWorkflowTest(unittest.TestCase):
    def setUp(self):
        self.text = (ROOT / ".github/workflows/release.yml").read_text()
        self.stage = self.job("stage-macos", "package-macos")
        self.package = self.job("package-macos", "verify-macos")
        self.release = self.job("verify-macos", "attest")
        self.attest = self.job("attest", "publish-macos")
        self.publish = self.job("publish-macos", None)
        self.macos = {
            "stage": self.stage,
            "package": self.package,
            "verify": self.release,
            "publish": self.publish,
        }

    def job(self, name, following):
        start = self.text.index("\n  %s:\n" % name)
        end = self.text.index("\n  %s:\n" % following) if following else len(self.text)
        return self.text[start:end]

    def test_no_macos_job_has_a_secret_an_environment_or_a_certificate_variable(self):
        for name, job in self.macos.items():
            self.assertNotIn("secrets.", job, name)
            self.assertNotIn("vars.", job, name)
            self.assertNotRegex(job, r"(?m)^\s+environment:", name)
            self.assertNotIn("MACOS_", job, name)
            self.assertNotIn("APPLE_", job, name)
        self.assertNotIn("sign-macos", self.text)

    def test_the_developer_id_tooling_is_gone(self):
        for name in ("notarize", "signing-keychain"):
            self.assertFalse((MACOS / name).exists(), name)
        for token in ("notarize", "signing-keychain", "developer-id", "macos-signing", "--notarized"):
            self.assertNotIn(token, self.text, token)

    def test_every_macos_job_runs_on_the_arm64_runner_and_stage_and_package_check_it(self):
        for name, job in self.macos.items():
            self.assertIn("runs-on: macos-15", job, name)
        for name in ("stage", "package"):
            self.assertIn('test "$(uname -m)" = "arm64"', self.macos[name], name)

    def test_the_packaging_job_uses_only_pinned_actions_and_runs_nothing_from_the_tree(self):
        for use in re.findall(r"uses: (\S+)", self.package):
            self.assertRegex(use, r"@[0-9a-f]{40}", use)
        for forbidden in ("--stage-release", "cargo", "bun ", "--version", "smoke", "stage-archive", "setup-"):
            self.assertNotIn(forbidden, self.package, forbidden)

    def test_every_action_that_shapes_the_signed_bytes_is_pinned(self):
        for name, job in self.macos.items():
            for use in re.findall(r"uses: (\S+)", job):
                self.assertRegex(use, r"@[0-9a-f]{40}", "%s: %s" % (name, use))

    def test_no_artifact_derived_value_is_interpolated_into_a_script(self):
        for name, job in (("package", self.package), ("verify", self.release)):
            self.assertNotIn("steps.stage.outputs", job, name)
        self.assertNotIn("$(ls", self.package + self.release)
        self.assertIn("entries outside", self.package)
        self.assertIn("parent-directory component", self.package)

    def test_the_staged_tree_travels_as_a_checked_tar(self):
        self.assertIn("stage.tar.sha256", self.stage)
        self.assertIn("shasum -a 256 -c stage.tar.sha256", self.package)
        self.assertIn("packaging/macos/package --stage-release", self.stage)
        self.assertIn("packaging/macos/package --adhoc-release", self.package)
        self.assertNotIn("--adhoc-release", self.stage)
        self.assertNotIn("--stage-release", self.package)
        self.assertNotIn("--development", self.text.split("\n  stage-macos:\n", 1)[1])

    def test_the_release_job_verifies_the_published_bytes_as_ad_hoc(self):
        self.assertIn("grep -q '^signing adhoc$'", self.release)
        self.assertIn("packaging/macos/verify-signed --adhoc", self.release)
        self.assertIn("RUNNER_TEMP/extracted", self.release)
        self.assertIn("packaging/smoke-hermes-plugin-release", self.release)
        self.assertIn('"$root/$binary" --version', self.release)
        self.assertNotIn("action-gh-release", self.stage + self.package + self.release)
        self.assertIn("needs: [package-macos]", self.release)
        self.assertIn("needs: [stage-macos]", self.package)

    def test_the_attest_job_covers_the_macos_archives(self):
        self.assertIn("needs: [build, verify-macos, sdk-pack]", self.attest)
        self.assertIn("pattern: macos-signed-*", self.attest)
        self.assertIn("actions/attest@", self.attest)

    def test_only_the_publishing_job_can_write_and_it_runs_nothing_from_the_archive(self):
        self.assertIn("contents: write", self.publish)
        for name, job in (("stage", self.stage), ("package", self.package), ("verify", self.release)):
            self.assertNotIn("contents: write", job, name)
        self.assertIn("contents: read", self.release)
        self.assertIn("action-gh-release", self.publish)
        for forbidden in ("tar -x", "--version", "smoke", "cargo", "bun ", "verify-signed", "checkout"):
            self.assertNotIn(forbidden, self.publish, forbidden)
        self.assertIn("shasum -a 256 -c", self.publish)


if __name__ == "__main__":
    unittest.main()
