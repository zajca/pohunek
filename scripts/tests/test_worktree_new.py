"""Regression checks for the worktree-new helper (stdlib only).

No test reaches a real git, cargo, or cp subprocess and none needs btrfs:
the executor is injected everywhere and emulates those commands inside a
temporary directory. Cargo and repository lock contention is exercised with
real `flock` calls on temporary files.
"""

import contextlib
import fcntl
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "worktree-new"
LOADER = importlib.machinery.SourceFileLoader("worktree_new", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
worktree_new = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(worktree_new)

# Error text of GNU cp when FICLONE crosses filesystems (tmpfs, other btrfs).
CROSS_DEVICE = "cp: failed to clone: Invalid cross-device link"
# Upper bound for a cross-thread handshake in the concurrency tests; it only
# bounds a hang when the code under test deadlocks, a passing run never
# waits for it.
HANDSHAKE_TIMEOUT_SECONDS = 10
# Commit ids of the fake repository: the base every new branch starts at,
# and a commit only other processes' refs point to.
BASE_COMMIT = "1" * 40
OTHER_COMMIT = "2" * 40
# Name prefix of a worktree that is still at its temporary path.
TEMP_PREFIX = ".worktree-new-"


def flock_is_blocked(path):
    """True when an exclusive non-blocking `flock` on `path` is refused.

    The file is opened here, so the attempt uses its own open file
    description, as a separate Cargo or worktree-new process would.
    """
    descriptor = os.open(path, os.O_RDWR | os.O_CREAT, 0o644)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        return True
    finally:
        os.close(descriptor)
    return False


class FakeExecutor:
    """Emulates the git/cargo/cp commands worktree-new runs.

    Both the commands the script runs and the name-based ones it must never
    run in rollback (`git worktree add -b`, `git branch -D`) are emulated, so
    a regression to them shows up as a wrong outcome rather than a crash.
    """

    def __init__(self, repo):
        self.common_dir = repo / ".git"
        self.commands = []
        # Branch name -> commit id.
        self.branches = {"main": BASE_COMMIT}
        # Commit-ish -> commit id.
        self.refs = {"origin/main": BASE_COMMIT, "HEAD": BASE_COMMIT}
        self.probe_fails = False
        self.copy_fails_for = None
        # How `git worktree add` fails: None (it succeeds), "before" (nothing
        # created), or "after-checkout" (a post-checkout hook failed on a
        # full worktree).
        self.add_fails = None
        # When True, `git branch <name> <commit>` fails without creating it.
        self.branch_fails = False
        # Registered worktree path -> checked-out branch name.
        self.registered = {}
        # Per-root override of `cargo metadata` target/build directories.
        self.layouts = {}
        # Called with (source, dest) before every emulated `cp`.
        self.on_copy = None
        # Called with the command before it is emulated; stands in for
        # another process acting between two of the script's commands.
        self.before_command = None
        # Called with the command after it is emulated.
        self.after_command = None

    def ran(self, *prefix):
        """True when some command starts with `prefix`."""
        return any(tuple(c[: len(prefix)]) == prefix for c in self.commands)

    def other_process_creates_worktree(self, path, branch):
        """Another process runs `git worktree add -b <branch> <path>`."""
        self.branches[branch] = OTHER_COMMIT
        path.mkdir(parents=True)
        (path / "uncommitted.txt").write_text("someone else's work")
        self.registered[path] = branch

    def __call__(self, command, cwd, check=True):
        command = [str(part) for part in command]
        if self.before_command is not None:
            self.before_command(command)
        self.commands.append(command)
        returncode, stdout, stderr = self.dispatch(command, Path(cwd))
        if self.after_command is not None:
            self.after_command(command)
        if check and returncode != 0:
            raise worktree_new.WorktreeError(
                f"command failed with exit {returncode}: "
                f"{' '.join(command)}: {stderr}"
            )
        return subprocess.CompletedProcess(command, returncode, stdout, stderr)

    def dispatch(self, command, cwd):
        tool, args = command[0], command[1:]
        if tool == "git":
            return self.git(args)
        if tool == "cargo" and args[0] == "metadata":
            # The key "temporary" matches any worktree still at its
            # temporary path, whose random name a test cannot know.
            key = ("temporary"
                   if cwd.name.startswith(TEMP_PREFIX) else cwd)
            target, build = self.layouts.get(
                key, (cwd / "target", cwd / "target")
            )
            payload = {
                "target_directory": str(target),
                "build_directory": str(build),
                "workspace_root": str(cwd),
            }
            return 0, json.dumps(payload), ""
        if tool == "cp":
            return self.cp(args)
        raise AssertionError(f"unexpected command {command}")

    def git(self, args):
        if args[:2] == ["rev-parse", "--path-format=absolute"]:
            return 0, f"{self.common_dir}\n", ""
        if args[0] == "check-ref-format":
            branch = args[-1]
            bad = ".." in branch or " " in branch or branch.endswith("/")
            return (1 if bad else 0), "", ""
        if args[0] == "show-ref":
            name = args[-1].removeprefix("refs/heads/")
            return (0 if name in self.branches else 1), "", ""
        if args[0] == "fetch":
            return 0, "", ""
        if args[:3] == ["rev-parse", "--verify", "--quiet"]:
            ref = args[3].removesuffix("^{commit}")
            if ref not in self.refs:
                return 1, "", ""
            return 0, f"{self.refs[ref]}\n", ""
        if args[:2] == ["branch", "--no-track"]:
            branch, commit = args[2], args[3]
            if self.branch_fails:
                return 128, "", "fatal: cannot lock ref"
            if branch in self.branches:
                return 128, "", f"fatal: a branch named '{branch}' already exists"
            self.branches[branch] = commit
            return 0, "", ""
        if args[:2] == ["update-ref", "-d"]:
            branch, expected = args[2].removeprefix("refs/heads/"), args[3]
            if self.branches.get(branch) != expected:
                return 1, "", f"error: cannot lock ref '{args[2]}'"
            del self.branches[branch]
            return 0, "", ""
        if args[:2] == ["worktree", "add"]:
            return self.worktree_add(args[2:])
        if args[:2] == ["worktree", "move"]:
            return self.worktree_move(Path(args[2]), Path(args[3]))
        if args[:2] == ["worktree", "list"]:
            lines = [f"worktree {self.common_dir.parent}\nbranch refs/heads/main"]
            lines += [f"worktree {path}\nbranch refs/heads/{branch}"
                      for path, branch in sorted(self.registered.items())]
            return 0, "\n\n".join(lines) + "\n", ""
        if args[:2] == ["worktree", "remove"]:
            path = Path(args[-1])
            if path not in self.registered:
                return 128, "", f"fatal: '{path}' is not a working tree"
            shutil.rmtree(path)
            del self.registered[path]
            return 0, "", ""
        if args[:2] == ["branch", "-D"]:
            if args[2] not in self.branches:
                return 1, "", f"error: branch '{args[2]}' not found"
            del self.branches[args[2]]
            return 0, "", ""
        raise AssertionError(f"unexpected git {args}")

    def worktree_add(self, args):
        """`worktree add [-b <new-branch>] <path> <commit-ish>`."""
        new_branch = None
        if args[0] == "-b":
            new_branch, args = args[1], args[2:]
        path, start = Path(args[0]), args[1]
        if self.add_fails == "before":
            return 128, "", "fatal: invalid reference"
        if new_branch is not None:
            if new_branch in self.branches:
                return 128, "", (f"fatal: a branch named '{new_branch}' "
                                 "already exists")
        elif start not in self.branches:
            return 128, "", f"fatal: invalid reference: {start}"
        if path.exists():
            return 128, "", f"fatal: '{path}' already exists"
        if new_branch is not None:
            self.branches[new_branch] = self.refs[start]
        branch = new_branch or start
        path.mkdir()
        (path / "Cargo.toml").write_text("[workspace]\n")
        self.registered[path] = branch
        if self.add_fails == "after-checkout":
            return 1, "", "error: post-checkout hook failed"
        return 0, "", ""

    def worktree_move(self, source, dest):
        """git 2.55 semantics, checked on a scratch repository: an existing
        directory (a symlink to one included) receives the worktree under
        its own name, any other existing entry fails the move."""
        if source not in self.registered:
            return 128, "", f"fatal: '{source}' is not a working tree"
        if dest.is_dir():
            dest = dest / source.name
        if dest.exists() or dest.is_symlink():
            return 128, "", f"fatal: '{dest}' already exists"
        source.rename(dest)
        self.registered[dest] = self.registered.pop(source)
        return 0, "", ""

    def temporary_worktrees(self):
        """Registered worktrees that still carry a temporary name."""
        return [path for path in self.registered
                if path.name.startswith(TEMP_PREFIX)]

    def cp(self, args):
        assert args[:2] == ["-a", "--reflink=always"], args
        source, dest = Path(args[2]), Path(args[3])
        if self.on_copy is not None:
            self.on_copy(source, dest)
        if self.probe_fails:
            return 1, "", CROSS_DEVICE
        if self.copy_fails_for is not None and source.name == self.copy_fails_for:
            return 1, "", "cp: No space left on device"
        if source.is_dir():
            shutil.copytree(source, dest, symlinks=True)
        else:
            shutil.copy2(source, dest)
        return 0, "", ""


def make_main_target(repo):
    """A main checkout target dir holding a debug profile cache."""
    target = repo / "target"
    debug = target / "debug"
    for name in (".fingerprint/dep-1", "deps", "build/dep-1/out",
                 "incremental/ws-1", "examples"):
        (debug / name).mkdir(parents=True)
    (debug / "deps/libdep-1.rlib").write_bytes(b"rlib")
    (debug / "build/dep-1/root-output").write_text(str(debug / "build/dep-1/out"))
    (debug / "pohunek-sessiond").write_bytes(b"main-branch worker")
    (debug / "libpohunek_daemon.rlib").write_bytes(b"uplifted")
    (debug / "examples/demo").write_bytes(b"example")
    for lock in worktree_new.CARGO_LOCK_FILES:
        (debug / lock).write_bytes(b"")
    (target / "CACHEDIR.TAG").write_text("Signature: 8a477f597d28d172789f06886806bc55\n")
    (target / ".rustc_info.json").write_text("{}")
    return target


class Harness:
    """A temporary main checkout plus a fake executor bound to it."""

    def __init__(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.repo = self.root / "pohunek"
        (self.repo / ".git").mkdir(parents=True)
        self.target = make_main_target(self.repo)
        self.worktrees = self.root / worktree_new.WORKTREES_DIR_NAME
        self.executor = FakeExecutor(self.repo)

    def cleanup(self):
        self._tmp.cleanup()

    def run(self, *argv):
        """Run main(); return (exit code, stdout, stderr)."""
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = worktree_new.main(list(argv), cwd=self.repo,
                                     executor=self.executor)
        return code, out.getvalue(), err.getvalue()


class HarnessCase(unittest.TestCase):
    def setUp(self):
        self.h = Harness()
        self.addCleanup(self.h.cleanup)

    def assert_no_worktree_created(self):
        self.assertFalse(self.h.executor.ran("git", "worktree", "add"))
        self.assertFalse((self.h.worktrees / "issue-1").exists())


class SlugValidationTests(unittest.TestCase):
    def test_accepts_repository_style_slugs(self):
        for slug in ("issue-168", "pr112-review", "a_b.c", "X9"):
            with self.subTest(slug=slug):
                worktree_new.validate_slug(slug)

    def test_rejects_escaping_or_malformed_slugs(self):
        bad = ["", "../x", "a/b", "/abs", "-rf", ".hidden", "a..b", "a b",
               "x.lock", "trailing.", "tab\t", "ü", "a" * 101]
        for slug in bad:
            with self.subTest(slug=slug):
                with self.assertRaises(worktree_new.WorktreeError):
                    worktree_new.validate_slug(slug)

    def test_max_length_slug_is_accepted(self):
        worktree_new.validate_slug("a" * worktree_new.MAX_SLUG_LENGTH)


class ArgumentTests(HarnessCase):
    def parse_exit(self, *argv):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as caught:
                worktree_new.main(list(argv), cwd=self.h.repo,
                                  executor=self.h.executor)
        return caught.exception.code

    def test_missing_slug_is_a_usage_error(self):
        self.assertEqual(self.parse_exit(), 2)

    def test_extra_positional_is_a_usage_error(self):
        self.assertEqual(self.parse_exit("a", "origin/main", "extra"), 2)

    def test_unknown_flag_is_a_usage_error(self):
        self.assertEqual(self.parse_exit("--reflink-auto", "a"), 2)

    def test_branch_requires_a_value(self):
        self.assertEqual(self.parse_exit("a", "--branch"), 2)

    def test_invalid_slug_fails_before_any_command(self):
        code, _, err = self.h.run("../escape")
        self.assertEqual(code, 1)
        self.assertIn("invalid slug", err)
        self.assertEqual(self.h.executor.commands, [])

    def test_invalid_branch_override_is_rejected(self):
        code, _, err = self.h.run("--branch", "zajca/bad..name", "issue-1")
        self.assertEqual(code, 1)
        self.assertIn("invalid branch name", err)
        self.assert_no_worktree_created()

    def test_branch_override_starting_with_dash_is_rejected(self):
        code, _, err = self.h.run("--branch=-D", "issue-1")
        self.assertEqual(code, 1)
        self.assertIn("invalid branch name", err)
        self.assert_no_worktree_created()


class SeededCreateTests(HarnessCase):
    def test_default_run_fetches_and_seeds_the_worktree_target(self):
        code, out, err = self.h.run("issue-1")
        self.assertEqual(code, 0, err)
        worktree = self.h.worktrees / "issue-1"
        commands = self.h.executor.commands
        self.assertIn(["git", "fetch", "origin", "main"], commands)
        self.assertIn(["git", "branch", "--no-track", "zajca/issue-1",
                       BASE_COMMIT], commands)
        adds = [c for c in commands if c[1:3] == ["worktree", "add"]]
        self.assertEqual(len(adds), 1)
        temp = adds[0][3]
        self.assertEqual(adds[0][4:], ["zajca/issue-1"])
        self.assertIn(["git", "worktree", "move", temp, str(worktree)],
                      commands)
        self.assertEqual(self.h.executor.registered,
                         {worktree: "zajca/issue-1"})
        debug = worktree / "target" / "debug"
        for name in worktree_new.SEED_SUBDIRS:
            self.assertTrue((debug / name).is_dir(), name)
        self.assertTrue((debug / "deps/libdep-1.rlib").is_file())
        self.assertTrue((worktree / "target/CACHEDIR.TAG").is_file())
        self.assertTrue((worktree / "target/.rustc_info.json").is_file())
        self.assertIn(f"cd {worktree} && cargo build", out)

    def test_uplifted_main_outputs_are_not_seeded(self):
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 0, err)
        debug = self.h.worktrees / "issue-1" / "target" / "debug"
        self.assertFalse((debug / "pohunek-sessiond").exists())
        self.assertFalse((debug / "libpohunek_daemon.rlib").exists())
        self.assertFalse((debug / "examples").exists())
        for lock in worktree_new.CARGO_LOCK_FILES:
            self.assertFalse((debug / lock).exists(), lock)

    def test_every_copy_is_a_mandatory_reflink(self):
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 0, err)
        copies = [c for c in self.h.executor.commands if c[0] == "cp"]
        self.assertTrue(copies)
        for command in copies:
            self.assertEqual(command[1:3], ["-a", "--reflink=always"])

    def test_probe_and_staging_leave_no_residue(self):
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 0, err)
        self.assertFalse((self.h.worktrees / worktree_new.PROBE_NAME).exists())
        target = self.h.worktrees / "issue-1" / "target"
        self.assertEqual(
            sorted(p.name for p in target.iterdir()),
            [".rustc_info.json", "CACHEDIR.TAG", "debug"],
        )

    def test_explicit_base_ref_is_used_without_fetch(self):
        self.h.executor.refs["zajca/base"] = OTHER_COMMIT
        code, _, err = self.h.run("issue-1", "zajca/base")
        self.assertEqual(code, 0, err)
        self.assertFalse(self.h.executor.ran("git", "fetch"))
        self.assertEqual(self.h.executor.branches["zajca/issue-1"],
                         OTHER_COMMIT)

    def test_branch_override_is_used(self):
        code, _, err = self.h.run("--branch", "zajca/issue-1/review", "issue-1")
        self.assertEqual(code, 0, err)
        self.assertIn("zajca/issue-1/review", self.h.executor.branches)

    def test_main_target_is_left_untouched(self):
        before = sorted(str(p.relative_to(self.h.target))
                        for p in self.h.target.rglob("*"))
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 0, err)
        after = sorted(str(p.relative_to(self.h.target))
                       for p in self.h.target.rglob("*"))
        self.assertEqual(before, after)


