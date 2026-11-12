"""Integration scenarios for `scripts/heavy-run` (stdlib only).

Every test drives the real script as a subprocess with a private config file
(`XDG_CONFIG_HOME` points at the fixture) and fake `systemd-run`/`systemctl`
on a private PATH that records their argv. The fake `systemd-run` runs the
command (everything after the first bare `--`, else after `env` and its
assignments) so the sweep, the lock and the exit status are the real script's
own. The fixture's `tmpfs_root` is an ordinary directory: meeting the tmpfs
check is a dedicated scenario of its own, and the remaining scenarios set
`require_tmpfs=false`.
"""

import fcntl
import os
import stat
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

import hermetic_git  # noqa: F401  (isolates git from the developer's configuration)

SCRIPT = Path(__file__).resolve().parents[1] / "heavy-run"
# Upper bound for a scenario step that waits on another process.
WAIT_SECONDS = 60
# Absolute ceiling for the contention scenario: the wrapper sleeps
# `poll_seconds` between full passes over busy slots.
CONTEND_DEADLINE = 120

FAKE_SYSTEMD_RUN = """\
#!/bin/sh
# Record argv, then run the command after the first bare `--`,
# else after `env` and its VAR=value assignments.
echo "$*" >> "$HEAVY_FAKE_LOG"
seen_env=0
while [ "$#" -gt 0 ]; do
    arg="$1"
    shift
    if [ "$seen_env" = "0" ]; then
        if [ "$arg" = "env" ]; then
            seen_env=1
            continue
        fi
        continue
    fi
    case "$arg" in
        --) break ;;
        *=*) export "$arg"; continue ;;
        *) set -- "$arg" "$@"; break ;;
    esac
done
if [ "$#" -eq 0 ]; then
    exit 0
fi
# Behave like `systemd-run --scope`: a SIGTERM to the scope kills the
# command with it, so a test that signals the wrapper sees the command die.
"$@" &
child=$!
trap 'kill -TERM "$child" 2>/dev/null' TERM INT
wait "$child"
rc=$?
trap - TERM INT
exit "$rc"
"""

FAKE_SYSTEMCTL = """\
#!/bin/sh
echo "$*" >> "$HEAVY_FAKE_LOG"
mode="${HEAVY_SYSTEMCTL_MODE:-ok}"
case "$mode" in
    ok) exit 0 ;;
    not-loaded)
        echo "Failed to stop unit: Unit pohunek-heavy-1-1.scope not loaded." >&2
        exit 5
        ;;
    broken)
        echo "Failed to stop unit: Access denied." >&2
        exit 1
        ;;
esac
"""


