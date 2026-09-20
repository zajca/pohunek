"""Regression checks for the destructive cargo-sweep-targets helper (stdlib only)."""

from pathlib import Path
import os
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "cargo-sweep-targets"


def run_sweep(*args, target=None):
    """Run the sweep script against a fixture target dir."""
    cmd = ["bash", str(SCRIPT), "--target-dir", str(target), *args]
    return subprocess.run(cmd, capture_output=True, text=True, check=False)


def make_entry(root, name, *, shape="debug", old=True):
    """Create a fixture entry; shape=None means non-Cargo output."""
    entry = Path(root) / name
    entry.mkdir(parents=True)
    if shape == "debug":
        nested = entry / "debug"
        nested.mkdir()
        (nested / "artifact").write_text("build output")
    elif shape == "sentinel":
        (entry / "CACHEDIR.TAG").write_text("cargo cache tag")
    old_time = 60 * 60 * 24 * 60
    stamp = int(__import__("time").time()) - (old_time if old else 0)
    for path in [entry, *entry.rglob("*")]:
        os.utime(path, (stamp, stamp))
    return entry


def make_target_root(entries=()):
    """Create a fixture target root with the Cargo sentinel."""
    tmp = tempfile.TemporaryDirectory()
    root = Path(tmp.name)
    (root / "CACHEDIR.TAG").write_text("cargo cache tag")
    for name, kwargs in entries:
        make_entry(root, name, **kwargs)
    return tmp, root


class SweepSafetyTests(unittest.TestCase):
    def test_refuses_repo_root(self):
        repo = Path(__file__).resolve().parents[2]
        result = run_sweep("--dry-run", target=repo)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing", result.stderr + result.stdout)

    def test_refuses_home_without_sentinel(self):
        with tempfile.TemporaryDirectory() as home:
            result = run_sweep("--dry-run", target=home)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("refusing", result.stderr + result.stdout)

    def test_refuses_filesystem_root(self):
        result = run_sweep("--dry-run", target="/")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refusing", result.stderr + result.stdout)

    def test_rejects_invalid_days(self):
        tmp, root = make_target_root()
        with tmp:
            for bad in ("0", "-1", "abc"):
                result = run_sweep("--days", bad, target=root)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("--days", result.stderr + result.stdout)

    def test_keeps_eval_doc_package_and_triples(self):
        tmp, root = make_target_root([
            ("pohunek-eval", {"shape": "debug"}),
            ("doc", {"shape": "debug"}),
            ("package", {"shape": "debug"}),
            ("x86_64-unknown-linux-gnu", {"shape": "debug"}),
            ("aarch64-apple-darwin", {"shape": "debug"}),
            ("x86_64-pc-windows-msvc", {"shape": "debug"}),
            ("wasm32-wasip1", {"shape": "debug"}),
            ("stale-branch", {"shape": "debug"}),
        ])
        with tmp:
            result = run_sweep("--dry-run", "--days", "30", target=root)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("would delete", result.stdout)
            self.assertIn("stale-branch", result.stdout)
            self.assertIn("cross-compile target output", result.stdout)
            self.assertNotIn("pohunek-eval", result.stdout.replace("sweep done", ""))
            delete_lines = [line for line in result.stdout.splitlines()
                            if "delete (stale" in line or "would delete" in line]
            self.assertEqual(len(delete_lines), 1)
            self.assertIn("stale-branch", delete_lines[0])

    def test_skips_non_cargo_shape(self):
        tmp, root = make_target_root([
            ("plain-notes", {"shape": None}),
            ("stale-branch", {"shape": "debug"}),
        ])
        with tmp:
            result = run_sweep("--dry-run", "--days", "30", target=root)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("skip (not a Cargo target directory)", result.stdout)
            self.assertIn("stale-branch", result.stdout)

    def test_missing_target_dirs_report_nothing_to_sweep(self):
        # A fresh checkout has no target/ at all: both the default target dir
        # (repo/target) and an explicitly passed missing dir are clean no-ops,
        # while the sentinel refusal stays for paths that do exist.
        script = str(SCRIPT)
        with tempfile.TemporaryDirectory() as fake_repo:
            fake_scripts = Path(fake_repo) / "scripts"
            fake_scripts.mkdir()
            (fake_scripts / "cargo-sweep-targets").symlink_to(script)
            missing_default = Path(fake_repo) / "target"
            self.assertFalse(missing_default.exists())
            default_run = subprocess.run(
                ["bash", str(fake_scripts / "cargo-sweep-targets"), "--dry-run"],
                capture_output=True, text=True, check=False,
            )
            self.assertEqual(default_run.returncode, 0, default_run.stderr)
            self.assertIn("nothing to sweep", default_run.stdout)
            explicit_run = run_sweep("--dry-run", target=missing_default)
            self.assertEqual(explicit_run.returncode, 0, explicit_run.stderr)
            self.assertIn("nothing to sweep", explicit_run.stdout)

    def test_nested_recency_keeps_active_target(self):
        tmp, root = make_target_root([("active-branch", {"shape": "debug"})])
        with tmp:
            entry = root / "active-branch"
            old_time = __import__("time").time() - 60 * 60 * 24 * 60
            os.utime(entry, (old_time, old_time))
            nested = entry / "debug" / "artifact"
            now = __import__("time").time()
            os.utime(nested, (now, now))
            result = run_sweep("--dry-run", "--days", "30", target=root)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("keep (recent)", result.stdout)
            self.assertNotIn("would delete", result.stdout)

    def test_deletes_stale_branch_target(self):
        tmp, root = make_target_root([("stale-branch", {"shape": "debug"})])
        with tmp:
            result = run_sweep("--days", "30", target=root)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("delete (stale", result.stdout)
            self.assertFalse((root / "stale-branch").exists())


if __name__ == "__main__":
    unittest.main()