class FailClosedTests(HarnessCase):
    def test_unsupported_reflink_fails_without_fallback(self):
        self.h.executor.probe_fails = True
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("reflink is not supported", err)
        self.assertIn("--no-seed", err)
        self.assertIn("Invalid cross-device link", err)
        self.assert_no_worktree_created()
        copies = [c for c in self.h.executor.commands if c[0] == "cp"]
        self.assertEqual(len(copies), 1, "only the probe may run")
        self.assertFalse((self.h.worktrees / worktree_new.PROBE_NAME).exists())

    def test_missing_source_profile_fails(self):
        shutil.rmtree(self.h.target / "debug")
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("no seed source", err)
        self.assertIn("--no-seed", err)
        self.assert_no_worktree_created()

    def test_source_without_fingerprints_fails(self):
        shutil.rmtree(self.h.target / "debug" / ".fingerprint")
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("not a Cargo profile dir", err)
        self.assert_no_worktree_created()

    def test_source_without_cachedir_tag_fails(self):
        (self.h.target / "CACHEDIR.TAG").unlink()
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("not a Cargo target dir", err)
        self.assert_no_worktree_created()

    def test_existing_destination_fails(self):
        (self.h.worktrees / "issue-1").mkdir(parents=True)
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("already exists", err)
        self.assertFalse(self.h.executor.ran("git", "worktree", "add"))

    def test_existing_destination_symlink_fails(self):
        self.h.worktrees.mkdir()
        (self.h.worktrees / "issue-1").symlink_to(self.h.root / "missing")
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("already exists", err)
        self.assertFalse(self.h.executor.ran("git", "worktree", "add"))

    def test_existing_branch_fails(self):
        self.h.executor.branches["zajca/issue-1"] = OTHER_COMMIT
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("branch zajca/issue-1 already exists", err)
        self.assert_no_worktree_created()

    def test_unresolvable_base_ref_fails(self):
        code, _, err = self.h.run("issue-1", "no-such-ref")
        self.assertEqual(code, 1)
        self.assertIn("does not name a commit", err)
        self.assert_no_worktree_created()

    def test_non_default_source_layout_fails(self):
        self.h.executor.layouts[self.h.repo] = (
            self.h.root / "shared-target", self.h.root / "shared-target")
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("CARGO_TARGET_DIR", err)
        self.assert_no_worktree_created()

    def test_separate_build_dir_fails(self):
        self.h.executor.layouts[self.h.repo] = (
            self.h.repo / "target", self.h.root / "build-dir")
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("build_directory", err)
        self.assert_no_worktree_created()

    def test_running_cargo_build_in_main_blocks_the_seed(self):
        lock = self.h.target / "debug" / ".cargo-build-lock"
        with open(lock, "rb") as held:
            fcntl.flock(held.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("a Cargo process holds", err)
        self.assert_no_worktree_created()

    def test_lock_file_absent_at_start_is_created_and_held_during_seed(self):
        debug = self.h.target / "debug"
        for lock in worktree_new.CARGO_LOCK_FILES:
            (debug / lock).unlink()
        observed = {}

        def probe_locks(source, dest):
            for lock in worktree_new.CARGO_LOCK_FILES:
                observed.setdefault(lock, []).append(
                    flock_is_blocked(debug / lock))

        self.h.executor.on_copy = probe_locks
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 0, err)
        self.assertEqual(set(observed), set(worktree_new.CARGO_LOCK_FILES))
        for lock, blocked in observed.items():
            self.assertTrue(all(blocked), f"{lock} was not held: {blocked}")
        for lock in worktree_new.CARGO_LOCK_FILES:
            self.assertFalse(flock_is_blocked(debug / lock), lock)

    def test_symlinked_cargo_lock_file_is_refused(self):
        outside = self.h.root / "outside"
        lock = self.h.target / "debug" / ".cargo-lock"
        lock.unlink()
        lock.symlink_to(outside)
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn(".cargo-lock", err)
        self.assertFalse(outside.exists())
        self.assert_no_worktree_created()

    def test_held_repository_lock_fails_before_any_check(self):
        lock = self.h.repo / ".git" / worktree_new.REPO_LOCK_NAME
        with open(lock, "wb") as held:
            fcntl.flock(held.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn(f"another worktree-new run holds {lock}", err)
        self.assertEqual(
            self.h.executor.commands,
            [["git", "rev-parse", "--path-format=absolute",
              "--git-common-dir"]],
        )

    def assert_symlink_refused(self, err):
        self.assertIn("is a symlink", err)
        self.assert_no_worktree_created()
        self.assertFalse(self.h.executor.ran("cp"))
        self.assertFalse(self.h.executor.ran("git", "branch"))

    def test_symlinked_seed_subdir_fails_closed(self):
        deps = self.h.target / "debug" / "deps"
        outside = self.h.root / "outside-deps"
        deps.rename(outside)
        deps.symlink_to(outside)
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assert_symlink_refused(err)
        self.assertIn(str(deps), err)

    def test_symlinked_profile_dir_fails_closed(self):
        debug = self.h.target / "debug"
        outside = self.h.root / "outside-debug"
        debug.rename(outside)
        debug.symlink_to(outside)
        for lock in worktree_new.CARGO_LOCK_FILES:
            (outside / lock).unlink()
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assert_symlink_refused(err)
        for lock in worktree_new.CARGO_LOCK_FILES:
            self.assertFalse((outside / lock).exists(), lock)

    def test_symlinked_cachedir_tag_fails_closed(self):
        tag = self.h.target / "CACHEDIR.TAG"
        outside = self.h.root / "outside-tag"
        tag.rename(outside)
        tag.symlink_to(outside)
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assert_symlink_refused(err)
        self.assertIn(str(tag), err)

    def test_bare_common_dir_is_rejected(self):
        self.h.executor.common_dir = self.h.root / "bare.git"
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("non-bare main checkout", err)


COMPARE_AND_DELETE = ["git", "update-ref", "-d", "refs/heads/zajca/issue-1",
                      BASE_COMMIT]


class RollbackTests(HarnessCase):
    def assert_nothing_left(self):
        self.assertFalse((self.h.worktrees / "issue-1").exists())
        self.assertNotIn("zajca/issue-1", self.h.executor.branches)
        self.assertEqual(self.h.executor.registered, {})
        self.assertEqual(sorted(p.name for p in self.h.worktrees.iterdir()),
                         [])
        self.assertFalse(self.h.executor.ran("git", "branch", "-D"))

    def test_copy_failure_removes_worktree_and_branch(self):
        self.h.executor.copy_fails_for = "deps"
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("seeding failed", err)
        self.assertIn("rolled back", err)
        self.assertNotIn("manually", err)
        self.assert_nothing_left()
        self.assertIn(COMPARE_AND_DELETE, self.h.executor.commands)

    def test_worktree_with_overridden_layout_is_rolled_back(self):
        self.h.executor.layouts["temporary"] = (
            self.h.root / "elsewhere", self.h.root / "elsewhere")
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("rolled back", err)
        self.assert_nothing_left()

    def test_checked_out_target_symlink_fails_the_seed(self):
        outside = self.h.root / "outside-target"
        outside.mkdir()

        def plant_target(command):
            # The branch's checkout carries its own `target` symlink.
            if command[:2] == ["cargo", "metadata"]:
                for temp in self.h.executor.temporary_worktrees():
                    if not (temp / "target").is_symlink():
                        (temp / "target").symlink_to(outside)

        self.h.executor.before_command = plant_target
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("seeding failed", err)
        self.assertEqual(list(outside.iterdir()), [])
        self.assert_nothing_left()


class WorktreeAddFailureTests(HarnessCase):
    def run_failing_add(self, mode, *argv, step="git worktree add"):
        self.h.executor.add_fails = mode
        code, _, err = self.h.run(*argv, "issue-1")
        self.assertEqual(code, 1)
        self.assertIn(f"{step} failed", err)
        self.assertNotIn("manually", err)
        self.assertFalse((self.h.worktrees / "issue-1").exists())
        self.assertNotIn("zajca/issue-1", self.h.executor.branches)
        self.assertEqual(self.h.executor.registered, {})
        self.assertFalse(self.h.executor.ran("git", "branch", "-D"))
        return err

    def test_branch_is_deleted_when_the_add_creates_nothing(self):
        self.run_failing_add("before")
        self.assertIn(COMPARE_AND_DELETE, self.h.executor.commands)
        self.assertFalse(self.h.executor.ran("git", "worktree", "remove"))

    def test_worktree_left_by_failed_hook_is_removed(self):
        err = self.run_failing_add("after-checkout")
        self.assertIn("post-checkout hook failed", err)
        removes = [c for c in self.h.executor.commands
                   if c[1:3] == ["worktree", "remove"]]
        self.assertEqual(len(removes), 1)
        self.assertTrue(Path(removes[0][-1]).name.startswith(TEMP_PREFIX))
        self.assertIn(COMPARE_AND_DELETE, self.h.executor.commands)

    def test_failure_before_creation_rolls_back_nothing(self):
        self.h.executor.branch_fails = True
        err = self.run_failing_add(None, step="git branch")
        self.assertIn("nothing to roll back", err)
        self.assertFalse(self.h.executor.ran("git", "update-ref"))
        self.assertFalse(self.h.executor.ran("git", "worktree", "remove"))
        self.assertFalse(self.h.executor.ran("git", "worktree", "add"))

    def test_no_seed_failure_is_rolled_back_too(self):
        self.run_failing_add("after-checkout", "--no-seed")

    def test_unregistered_leftover_dir_is_reported_not_deleted(self):
        worktree = self.h.worktrees / "issue-1"
        worktree.mkdir(parents=True)
        (worktree / "keep").write_text("unknown owner")
        removed, problems = worktree_new.rollback(
            self.h.repo, (worktree,), "zajca/issue-1", None, self.h.executor)
        self.assertEqual(removed, [])
        self.assertEqual(len(problems), 1)
        self.assertIn("not a registered worktree", problems[0])
        self.assertTrue((worktree / "keep").exists())

    def test_branch_checked_out_elsewhere_is_left_in_place(self):
        other = self.h.worktrees / "someone-else"
        self.h.executor.branches["zajca/issue-1"] = BASE_COMMIT
        self.h.executor.registered[other] = "zajca/issue-1"
        other.mkdir(parents=True)
        removed, problems = worktree_new.rollback(
            self.h.repo, (), "zajca/issue-1", BASE_COMMIT, self.h.executor)
        self.assertEqual(removed, [])
        self.assertIn("left branch zajca/issue-1 in place: it is checked out",
                      problems[0])
        self.assertEqual(self.h.executor.branches["zajca/issue-1"],
                         BASE_COMMIT)
        self.assertFalse(self.h.executor.ran("git", "update-ref"))

    def test_retry_after_rollback_succeeds(self):
        self.run_failing_add("after-checkout")
        self.h.executor.add_fails = None
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 0, err)