class Fixture(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="heavy-run-")).resolve()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        # A short `tmpfs_root`: the script refuses roots longer than the
        # 9-byte socket budget, and the temp dir path is far longer. The
        # `/tmp/hNNN` dir (8 bytes) is the root itself.
        self.root = Path("/tmp") / f"h{os.getpid() % 1000:03d}"
        self.root.mkdir(exist_ok=True)
        self.addCleanup(shutil.rmtree, self.root, True)
        self.locks = self.tmp / "locks"
        self.locks.mkdir()
        (self.bin / "systemd-run").write_text(FAKE_SYSTEMD_RUN)
        (self.bin / "systemd-run").chmod(0o755)
        (self.bin / "systemctl").write_text(FAKE_SYSTEMCTL)
        (self.bin / "systemctl").chmod(0o755)
        self.log = self.tmp / "calls.log"
        self.config_home = self.tmp / "config"
        (self.config_home / "pohunek").mkdir(parents=True)
        self.write_config(slots=1, require_tmpfs=False)
        self.env = dict(
            os.environ,
            PATH=f"{self.bin}{os.pathsep}/usr/bin{os.pathsep}/bin",
            XDG_CONFIG_HOME=str(self.config_home),
            HEAVY_FAKE_LOG=str(self.log),
        )
        self.log.touch()

    def write_config(self, slots=1, require_tmpfs=False, extra=(), drop=()):
        lines = [
            line
            for line in [
                f"slots={slots}",
                f"lock_dir={self.locks}",
                "memory_max=8G",
                f"tmpfs_root={self.root}",
                "poll_seconds=1",
                f"require_tmpfs={'true' if require_tmpfs else 'false'}",
                *extra,
            ]
            if line.split('=')[0] not in drop
        ]
        (self.config_home / "pohunek" / "heavy-run.conf").write_text("\n".join(lines) + "\n")

    def run_script(self, *argv, check_slot=True, extra_env=None, timeout=WAIT_SECONDS):
        env = dict(self.env, **(extra_env or {}))
        result = subprocess.run(
            ["sh", str(SCRIPT), *argv], capture_output=True, text=True, env=env, timeout=timeout
        )
        calls = self.log.read_text().splitlines()
        if check_slot:
            runs = [line for line in calls if line.startswith("--user --scope --unit=")]
            self.assertEqual(len(runs), 1, calls)
            self.assertIn("TMPDIR=", runs[0])
        return result, calls

    def slot_dir(self, slot=1):
        return self.root / str(slot)

    def test_the_command_runs_with_the_slot_dir_as_tmpdir(self):
        marker = self.tmp / "tmpdir-was"
        result, _ = self.run_script("sh", "-c", f'echo "$TMPDIR" > "{marker}"')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(marker.read_text().strip(), str(self.slot_dir()))

    def test_a_failing_command_propagates_its_status_and_stops_the_scope(self):
        result, calls = self.run_script("sh", "-c", "exit 41")
        self.assertEqual(result.returncode, 41, result.stderr)
        stops = [line for line in calls if line.startswith("--user stop pohunek-heavy-1-")]
        self.assertEqual(len(stops), 1, calls)
        self.assertFalse(any(self.slot_dir().iterdir()))

    def test_the_scope_is_stopped_after_success(self):
        result, calls = self.run_script("true")
        self.assertEqual(result.returncode, 0, result.stderr)
        stops = [line for line in calls if line.startswith("--user stop pohunek-heavy-1-")]
        self.assertEqual(len(stops), 1, calls)

    def test_the_sweep_leaves_the_sibling_slot_untouched(self):
        self.write_config(slots=2)
        sibling = self.slot_dir(2)
        sibling.mkdir()
        (sibling / "keep").write_text("sibling fixture")
        result, _ = self.run_script("sh", "-c", f'echo leftover > "{self.slot_dir()}/drop"')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((sibling / "keep").read_text(), "sibling fixture")
        self.assertFalse(any(self.slot_dir().iterdir()))

    def test_a_held_sibling_lock_sends_the_run_to_the_free_slot(self):
        self.write_config(slots=2)
        sibling = self.slot_dir(2)
        held = self.locks / "pohunek-heavy-slot-1.lock"
        held.touch()
        with open(held, "w") as lock_file:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            marker = self.tmp / "used-slot"
            result, calls = self.run_script("sh", "-c", f'echo "$TMPDIR" > "{marker}"')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(marker.read_text().strip(), str(sibling))
            stops = [line for line in calls if line.startswith("--user stop pohunek-heavy-2-")]
            self.assertEqual(len(stops), 1, calls)
        self.assertFalse(any(sibling.iterdir()))

    def test_a_busy_slot_waits_and_then_runs_the_command(self):
        # Hold both slot locks from this process, release them after a
        # delay, and prove the wrapper waits and then runs the command
        # exactly once: no exit 0 without having run it.
        self.write_config(slots=2)
        markers = []
        holders = []
        try:
            for slot in (1, 2):
                path = self.locks / f"pohunek-heavy-slot-{slot}.lock"
                path.touch()
                holder = open(path, "w")
                fcntl.flock(holder.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                holders.append(holder)
            marker = self.tmp / "ran-after-wait"
            markers.append(marker)
            proc = subprocess.Popen(
                ["sh", str(SCRIPT), "sh", "-c", f'echo ran > "{marker}"'],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                env=self.env,
            )
            try:
                time.sleep(3)
                if proc.poll() is not None:
                    _, early_err = proc.communicate(timeout=WAIT_SECONDS)
                    self.fail(f"the wrapper exited rc={proc.returncode} while both slots were held: {early_err}")
                for holder in holders:
                    fcntl.flock(holder.fileno(), fcntl.LOCK_UN)
                    holder.close()
                holders.clear()
                stdout, stderr = proc.communicate(timeout=CONTEND_DEADLINE)
                self.assertEqual(proc.returncode, 0, stderr)
                self.assertEqual(marker.read_text().strip(), "ran")
                ran = [line for line in self.log.read_text().splitlines()
                       if "TMPDIR=" in line and line.startswith("--user --scope")]
                self.assertEqual(len(ran), 1, self.log.read_text())
                self.assertTrue(stdout == "", f"unexpected wrapper output: {stdout!r}")
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.communicate(timeout=WAIT_SECONDS)
        finally:
            for holder in holders:
                try:
                    fcntl.flock(holder.fileno(), fcntl.LOCK_UN)
                except OSError:
                    pass
                holder.close()

    def test_the_slot_directory_stays_0700(self):
        marker = self.tmp / "mode-was"
        result, _ = self.run_script(
            "sh", "-c", f'stat -c %a "$TMPDIR" > "{marker}"'
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(marker.read_text().strip(), "700")
        self.assertEqual(stat.S_IMODE(self.slot_dir().stat().st_mode), 0o700)

    def test_a_long_tmpfs_root_is_refused_before_any_lock(self):
        long_root = self.tmp / "long-root-for-sockets"
        lines = (self.config_home / "pohunek" / "heavy-run.conf").read_text().splitlines()
        lines = [line if not line.startswith("tmpfs_root=") else f"tmpfs_root={long_root}"
                 for line in lines]
        (self.config_home / "pohunek" / "heavy-run.conf").write_text("\n".join(lines) + "\n")
        result, calls = self.run_script("true", check_slot=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("9-byte", result.stderr)
        self.assertEqual(calls, [])

    def test_the_acquisition_sweep_clears_a_stale_leftover(self):
        stale = self.slot_dir()
        stale.mkdir()
        (stale / "crashed").write_text("a release sweep that never ran")
        result, _ = self.run_script("true")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any(stale.iterdir()))

    def test_a_missing_config_key_fails_without_running_the_command(self):
        self.write_config(drop=("memory_max",))
        result, calls = self.run_script("true", check_slot=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("memory_max", result.stderr)
        self.assertEqual(calls, [])

    def test_a_missing_config_file_fails_without_running_the_command(self):
        (self.config_home / "pohunek" / "heavy-run.conf").unlink()
        result, calls = self.run_script("true", check_slot=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("heavy-run.conf", result.stderr)
        self.assertEqual(calls, [])

    def test_a_missing_tmpfs_check_fails(self):
        self.write_config(require_tmpfs=True)
        result, calls = self.run_script("true", check_slot=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(str(self.root), result.stderr)
        self.assertEqual(calls, [])

    def test_a_not_loaded_stop_still_sweeps_and_keeps_the_status(self):
        self.slot_dir().mkdir()
        (self.slot_dir() / "drop").write_text("leftover")
        result, _ = self.run_script(
            "sh", "-c", "exit 7", extra_env={"HEAVY_SYSTEMCTL_MODE": "not-loaded"}
        )
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertFalse(any(self.slot_dir().iterdir()))

    def test_a_broken_stop_is_reported_and_still_sweeps(self):
        result, _ = self.run_script(
            "sh", "-c", "exit 3", extra_env={"HEAVY_SYSTEMCTL_MODE": "broken"}
        )
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertIn("Access denied", result.stderr)
        self.assertFalse(any(self.slot_dir().iterdir()))

    def test_sigterm_to_the_wrapper_only_still_stops_the_scope_and_sweeps(self):
        if not sys.platform.startswith("linux"):
            self.skipTest("sends SIGTERM to the wrapper PID only")
        marker = self.slot_dir() / "drop"
        proc = subprocess.Popen(
            ["sh", str(SCRIPT), "sh", "-c", f'echo leftover > "{marker}"; sleep 60'],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            env=self.env,
        )
        try:
            deadline = time.monotonic() + WAIT_SECONDS
            while time.monotonic() < deadline and not marker.exists():
                time.sleep(0.1)
            self.assertTrue(marker.exists(), "the command never started")
            # Signal the wrapper PID only: its trap must fire right away
            # (the command is a background child) and the scope stop must
            # kill the command tree underneath it.
            os.kill(proc.pid, signal.SIGTERM)
            _, stderr = proc.communicate(timeout=WAIT_SECONDS)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.communicate(timeout=WAIT_SECONDS)
        self.assertEqual(proc.returncode, 143, stderr)
        calls = self.log.read_text().splitlines()
        stops = [line for line in calls if line.startswith("--user stop pohunek-heavy-1-")]
        self.assertEqual(len(stops), 1, calls)
        self.assertFalse(any(self.slot_dir().iterdir()))

    def test_the_script_parses_as_posix_sh_and_is_executable(self):
        self.assertTrue(SCRIPT.stat().st_mode & 0o111, "heavy-run must be executable")
        for shell in ("sh", "bash"):
            if shutil.which(shell) is None:
                continue
            result = subprocess.run(
                [shell, "-n", str(SCRIPT)], capture_output=True, text=True
            )
            self.assertEqual(result.returncode, 0, f"{shell} -n failed: {result.stderr}")
        self.assertTrue(
            SCRIPT.read_text().startswith("#!/bin/sh\n"), "heavy-run must use /bin/sh"
        )


if __name__ == "__main__":
    unittest.main()
