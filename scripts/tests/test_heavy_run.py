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
import random
import stat
import string
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
# Behaves like a scope: writes the command's PID to the per-fixture pid
# directory (substituted in by the test) under <unit>, so the fake
# `systemctl stop` can find and kill it.
echo "$*" >> "$HEAVY_FAKE_LOG"
unit=""
prev=""
for arg in "$@"; do
    if [ "$prev" = "--unit" ]; then
        unit="$arg"
    fi
    case "$arg" in
        --unit=*) unit="${arg#--unit=}" ;;
    esac
    prev="$arg"
done
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
"$@" &
child=$!
if [ -n "$unit" ] && [ -n "$FAKE_PID_DIR" ]; then
    echo "$child" > "$FAKE_PID_DIR/$unit"
fi
wait "$child"
rc=$?
if [ -n "$unit" ] && [ -n "$FAKE_PID_DIR" ]; then
    rm -f "$FAKE_PID_DIR/$unit"
fi
exit "$rc"
"""

FAKE_SYSTEMCTL = """\
#!/bin/sh
echo "$*" >> "$HEAVY_FAKE_LOG"
mode="${HEAVY_SYSTEMCTL_MODE:-ok}"
case "$mode" in
    ok) ;;
    not-loaded)
        echo "Failed to stop unit: Unit pohunek-heavy-1-1.scope not loaded." >&2
        exit 5
        ;;
    broken)
        echo "Failed to stop unit: Access denied." >&2
        exit 1
        ;;
esac
# A scope stop kills the command tree underneath it: look up the PID the
# fake `systemd-run` recorded for `--unit` and kill it. The argv are
# `stop <unit>` (`--user` is before `stop`), so the unit is the last word.
# `$FAKE_PID_DIR` is substituted in by the test (it is not exported).
unit=""
for arg in "$@"; do unit="$arg"; done
pid_file="$FAKE_PID_DIR/$unit"
if [ -f "$pid_file" ]; then
    pid="$(cat "$pid_file")"
    if kill -0 "$pid" 2>/dev/null; then
        echo "stop found unit $unit alive at pid $pid" >> "$HEAVY_FAKE_LOG"
        kill -KILL "$pid" 2>/dev/null
    else
        echo "stop found unit $unit already gone" >> "$HEAVY_FAKE_LOG"
    fi
else
    echo "stop found no pid file for unit $unit" >> "$HEAVY_FAKE_LOG"