class PlainGitRaceTests(HarnessCase):
    """Another process runs plain `git` between the script's commands.

    The repository lock does not bind it, so rollback must prove ownership
    rather than infer it from the destination and branch names.
    """

    def act_once_before(self, predicate, action):
        """Run `action` once, just before the first command matching it."""
        fired = []

        def hook(command):
            if not fired and predicate(command):
                fired.append(command)
                action()

        self.h.executor.before_command = hook
        return fired

    def assert_other_worktree_untouched(self, worktree):
        self.assertEqual(self.h.executor.registered.get(worktree),
                         "zajca/issue-1")
        self.assertEqual((worktree / "uncommitted.txt").read_text(),
                         "someone else's work")
        self.assertEqual(self.h.executor.branches["zajca/issue-1"],
                         OTHER_COMMIT)
        self.assertFalse(self.h.executor.ran("git", "branch", "-D"))
        self.assertEqual(self.h.executor.temporary_worktrees(), [])

    def test_worktree_and_branch_created_before_ours_are_left_alone(self):
        worktree = self.h.worktrees / "issue-1"
        fired = self.act_once_before(
            lambda c: c[1:3] in (["branch", "--no-track"], ["worktree", "add"]),
            lambda: self.h.executor.other_process_creates_worktree(
                worktree, "zajca/issue-1"),
        )
        code, _, err = self.h.run("issue-1")
        self.assertTrue(fired)
        self.assertEqual(code, 1)
        self.assertIn("already exists", err)
        self.assertIn("nothing to roll back", err)
        self.assert_other_worktree_untouched(worktree)
        self.assertFalse(self.h.executor.ran("git", "worktree", "remove"))
        self.assertFalse(self.h.executor.ran("git", "update-ref"))

    def test_destination_created_before_the_move_is_left_alone(self):
        worktree = self.h.worktrees / "issue-1"

        def create_other(command):
            # A different branch: git never checks one branch out twice.
            if command[1:3] == ["worktree", "add"]:
                self.h.executor.other_process_creates_worktree(
                    worktree, "zajca/other")

        self.h.executor.after_command = create_other
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("git worktree move failed", err)
        self.assertIn("appeared during the run", err)
        self.assertFalse(self.h.executor.ran("git", "worktree", "move"))
        self.assertIn("rolled back", err)
        self.assertEqual(self.h.executor.registered, {worktree: "zajca/other"})
        self.assertEqual(sorted(p.name for p in worktree.iterdir()),
                         ["uncommitted.txt"])
        self.assertNotIn("zajca/issue-1", self.h.executor.branches)

    def test_directory_appearing_inside_the_move_window_is_left_alone(self):
        worktree = self.h.worktrees / "issue-1"
        real_move = self.h.executor.worktree_move

        def racing_move(source, dest):
            # The directory appears after the script's last check, as git
            # itself runs: git then nests the worktree inside it.
            dest.mkdir()
            (dest / "keep").write_text("someone else's dir")
            return real_move(source, dest)

        self.h.executor.worktree_move = racing_move
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("git moved the worktree inside it", err)
        self.assertIn("rolled back", err)
        self.assertNotIn("manually", err)
        self.assertEqual(sorted(p.name for p in worktree.iterdir()), ["keep"])
        self.assertEqual(self.h.executor.registered, {})
        self.assertNotIn("zajca/issue-1", self.h.executor.branches)

    def test_branch_moved_after_creation_is_not_deleted(self):
        def move_branch():
            self.h.executor.branches["zajca/issue-1"] = OTHER_COMMIT

        self.act_once_before(lambda c: c[1:3] == ["worktree", "add"],
                             move_branch)
        self.h.executor.add_fails = "before"
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("left branch zajca/issue-1 in place: it changed after "
                      "creation", err)
        self.assertEqual(self.h.executor.branches["zajca/issue-1"],
                         OTHER_COMMIT)
        self.assertIn(COMPARE_AND_DELETE, self.h.executor.commands)
        self.assertFalse(self.h.executor.ran("git", "branch", "-D"))


