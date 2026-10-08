"""Scenarios for packaging/smoke-archive (stdlib only).

Each scenario builds a tiny synthetic daemon archive with the real
packaging/write-manifest and packaging/archive, a stage directory with a real
STAGE.sha256, and a stub consumer executable that plays the release consumer
(it reads the POHUNEK_CONSUMER_* contract and writes the report). The script
itself, its extraction, its namespace entry and its report checks are real.
Run: python3 -m unittest scripts.tests.test_smoke_archive
"""

import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
PACKAGING = ROOT / "packaging"
SCRIPT = PACKAGING / "smoke-archive"
TARGET = "x86_64-unknown-linux-gnu"
VERSION = "1.2.3"
NAME = f"pohunek-daemon-{VERSION}-{TARGET}"
EPOCH = "1700000000"
BINARIES = ("pohunek", "pohunekd", "pohunek-sessiond")
# A marker sleep duration no other process uses, so a leftover is recognizable.
MARKER_SLEEP = "2987"

STUB_PREAMBLE = """#!/bin/sh
set -eu
pkg="sha256:$(sha256sum "$POHUNEK_CONSUMER_PACKAGE" | cut -d' ' -f1)"
h() { sha256sum "$POHUNEK_CONSUMER_BIN_DIR/$1" | cut -d' ' -f1; }
"""

