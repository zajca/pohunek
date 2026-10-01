"""Regression checks for the macOS signing and notarization tooling (stdlib only).

The tools need a Mac, a certificate, and Apple's notary service, so these tests
run them against shims of `codesign`, `security`, `xcrun`, `ditto`, `plutil`,
and `spctl` that record their arguments and print canned output. They prove the
tooling's own behavior: every credential is required, every item is signed with
the documented flags, the notary verdict gates the build, and secrets stay out
of the output and the environment file.
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

TEAM = "ABCDE12345"
IDENTITY = "0123456789ABCDEF0123456789ABCDEF01234567"
KEYCHAIN = "/tmp/signing.keychain-db"

GOOD_DETAILS = """Executable=/x/pohunek
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
  *" --check-notarization "*) exit "${SHIM_CODESIGN_NOTARIZED_STATUS:-0}" ;;
  *" --verify "*) exit "${SHIM_CODESIGN_VERIFY_STATUS:-0}" ;;
esac
exit 0
"""

XCRUN = """#!/bin/sh
printf 'xcrun %s\\n' "$*" >> "$SHIM_LOG"
case "$1 $2" in
  "notarytool submit")
    printf '%s\\n' "$*" | grep -q -e '--key ' || exit 90
    cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>id</key><string>${SHIM_NOTARY_ID-11111111-2222-3333-4444-555555555555}</string>
<key>status</key><string>${SHIM_NOTARY_STATUS:-Accepted}</string>
</dict></plist>
PLIST
    exit "${SHIM_NOTARY_EXIT:-0}" ;;
  "notarytool log") echo "notary log for $3: issues found"; exit 0 ;;
  "stapler staple") exit 0 ;;
  "stapler validate") exit "${SHIM_STAPLER_VALIDATE_STATUS:-0}" ;;
esac
exit 91
"""

DITTO = """#!/bin/sh
printf 'ditto %s\\n' "$*" >> "$SHIM_LOG"
for last; do :; done
: > "$last"
"""

PLUTIL = """#!/usr/bin/env python3
import re, sys
# plutil -extract KEY raw -o - FILE
key, file = sys.argv[2], sys.argv[-1]
match = re.search(r"<key>%s</key>\\s*<string>(.*?)</string>" % re.escape(key), open(file).read(), re.S)
if not match:
    sys.exit(1)