class TemporaryPathTests(HarnessCase):
    def test_prefix_matches_the_script(self):
        self.assertEqual(TEMP_PREFIX, worktree_new.TEMP_WORKTREE_PREFIX)

    def test_success_leaves_only_the_final_worktree(self):
        seen = []
        self.h.executor.on_copy = lambda source, dest: seen.append(dest)
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 0, err)
        worktree = self.h.worktrees / "issue-1"
        self.assertEqual(self.h.executor.registered,
                         {worktree: "zajca/issue-1"})
        self.assertEqual(sorted(p.name for p in self.h.worktrees.iterdir()),
                         ["issue-1"])
        temp_name = f"{TEMP_PREFIX}issue-1-{os.getpid()}-"
        seeded = [d for d in seen
                  if any(p.name.startswith(temp_name) for p in d.parents)]
        self.assertTrue(seeded, "the seed must land in the temporary path")

    def test_temporary_names_are_unique(self):
        names = {worktree_new.temporary_worktree_path(self.h.worktrees, "a")
                 for _ in range(2)}
        self.assertEqual(len(names), 2)

    def test_failure_leaves_no_temporary_worktree(self):
        self.h.executor.copy_fails_for = "incremental"
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertEqual(self.h.executor.temporary_worktrees(), [])
        self.assertEqual(self.h.executor.registered, {})
        self.assertEqual(list(self.h.worktrees.iterdir()), [])