fi
exit 0
"""


class Fixture(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="heavy-run-")).resolve()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        # A short `tmpfs_root`: the N4 per-slot budget keeps every
        # `<root>/<slot>` within the 9-byte RUN_BASE_MAX_LENGTH, while the
        # temp dir path is far longer. The name is created exclusively
        # (the make_run_base pattern in scripts/test-partitions:
        # RUN_BASE_PARENT is /tmp, the name fills RUN_BASE_NAME_LENGTH);
        # only what this fixture created is ever removed.
        self.root = None
        alphabet = string.ascii_lowercase + string.digits
        for _ in range(100):
            candidate = Path("/tmp") / ("h" + random.choice(alphabet))
            try:
                os.mkdir(candidate, mode=0o700)
            except FileExistsError:
                continue
            self.root = candidate
            break
        self.assertIsNotNone(self.root, "could not create a short fixture root")
        self.addCleanup(shutil.rmtree, self.root, True)
        self.locks = self.tmp / "locks"
        self.locks.mkdir()
        # `write_config` runs before `self.env` exists, so the fake
        # `systemd-run` defaults to a per-fixture directory beside the
        # log instead of reading the environment.
        self.pid_dir = self.tmp / "pids"
        self.pid_dir.mkdir()
        (self.bin / "systemd-run").write_text(
            FAKE_SYSTEMD_RUN.replace("$FAKE_PID_DIR", str(self.pid_dir)))
        for name in ("sh", "echo", "cat", "stat", "flock", "sleep", "find", "rm", "chmod", "mkdir", "mountpoint", "findmnt"):
            target = self.bin / name
            if not target.exists():
                target.symlink_to(f"/usr/bin/{name}")
        (self.bin / "systemd-run").chmod(0o755)
        (self.bin / "systemctl").write_text(
            FAKE_SYSTEMCTL.replace("$FAKE_PID_DIR", str(self.pid_dir)))
        (self.bin / "systemctl").chmod(0o755)
        self.log = self.tmp / "calls.log"
        self.config_home = self.tmp / "config"
        (self.config_home / "pohunek").mkdir(parents=True)
        self.write_config(slots=1, require_tmpfs=False)
        # PATH keeps the system directories after the fixture bin: the
        # script itself needs flock/sleep/find/rm there, only systemd-run
        # and systemctl come from the fixture.
        self.env = dict(
            os.environ,
            PATH=str(self.bin),
            XDG_CONFIG_HOME=str(self.config_home),
            HEAVY_FAKE_LOG=str(self.log),
        )
        self.log.touch()

    def write_config(self, slots=2, lock_dir=None, tmpfs_root=None,
                     require_tmpfs=False, extra=(), drop=()):
        lines = [
            line
            for line in [
                f"slots={slots}",
                f"lock_dir={self.locks if lock_dir is None else lock_dir}",
                "memory_max=8G",
                f"tmpfs_root={self.root if tmpfs_root is None else tmpfs_root}",
                "poll_seconds=1",
                f"require_tmpfs={'true' if require_tmpfs else 'false'}",
                *extra,
            ]
            if line.split('=')[0] not in drop
        ]
        (self.config_home / "pohunek" / "heavy-run.conf").write_text("\n".join(lines) + "\n")

    def run_script(self, *argv, check_slot=True, check_status=True, expected_rc=0, extra_env=None, timeout=WAIT_SECONDS):
        env = dict(self.env, **(extra_env or {}))
        result = subprocess.run(
            ["sh", str(SCRIPT), *argv], capture_output=True, text=True, env=env, timeout=timeout
        )
        calls = self.log.read_text().splitlines()
        if check_status:
            self.assertEqual(result.returncode, expected_rc,
                               f"want rc {expected_rc}: {result.stderr}")
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
        result, calls = self.run_script("sh", "-c", "exit 43", expected_rc=43)
        stops = [line for line in calls if line.startswith("--user stop pohunek-heavy-1-")]
        self.assertEqual(len(stops), 1, calls)
        self.assertFalse(any(self.slot_dir().iterdir()))

    def test_the_scope_is_stopped_after_success(self):
        self.write_config(slots=1)
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
                # Wait for the wrapper's own sleep child to exist: the first
                # full pass over both busy slots has finished, so the outer
                # wait loop (r2) is engaged and any exit now is the bug.
                deadline = time.monotonic() + WAIT_SECONDS
                while True:
                    children = Path(f"/proc/{proc.pid}/task").glob("*/children")
                    sleepers = [child.read_text().split() for child in children
                                if child.exists()]
                    if any(sleepers):
                        break
                    if proc.poll() is not None or time.monotonic() > deadline:
                        break
                    time.sleep(0.1)
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
        self.write_config(tmpfs_root="/mnt/ram-tmpfs-root")
        result, calls = self.run_script("true", check_slot=False, expected_rc=1)
        self.assertIn("exceeding the 9-byte socket budget", result.stderr)
        self.assertEqual(calls, [])

    def test_a_root_that_only_overflows_at_slot_two_is_refused(self):
        # `/abc/de/12` is 10 bytes and would pass a root-only check (the
        # root is 7 bytes), but the longest slot path overflows the budget.
        self.write_config(slots=12, tmpfs_root="/abc/de")
        result, calls = self.run_script("true", check_slot=False, expected_rc=1)
        self.assertIn("exceeding the 9-byte socket budget", result.stderr)
        self.assertEqual(calls, [])

    def test_a_nine_byte_tmpfs_root_runs_the_command(self):
        # Nine bytes is the budget itself, so it is accepted; running the
        # real script proves the check does not reject the boundary. (The
        # command really runs: its marker only exists if systemd-run ran.)
        self.write_config(slots=1)
        marker = self.tmp / "nine-byte-boundary"
        result, calls = self.run_script("sh", "-c", f'echo ran > "{marker}"')
        self.assertEqual(marker.read_text().strip(), "ran")
        self.assertLessEqual(len(str(self.slot_dir())), 9)

    def test_an_unreadable_stale_dir_fails_the_release_sweep(self):
        stale = self.slot_dir()
        stale.mkdir()
        (stale / "sub").mkdir()
        ((stale / "sub") / "drop").write_text("leftover")
        (stale / "sub").chmod(0)
        try:
            result, _ = self.run_script("sh", "-c", "exit 43", check_status=False, check_slot=False)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("warning: sweep", result.stderr)
            self.assertIn("cannot sweep slot directory", result.stderr)
            # The entry itself survives: a failed release sweep never
            # deletes what it could not sweep (`rm` cannot touch the
            # mode-0 directory, so it stays put).
            self.assertTrue((stale / "sub").exists())
        finally:
            (stale / "sub").chmod(0o755)

    def test_an_uncreatable_slot_dir_refuses_without_running(self):
        blocker = self.tmp / "blocker"
        blocker.write_text("not a directory")
        self.write_config(slots=1, tmpfs_root=str(blocker))
        result, calls = self.run_script("true", check_slot=False, expected_rc=1)
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
        result, calls = self.run_script("true", check_slot=False, expected_rc=1)
        self.assertIn("memory_max", result.stderr)
        self.assertEqual(calls, [])

    def test_a_missing_config_file_fails_without_running_the_command(self):
        (self.config_home / "pohunek" / "heavy-run.conf").unlink()
        result, calls = self.run_script("true", check_slot=False, expected_rc=1)
        self.assertIn("heavy-run.conf", result.stderr)
        self.assertEqual(calls, [])

    def test_a_missing_tmpfs_check_fails(self):
        self.write_config(require_tmpfs=True)
        result, calls = self.run_script("true", check_slot=False, expected_rc=1)
        self.assertIn(str(self.root), result.stderr)
        self.assertEqual(calls, [])

    def test_a_not_loaded_stop_still_sweeps_and_keeps_the_status(self):
        self.slot_dir().mkdir()
        (self.slot_dir() / "drop").write_text("leftover")
        result, _ = self.run_script(
            "sh", "-c", "exit 7", expected_rc=7,
            extra_env={"HEAVY_SYSTEMCTL_MODE": "not-loaded"}
        )
        self.assertFalse(any(self.slot_dir().iterdir()))

    def test_a_broken_stop_is_reported_and_still_sweeps(self):
        result, _ = self.run_script(
            "sh", "-c", "exit 3", expected_rc=3,
            extra_env={"HEAVY_SYSTEMCTL_MODE": "broken"}
        )
        self.assertIn("Access denied", result.stderr)
        self.assertFalse(any(self.slot_dir().iterdir()))

    def test_all_documented_config_keys_parse_from_the_docs_verbatim(self):
        # The config example in docs/development.md "Heavy runs" must be
        # installable as written: copy its key=value lines verbatim (no
        # comment stripping, no edits) and drive the real script with them.
        example = []
        in_example = False
        for line in Path(__file__).resolve().parents[2].joinpath(
                "docs", "development.md").read_text().splitlines():
            if line.strip() == "```ini":
                in_example = True
                continue
            if in_example:
                if line.strip() == "```":
                    break
                if line.startswith(("slots=", "lock_dir=", "memory_max=",
                                     "tmpfs_root=", "poll_seconds=", "require_tmpfs=")):
                    example.append(line)
        self.assertEqual(len(example), 6, "the documented example must list all six keys")
        self.write_config(slots=1)
        # The documented example points at the host paths; redirect only
        # those values at the fixture, keeping the key order and comments
        # exactly as documented.
        conf = self.config_home / "pohunek" / "heavy-run.conf"
        conf.write_text("\n".join(
            f"lock_dir={self.locks}" if line.startswith("lock_dir=") else
            f"tmpfs_root={self.root}" if line.startswith("tmpfs_root=") else
            "require_tmpfs=false" if line.startswith("require_tmpfs=") else line
            for line in example) + "\n")
        marker = self.tmp / "docs-example"
        result, calls = self.run_script("sh", "-c", f'echo ran > "{marker}"')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(marker.read_text().strip(), "ran")
        self.assertTrue(any("TMPDIR=" in line and line.startswith("--user --scope")
                             for line in calls), calls)

    def test_sigterm_to_the_wrapper_only_still_stops_the_scope_and_sweeps(self):
        if not sys.platform.startswith("linux"):
            self.skipTest("sends SIGTERM to the wrapper PID only")
        marker = self.slot_dir() / "started"
        marker.unlink(missing_ok=True)
        proc = subprocess.Popen(
            ["sh", str(SCRIPT), "sh", "-c", f'echo started > "{marker}"; sleep 600'],
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
            env=self.env,
        )
        try:
            deadline = time.monotonic() + WAIT_SECONDS
            while time.monotonic() < deadline and not marker.exists():
                time.sleep(0.1)
            self.assertTrue(marker.exists(), "the command never started")
            # Signal the wrapper PID only: its trap must fire right away
            # (the command is a background child) and the scope stop must
            # kill the command tree underneath it. The pipes go to DEVNULL,
            # so `communicate` returns when the wrapper exits, not when the
            # orphaned command ends: no wall-clock comparison is involved.
            os.kill(proc.pid, signal.SIGTERM)
            _, stderr = proc.communicate(timeout=WAIT_SECONDS)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.communicate(timeout=WAIT_SECONDS)
        self.assertEqual(proc.returncode, 143, stderr)
        calls = self.log.read_text().splitlines()
        alive = [line for line in calls if "stop found unit pohunek-heavy-1-" in line
                 and "alive at pid" in line]
        self.assertEqual(len(alive), 1, calls)
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