print(match.group(1))
"""

SPCTL = """#!/bin/sh
printf 'spctl %s\\n' "$*" >> "$SHIM_LOG"
exit "${SHIM_SPCTL_STATUS:-0}"
"""

SECURITY = """#!/bin/sh
printf 'security %s\\n' "$(printf '%s' "$*" | sed -e 's/-P [^ ]*/-P <redacted>/' -e 's/-k [a-f0-9]\\{48\\}/-k <redacted>/' -e 's/-p [a-f0-9]\\{48\\}/-p <redacted>/')" >> "$SHIM_LOG"
case "$1" in
  list-keychains)
    if [ "$2 $3" = "-d user" ] && [ "$#" -eq 3 ]; then
      printf '    "/Users/runner/Library/Keychains/login.keychain-db"\\n    "/Library/Keychains/System.keychain"\\n'
    fi ;;
  import) exit "${SHIM_SECURITY_IMPORT_STATUS:-0}" ;;
  find-identity) printf '  1) %s "Developer ID Application: Example (ABCDE12345)"\\n     1 valid identities found\\n' "$SHIM_IDENTITY" ;;
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
        for name, text in (
            ("codesign", CODESIGN),
            ("xcrun", XCRUN),
            ("ditto", DITTO),
            ("plutil", PLUTIL),
            ("spctl", SPCTL),
            ("security", SECURITY),
        ):
            write_shim(self.tools, name, text)
        self.log = self.root / "shim.log"
        self.log.write_text("")
        self.details = self.root / "details.txt"
        self.details.write_text(GOOD_DETAILS)
        self.staging = self.root / "staging"
        self.staging.mkdir()

    def env(self, **extra):
        env = {
            "PATH": "{}:{}".format(self.tools, os.environ["PATH"]),
            "SHIM_LOG": str(self.log),
            "SHIM_CODESIGN_DETAILS": str(self.details),
            "SHIM_IDENTITY": IDENTITY,
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

    def app(self):
        self.macho("Pohunek.app/Contents/MacOS/pohunek-gui")
        (self.staging / "Pohunek.app/Contents/Info.plist").write_text("<plist/>")
        return self.staging / "Pohunek.app"


SIGNING_ENV = {"MACOS_SIGNING_IDENTITY": IDENTITY, "MACOS_SIGNING_KEYCHAIN": KEYCHAIN}


class SignTest(Base):
    def test_every_macho_is_signed_with_the_hardened_runtime_and_a_timestamp(self):
        self.macho("pohunek")
        self.macho("pohunekd")
        (self.staging / "README.md").write_text("text")
        result = self.run_tool("sign", self.staging, env=SIGNING_ENV)
        self.assertEqual(result.returncode, 0, result.stderr)
        signs = [c for c in self.calls("codesign") if "--sign" in c]
        self.assertEqual(len(signs), 2)
        for name in ("pohunek", "pohunekd"):
            line = next(c for c in signs if c.endswith("/" + name))
            for flag in (
                "--force",
                "--options runtime",
                "--timestamp",
                "--sign " + IDENTITY,
                "--keychain " + KEYCHAIN,
                "--identifier io.github.zajca.pohunek." + name,
            ):
                self.assertIn(flag, line)
        verifies = [c for c in self.calls("codesign") if "--verify --strict" in c]
        self.assertEqual(len(verifies), 2)

    def test_an_app_bundle_signs_its_executable_before_the_bundle(self):
        app = self.app()
        self.macho("pohunek")
        result = self.run_tool("sign", self.staging, env=SIGNING_ENV)
        self.assertEqual(result.returncode, 0, result.stderr)
        signs = [c.split(" ")[-1] for c in self.calls("codesign") if "--sign" in c]
        self.assertEqual(
            signs,
            [
                str(self.staging / "pohunek"),
                str(app / "Contents/MacOS/pohunek-gui"),
                str(app),
            ],
        )
        self.assertTrue(any("--verify --deep --strict" in c for c in self.calls("codesign")))
        # The executable inside the bundle is never signed on its own with an
        # identifier of its own.
        self.assertFalse(any("--identifier" in c and "pohunek-gui" in c for c in self.calls("codesign")))

    def test_missing_credentials_or_nothing_to_sign_fail(self):
        self.macho("pohunek")
        for missing in SIGNING_ENV:
            env = {k: v for k, v in SIGNING_ENV.items() if k != missing}
            result = self.run_tool("sign", self.staging, env=dict(env, **{missing: ""}))
            self.assertNotEqual(result.returncode, 0, missing)
            self.assertIn(missing, result.stderr)
        self.assertEqual(self.calls("codesign"), [])
        empty = self.root / "empty"
        empty.mkdir()
        result = self.run_tool("sign", empty, env=SIGNING_ENV)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("nothing to sign", result.stderr)

    def test_a_failed_verification_fails_the_signing(self):
        self.macho("pohunek")
        result = self.run_tool("sign", self.staging, env=dict(SIGNING_ENV, SHIM_CODESIGN_VERIFY_STATUS="1"))
        self.assertNotEqual(result.returncode, 0)


NOTARY_ENV = {
    "APPLE_NOTARY_KEY_P8_BASE64": "c2VjcmV0LWtleS1tYXRlcmlhbA==",
    "APPLE_NOTARY_KEY_ID": "KEYID12345",
    "APPLE_NOTARY_ISSUER_ID": "69a6de7a-0000-0000-0000-000000000000",
}


class NotarizeTest(Base):
    def test_an_accepted_submission_is_stapled_and_recorded(self):
        self.app()
        summary = self.root / "summary.md"
        result = self.run_tool("notarize", self.staging, env=dict(NOTARY_ENV, GITHUB_STEP_SUMMARY=str(summary)))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("11111111-2222-3333-4444-555555555555", result.stdout)
        submit = self.calls("xcrun notarytool submit")[0]
        for text in ("--wait", "--key ", "--key-id KEYID12345", "--issuer 69a6de7a", "--output-format plist"):
            self.assertIn(text, submit)
        self.assertEqual(len(self.calls("ditto -c -k --keepParent " + str(self.staging))), 1)
        self.assertTrue(any(c.startswith("xcrun stapler staple") and c.endswith("Pohunek.app") for c in self.calls()))
        self.assertTrue(any(c.startswith("xcrun stapler validate") for c in self.calls()))
        self.assertIn("Accepted", summary.read_text())

    def test_a_rejected_submission_prints_the_log_and_fails(self):
        self.app()
        result = self.run_tool("notarize", self.staging, env=dict(NOTARY_ENV, SHIM_NOTARY_STATUS="Invalid"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("not accepted", result.stderr)
        self.assertIn("notary log for 11111111", result.stderr)
        self.assertEqual(self.calls("xcrun stapler"), [])

    def test_a_failing_notarytool_is_never_trusted_even_with_an_accepted_plist(self):
        self.app()
        result = self.run_tool("notarize", self.staging, env=dict(NOTARY_ENV, SHIM_NOTARY_EXIT="1"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("exit status 1", result.stderr)
        self.assertEqual(self.calls("xcrun stapler"), [])

    def test_no_submission_id_fails(self):
        result = self.run_tool("notarize", self.staging, env=dict(NOTARY_ENV, SHIM_NOTARY_ID="", SHIM_NOTARY_EXIT="1"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not trusted", result.stderr)

    def test_every_credential_is_required(self):
        for missing in NOTARY_ENV:
            result = self.run_tool("notarize", self.staging, env=dict(NOTARY_ENV, **{missing: ""}))
            self.assertNotEqual(result.returncode, 0, missing)
            self.assertIn(missing, result.stderr)
        self.assertEqual(self.calls("xcrun"), [])

    def test_the_key_material_never_reaches_the_output(self):
        self.app()
        result = self.run_tool("notarize", self.staging, env=NOTARY_ENV)
        output = result.stdout + result.stderr
        self.assertNotIn(NOTARY_ENV["APPLE_NOTARY_KEY_P8_BASE64"], output)
        # Key id and issuer appear only in the masking commands, which the
        # Actions runner turns into masked log text.
        for value in (NOTARY_ENV["APPLE_NOTARY_KEY_ID"], NOTARY_ENV["APPLE_NOTARY_ISSUER_ID"]):
            lines = [line for line in output.splitlines() if value in line]
            self.assertTrue(lines and all(line.startswith("::add-mask::") for line in lines), value)


class VerifySignedTest(Base):
    def verify(self, *args, **env):
        return self.run_tool("verify-signed", *args, env=env)

    def test_a_correctly_signed_tree_passes(self):
        self.macho("pohunek")
        self.app()
        result = self.verify("--team-id", TEAM, self.staging)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("2 signed item(s) verified", result.stdout)

    def test_each_defect_is_reported(self):
        self.macho("pohunek")
        cases = {
            "ad-hoc": ("Signature=adhoc\n" + GOOD_DETAILS, "ad-hoc signature"),
            "authority": (GOOD_DETAILS.replace("Developer ID Application", "Apple Development"), "Developer ID Application"),
            "runtime": (GOOD_DETAILS.replace("(runtime)", "()"), "hardened runtime"),
            "timestamp": (GOOD_DETAILS.replace("Timestamp=Oct 1, 2026 at 10:00:00\n", ""), "timestamp"),
            "team": (GOOD_DETAILS.replace(TEAM, "ZZZZZ99999"), "team identifier"),
        }
        for label, (details, message) in cases.items():
            self.details.write_text(details)
            result = self.verify("--team-id", TEAM, self.staging)
            self.assertEqual(result.returncode, 1, label)
            self.assertIn(message, result.stderr, label)

    def test_a_bare_binary_must_be_notarized_when_notarization_is_required(self):
        self.macho("pohunek")
        ok = self.verify("--notarized", "--team-id", TEAM, self.staging)
        self.assertEqual(ok.returncode, 0, ok.stderr)
        self.assertTrue(any("-R=notarized --check-notarization" in c for c in self.calls("codesign")))
        bad = self.verify("--notarized", "--team-id", TEAM, self.staging, SHIM_CODESIGN_NOTARIZED_STATUS="3")
        self.assertEqual(bad.returncode, 1)
        self.assertIn("not notarized", bad.stderr)
        # Without the switch the check does not run.
        self.log.write_text("")
        self.verify("--team-id", TEAM, self.staging)
        self.assertFalse(any("--check-notarization" in c for c in self.calls("codesign")))

    def test_a_failing_verification_is_reported(self):
        self.macho("pohunek")
        result = self.verify("--team-id", TEAM, self.staging, SHIM_CODESIGN_VERIFY_STATUS="1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("does not verify", result.stderr)

    def test_notarization_needs_a_ticket_and_a_gatekeeper_verdict(self):
        self.app()
        ok = self.verify("--notarized", "--team-id", TEAM, self.staging)
        self.assertEqual(ok.returncode, 0, ok.stderr)
        self.assertTrue(any(c.startswith("spctl --assess --type execute") for c in self.calls()))
        no_ticket = self.verify("--notarized", "--team-id", TEAM, self.staging, SHIM_STAPLER_VALIDATE_STATUS="1")
        self.assertEqual(no_ticket.returncode, 1)
        self.assertIn("stapled notarization ticket", no_ticket.stderr)
        rejected = self.verify("--notarized", "--team-id", TEAM, self.staging, SHIM_SPCTL_STATUS="3")
        self.assertEqual(rejected.returncode, 1)
        self.assertIn("Gatekeeper rejects", rejected.stderr)

    def test_a_team_id_is_required_and_an_unsigned_tree_is_not_a_pass(self):
        self.macho("pohunek")
        self.assertEqual(self.verify(self.staging).returncode, 2)
        empty = self.root / "empty"
        empty.mkdir()
        result = self.verify("--team-id", TEAM, empty)
        self.assertEqual(result.returncode, 1)
        self.assertIn("nothing signed", result.stderr)


class PackageReleaseTest(Base):
    def test_a_release_without_credentials_fails_before_any_work(self):
        full = {
            **SIGNING_ENV,
            "MACOS_TEAM_ID": TEAM,
            **NOTARY_ENV,
        }
        staging = self.root / "pohunek-daemon-1.2.3-aarch64-apple-darwin"
        staging.mkdir()
        for missing in full:
            env = dict(full)
            env[missing] = ""
            result = self.run_tool("package", "--sign-release", "daemon", "1.2.3", staging, env=env)
            self.assertEqual(result.returncode, 1, missing)
            self.assertIn(missing, result.stderr)
            self.assertIn("never produced unsigned", result.stderr)
        self.assertEqual(self.calls(), [])

    def test_a_development_tree_or_a_misnamed_tree_is_never_signed_as_a_release(self):
        full = {**SIGNING_ENV, "MACOS_TEAM_ID": TEAM, **NOTARY_ENV}
        for name in (
            "pohunek-daemon-1.2.3-aarch64-apple-darwin-unsigned-development",
            "pohunek-cli-1.2.3-aarch64-apple-darwin",
        ):
            staging = self.root / name
            staging.mkdir()
            result = self.run_tool("package", "--sign-release", "daemon", "1.2.3", staging, env=full)
            self.assertEqual(result.returncode, 1, name)
            self.assertIn(
                "development staging" if name.endswith("development") else "unexpected staging directory name",
                result.stderr,
            )
        self.assertEqual(self.calls(), [])

    def test_the_signing_step_audits_signs_notarizes_verifies_and_archives_in_order(self):
        name = "pohunek-cli-1.2.3-aarch64-apple-darwin"
        staging = self.root / name
        program = staging / "pohunek"
        program.parent.mkdir()
        program.write_bytes(MACHO)
        program.chmod(0o755)
        (staging / "README.md").write_text("text\n")
        # The audit reads the tree through otool, lipo, and strings, which the
        # shims below answer for every file.
        audit_tools = self.root / "audit-tools"
        audit_tools.mkdir()
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
        env = {
            **SIGNING_ENV,
            "MACOS_TEAM_ID": TEAM,
            **NOTARY_ENV,
            "SOURCE_DATE_EPOCH": "1700000000",
            "PATH": "%s:%s:%s" % (audit_tools, self.tools, os.environ["PATH"]),
        }
        result = self.run_tool("package", "--sign-release", "cli", "1.2.3", staging, env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        archive = Path(result.stdout.strip())
        self.assertEqual(archive, self.root / (name + ".tar.gz"))
        self.assertTrue(archive.is_file())
        checksum = (self.root / (name + ".tar.gz.sha256")).read_text()
        self.assertTrue(checksum.strip().endswith(name + ".tar.gz"))
        manifest = (staging / "MANIFEST").read_text()
        self.assertIn("signing developer-id\n", manifest)
        self.assertIn("minimum-macos 14.0\n", manifest)
        self.assertIn("component cli\n", manifest)
        calls = self.calls()
        first_sign = next(i for i, c in enumerate(calls) if c.startswith("codesign") and "--sign" in c)
        notarize = next(i for i, c in enumerate(calls) if c.startswith("xcrun notarytool submit"))
        verify = next(i for i, c in enumerate(calls) if c.startswith("codesign") and "-dvv" in c)
        self.assertLess(first_sign, notarize)
        self.assertLess(notarize, verify)

    def test_a_staged_tree_with_a_symlink_is_refused_before_anything_is_signed(self):
        name = "pohunek-cli-1.2.3-aarch64-apple-darwin"
        staging = self.root / name
        staging.mkdir()
        (staging / "link").symlink_to("/etc/passwd")
        env = {**SIGNING_ENV, "MACOS_TEAM_ID": TEAM, **NOTARY_ENV}
        result = self.run_tool("package", "--sign-release", "cli", "1.2.3", staging, env=env)
        self.assertEqual(result.returncode, 1)
        self.assertIn("symbolic link or special file", result.stderr)
        self.assertEqual(self.calls(), [])

    def test_modes_and_components_are_validated(self):
        for args in (
            ("--sideload", "daemon", "1.2.3", self.root, self.root, self.root),
            ("--development", "relay", "1.2.3", self.root, self.root, self.root),
            ("--development", "daemon"),
            ("--release", "daemon", "1.2.3", self.root, self.root, self.root),
        ):
            result = self.run_tool("package", *args)
            self.assertEqual(result.returncode, 1, args)


class SigningKeychainTest(Base):
    def keychain(self, action, **env):
        runner_temp = self.root / "runner"
        runner_temp.mkdir(exist_ok=True)
        github_env = self.root / "github.env"
        base = {
            "RUNNER_TEMP": str(runner_temp),
            "GITHUB_ENV": str(github_env),
        }
        base.update(env)
        return self.run_tool("signing-keychain", action, env=base), github_env, runner_temp

    def test_missing_certificate_secrets_fail_without_touching_the_keychain(self):
        for missing in ("MACOS_CERTIFICATE_P12_BASE64", "MACOS_CERTIFICATE_PASSWORD"):
            env = {"MACOS_CERTIFICATE_P12_BASE64": "AAAA", "MACOS_CERTIFICATE_PASSWORD": "pw", missing: ""}
            result, _, _ = self.keychain("create", **env)
            self.assertNotEqual(result.returncode, 0, missing)
            self.assertIn(missing, result.stderr)
            self.assertIn("cannot be signed", result.stderr)
        self.assertEqual(self.calls("security"), [])

    def test_create_imports_the_certificate_and_exports_only_identity_and_path(self):
        secret = "UEsDBBQAAAAIAHNlY3JldC1wMTItbWF0ZXJpYWw="
        password = "p12-passphrase-value"
        result, github_env, runner_temp = self.keychain(
            "create", MACOS_CERTIFICATE_P12_BASE64=secret, MACOS_CERTIFICATE_PASSWORD=password
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        exported = github_env.read_text()
        self.assertIn("MACOS_SIGNING_IDENTITY=" + IDENTITY, exported)
        self.assertIn("MACOS_SIGNING_KEYCHAIN=" + str(runner_temp / "pohunek-signing/signing.keychain-db"), exported)
        for value in (secret, password):
            for text in (exported, result.stdout, result.stderr):
                self.assertNotIn(value, text)
        calls = "\n".join(self.calls("security"))
        for expected in ("create-keychain", "set-keychain-settings", "unlock-keychain", "import", "-T /usr/bin/codesign", "set-key-partition-list", "find-identity"):
            self.assertIn(expected, calls)
        # The keychain joins the front of the user search list, the original
        # entries follow.
        self.assertRegex(calls, r"list-keychains -d user -s \S+signing\.keychain-db /Users/runner/Library/Keychains/login\.keychain-db /Library/Keychains/System\.keychain")
        self.assertFalse((runner_temp / "pohunek-signing/certificate.p12").exists())
        self.assertRegex(result.stdout, r"::add-mask::[a-f0-9]{48}")

    def test_delete_restores_the_search_list_and_removes_the_state(self):
        env = {"MACOS_CERTIFICATE_P12_BASE64": "AAAA", "MACOS_CERTIFICATE_PASSWORD": "pw"}
        _, _, runner_temp = self.keychain("create", **env)
        keychain = runner_temp / "pohunek-signing/signing.keychain-db"
        keychain.write_text("keychain")
        self.log.write_text("")
        result, _, _ = self.keychain("delete")
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls("security")
        self.assertTrue(any(c.startswith("security delete-keychain") for c in calls))
        self.assertTrue(any("list-keychains -d user -s /Users/runner/Library/Keychains/login.keychain-db /Library/Keychains/System.keychain" in c for c in calls))
        self.assertFalse((runner_temp / "pohunek-signing").exists())

    def test_a_failing_import_leaves_no_keychain_certificate_or_state(self):
        result, _, runner_temp = self.keychain(
            "create",
            MACOS_CERTIFICATE_P12_BASE64="UEsDBBQ=",
            MACOS_CERTIFICATE_PASSWORD="pw",
            SHIM_SECURITY_IMPORT_STATUS="1",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((runner_temp / "pohunek-signing").exists())
        calls = self.calls("security")
        # The search list is restored to its original entries.
        self.assertEqual(
            calls[-1],
            "security list-keychains -d user -s /Users/runner/Library/Keychains/login.keychain-db /Library/Keychains/System.keychain",
        )

    def test_delete_without_a_keychain_is_a_no_op(self):
        result, _, _ = self.keychain("delete")
        self.assertEqual(result.returncode, 0)
        self.assertEqual(self.calls("security"), [])

    def test_a_leftover_state_directory_refuses_a_second_create(self):
        env = {"MACOS_CERTIFICATE_P12_BASE64": "AAAA", "MACOS_CERTIFICATE_PASSWORD": "pw"}
        self.keychain("create", **env)
        result, _, _ = self.keychain("create", **env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already exists", result.stderr)


class ReleaseWorkflowTest(unittest.TestCase):
    SECRETS = {
        "MACOS_CERTIFICATE_P12_BASE64",
        "MACOS_CERTIFICATE_PASSWORD",
        "APPLE_NOTARY_KEY_P8_BASE64",
        "APPLE_NOTARY_KEY_ID",
        "APPLE_NOTARY_ISSUER_ID",
    }

    def setUp(self):
        self.text = (ROOT / ".github/workflows/release.yml").read_text()
        self.stage = self.job("stage-macos", "sign-macos")
        self.sign = self.job("sign-macos", "verify-macos")
        self.release = self.job("verify-macos", "publish-macos")
        self.publish = self.job("publish-macos", None)

    def job(self, name, following):
        start = self.text.index("\n  %s:\n" % name)
        end = self.text.index("\n  %s:\n" % following) if following else len(self.text)
        return self.text[start:end]

    def test_secrets_exist_only_in_the_signing_job_and_only_in_its_steps(self):
        for name, job in (("stage", self.stage), ("release", self.release), ("publish", self.publish)):
            self.assertNotIn("secrets.", job, name)
            self.assertNotIn("macos-signing", job, name)
        header = self.sign.split("    steps:", 1)[0]
        self.assertNotIn("secrets.", header, "no secret may be exposed to the whole job")
        self.assertIn("environment: macos-signing", header)
        for secret in re.findall(r"secrets\.([A-Z0-9_]+)", self.sign):
            self.assertIn(secret, self.SECRETS)
        # Third-party actions never see a secret.
        for block in re.split(r"\n      - ", self.sign):
            if "uses:" in block:
                self.assertNotIn("secrets.", block)

    def test_the_signing_job_uses_only_pinned_actions_and_runs_nothing_from_the_tree(self):
        for use in re.findall(r"uses: (\S+)", self.sign):
            self.assertRegex(use, r"@[0-9a-f]{40}$", use)
        for forbidden in ("--stage-release", "cargo", "bun ", "--version", "smoke", "stage-archive", "setup-"):
            self.assertNotIn(forbidden, self.sign, forbidden)

    def test_every_action_that_shapes_the_signed_bytes_is_pinned(self):
        for name, job in (("stage", self.stage), ("sign", self.sign), ("release", self.release), ("publish", self.publish)):
            for use in re.findall(r"uses: (\S+)", job):
                self.assertRegex(use, r"@[0-9a-f]{40}$", "%s: %s" % (name, use))

    def test_the_team_id_is_a_repository_variable_every_job_can_read(self):
        # Environment-level variables are visible only to jobs that name the
        # environment; the verification job does not.
        self.assertIn("vars.MACOS_TEAM_ID", self.release)
        readme = (ROOT / "README.md").read_text()
        self.assertIn("repository variable `MACOS_TEAM_ID`", readme)

    def test_no_artifact_derived_value_is_interpolated_into_a_script(self):
        # Step outputs computed from the downloaded artifacts must never reach a
        # `${{ }}` expression in the signing or verification jobs.
        for name, job in (("sign", self.sign), ("verify", self.release)):
            self.assertNotIn("steps.stage.outputs", job, name)
        self.assertNotIn("$(ls", self.sign + self.release)
        self.assertIn("entries outside", self.sign)
        self.assertIn("parent-directory component", self.sign)

    def test_the_staged_tree_travels_as_a_checked_tar(self):
        self.assertIn("stage.tar.sha256", self.stage)
        self.assertIn("shasum -a 256 -c stage.tar.sha256", self.sign)
        self.assertIn("packaging/macos/package --stage-release", self.stage)
        self.assertIn("packaging/macos/package --sign-release", self.sign)
        self.assertNotIn("--sign-release", self.stage)
        self.assertNotIn("--stage-release", self.sign)
        self.assertNotIn("--development", self.text.split("\n  stage-macos:\n", 1)[1])

    def test_missing_credentials_fail_the_signing_job_before_any_work(self):
        order = [
            self.sign.index("Require signing and notarization credentials"),
            self.sign.index("Download the staged tree"),
            self.sign.index("Create ephemeral signing keychain"),
        ]
        self.assertEqual(order, sorted(order))
        self.assertIn("exit 1", self.sign.split("Download the staged tree", 1)[0])
        self.assertIn("grep -q '^signing developer-id$'", self.release)

    def test_the_keychain_is_always_removed_and_nothing_runs_between(self):
        remove = self.sign.split("- name: Remove ephemeral signing keychain", 1)[1].split("\n      - ", 1)[0]
        self.assertIn("if: always()", remove)
        create = self.sign.index("name: Create ephemeral signing keychain")
        removal = self.sign.index("name: Remove ephemeral signing keychain")
        self.assertEqual(self.sign[create:removal].count("- name:"), 1)

    def test_the_release_job_verifies_the_published_bytes_and_publishes_only_signed_archives(self):
        self.assertIn("verify-signed --notarized", self.release)
        self.assertIn("RUNNER_TEMP/extracted", self.release)
        self.assertIn("packaging/smoke-hermes-plugin-release", self.release)
        self.assertNotIn("action-gh-release", self.stage + self.sign + self.release)
        self.assertIn("needs: [sign-macos]", self.release)
        self.assertIn("needs: [stage-macos]", self.sign)

    def test_only_the_publishing_job_can_write_and_it_runs_nothing_from_the_archive(self):
        self.assertIn("contents: write", self.publish)
        for name, job in (("stage", self.stage), ("sign", self.sign), ("verify", self.release)):
            self.assertNotIn("contents: write", job, name)
        self.assertIn("contents: read", self.release)
        self.assertIn("needs: [verify-macos]", self.publish)
        self.assertIn("action-gh-release", self.publish)
        for forbidden in ("tar -x", "--version", "smoke", "cargo", "bun ", "verify-signed", "checkout"):
            self.assertNotIn(forbidden, self.publish, forbidden)
        self.assertIn("shasum -a 256 -c", self.publish)


if __name__ == "__main__":
    unittest.main()
