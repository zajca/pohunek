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
import select
import selectors
import shutil
import signal
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


def process_tree(root_pid):
    pending = [root_pid]
    found = []
    while pending:
        pid = pending.pop()
        found.append(pid)
        children = Path("/proc") / str(pid) / "task" / str(pid) / "children"
        try:
            pending.extend(int(child) for child in children.read_text().split())
        except FileNotFoundError:
            pass
    return found


def process_snapshot(pids):
    snapshot = []
    for pid in pids:
        proc = Path("/proc") / str(pid)
        try:
            status = (proc / "status").read_text().splitlines()
            details = {key: value.strip() for key, value in (line.split(":", 1) for line in status if ":" in line)}
            fds = []
            for fd in (proc / "fd").iterdir():
                if fd.name in ("0", "1", "2", "3"):
                    try:
                        fds.append(f"{fd.name}={os.readlink(fd)}")
                    except OSError:
                        pass
            snapshot.append(
                f"{pid}: {details.get('Name')} {details.get('State')} ppid={details.get('PPid')} "
                f"pgid={os.getpgid(pid)} fds={','.join(fds)}"
            )
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            snapshot.append(f"{pid}: exited or inaccessible")
    return "; ".join(snapshot)


class _SmokeArchiveFixture:
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

    def smoke(self, *extra, archive=None, stage=None, node_dir=None, timeout=None):
        env = dict(os.environ)
        env["TMPDIR"] = str(self.tmpdir)
        if node_dir is not None:
            env["PATH"] = f"{node_dir}:{env['PATH']}"
        command = [
            str(SCRIPT),
            "--archive",
            str(archive or self.archive),
            "--stage",
            str(stage or self.stage),
            "--consumer",
            str(self.consumer),
            *extra,
        ]
        if timeout is not None:
            command = ["timeout", "--signal=TERM", "--kill-after=2s", f"{timeout}s", *command]
        return subprocess.run(
            command,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

    def assert_cleaned_up(self):
        self.assertEqual(list(self.tmpdir.iterdir()), [], "the temp directory was left behind")
        self.assertEqual(processes_with_marker(), [], "a process was left behind")


    def assert_termination_kills_namespace(self, isolation):
        fifo_probe = (
            'if (exec 8>"${TMPDIR%/t}/sudo-cancel") 2>/dev/null; '
            'then echo FIFO-WRITABLE; else echo FIFO-REFUSED; fi\n'
            if isolation == "sudo" else ""
        )
        self.write_consumer(
            STUB_PREAMBLE
            + f"sleep {MARKER_SLEEP} &\nchild=$!\n"
            + 'while [ "$(cat "/proc/$child/comm")" != sleep ]; do :; done\n'
            + fifo_probe
            + "echo SMOKE-CONSUMER-READY\nwait\n"
        )
        env = dict(os.environ)
        env["TMPDIR"] = str(self.tmpdir)
        command = [
            str(SCRIPT), "--archive", str(self.archive), "--stage", str(self.stage),
            "--consumer", str(self.consumer), f"--isolation={isolation}", "--runtime", "pi",
        ]
        process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        launcher_group = None
        child_fds = []
        verified_cleanup = False
        try:
            output = b""
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                while b"SMOKE-CONSUMER-READY\n" not in output:
                    self.assertTrue(selector.select(timeout=10), "consumer did not become ready")
                    chunk = os.read(process.stdout.fileno(), 4096)
                    self.assertTrue(chunk, "consumer exited before becoming ready")
                    output += chunk
            if isolation == "sudo":
                self.assertIn(b"FIFO-REFUSED\n", output, "consumer can hold the cancellation FIFO open")
            markers = processes_with_marker()
            self.assertEqual(len(markers), 1, "consumer workload did not start")
            children = (Path("/proc") / str(process.pid) / "task" / str(process.pid) / "children").read_text().split()
            self.assertEqual(len(children), 1, "expected one namespace launcher")
            launcher_pid = int(children[0])
            descendants = process_tree(launcher_pid)
            self.assertIn(int(markers[0]), descendants, "background workload left the namespace tree")
            for pid in descendants:
                fd = os.pidfd_open(pid)
                self.addCleanup(os.close, fd)
                child_fds.append((pid, fd))
            self.assertEqual(os.getpgid(launcher_pid), launcher_pid, "launcher does not own a private group")
            self.assertNotEqual(launcher_pid, os.getpgrp(), "launcher shares the caller's group")
            launcher_group = launcher_pid
            if isolation == "userns":
                for pid in descendants:
                    self.assertEqual(os.getpgid(pid), launcher_pid, "namespace process left the launcher group")
            else:
                unshare = [pid for pid in descendants if (Path("/proc") / str(pid) / "comm").read_text().strip() == "unshare"]
                self.assertEqual(len(unshare), 1, "expected one root namespace launcher")
                root_group = os.getpgid(unshare[0])
                self.assertIn(root_group, descendants, "root supervisor is not the group leader")
                self.assertEqual(os.getpgid(int(markers[0])), root_group, "sudo workload left the root supervisor group")
            with selectors.DefaultSelector() as exited:
                for _, fd in child_fds:
                    exited.register(fd, selectors.EVENT_READ)
                os.kill(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired as error:
                    exited_pids = [pid for pid, fd in child_fds if select.select([fd], [], [], 0)[0]]
                    raise AssertionError(
                        f"outer cancellation timed out: outer={process.poll()}, exited={exited_pids}, "
                        f"processes={process_snapshot([process.pid, *descendants])}"
                    ) from error
                try:
                    _, stderr = process.communicate(timeout=10)
                except subprocess.TimeoutExpired as error:
                    exited_pids = [pid for pid, fd in child_fds if select.select([fd], [], [], 0)[0]]
                    raise AssertionError(
                        f"cancellation timed out: outer={process.poll()}, exited={exited_pids}, "
                        f"processes={process_snapshot([process.pid, *descendants])}, "
                        f"stderr={error.stderr!r}"
                    ) from error
                self.assertEqual(process.returncode, 143, stderr.decode(errors="replace"))
                while exited.get_map():
                    ready = exited.select(timeout=10)
                    self.assertTrue(ready, "namespace workload survived cancellation")
                    for key, _ in ready:
                        exited.unregister(key.fileobj)
            self.assert_cleaned_up()
            verified_cleanup = True
        finally:
            if process.poll() is None:
                process.kill()
            if not verified_cleanup and launcher_group is not None:
                for pid, fd in child_fds:
                    with selectors.DefaultSelector() as alive:
                        alive.register(fd, selectors.EVENT_READ)
                        if alive.select(timeout=0):
                            continue
                    try:
                        group = os.getpgid(pid)
                    except ProcessLookupError:
                        continue
                    if group == launcher_group:
                        try:
                            os.killpg(launcher_group, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        break
            for _, fd in child_fds:
                try:
                    signal.pidfd_send_signal(fd, signal.SIGKILL)
                except (ProcessLookupError, PermissionError):
                    pass
            for pid in processes_with_marker():
                os.kill(int(pid), signal.SIGKILL)
            try:
                process.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                process.stdout.close()
                process.stderr.close()
                process.wait(timeout=10)

@unittest.skipUnless(
    sys.platform.startswith("linux") and shutil.which("unshare") and shutil.which("jq") and userns_available(),
    "needs Linux with unprivileged user namespaces, unshare and jq",
)
class SmokeArchiveTest(_SmokeArchiveFixture, unittest.TestCase):
    # -- scenarios ----------------------------------------------------------

    def test_success_runs_every_shipped_runtime_in_a_hermetic_environment(self):
        node_dir = self.base / "node-bin"
        node_dir.mkdir()
        (node_dir / "node").write_text("#!/bin/sh\nexit 0\n")
        (node_dir / "node").chmod(0o755)
        self.write_consumer(
            STUB_PREAMBLE
            + """
for tool in sh python3 ps sed; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done
echo "ENV-PATH=$PATH"; echo "ENV-HOME=$HOME"; echo "ENV-PWD=$(pwd)"
echo "ENV-POHUNEK=$(set | grep -c '^POHUNEK_' || true)"
echo "ENV-CATALOG=$POHUNEK_CONSUMER_CATALOG"; echo "ENV-ARGS=$*"
"""
            + STUB_REPORT
        )
        result = self.smoke(node_dir=node_dir)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("pi ok", result.stdout)
        self.assertIn("codex ok", result.stdout)
        self.assertIn(f"ENV-PATH={self.stage}/pi/bin:{node_dir}:/usr/bin:/bin", result.stdout)
        self.assertIn(f"ENV-PATH={self.stage}/codex/bin:{node_dir}:/usr/bin:/bin", result.stdout)
        self.assertIn("/runtime/runtime-catalog.json", result.stdout)
        self.assertIn(f"--exact the_release_binaries_serve_the_runtime_out_of_process", result.stdout)
        # Exactly the six consumer-contract variables reach the consumer.
        self.assertIn("ENV-POHUNEK=6", result.stdout)
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

    @unittest.skipUnless(shutil.which("timeout"), "needs the timeout command as a hang guard")
    def test_a_sha256_listed_fifo_is_refused_before_hashing(self):
        payload = self.stage / "pi" / "bin" / "pi"
        payload.unlink()
        os.mkfifo(payload)
        result = self.smoke("--runtime", "pi", timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn(result.returncode, (124, 137), "the FIFO reached the hang guard")
        self.assertIn("file types differ from STAGE.sha256", result.stderr)
        self.assert_cleaned_up()

    def test_an_unlisted_file_is_refused(self):
        (self.stage / "pi" / "extra").write_text("not listed\n")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("manifests do not list", result.stderr)

    def test_an_empty_directory_with_a_backslash_name_is_refused(self):
        (self.stage / "pi" / "bad\\name").mkdir()
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsafe directory name", result.stderr)

    def test_an_empty_directory_with_a_newline_name_is_refused(self):
        (self.stage / "pi" / "bad\nname").mkdir()
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsafe directory name", result.stderr)

    def test_an_empty_directory_with_a_unicode_control_name_is_refused(self):
        (self.stage / "pi" / "bad\u0085name").mkdir()
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unicode control in a directory name", result.stderr)

    def test_a_link_leaving_the_stage_is_refused(self):
        outside = self.base / "outside"
        outside.write_text("x\n")
        (self.stage / "pi" / "escape").symlink_to(outside)
        with (self.stage / "pi" / "STAGE.links").open("a") as manifest:
            manifest.write(f"link\tescape\t{outside}\n")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid STAGE.links link path or target", result.stderr)

    def test_a_retargeted_link_is_refused(self):
        stage = self.stage / "pi"
        (stage / "bin" / "alias").symlink_to("pi")
        with (stage / "STAGE.links").open("a") as manifest:
            manifest.write("link\tbin/alias\tpi\n")
        result = self.smoke("--runtime", "pi")
        self.assertEqual(result.returncode, 0, result.stderr)
        (stage / "bin" / "alias").unlink()
        (stage / "bin" / "alias").symlink_to("../bin/pi")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("link differs from STAGE.links", result.stderr)

    def test_a_link_target_with_an_appended_newline_differs_from_manifest(self):
        stage = self.stage / "pi"
        (stage / "bin" / "alias").symlink_to("pi\n")
        with (stage / "STAGE.links").open("a") as manifest:
            manifest.write("link\tbin/alias\tpi\n")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("link differs from STAGE.links", result.stderr)

    def test_an_absolute_link_target_inside_stage_is_refused(self):
        stage = self.stage / "pi"
        target = stage / "bin" / "pi"
        (stage / "bin" / "alias").symlink_to(target)
        with (stage / "STAGE.links").open("a") as manifest:
            manifest.write(f"link\tbin/alias\t{target}\n")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid STAGE.links link path or target", result.stderr)

    def test_a_link_target_that_exits_and_reenters_stage_is_refused(self):
        stage = self.stage / "pi"
        (stage / "alias").symlink_to("../pi/bin/pi")
        with (stage / "STAGE.links").open("a") as manifest:
            manifest.write("link\talias\t../pi/bin/pi\n")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid STAGE.links link path or target", result.stderr)

    def test_a_link_target_can_use_parent_inside_stage(self):
        stage = self.stage / "pi"
        (stage / "bin" / "alias").symlink_to("../bin/pi")
        with (stage / "STAGE.links").open("a") as manifest:
            manifest.write("link\tbin/alias\t../bin/pi\n")
        result = self.smoke("--runtime", "pi")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_a_link_chain_inside_stage_is_accepted(self):
        stage = self.stage / "pi"
        (stage / "alias").symlink_to(".")
        (stage / "second").symlink_to("alias/bin/pi")
        with (stage / "STAGE.links").open("a") as manifest:
            manifest.write("link\talias\t.\nlink\tsecond\talias/bin/pi\n")
        result = self.smoke("--runtime", "pi")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_a_link_chain_that_exits_and_reenters_stage_is_refused(self):
        stage = self.stage / "pi"
        external = self.stage / "external"
        external.mkdir()
        (external / "return").symlink_to(stage / "bin" / "pi")
        (stage / "bin" / "alias").symlink_to(".")
        (stage / "bin" / "escape").symlink_to("alias/../../external/return")
        with (stage / "STAGE.links").open("a") as manifest:
            manifest.write("link\tbin/alias\t.\nlink\tbin/escape\talias/../../external/return\n")
        self.assertEqual((stage / "bin" / "escape").resolve(strict=True), stage / "bin" / "pi")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("link leaves the staged upstream", result.stderr)

    @unittest.skipUnless(shutil.which("timeout"), "needs the timeout command as a hang guard")
    def test_a_symlink_cycle_is_refused_without_hanging(self):
        stage = self.stage / "pi"
        (stage / "bin" / "a").symlink_to("b")
        (stage / "bin" / "b").symlink_to("a")
        with (stage / "STAGE.links").open("a") as manifest:
            manifest.write("link\tbin/a\tb\nlink\tbin/b\ta\n")
        result = self.smoke("--runtime", "pi", timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn(result.returncode, (124, 137), "the symlink cycle reached the hang guard")
        self.assertIn("unresolvable link in staged upstream", result.stderr)
        self.assert_cleaned_up()

    def test_a_link_with_missing_intermediate_target_is_refused(self):
        stage = self.stage / "pi"
        (stage / "bin" / "alias").symlink_to("missing/target")
        with (stage / "STAGE.links").open("a") as manifest:
            manifest.write("link\tbin/alias\tmissing/target\n")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unresolvable link in staged upstream", result.stderr)

    def test_a_link_target_with_unicode_control_is_refused(self):
        stage = self.stage / "pi"
        target = "pi\u0085"
        (stage / "bin" / "alias").symlink_to(target)
        with (stage / "STAGE.links").open("a") as manifest:
            manifest.write(f"link\tbin/alias\t{target}\n")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unicode control character in STAGE.links", result.stderr)

    def test_a_changed_executable_bit_is_refused(self):
        (self.stage / "pi" / "bin" / "pi").chmod(0o644)
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("executable bits differ from STAGE.links", result.stderr)

    def test_an_exec_record_for_a_file_missing_from_sha256_is_refused(self):
        with (self.stage / "pi" / "STAGE.links").open("a") as manifest:
            manifest.write("exec\tSTAGE.sha256\n")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid STAGE.links exec path", result.stderr)

    def test_duplicate_exec_record_is_refused(self):
        with (self.stage / "pi" / "STAGE.links").open("a") as manifest:
            manifest.write("exec\tbin/pi\n")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("duplicate STAGE.links exec path", result.stderr)

    def test_duplicate_link_record_is_refused(self):
        stage = self.stage / "pi"
        (stage / "bin" / "alias").symlink_to("pi")
        with (stage / "STAGE.links").open("a") as manifest:
            manifest.write("link\tbin/alias\tpi\nlink\tbin/alias\tpi\n")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("duplicate STAGE.links link path", result.stderr)

    def test_unterminated_links_record_is_refused(self):
        (self.stage / "pi" / "STAGE.links").write_text("exec\tbin/pi")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unterminated STAGE.links entry", result.stderr)

    def test_links_record_with_an_extra_field_is_refused(self):
        (self.stage / "pi" / "STAGE.links").write_text("exec\tbin/pi\textra\n")
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid STAGE.links entry", result.stderr)

    def test_duplicate_sha256_path_is_refused(self):
        manifest = self.stage / "pi" / "STAGE.sha256"
        with manifest.open("a") as output:
            output.write(manifest.read_text())
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("duplicate STAGE.sha256 path", result.stderr)

    def test_unterminated_sha256_record_is_refused(self):
        manifest = self.stage / "pi" / "STAGE.sha256"
        manifest.write_text(manifest.read_text().rstrip("\n"))
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unterminated STAGE.sha256 entry", result.stderr)

    def test_unsafe_sha256_path_is_refused_before_hashing(self):
        manifest = self.stage / "pi" / "STAGE.sha256"
        manifest.write_text(manifest.read_text().replace("bin/pi", "../pi"))
        result = self.smoke("--runtime", "pi")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsafe STAGE.sha256 path", result.stderr)

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

    def test_termination_kills_the_userns_namespace_and_its_workload(self):
        self.assert_termination_kills_namespace("userns")

    def test_the_userns_strategy_can_be_forced(self):
        result = self.smoke("--isolation=userns", "--runtime", "pi")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("isolation userns", result.stdout)

    def test_userns_strategy_does_not_run_shadow_tools_from_caller_path(self):
        shadow = self.base / "shadow"
        shadow.mkdir()
        marker = self.base / "shadow-invoked"
        for tool in ("unshare", "ip", "setpriv", "sudo"):
            script = shadow / tool
            script.write_text(f"#!/bin/sh\nprintf '%s\\n' {tool} >> '{marker}'\nexit 77\n")
            script.chmod(0o755)
        (shadow / "node").symlink_to(Path(shutil.which("node")).resolve())

        result = self.smoke("--isolation=userns", "--runtime", "pi", node_dir=shadow)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(marker.exists(), result.stdout + result.stderr)
        self.assert_cleaned_up()

    @unittest.skipUnless(
        shutil.which("sudo") and shutil.which("setpriv") and sudo_available(),
        "skipped: `sudo -n unshare` is unavailable here, so the sudo strategy is not exercised locally",
    )
    def test_the_sudo_strategy_isolates_and_drops_to_the_invoking_user(self):
        self.write_consumer(STUB_PREAMBLE + 'echo "UID=$(id -u)"\n' + STUB_REPORT)
        result = self.smoke("--isolation=sudo", "--runtime", "pi")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"UID={os.getuid()}", result.stdout)


@unittest.skipUnless(
    sys.platform.startswith("linux") and shutil.which("unshare") and shutil.which("jq") and shutil.which("node"),
    "needs Linux with unshare, jq and node",
)
class SmokeArchivePreflightTest(_SmokeArchiveFixture, unittest.TestCase):
    @unittest.skipUnless(
        shutil.which("sudo") and shutil.which("setpriv") and sudo_available(),
        "skipped: `sudo -n unshare` is unavailable here, so sudo cancellation is not exercised locally",
    )
    def test_termination_kills_the_sudo_namespace_and_its_workload(self):
        self.assert_termination_kills_namespace("sudo")

    @unittest.skipUnless(
        shutil.which("sudo") and shutil.which("timeout") and sudo_available(),
        "needs passwordless sudo and GNU timeout",
    )
    def test_sudo_supervisor_opens_fifo_after_writer_has_closed(self):
        fifo = self.base / "early-cancel"
        os.mkfifo(fifo)
        # With no writer from the start, the old blocking `exec 3<` never
        # reached its watchdog. Keep timeout inside sudo to reap that root
        # process if the regression returns.
        command = [
            "/usr/bin/sudo", "-n", "/usr/bin/timeout", "--foreground",
            "--signal=TERM", "--kill-after=2s", "5s", "/usr/bin/setsid",
            "/bin/sh", str(SCRIPT), "__sudo_supervisor", str(os.getuid()),
            str(os.getgid()), str(fifo), str(self.tmpdir), "/nonexistent",
            "/nonexistent", str(self.consumer), "/usr/bin", "pi",
        ]
        result = subprocess.run(command, capture_output=True, text=True, timeout=10)
        self.assertNotEqual(result.returncode, 124, "root FIFO reader blocked without a writer")
        self.assertNotIn("sudo:", result.stderr, result.stderr)
        self.assertNotIn("sudo supervisor has no private process group", result.stderr)
        self.assert_cleaned_up()

    def test_sudo_strategy_does_not_run_shadow_tools_from_caller_path(self):
        shadow = self.base / "shadow"
        shadow.mkdir()
        marker = self.base / "shadow-invoked"
        for tool in ("sudo", "unshare", "ip", "setpriv"):
            script = shadow / tool
            script.write_text(f"#!/bin/sh\nprintf '%s\\n' {tool} >> '{marker}'\nexit 77\n")
            script.chmod(0o755)
        (shadow / "node").symlink_to(Path(shutil.which("node")).resolve())

        result = self.smoke("--isolation=sudo", "--runtime", "pi", node_dir=shadow)
        self.assertFalse(marker.exists(), result.stdout + result.stderr)
        if result.returncode != 0:
            self.assertIn("isolation sudo is not available", result.stderr)
        self.assert_cleaned_up()

    def test_node_symlink_into_checkout_is_refused_before_isolation(self):
        node_dir = self.base / "node-bin"
        node_dir.mkdir()
        (node_dir / "node").symlink_to(SCRIPT)
        result = self.smoke("--runtime", "pi", node_dir=node_dir)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("node on PATH resolves inside the checkout", result.stderr)
        self.assert_cleaned_up()


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
