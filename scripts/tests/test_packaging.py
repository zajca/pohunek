"""Regression checks for the release packaging scripts (stdlib only).

The scripts are POSIX sh and run on Linux and macOS, so everything here runs on
any host: staging, the archive manifest, and the deterministic archive.
"""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
PACKAGING = ROOT / "packaging"
EPOCH = "1700000000"
TARGET = "x86_64-unknown-linux-gnu"
VERSION = "1.2.3"

FAKE_POHUNEK = """#!/bin/sh
if [ "$1" = completions ]; then
  printf 'completion for %s\\n' "$2"
fi
"""


def run(args, cwd=None, env=None, check=True):
    merged = dict(os.environ)
    if env:
        merged.update(env)
    result = subprocess.run(
        [str(a) for a in args],
        cwd=cwd,
        env=merged,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if check and result.returncode != 0:
        raise AssertionError(f"{args} failed: {result.stdout}{result.stderr}")
    return result


class Workspace:
    """A repository-root-like directory with built binaries and docs."""

    def __init__(self, test):
        self.root = Path(tempfile.mkdtemp(prefix="pohunek-packaging-"))
        test.addCleanup(shutil.rmtree, self.root, ignore_errors=True)
        self.bindir = self.root / "bin"
        self.docs = self.root / "docs-site"
        self.out = self.root / "dist"
        for directory in (self.bindir, self.docs / "offline", self.out):
            directory.mkdir(parents=True)
        (self.docs / "offline" / "index.html").write_text("<html></html>\n")
        (self.docs / "manifest.json").write_text("{}\n")
        for name in ("pohunekd", "pohunek-sessiond", "pohunek-gui", "pohunek-relayd"):
            self.executable(name, "#!/bin/sh\nexit 0\n")
        self.executable("pohunek", FAKE_POHUNEK)
        (self.root / "README.md").write_text("readme\n")
        (self.root / "LICENSE").write_text("license\n")
        (self.root / "packaging").mkdir()
        shutil.copy(PACKAGING / "install-daemon.sh", self.root / "packaging")
        (self.root / "scripts").mkdir()
        (self.root / "scripts" / "smoke-hermes-plugin-release").write_text("#!/bin/sh\n")

    def executable(self, name, text):
        path = self.bindir / name
        path.write_text(text)
        path.chmod(0o755)

    def stage(self, component, target=TARGET):
        result = run(
            [
                PACKAGING / "stage-archive",
                component,
                VERSION,
                target,
                self.bindir,
                self.docs,
                self.out,
            ],
            cwd=self.root,
        )
        return result.stdout.strip()


class StageArchiveTest(unittest.TestCase):
    def test_daemon_archive_holds_the_binaries_installer_completions_and_docs(self):
        ws = Workspace(self)
        name = ws.stage("daemon")
        self.assertEqual(name, f"pohunek-daemon-{VERSION}-{TARGET}")
        staging = ws.out / name
        for member in (
            "pohunek",
            "pohunekd",
            "pohunek-sessiond",
            "packaging/install-daemon.sh",
            "completions/pohunek.bash",
            "completions/_pohunek",
            "completions/pohunek.fish",
            "docs/offline/index.html",
            "docs/manifest.json",
            "README.md",
            "LICENSE",
        ):
            self.assertTrue((staging / member).is_file(), member)
        self.assertEqual((staging / "completions/_pohunek").read_text(), "completion for zsh\n")
        self.assertFalse((staging / "pohunek-gui").exists())

    def test_cli_archive_carries_the_packaged_smoke_and_no_daemon(self):
        ws = Workspace(self)
        staging = ws.out / ws.stage("cli")
        self.assertTrue((staging / "packaging/smoke-hermes-plugin-release").is_file())
        self.assertFalse((staging / "pohunekd").exists())
        self.assertFalse((staging / "packaging/install-daemon.sh").exists())

    def test_gui_and_relay_archives_hold_one_binary(self):
        ws = Workspace(self)
        self.assertTrue((ws.out / ws.stage("gui") / "pohunek-gui").is_file())
        self.assertTrue((ws.out / ws.stage("relay") / "pohunek-relayd").is_file())

    def test_macos_gui_archive_holds_the_app_bundle(self):
        ws = Workspace(self)
        app = ws.bindir / "Pohunek.app" / "Contents" / "MacOS"
        app.mkdir(parents=True)
        (app / "pohunek-gui").write_text("binary\n")
        staging = ws.out / ws.stage("gui", "aarch64-apple-darwin")
        self.assertTrue((staging / "Pohunek.app/Contents/MacOS/pohunek-gui").is_file())
        self.assertFalse((staging / "pohunek-gui").exists())

    def test_the_output_directory_is_created_when_missing(self):
        ws = Workspace(self)
        out = ws.root / "fresh" / "dist"
        run(
            [PACKAGING / "stage-archive", "cli", VERSION, TARGET, ws.bindir, ws.docs, out],
            cwd=ws.root,
        )
        self.assertTrue((out / ("pohunek-cli-%s-%s" % (VERSION, TARGET)) / "pohunek").is_file())

    def test_a_missing_binary_or_bad_argument_is_refused(self):
        ws = Workspace(self)
        (ws.bindir / "pohunekd").unlink()
        result = run(
            [PACKAGING / "stage-archive", "daemon", VERSION, TARGET, ws.bindir, ws.docs, ws.out],
            cwd=ws.root,
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("built binary is missing", result.stderr)
        for args in (("nope", VERSION, TARGET), ("cli", "1.2", TARGET), ("cli", VERSION, "X/Y")):
            result = run(
                [PACKAGING / "stage-archive", *args, ws.bindir, ws.docs, ws.out],
                cwd=ws.root,
                check=False,
            )
            self.assertNotEqual(result.returncode, 0, args)


class WriteManifestTest(unittest.TestCase):
    def manifest(self, ws, name, *args, check=True):
        return run(
            [PACKAGING / "write-manifest", ws.out / name, *args],
            check=check,
        )

    def test_manifest_lists_every_file_with_its_digest(self):
        ws = Workspace(self)
        name = ws.stage("daemon")
        self.manifest(ws, name, "daemon", VERSION, TARGET, "none")
        lines = (ws.out / name / "MANIFEST").read_text().splitlines()
        self.assertEqual(
            lines[:5],
            [
                "pohunek-archive-manifest 1",
                "component daemon",
                f"version {VERSION}",
                f"target {TARGET}",
                "signing none",
            ],
        )
        entries = {}
        for line in lines[5:]:
            tag, digest, path = line.split(" ", 2)
            self.assertEqual(tag, "sha256")
            entries[path] = digest
        paths = list(entries)
        self.assertEqual(paths, sorted(paths))
        self.assertNotIn("MANIFEST", entries)
        for path, digest in entries.items():
            data = (ws.out / name / path).read_bytes()
            self.assertEqual(hashlib.sha256(data).hexdigest(), digest, path)
        self.assertIn("pohunekd", entries)

    def test_darwin_manifest_records_the_minimum_os(self):
        ws = Workspace(self)
        name = ws.stage("daemon")
        self.manifest(ws, name, "daemon", VERSION, "aarch64-apple-darwin", "unsigned-development", "14.0")
        text = (ws.out / name / "MANIFEST").read_text()
        self.assertIn("signing unsigned-development\n", text)
        self.assertIn("minimum-macos 14.0\n", text)

    def test_invalid_input_is_refused_and_leaves_no_manifest(self):
        ws = Workspace(self)
        name = ws.stage("daemon")
        for args, message in (
            (("daemon", VERSION, "aarch64-apple-darwin", "none"), "needs <minimum-macos>"),
            (("daemon", VERSION, TARGET, "none", "14.0"), "applies only to an apple-darwin"),
            (("daemon", "1.2", TARGET, "none"), "version must be"),
            (("daemon", VERSION, TARGET, "signed"), "unsupported signing state"),
            (("nope", VERSION, TARGET, "none"), "unsupported component"),
        ):
            result = self.manifest(ws, name, *args, check=False)
            self.assertNotEqual(result.returncode, 0, args)
            self.assertIn(message, result.stderr)
        self.assertFalse((ws.out / name / "MANIFEST").exists())
        self.assertFalse((ws.out / name / "MANIFEST.tmp").exists())

    def test_symlinks_and_unsafe_names_are_refused(self):
        ws = Workspace(self)
        name = ws.stage("daemon")
        link = ws.out / name / "link"
        link.symlink_to("pohunek")
        result = self.manifest(ws, name, "daemon", VERSION, TARGET, "none", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("symbolic link", result.stderr)
        link.unlink()
        (ws.out / name / "bad name").write_text("x")
        result = self.manifest(ws, name, "daemon", VERSION, TARGET, "none", check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported character", result.stderr)

    def test_rewriting_replaces_the_previous_manifest(self):
        ws = Workspace(self)
        name = ws.stage("daemon")
        self.manifest(ws, name, "daemon", VERSION, TARGET, "none")
        first = (ws.out / name / "MANIFEST").read_text()
        self.manifest(ws, name, "daemon", VERSION, TARGET, "none")
        self.assertEqual((ws.out / name / "MANIFEST").read_text(), first)


class ArchiveTest(unittest.TestCase):
    def build(self, ws, name, out, umask=0o022, epoch=EPOCH):
        out.mkdir(exist_ok=True)
        previous = os.umask(umask)
        try:
            run(
                [PACKAGING / "archive", ws.out, name, out],
                env={"SOURCE_DATE_EPOCH": epoch},
            )
        finally:
            os.umask(previous)
        return out / f"{name}.tar.gz"

    def test_equal_trees_give_byte_identical_archives_whatever_the_host_state(self):
        first = Workspace(self)
        name = first.stage("daemon")
        run([PACKAGING / "write-manifest", first.out / name, "daemon", VERSION, TARGET, "none"])
        second = Workspace(self)
        second.stage("daemon")
        run([PACKAGING / "write-manifest", second.out / name, "daemon", VERSION, TARGET, "none"])
        # Different file times and modes on the way in must not show.
        for path in (second.out / name).rglob("*"):
            os.utime(path, (1_000_000_000, 1_000_000_000))
        (second.out / name / "README.md").chmod(0o600)
        a = self.build(first, name, first.root / "a", umask=0o022)
        b = self.build(second, name, second.root / "b", umask=0o077)
        self.assertEqual(a.read_bytes(), b.read_bytes())
        self.assertEqual(
            (a.parent / f"{name}.tar.gz.sha256").read_text(),
            f"{hashlib.sha256(a.read_bytes()).hexdigest()}  {name}.tar.gz\n",
        )

    def test_members_are_sorted_root_owned_dated_and_mode_normalized(self):
        ws = Workspace(self)
        name = ws.stage("daemon")
        archive = self.build(ws, name, ws.root / "o")
        with tarfile.open(archive) as tar:
            members = tar.getmembers()
        names = [m.name for m in members]
        self.assertEqual(names, sorted(names))
        self.assertEqual(names[0], name)
        for member in members:
            self.assertEqual((member.uid, member.gid), (0, 0), member.name)
            self.assertEqual(member.mtime, int(EPOCH), member.name)
            if member.isdir():
                self.assertEqual(member.mode, 0o755, member.name)
        modes = {m.name.rsplit("/", 1)[-1]: m.mode for m in members if m.isfile()}
        self.assertEqual(modes["pohunekd"], 0o755)
        self.assertEqual(modes["README.md"], 0o644)
        # No AppleDouble or extended-header members.
        self.assertFalse([n for n in names if "/._" in n or "PaxHeaders" in n])

    def test_relative_arguments_mean_the_callers_directory(self):
        # The release workflow calls `archive dist "$name" dist` from the
        # repository root.
        ws = Workspace(self)
        name = ws.stage("daemon")
        run([PACKAGING / "write-manifest", ws.out / name, "daemon", VERSION, TARGET, "none"])
        run(
            [PACKAGING / "archive", "dist", name, "dist"],
            cwd=ws.root,
            env={"SOURCE_DATE_EPOCH": EPOCH},
        )
        self.assertTrue((ws.root / "dist" / (name + ".tar.gz")).is_file())
        self.assertTrue((ws.root / "dist" / (name + ".tar.gz.sha256")).is_file())
        self.assertFalse((ws.root / "dist" / "dist").exists())

    def test_the_output_directory_is_created_when_missing(self):
        ws = Workspace(self)
        name = ws.stage("daemon")
        out = ws.root / "new" / "out"
        run([PACKAGING / "archive", ws.out, name, out], env={"SOURCE_DATE_EPOCH": EPOCH})
        self.assertTrue((out / (name + ".tar.gz")).is_file())

    def test_a_different_commit_time_changes_the_archive(self):
        ws = Workspace(self)
        name = ws.stage("daemon")
        a = self.build(ws, name, ws.root / "a").read_bytes()
        b = self.build(ws, name, ws.root / "b", epoch="1700000001").read_bytes()
        self.assertNotEqual(a, b)

    def test_a_missing_epoch_or_bad_name_is_refused(self):
        ws = Workspace(self)
        name = ws.stage("daemon")
        env = {k: v for k, v in os.environ.items() if k != "SOURCE_DATE_EPOCH"}
        result = subprocess.run(
            [str(PACKAGING / "archive"), str(ws.out), name, str(ws.out)],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SOURCE_DATE_EPOCH", result.stderr)
        result = run(
            [PACKAGING / "archive", ws.out, "../x", ws.out],
            env={"SOURCE_DATE_EPOCH": EPOCH},
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