STUB_REPORT = """
printf '{"schema":1,"runtime":"%s","package_digest":"%s","upstream_version":"9.9.9","executables":{"pohunek":"%s","pohunekd":"%s","pohunek-sessiond":"%s"}}\\n' \\
  "$POHUNEK_CONSUMER_RUNTIME" "$pkg" "$(h pohunek)" "$(h pohunekd)" "$(h pohunek-sessiond)" > "$POHUNEK_CONSUMER_REPORT"
"""


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def userns_available():
    probe = subprocess.run(
        ["unshare", "--user", "--map-root-user", "--net", "--pid", "--fork", "--kill-child", "--mount-proc", "true"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return probe.returncode == 0


def sudo_available():
    probe = subprocess.run(
        ["sudo", "-n", "unshare", "--net", "true"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return probe.returncode == 0


def processes_with_marker():
    found = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            cmdline = (entry / "cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        if cmdline[:2] == [b"sleep", MARKER_SLEEP.encode()]:
            found.append(entry.name)
    return found


@unittest.skipUnless(
    sys.platform.startswith("linux") and shutil.which("unshare") and shutil.which("jq") and userns_available(),
    "needs Linux with unprivileged user namespaces, unshare and jq",
)
class SmokeArchiveTest(unittest.TestCase):
    def setUp(self):
        self.base = Path(tempfile.mkdtemp(prefix="pohunek-smoke-test-"))
        self.addCleanup(shutil.rmtree, self.base, ignore_errors=True)
        self.tmpdir = self.base / "tmp"
        self.tmpdir.mkdir()
        self.stage = self.base / "stage"
        self.stage.mkdir()
        self.consumer = self.base / "consumer"
        self.write_consumer(STUB_PREAMBLE + STUB_REPORT)
        self.archive = self.build_archive(["pi", "codex"])
        for runtime in ("pi", "codex"):
            self.make_stage(runtime)

    # -- fixtures -----------------------------------------------------------

    def build_archive(self, runtimes, catalog_digest_override=None):
        tree = self.base / "tree" / NAME
        shutil.rmtree(self.base / "tree", ignore_errors=True)
        tree.mkdir(parents=True)
        for binary in BINARIES:
            (tree / binary).write_text(f"#!/bin/sh\n# {binary}\n")
            (tree / binary).chmod(0o755)
        (tree / "runtime-catalog-anchor.json").write_text('{"anchor":"fixture"}\n')
        (tree / "packaging").mkdir()
        shutil.copy(PACKAGING / "verify-archive", tree / "packaging" / "verify-archive")
        (tree / "runtime" / "packages").mkdir(parents=True)
        entries = []
        for runtime in runtimes:
            package = f"package bytes of {runtime}\n".encode()
            (tree / "runtime" / "packages" / f"{runtime}.tar.zst").write_bytes(package)
            digest = catalog_digest_override or f"sha256:{sha256_hex(package)}"
            entries.append({"runtime_id": runtime, "package_id": f"pohunek.runtime.{runtime}", "digest": digest})
        (tree / "runtime" / "runtime-catalog.json").write_text(json.dumps({"catalog": {"entries": entries}}))
        run_checked([PACKAGING / "write-manifest", tree, "daemon", VERSION, TARGET, "none"])
        out = self.base / "dist"
        shutil.rmtree(out, ignore_errors=True)
        run_checked([PACKAGING / "archive", self.base / "tree", NAME, out], env={"SOURCE_DATE_EPOCH": EPOCH})
        return out / f"{NAME}.tar.gz"

    def make_stage(self, runtime, content=None):
        directory = self.stage / runtime
        shutil.rmtree(directory, ignore_errors=True)
        (directory / "bin").mkdir(parents=True)
        payload = content or f"#!/bin/sh\necho {runtime}\n".encode()
        (directory / "bin" / runtime).write_bytes(payload)
        (directory / "bin" / runtime).chmod(0o755)
        (directory / "STAGE.sha256").write_text(f"{sha256_hex(payload)}  bin/{runtime}\n")
        # `compat stage-upstream` always writes the link and executable-bit manifest.
        (directory / "STAGE.links").write_text(f"exec\tbin/{runtime}\n")

    def write_consumer(self, text):
        self.consumer.write_text(text)
        self.consumer.chmod(0o755)

    def smoke(self, *extra, archive=None, stage=None):
        env = dict(os.environ)
        env["TMPDIR"] = str(self.tmpdir)
        return subprocess.run(
            [
                str(SCRIPT),
                "--archive",
                str(archive or self.archive),
                "--stage",
                str(stage or self.stage),
                "--consumer",
                str(self.consumer),
                *extra,
            ],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

    def assert_cleaned_up(self):
        self.assertEqual(list(self.tmpdir.iterdir()), [], "the temp directory was left behind")
        self.assertEqual(processes_with_marker(), [], "a process was left behind")

    # -- scenarios ----------------------------------------------------------

    def test_success_runs_every_shipped_runtime_in_a_hermetic_environment(self):
        self.write_consumer(
            STUB_PREAMBLE
            + """
echo "ENV-PATH=$PATH"; echo "ENV-HOME=$HOME"; echo "ENV-PWD=$(pwd)"
echo "ENV-POHUNEK=$(set | grep -c '^POHUNEK_' || true)"
echo "ENV-CATALOG=$POHUNEK_CONSUMER_CATALOG"; echo "ENV-ARGS=$*"
"""
            + STUB_REPORT
        )
        result = self.smoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("pi ok", result.stdout)
        self.assertIn("codex ok", result.stdout)
        self.assertIn(f"ENV-PATH={self.stage}/pi/bin:", result.stdout)
        self.assertIn(f"ENV-PATH={self.stage}/codex/bin:", result.stdout)
        self.assertIn("/runtime/runtime-catalog.json", result.stdout)
        self.assertIn(f"--exact the_release_binaries_serve_the_runtime_out_of_process", result.stdout)
        # Exactly the five consumer-contract variables reach the consumer.
        self.assertIn("ENV-POHUNEK=5", result.stdout)
        for line in result.stdout.splitlines():
            if line.startswith(("ENV-HOME=", "ENV-PWD=")):
                self.assertNotIn(str(ROOT), line)
                self.assertIn("/run/", line)
        self.assert_cleaned_up()

    def test_runtime_flag_restricts_the_shipped_set_and_unknown_runtime_fails(self):
        result = self.smoke("--runtime", "pi")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("pi ok", result.stdout)
        self.assertNotIn("codex ok", result.stdout)
        result = self.smoke("--runtime", "claude")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not shipped", result.stderr)

    def test_a_shipped_package_without_a_staged_upstream_is_refused(self):
        shutil.rmtree(self.stage / "codex")
        result = self.smoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no staged upstream for the shipped package codex", result.stderr)
        self.assert_cleaned_up()

    def test_a_modified_staged_file_is_refused_inside_the_namespace(self):
        (self.stage / "pi" / "bin" / "pi").write_text("#!/bin/sh\necho tampered\n")
        result = self.smoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match STAGE.sha256", result.stderr)
        self.assert_cleaned_up()

    def test_an_unlisted_file_and_a_link_leaving_the_stage_are_refused(self):
        (self.stage / "pi" / "extra").write_text("not listed\n")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not list", result.stderr)
        (self.stage / "pi" / "extra").unlink()
        outside = self.base / "outside"
        outside.write_text("x\n")
        (self.stage / "pi" / "escape").symlink_to(outside)
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("link leaves the staged upstream", result.stderr)

    def test_path_traversal_and_link_members_are_refused_before_extraction(self):
        for member_name, kind in (("../evil", "file"), ("/abs/evil", "file"), (f"{NAME}/link", "symlink")):
            hostile = self.base / "hostile.tar.gz"
            with tarfile.open(hostile, "w:gz") as tar:
                info = tarfile.TarInfo(member_name)
                if kind == "symlink":
                    info.type = tarfile.SYMTYPE
                    info.linkname = "/etc/passwd"
                    tar.addfile(info)
                else:
                    payload = b"x"
                    info.size = len(payload)
                    tar.addfile(info, io.BytesIO(payload))
            result = self.smoke(archive=hostile)
            self.assertNotEqual(result.returncode, 0, member_name)
            self.assertRegex(result.stderr, "unsafe member path|link or special member")
            self.assertFalse((self.base / "evil").exists())
        self.assert_cleaned_up()

    def test_a_catalog_digest_that_does_not_match_the_package_is_refused(self):
        self.archive = self.build_archive(["pi"], catalog_digest_override="sha256:" + "0" * 64)
        result = self.smoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match its catalog digest", result.stderr)

    def test_a_report_with_a_foreign_executable_or_package_digest_is_refused(self):
        self.write_consumer(STUB_PREAMBLE + STUB_REPORT.replace('"$(h pohunekd)"', '"' + "1" * 64 + '"'))
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match the archive's catalog digest and binaries", result.stderr)
        self.write_consumer(STUB_PREAMBLE + STUB_REPORT.replace('"$pkg"', '"sha256:' + "2" * 64 + '"'))
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match the archive's catalog digest and binaries", result.stderr)
        self.assert_cleaned_up()

    def test_the_namespace_has_loopback_but_no_route_out(self):
        probe = f"""
{sys.executable} - <<'PY'
import errno, socket, sys
server = socket.socket(); server.bind(("127.0.0.1", 0)); server.listen(1)
client = socket.create_connection(server.getsockname(), timeout=3)
client.close(); server.close()
for host in ("192.0.2.1", "8.8.8.8"):
    try:
        socket.create_connection((host, 53), timeout=3).close()
    except OSError as error:
        if error.errno != errno.ENETUNREACH:
            print("OUTBOUND-WRONG-ERROR", host, error); sys.exit(1)
        print("OUTBOUND-REFUSED", host, error.errno)
    else:
        print("OUTBOUND-REACHED", host); sys.exit(1)
PY
"""
        self.write_consumer(STUB_PREAMBLE + probe + STUB_REPORT)
        result = self.smoke("--runtime", "pi")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("OUTBOUND-REFUSED 192.0.2.1 101", result.stdout)
        self.assertIn("OUTBOUND-REFUSED 8.8.8.8", result.stdout)
        self.assertNotIn("OUTBOUND-REACHED", result.stdout)

    def test_a_failing_consumer_leaves_no_process_and_no_temp_directory(self):
        self.write_consumer(STUB_PREAMBLE + f"sleep {MARKER_SLEEP} &\nexit 3\n")
        result = self.smoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("consumer run failed for runtime", result.stderr)
        self.assert_cleaned_up()

    def test_a_process_outliving_a_successful_consumer_fails_the_run(self):
        self.write_consumer(STUB_PREAMBLE + f"sleep {MARKER_SLEEP} &\n" + STUB_REPORT)
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("outlived the consumer run", result.stderr)
        self.assert_cleaned_up()

    def test_the_userns_strategy_can_be_forced(self):
        result = self.smoke("--isolation=userns", "--runtime", "pi")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("isolation userns", result.stdout)

    @unittest.skipUnless(
        shutil.which("sudo") and shutil.which("setpriv") and sudo_available(),
        "skipped: `sudo -n unshare` is unavailable here, so the sudo strategy is not exercised locally",
    )
    def test_the_sudo_strategy_isolates_and_drops_to_the_invoking_user(self):
        self.write_consumer(STUB_PREAMBLE + 'echo "UID=$(id -u)"\n' + STUB_REPORT)
        result = self.smoke("--isolation=sudo", "--runtime", "pi")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"UID={os.getuid()}", result.stdout)


def run_checked(args, env=None):
    merged = dict(os.environ)
    merged.update(env or {})
    result = subprocess.run(
        [str(a) for a in args], env=merged, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
    )
    if result.returncode != 0:
        raise AssertionError(f"{args} failed: {result.stdout}{result.stderr}")
    return result


if __name__ == "__main__":
    unittest.main()
