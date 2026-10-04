"""Release compatibility check: changed schema/protocol constants need notes.

`scripts/release` compares the persisted-state and protocol constants at the
previous tag with HEAD and refuses to proceed unless the notes file names every
constant that changed. The tests run the real script in a throwaway repository
with a stub `cargo`, using `--dry-run` so nothing is committed or pushed.
"""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "release"

STORE_FILE = "crates/daemon/src/store/schema.rs"
PROTOCOL_FILE = "crates/protocol/src/version.rs"

CARGO_TOML = '[workspace.package]\nversion = "0.1.0"\n'


def write(root, relative, text):
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


class ReleaseCompatCheckTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="release-compat-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.repo = self.tmp / "repo"
        (self.repo / "scripts").mkdir(parents=True)
        shutil.copy(SCRIPT, self.repo / "scripts" / "release")
        write(self.repo, "Cargo.toml", CARGO_TOML)
        self.set_constants(store=2, protocol=4)
        stub_bin = self.tmp / "bin"
        stub_bin.mkdir()
        cargo = stub_bin / "cargo"
        cargo.write_text("#!/bin/sh\nexit 0\n")
        cargo.chmod(0o755)
        self.env = dict(os.environ, PATH=f"{stub_bin}:{os.environ['PATH']}")
        self.env["GIT_CONFIG_GLOBAL"] = os.devnull
        self.env["GIT_CONFIG_SYSTEM"] = os.devnull
        for role in ("AUTHOR", "COMMITTER"):
            self.env[f"GIT_{role}_NAME"] = "test"
            self.env[f"GIT_{role}_EMAIL"] = "test@example.invalid"
        self.git("init", "-q", "-b", "main")
        self.git("add", "-A")
        self.commit("initial")
        self.git("tag", "-a", "v0.1.0", "-m", "v0.1.0")

    def git(self, *args):
        subprocess.run(
            ["git", "-C", str(self.repo), *args], check=True, env=self.env,
            stdout=subprocess.DEVNULL,
        )

    def commit(self, message):
        self.git("-c", "commit.gpgsign=false", "commit", "-q", "-m", message)

    def set_constants(self, store, protocol):
        write(
            self.repo, STORE_FILE,
            f"pub const STORE_SCHEMA_VERSION: u32 = {store};\n",
        )
        write(
            self.repo, PROTOCOL_FILE,
            "pub const PROTOCOL_VERSION: ProtocolVersion = "
            f"ProtocolVersion({protocol});\n"
            "pub const MIN_PROTOCOL_VERSION: ProtocolVersion = PROTOCOL_VERSION;\n",
        )

    def release(self, *extra):
        return subprocess.run(
            [str(self.repo / "scripts" / "release"), "patch", "--dry-run", *extra],
            capture_output=True, text=True, env=self.env,
        )

    def change(self, store, protocol):
        self.set_constants(store, protocol)
        self.git("add", "-A")
        self.commit("change constants")

    def test_unchanged_constants_need_no_notes(self):
        result = self.release()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("no schema or protocol constant changed", result.stdout)

    def test_changed_constant_without_notes_fails_and_names_it(self):
        self.change(store=3, protocol=4)
        result = self.release()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("STORE_SCHEMA_VERSION", result.stdout + result.stderr)
        self.assertIn("--notes", result.stderr)

    def test_notes_must_name_every_changed_constant(self):
        self.change(store=3, protocol=5)
        notes = self.tmp / "notes.md"
        notes.write_text("- STORE_SCHEMA_VERSION 2 -> 3: records gain a field\n")
        result = self.release("--notes", str(notes))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lack a line for", result.stderr)
        self.assertIn("PROTOCOL_VERSION", result.stderr)

    def test_complete_notes_pass(self):
        self.change(store=3, protocol=5)
        notes = self.tmp / "notes.md"
        notes.write_text(
            "- STORE_SCHEMA_VERSION 2 -> 3\n- PROTOCOL_VERSION 4 -> 5\n"
            "- MIN_PROTOCOL_VERSION 4 -> 5\n"
        )
        result = self.release("--notes", str(notes))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("name every changed constant", result.stdout)

    def test_a_longer_constant_name_does_not_satisfy_a_shorter_one(self):
        self.change(store=2, protocol=5)
        notes = self.tmp / "notes.md"
        notes.write_text("- MIN_PROTOCOL_VERSION is now 5\n")
        result = self.release("--notes", str(notes))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lack a line for: PROTOCOL_VERSION", result.stderr)

    def test_an_alias_changes_with_its_target(self):
        self.change(store=2, protocol=5)
        result = self.release()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("MIN_PROTOCOL_VERSION changed", result.stdout)
        notes = self.tmp / "notes.md"
        notes.write_text("- PROTOCOL_VERSION 4 -> 5\n")
        result = self.release("--notes", str(notes))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lack a line for: MIN_PROTOCOL_VERSION", result.stderr)

    def test_offset_alias_resolves_and_detects_a_move(self):
        write(
            self.repo, PROTOCOL_FILE,
            "pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(5);\n"
            "pub const MIN_PROTOCOL_VERSION: ProtocolVersion = PROTOCOL_VERSION - 1;\n",
        )
        self.git("add", "-A")
        self.commit("minimum follows the maximum")
        self.git("tag", "-a", "v0.1.1", "-m", "v0.1.1")
        write(
            self.repo, PROTOCOL_FILE,
            "pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(6);\n"
            "pub const MIN_PROTOCOL_VERSION: ProtocolVersion = PROTOCOL_VERSION - 1;\n",
        )
        self.git("add", "-A")
        self.commit("bump protocol")
        result = self.release()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("MIN_PROTOCOL_VERSION changed since v0.1.1 "
                      "(ProtocolVersion(4) -> ProtocolVersion(5))", result.stdout)

    def test_an_initializer_it_cannot_evaluate_fails_closed(self):
        write(
            self.repo, PROTOCOL_FILE,
            "pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(4);\n"
            "pub const MIN_PROTOCOL_VERSION: ProtocolVersion = "
            "ProtocolVersion(PROTOCOL_VERSION.0 / 2);\n",
        )
        self.git("add", "-A")
        self.commit("opaque minimum")
        result = self.release()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot evaluate the initializer of MIN_PROTOCOL_VERSION",
                      result.stderr)

    def test_constant_absent_at_the_previous_tag_counts_as_changed(self):
        (self.repo / STORE_FILE).unlink()
        self.git("add", "-A")
        self.commit("drop store constant")
        self.git("tag", "-a", "v0.1.1", "-m", "v0.1.1")
        write(
            self.repo, STORE_FILE,
            "pub const STORE_SCHEMA_VERSION: u32 = 2;\n",
        )
        self.git("add", "-A")
        self.commit("introduce store constant")
        result = self.release()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("absent -> 2", result.stdout)

    def test_notes_flag_requires_a_file(self):
        result = self.release("--notes")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--notes needs a file", result.stderr)


if __name__ == "__main__":
    unittest.main()