class NoSeedTests(HarnessCase):
    def test_no_seed_skips_cargo_and_copies(self):
        code, out, err = self.h.run("--no-seed", "issue-1")
        self.assertEqual(code, 0, err)
        self.assertFalse(self.h.executor.ran("cp"))
        self.assertFalse(self.h.executor.ran("cargo"))
        self.assertTrue(self.h.executor.ran("git", "worktree", "add"))
        self.assertFalse((self.h.worktrees / "issue-1" / "target").exists())
        self.assertIn("not seeded", out)

    def test_no_seed_works_without_a_source_or_reflink(self):
        shutil.rmtree(self.h.target)
        self.h.executor.probe_fails = True
        code, _, err = self.h.run("--no-seed", "issue-1")
        self.assertEqual(code, 0, err)


class ConcurrencyTests(HarnessCase):
    def test_concurrent_run_for_the_same_slug_never_touches_the_winner(self):
        # Run A stops inside `git fetch`, after its existence checks and
        # before its `git worktree add`; run B starts in that window.
        a_in_fetch, release_a = threading.Event(), threading.Event()
        shared = self.h.executor

        def gated(command, cwd, check=True):
            if [str(part) for part in command[:2]] == ["git", "fetch"]:
                a_in_fetch.set()
                release_a.wait(HANDSHAKE_TIMEOUT_SECONDS)
            return shared(command, cwd, check)

        outcome = {}

        def run_a():
            try:
                outcome["a"] = worktree_new.main(
                    ["issue-1"], cwd=self.h.repo, executor=gated)
            finally:
                a_in_fetch.set()

        with contextlib.redirect_stdout(io.StringIO()), \
                contextlib.redirect_stderr(io.StringIO()):
            thread_a = threading.Thread(target=run_a)
            thread_a.start()
            self.assertTrue(a_in_fetch.wait(HANDSHAKE_TIMEOUT_SECONDS))
            args = worktree_new.build_parser().parse_args(["issue-1"])
            try:
                worktree_new.create(args, self.h.repo, shared, io.StringIO())
            except worktree_new.WorktreeError as error:
                outcome["b"] = str(error)
            else:
                outcome["b"] = "succeeded"
            release_a.set()
            thread_a.join(HANDSHAKE_TIMEOUT_SECONDS)
        self.assertFalse(thread_a.is_alive())

        lock = self.h.repo / ".git" / worktree_new.REPO_LOCK_NAME
        self.assertIn(f"another worktree-new run holds {lock}", outcome["b"])
        self.assertEqual(outcome["a"], 0)
        self.assertFalse(shared.ran("git", "worktree", "remove"))
        self.assertFalse(shared.ran("git", "branch", "-D"))
        adds = [c for c in shared.commands if c[1:3] == ["worktree", "add"]]
        self.assertEqual(len(adds), 1)
        worktree = self.h.worktrees / "issue-1"
        self.assertIn(worktree, shared.registered)
        self.assertIn("zajca/issue-1", shared.branches)
        self.assertTrue((worktree / "target" / "debug" / "deps").is_dir())


