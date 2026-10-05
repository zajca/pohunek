"""Checks of scripts/upgrade-test that need no privileges, network, or systemd.

The script defines its functions and stops when it is sourced, so the cleanup
trap runs here with `priv` (the only privileged entry point) replaced by a
function that logs.
"""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "upgrade-test"


def run_sourced(body: str, log: Path, home: Path) -> subprocess.CompletedProcess:
    """Sources the script with logging stand-ins and runs `body` under its trap."""
    program = f"""
set -euo pipefail
source "{SCRIPT}"
priv() {{ echo "PRIV $*" >> "{log}"; }}
as_user() {{ echo "AS_USER $*" >> "{log}"; }}
id() {{ if [ "${{1:-}}" = -u ]; then echo 4242; else return 1; fi; }}
getent() {{ echo "x:x:4242:4242::{home}:/bin/bash"; }}
staging=$(mktemp -d)
collect_dir=$(mktemp -d)
artifacts_dir=$(mktemp -d)
trap cleanup EXIT
{body}
"""
    return subprocess.run(
        ["bash", "-c", program], capture_output=True, text=True, check=False
    )


class UpgradeTestScript(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.log = Path(self.tmp.name) / "priv.log"
        self.log.write_text("")
        self.home = Path(self.tmp.name) / "home"
        self.home.mkdir()

    def logged(self) -> str:
        return self.log.read_text()

    def test_a_colliding_account_is_never_terminated_or_deleted(self) -> None:
        result = run_sourced(
            "account_exists() { return 0; }\ncreate_account_user\n", self.log, self.home
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already exists", result.stderr)
        for destructive in ("userdel", "terminate-user", "disable-linger", "stop user@"):
            self.assertNotIn(destructive, self.logged())
        self.assertNotIn("useradd", self.logged())

    def test_a_pre_existing_name_in_id_is_refused_the_same_way(self) -> None:
        result = run_sourced(
            'id() { return 0; }\naccount_exists() { return 1; }\ncreate_account_user\n',
            self.log,
            self.home,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("userdel", self.logged())

    def test_an_account_this_run_created_is_removed_on_failure(self) -> None:
        result = run_sourced(
            "account_exists() { return 1; }\ncurrent_user=false\ncreate_account_user\nfalse\n",
            self.log,
            self.home,
        )
        self.assertNotEqual(result.returncode, 0)
        log = self.logged()
        self.assertIn("PRIV useradd --create-home", log)
        self.assertIn("PRIV loginctl terminate-user phkupg", log)
        self.assertIn("PRIV userdel --remove phkupg", log)

    def test_cleanup_without_a_created_account_runs_no_account_step(self) -> None:
        result = run_sourced("user=someone-else\nuid=1000\nfalse\n", self.log, self.home)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.logged().count("userdel"), 0)
        self.assertNotIn("terminate-user", self.logged())

    def test_account_names_are_the_prefix_and_digits(self) -> None:
        names = {"phkupg123": 0, "phkupg": 1, "phkupgx1": 1, "root": 1, "phkupg1;rm": 1}
        for name, expected in names.items():
            result = run_sourced(
                f'valid_account_name "{name}"\n', self.log, self.home
            )
            self.assertEqual(result.returncode != 0, bool(expected), name)

    def test_a_failed_backup_restores_nothing(self) -> None:
        head = Path(self.tmp.name) / "head"
        head.mkdir()
        (head / "Cargo.toml").write_text("original-toml")
        (head / "Cargo.lock").write_text("original-lock")
        # The second copy fails after the first succeeded: a partial backup.
        body = f"""
head_dir="{head}"
cp() {{ case "$1" in */Cargo.lock) return 1 ;; *) command cp "$@" ;; esac; }}
backup_manifests && echo unexpected-success
echo "backup=[$manifest_backup]"
"""
        result = run_sourced(body, self.log, self.home)
        self.assertNotIn("unexpected-success", result.stdout)
        self.assertIn("backup=[]", result.stdout)
        self.assertEqual((head / "Cargo.toml").read_text(), "original-toml")
        self.assertEqual((head / "Cargo.lock").read_text(), "original-lock")

    def test_restore_needs_a_complete_backup_and_a_modification(self) -> None:
        head = Path(self.tmp.name) / "head"
        head.mkdir()
        (head / "Cargo.toml").write_text("stamped")
        (head / "Cargo.lock").write_text("stamped-lock")
        backup = Path(self.tmp.name) / "backup"
        backup.mkdir()
        (backup / "Cargo.toml").write_text("original-toml")
        (backup / "Cargo.lock").write_text("original-lock")
        untouched = f'head_dir="{head}"\nmanifest_backup="{backup}"\nmanifest_modified=0\nrestore_manifests\n'
        run_sourced(untouched, self.log, self.home)
        self.assertEqual((head / "Cargo.toml").read_text(), "stamped")
        backup.mkdir(exist_ok=True)
        (backup / "Cargo.toml").write_text("original-toml")
        (backup / "Cargo.lock").write_text("original-lock")
        modified = f'head_dir="{head}"\nmanifest_backup="{backup}"\nmanifest_modified=1\nrestore_manifests\n'
        run_sourced(modified, self.log, self.home)
        self.assertEqual((head / "Cargo.toml").read_text(), "original-toml")
        self.assertEqual((head / "Cargo.lock").read_text(), "original-lock")

    def run_script(self, *args: str, **env: str) -> subprocess.CompletedProcess:
        environment = {"PATH": os.environ["PATH"], **env}
        return subprocess.run(
            [str(SCRIPT), "--previous", "v0.33.0", "--repo", "o/r", "--head-dir", ".",
             "--cache-dir", "/nonexistent-cache", "--artifacts-dir", "/nonexistent-out", *args],
            capture_output=True, text=True, check=False, env=environment,
        )

    def test_current_user_mode_refuses_outside_ci(self) -> None:
        for env in ({}, {"CI": "true"}, {"GITHUB_ACTIONS": "true"}):
            result = self.run_script("--current-user", **env)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("hosted CI runner", result.stderr)

    def test_the_two_modes_are_exclusive_and_one_is_required(self) -> None:
        both = self.run_script("--current-user", "--create-account")
        self.assertIn("mutually exclusive", both.stderr)
        neither = self.run_script()
        self.assertIn("--create-account or --current-user is required", neither.stderr)


if __name__ == "__main__":
    unittest.main()