class LockTests(unittest.TestCase):
    def test_missing_lock_files_are_created_and_held(self):
        with tempfile.TemporaryDirectory() as tmp:
            with worktree_new.hold_cargo_locks(Path(tmp)):
                for lock in worktree_new.CARGO_LOCK_FILES:
                    self.assertTrue(flock_is_blocked(Path(tmp) / lock), lock)
            for lock in worktree_new.CARGO_LOCK_FILES:
                self.assertFalse(flock_is_blocked(Path(tmp) / lock), lock)

    def test_missing_profile_dir_is_a_seed_source_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(worktree_new.WorktreeError) as caught:
                with worktree_new.hold_cargo_locks(Path(tmp) / "debug"):
                    pass
            self.assertIn("no seed source", str(caught.exception))
            self.assertFalse((Path(tmp) / "debug").exists())

    def test_repository_lock_is_exclusive_and_released(self):
        with tempfile.TemporaryDirectory() as tmp:
            lock = Path(tmp) / worktree_new.REPO_LOCK_NAME
            with worktree_new.hold_repo_lock(Path(tmp)) as held:
                self.assertEqual(held, lock)
                self.assertTrue(flock_is_blocked(lock))
                with self.assertRaises(worktree_new.WorktreeError) as caught:
                    with worktree_new.hold_repo_lock(Path(tmp)):
                        pass
                self.assertIn("another worktree-new run holds",
                              str(caught.exception))
            self.assertFalse(flock_is_blocked(lock))

    def test_locks_are_held_exclusively_and_released(self):
        with tempfile.TemporaryDirectory() as tmp:
            lock = Path(tmp) / ".cargo-lock"
            lock.write_bytes(b"")
            with worktree_new.hold_cargo_locks(Path(tmp)):
                with open(lock, "rb") as other:
                    with self.assertRaises(BlockingIOError):
                        fcntl.flock(other.fileno(),
                                    fcntl.LOCK_SH | fcntl.LOCK_NB)
            with open(lock, "rb") as other:
                fcntl.flock(other.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)


if __name__ == "__main__":
    unittest.main()
