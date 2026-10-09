"""Regression checks for the worktree-new helper (stdlib only).

Most tests inject an executor that emulates git, cargo, and cp in a temporary
directory. The CLI tests run the real script against a private Git repository;
seed-source validation also runs real cargo metadata with a private CARGO_HOME.
No test needs btrfs: seed-copy scenarios use emulated cp or fail before copying.
Cargo and repository lock contention uses real flock on temporary files.
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
import sys
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
CLI_TIMEOUT_SECONDS = 30
# Commit ids of the fake repository: the base every new branch starts at,
# and a commit only other processes' refs point to.
BASE_COMMIT = "1" * 40
OTHER_COMMIT = "2" * 40
# Name prefix of a worktree that is still at its temporary path.
TEMP_PREFIX = ".worktree-new-"
# Name prefix of this run's temporary branch for the slug `issue-1`.
TEMP_BRANCH = "zajca/worktree-new-tmp-issue-1-"
# Committer time of the last `Cargo.lock` commit on the fake base, and the
# mtime the tests give the seed profile's build activity directories: the
# seed is fresh by default, stale when the lockfile commit is newer.
OLD_EPOCH = 1_000_000_000
NEW_EPOCH = 1_700_000_000
# A time far beyond what `time.gmtime` renders.
OUT_OF_RANGE_EPOCH = 10 ** 18
# A predictable name in the worktree parent that belongs to someone else;
# the reflink probe must never touch it.
FOREIGN_PROBE_NAME = ".worktree-new-reflink-probe"


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
        # stdout of `git log -1 --first-parent --format=%ct <base> -- Cargo.lock`; empty
        # when no commit touches the lockfile.
        self.lockfile_log = f"{OLD_EPOCH}\n"
        # stdout of `git rev-parse <base>:Cargo.lock` and of
        # `git hash-object -- Cargo.lock`; equal by default.
        self.base_blob = f"{'a' * 40}\n"
        self.main_blob = f"{'a' * 40}\n"
        self.base_blob_fails = False
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
        if args[:3] == ["rev-parse", "--verify", "--quiet"] \
                and args[3].endswith(":Cargo.lock"):
            if self.base_blob_fails:
                return 1, "", ""
            return 0, self.base_blob, ""
        if args[:3] == ["hash-object", "--", "Cargo.lock"]:
            return 0, self.main_blob, ""
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
        if args[:2] == ["branch", "-m"]:
            old, new = args[2], args[3]
            if old not in self.branches:
                return 128, "", f"error: refname {old} not found"
            if new in self.branches:
                return 128, "", f"fatal: a branch named '{new}' already exists"
            self.branches[new] = self.branches.pop(old)
            for path, branch in self.registered.items():
                if branch == old:
                    self.registered[path] = new
            return 0, "", ""
        if args[:2] == ["update-ref", "-d"]:
            branch, expected = args[2].removeprefix("refs/heads/"), args[3]
            if self.branches.get(branch) != expected:
                return 1, "", f"error: cannot lock ref '{args[2]}'"
            del self.branches[branch]
            return 0, "", ""
        if args[:4] == ["log", "-1", "--first-parent", "--format=%ct"]:
            assert args[5:] == ["--", "Cargo.lock"], args
            return 0, self.lockfile_log, ""
        if args[:2] == ["worktree", "add"]:
            return self.worktree_add(args[2:])
        if args[:2] == ["worktree", "move"]:
            return self.worktree_move(Path(args[2]), Path(args[3]))
        if args[:2] == ["worktree", "list"]:
            assert args[2:] == ["--porcelain", "-z"], args
            return 0, self.porcelain_z(), ""
        if args[:2] == ["worktree", "remove"]:
            path = Path(args[-1])
            if path not in self.registered:
                return 128, "", f"fatal: '{path}' is not a working tree"
            if "--force" not in args and self.untracked_files(path):
                return 128, "", (f"fatal: '{path}' contains modified or "
                                 "untracked files, use --force to delete it")
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

    @staticmethod
    def untracked_files(worktree):
        """Files git would count as untracked: the emulated checkout tracks
        only `Cargo.toml` and ignores `target/`."""
        return [p for p in worktree.rglob("*")
                if p.is_file() and p.name != "Cargo.toml"
                and "target" not in p.relative_to(worktree).parts]

    def porcelain_z(self):
        """`git worktree list --porcelain -z` as git 2.55 prints it: each
        attribute ends with NUL and each record with one more NUL."""
        records = [(self.common_dir.parent, "main")]
        records += sorted(self.registered.items())
        out = ""
        for path, branch in records:
            out += f"worktree {path}\0HEAD {BASE_COMMIT}\0"
            out += f"branch refs/heads/{branch}\0" if branch else "detached\0"
            out += "\0"
        return out

    def temporary_branches(self):
        """Branches that still carry a temporary name."""
        return [b for b in self.branches if b.startswith(TEMP_BRANCH)]

    def ref_deletions(self):
        """Every `git update-ref -d` the script ran."""
        return [c for c in self.commands if c[1:3] == ["update-ref", "-d"]]

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
    set_build_time(target, NEW_EPOCH + 1000)
    return target


def set_build_time(target, epoch):
    """Give the profile's build activity directories the mtime `epoch`."""
    for name in worktree_new.BUILD_ACTIVITY_DIRS:
        os.utime(target / worktree_new.PROFILE / name, (epoch, epoch))


# Minimal Cargo source for the real-CLI seed-source fixtures: a virtual
# workspace manifest with no members, enough for `cargo metadata` to
# succeed offline and report the repo's default `<root>/target` layout.
CLI_FIXTURE_MANIFEST = """\
[workspace]
"""


def make_cli_seed_source(repo):
    """Write a `Cargo.toml` and build the `make_main_target` seed shape.

    The commit is the caller's: it writes only the virtual-workspace
    manifest, whose `cargo metadata` succeeds offline with the default
    `<root>/target` layout, and a `make_main_target`-shaped target dir
    that gives `check_seed_source` everything it validates. Everything
    lives inside the caller's temporary root, so no operator Cargo state
    is read or written.
    """
    (repo / "Cargo.toml").write_text(CLI_FIXTURE_MANIFEST)
    return make_main_target(repo)


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


class WorktreeCliFixture:
    """Private real Git repository and script runner for `worktree-new`.

    Non-TestCase fixture base shared by the CLI test classes: the repo, a
    hermetic environment, and the helpers that run real `git` and the real
    script. It defines no test methods, so inheriting it never duplicates
    another class's scenarios.
    """

    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="worktree-new-cli-")
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.repo = self.root / "pohunek"
        self.repo.mkdir()
        self.env = dict(os.environ)
        self.env.update({
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_SYSTEM": os.devnull,
            "GIT_AUTHOR_NAME": "fixture",
            "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
            "GIT_COMMITTER_NAME": "fixture",
            "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
            "GIT_CONFIG_COUNT": "1",
            "GIT_CONFIG_KEY_0": "commit.gpgsign",
            "GIT_CONFIG_VALUE_0": "false",
        })
        self.git("init", "-q", "-b", "main")
        (self.repo / "README.md").write_text("fixture\n")
        self.git("add", "README.md")
        self.git("commit", "-q", "-m", "fixture")
        self.base = self.git("rev-parse", "HEAD").stdout.strip()
        self.worktrees = self.root / "pohunek-worktrees"

    def git(self, *args):
        result = subprocess.run(
            ["git", *args], cwd=self.repo, env=self.env,
            capture_output=True, text=True, check=False,
            timeout=CLI_TIMEOUT_SECONDS,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def run_script(self, *args):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *args], cwd=self.repo, env=self.env,
            capture_output=True, text=True, check=False,
            timeout=CLI_TIMEOUT_SECONDS,
        )


class WorktreeCliTests(WorktreeCliFixture, unittest.TestCase):
    """Exercise argument validation through the real script and a private Git repo."""

    def test_valid_slugs_create_branches_and_registered_worktrees(self):
        for slug in ("issue-168", "pr112-review", "a_b.c", "X9", "a" * 100):
            with self.subTest(slug=slug):
                result = self.run_script("--no-seed", slug, "HEAD")
                self.assertEqual(result.returncode, 0, result.stderr)
                destination = self.worktrees / slug
                self.assertTrue(destination.is_dir())
                self.assertFalse((destination / "target").exists())
                self.assertIn("not seeded (--no-seed)", result.stdout)
                self.assertEqual(
                    subprocess.run(
                        ["git", "-C", str(destination), "rev-parse", "HEAD"],
                        env=self.env, capture_output=True, text=True, check=True,
                    ).stdout.strip(),
                    self.base,
                )
                self.assertIn(f"branch zajca/{slug} from HEAD", result.stdout)
                self.assertIn(str(destination), self.git("worktree", "list", "--porcelain").stdout)

    def test_invalid_slugs_create_no_branch_or_worktree(self):
        bad = ("", "../x", "a/b", "/abs", "-rf", ".hidden", "a..b",
               "a b", "x.lock", "trailing.", "tab\t", "ü", "a" * 101)
        for slug in bad:
            with self.subTest(slug=slug):
                result = self.run_script("--no-seed", "--", slug, "HEAD")
                self.assertNotEqual(result.returncode, 0)
                if not slug:
                    expected = "must not be empty"
                elif len(slug) > 100:
                    expected = "longer than 100 characters"
                else:
                    expected = "invalid slug"
                self.assertIn(expected, result.stderr)
                self.assertFalse(self.worktrees.exists())
                self.assertEqual(self.git("branch", "--list", "zajca/*").stdout, "")

    def test_malformed_command_lines_are_usage_errors(self):
        for args in ((), ("a", "HEAD", "extra"), ("--reflink-auto", "a"),
                     ("a", "--branch")):
            with self.subTest(args=args):
                result = self.run_script(*args)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertFalse(self.worktrees.exists())

    def test_invalid_branch_overrides_create_no_worktree(self):
        for branch in ("zajca/bad..name", "-D"):
            with self.subTest(branch=branch):
                result = self.run_script("--no-seed", f"--branch={branch}", "issue-1", "HEAD")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("invalid branch name", result.stderr)
                self.assertFalse(self.worktrees.exists())
                self.assertEqual(self.git("branch", "--list", "zajca/*").stdout, "")

    def test_existing_destination_directory_is_not_modified(self):
        destination = self.worktrees / "issue-1"
        destination.mkdir(parents=True)
        sentinel = destination / "owned.txt"
        sentinel.write_text("keep this file\n")

        result = self.run_script("--no-seed", "issue-1", "HEAD")

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("already exists", result.stderr)
        self.assertEqual(sentinel.read_text(), "keep this file\n")
        self.assertEqual(self.git("branch", "--list", "zajca/issue-1").stdout, "")
        self.assertNotIn(str(destination), self.git("worktree", "list", "--porcelain").stdout)

    def test_existing_destination_symlink_is_not_followed(self):
        self.worktrees.mkdir()
        destination = self.worktrees / "issue-1"
        destination.symlink_to(self.root / "missing")

        result = self.run_script("--no-seed", "issue-1", "HEAD")

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("already exists", result.stderr)
        self.assertTrue(destination.is_symlink())
        self.assertEqual(self.git("branch", "--list", "zajca/issue-1").stdout, "")
        self.assertNotIn(str(destination), self.git("worktree", "list", "--porcelain").stdout)

    def test_existing_branch_is_not_replaced(self):
        self.git("branch", "zajca/issue-1", "HEAD")
        (self.repo / "README.md").write_text("new main commit\n")
        self.git("add", "README.md")
        self.git("commit", "-q", "-m", "advance main")
        self.assertNotEqual(self.git("rev-parse", "HEAD").stdout.strip(), self.base)

        result = self.run_script("--no-seed", "issue-1", "HEAD")

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("branch zajca/issue-1 already exists", result.stderr)
        self.assertEqual(self.git("rev-parse", "refs/heads/zajca/issue-1").stdout.strip(), self.base)
        self.assertFalse((self.worktrees / "issue-1").exists())

    def test_unresolvable_base_ref_creates_no_branch_or_worktree(self):
        result = self.run_script("--no-seed", "issue-1", "no-such-ref")

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("does not name a commit", result.stderr)
        self.assertEqual(self.git("branch", "--list", "zajca/issue-1").stdout, "")
        self.assertFalse((self.worktrees / "issue-1").exists())

    def test_conflicting_seed_flags_are_a_usage_error(self):
        result = self.run_script("--force-seed", "--no-seed", "issue-1", "HEAD")

        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("not allowed with", result.stderr)
        # The argument check happens before any effect is made.
        self.assertFalse(self.worktrees.exists())
        self.assertEqual(self.git("branch", "--list", "zajca/*").stdout, "")

    def test_held_repository_lock_refuses_a_second_process(self):
        lock = self.repo / ".git" / worktree_new.REPO_LOCK_NAME
        with open(lock, "wb") as held:
            fcntl.flock(held.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.run_script("--no-seed", "issue-1", "HEAD")

        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(f"another worktree-new run holds {lock}", result.stderr)
        self.assertFalse(self.worktrees.exists())
        self.assertEqual(self.git("branch", "--list", "zajca/*").stdout, "")

    def install_shims(self, record):
        """Record the script's git invocations through `PATH`.

        The git shim logs the command words, then execs the real git, so
        the run still operates on the private repository for real. The
        cargo shim answers the layout question the script needs, so the
        seed path short-circuits on its own lockfile probe without a real
        Cargo toolchain.
        """
        shim = self.root / "shim"
        shim.mkdir()
        shim_git = shim / "git"
        shim_git.write_text(
            "#!/bin/sh\n"
            f"printf '%s\\n' \"$*\" >> \"{record}\"\n"
            f"exec \"{shutil.which('git')}\" \"$@\"\n"
        )
        shim_git.chmod(0o755)
        shim_cargo = shim / "cargo"
        shim_cargo.write_text(
            "#!/bin/sh\n"
            "printf '{\"target_directory\": \"%s/target\"}\\n' \"$(pwd)\"\n"
        )
        shim_cargo.chmod(0o755)
        self.env["PATH"] = f"{shim}{os.pathsep}{self.env['PATH']}"
        return record

    def test_no_seed_runs_no_lockfile_history_question(self):
        """Against a stale seed setup, only the default seeding runs git log."""
        (self.repo / "Cargo.lock").write_text("lockfile = \"only a mark\"\n")
        self.git("add", "Cargo.lock")
        self.git("commit", "-q", "-m", "lock")
        target = self.repo / "target"
        target.mkdir()
        (target / worktree_new.CACHEDIR_TAG).write_text("cachedir\n")
        for name in worktree_new.BUILD_ACTIVITY_DIRS:
            (target / worktree_new.PROFILE / name).mkdir(parents=True)
            os.utime(target / worktree_new.PROFILE / name,
                     (NEW_EPOCH, NEW_EPOCH))
        record = self.install_shims(self.root / "git-commands.log")

        default = self.run_script("issue-1", "HEAD")

        # The stale seed is refused before any copy, so the run completes
        # without a seeding filesystem, and the probe is readable in the log.
        self.assertEqual(default.returncode, 0, default.stderr)
        self.assertIn("not seeded (stale seed", default.stdout)
        self.assertNotIn("not seeded (--no-seed)", default.stdout)
        entries = record.read_text().splitlines()
        self.assertTrue(entries, "the shim must see at least one git call")
        self.assertTrue(any(
            command.split()[0] == "log" for command in entries), entries)

        record.unlink()

        unseeded = self.run_script("--no-seed", "issue-2", "HEAD")

        self.assertEqual(unseeded.returncode, 0, unseeded.stderr)
        self.assertIn("not seeded (--no-seed)", unseeded.stdout)
        self.assertTrue((self.worktrees / "issue-2").is_dir())
        entries = record.read_text().splitlines()
        self.assertTrue(entries, "the shim must see every git call")
        self.assertFalse(any(
            command.split()[0] == "log" for command in entries), entries)

    def test_two_runs_for_the_same_slug_get_distinct_temporary_names(self):
        record = self.root / "post-checkout-record"
        hook = self.repo / ".git" / "hooks" / "post-checkout"
        hook.write_text(
            "#!/bin/sh\n"
            # `git worktree add` runs this in the new worktree: record the
            # temporary path and the temporary branch it checks out.
            "printf '%s\\t%s\\n' \"$(git rev-parse --show-toplevel)\""
            " \"$(git symbolic-ref --short HEAD 2>/dev/null || true)\""
            f" >> \"{record}\"\n"
        )
        hook.chmod(0o755)

        first = self.run_script("--no-seed", "issue-1", "HEAD")
        self.assertEqual(first.returncode, 0, first.stderr)
        first_temp, first_branch = self.recorded_temp_name(record, index=0)
        self.git("worktree", "remove", str(self.worktrees / "issue-1"))
        self.git("branch", "-D", "zajca/issue-1")

        second = self.run_script("--no-seed", "issue-1", "HEAD")
        self.assertEqual(second.returncode, 0, second.stderr)
        second_temp, second_branch = self.recorded_temp_name(record, index=1)

        self.assertNotEqual(first_temp, second_temp)
        self.assertNotEqual(first_branch, second_branch)
        for temp, branch in ((first_temp, first_branch),
                             (second_temp, second_branch)):
            # One token names both the path and the branch.
            self.assertEqual(
                temp.name.removeprefix(worktree_new.TEMP_WORKTREE_PREFIX),
                branch.removeprefix(
                    worktree_new.BRANCH_PREFIX + worktree_new.TEMP_BRANCH_PREFIX),
            )
            self.assertTrue(temp.name.startswith(
                f"{worktree_new.TEMP_WORKTREE_PREFIX}issue-1-"), temp)
        # The second run ends with the destination state the first one had.
        self.assertTrue((self.worktrees / "issue-1").is_dir())
        self.assertEqual(
            self.git("branch", "--list", "zajca/worktree-new-tmp-*").stdout, "")

    def recorded_temp_name(self, record, index):
        lines = record.read_text().splitlines()
        self.assertEqual(len(lines), index + 1, lines)
        path, branch = lines[index].split("\t", 1)
        return Path(path), branch


class SeedSourceValidationCliTests(WorktreeCliFixture, unittest.TestCase):
    """Seed-source validation and stale-skip checks through the real script.

    Each case runs the real `scripts/worktree-new` against the private
    real Git repository with real `cargo metadata`, breaks exactly one
    condition in the main checkout's target dir, and must fail before any
    cp, worktree, or branch exists — so the scenarios never need reflink
    support, and no rollback or destination cleanup is ever involved.
    """

    def setUp(self):
        super().setUp()
        # Real `cargo metadata` must run hermetic: a private CARGO_HOME so
        # no operator config is read, offline mode, and no environment
        # override that would move the reported target/build directories
        # away from `<repo>/target`.
        self.cargo_home = self.root / "cargo-home"
        self.cargo_home.mkdir()
        self.env["CARGO_HOME"] = str(self.cargo_home)
        self.env["CARGO_NET_OFFLINE"] = "true"
        for name in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR",
                     "CARGO_BUILD_BUILD_DIR"):
            self.env.pop(name, None)

    def prepare_seedable_repo(self):
        """A committed minimal Cargo manifest plus the seed shape, so the
        default seeding run reaches source validation with real cargo."""
        target = make_cli_seed_source(self.repo)
        self.git("add", "Cargo.toml")
        self.git("commit", "-q", "-m", "cargo fixture")
        return target

    def assert_refused_at_source_validation(self, result, *error_texts):
        """Exit 1 with the expected error and recovery hint, and nothing
        created: no destination, no branch under the prefix, and the
        worktrees root, which the script makes before the checks, empty —
        probe, staging, and worktree names all come only after them."""
        self.assertEqual(result.returncode, 1, result.stderr)
        for text in error_texts:
            self.assertIn(text, result.stderr)
        self.assertTrue(self.worktrees.is_dir())
        self.assertEqual(
            sorted(p.name for p in self.worktrees.iterdir()), [])
        self.assertEqual(self.git("branch", "--list", "zajca/*").stdout, "")

    def assert_refused_without_creation(self, result, *error_texts):
        self.assertEqual(result.returncode, 1, result.stderr)
        for text in error_texts:
            self.assertIn(text, result.stderr)
        if self.worktrees.exists():
            self.assertEqual(list(self.worktrees.iterdir()), [])
        self.assertEqual(self.git("branch", "--list", "zajca/*").stdout, "")

    def test_missing_source_profile_fails(self):
        target = self.prepare_seedable_repo()
        shutil.rmtree(target / worktree_new.PROFILE)

        result = self.run_script("issue-1", "HEAD")

        self.assert_refused_at_source_validation(
            result, "no seed source", "--no-seed")

    def test_source_without_fingerprints_fails(self):
        target = self.prepare_seedable_repo()
        shutil.rmtree(target / worktree_new.PROFILE / ".fingerprint")

        result = self.run_script("issue-1", "HEAD")

        self.assert_refused_at_source_validation(
            result, "no seed source", "not a Cargo profile dir", "--no-seed")

    def test_source_without_cachedir_tag_fails(self):
        target = self.prepare_seedable_repo()
        (target / worktree_new.CACHEDIR_TAG).unlink()

        result = self.run_script("issue-1", "HEAD")

        self.assert_refused_at_source_validation(
            result, "no seed source", "not a Cargo target dir", "--no-seed")

    def test_non_default_target_directory_fails_before_creation(self):
        self.prepare_seedable_repo()
        self.env["CARGO_TARGET_DIR"] = str(self.root / "shared-target")

        result = self.run_script("issue-1", "HEAD")

        self.assert_refused_without_creation(result, "CARGO_TARGET_DIR", "--no-seed")

    def test_missing_activity_directory_fails_before_copy(self):
        target = self.prepare_seedable_repo()
        shutil.rmtree(target / worktree_new.PROFILE / "deps")

        result = self.run_script("issue-1", "HEAD")

        self.assert_refused_without_creation(result, "no seed source", "--no-seed")

    def test_symlinked_profile_directory_is_refused_without_touching_target(self):
        target = self.prepare_seedable_repo()
        profile = target / worktree_new.PROFILE
        outside = self.root / "outside-profile"
        profile.rename(outside)
        profile.symlink_to(outside)
        for name in worktree_new.CARGO_LOCK_FILES:
            (outside / name).unlink()

        result = self.run_script("issue-1", "HEAD")

        self.assert_refused_without_creation(result, "is a symlink", str(profile))
        for name in worktree_new.CARGO_LOCK_FILES:
            self.assertFalse((outside / name).exists())

    def test_symlinked_seed_subdirectory_is_refused(self):
        target = self.prepare_seedable_repo()
        deps = target / worktree_new.PROFILE / "deps"
        outside = self.root / "outside-deps"
        deps.rename(outside)
        deps.symlink_to(outside)

        result = self.run_script("issue-1", "HEAD")

        self.assert_refused_without_creation(result, "is a symlink", str(deps))

    def test_symlinked_cachedir_tag_is_refused(self):
        target = self.prepare_seedable_repo()
        tag = target / worktree_new.CACHEDIR_TAG
        outside = self.root / "outside-tag"
        tag.rename(outside)
        tag.symlink_to(outside)

        result = self.run_script("issue-1", "HEAD")

        self.assert_refused_without_creation(result, "is a symlink", str(tag))

    def test_nested_seed_file_symlink_is_refused(self):
        target = self.prepare_seedable_repo()
        output = target / worktree_new.PROFILE / "build" / "dep-1" / "out"
        outside = self.root / "outside-file"
        outside.write_text("fixture content")
        link = output / "generated.rs"
        link.symlink_to(outside)

        result = self.run_script("issue-1", "HEAD")

        self.assert_refused_without_creation(result, "is a symlink", str(link))
        self.assertEqual(outside.read_text(), "fixture content")

    def test_nested_seed_directory_symlink_is_refused(self):
        target = self.prepare_seedable_repo()
        link = target / worktree_new.PROFILE / ".fingerprint" / "dep-1" / "linked"
        link.symlink_to(self.root)

        result = self.run_script("issue-1", "HEAD")

        self.assert_refused_without_creation(result, "is a symlink", str(link))

    def test_running_cargo_build_blocks_seeding_in_another_process(self):
        target = self.prepare_seedable_repo()
        lock = target / worktree_new.PROFILE / ".cargo-build-lock"
        with open(lock, "rb") as held:
            fcntl.flock(held.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.run_script("issue-1", "HEAD")

        self.assert_refused_without_creation(result, "a Cargo process holds")

    def test_symlinked_cargo_lock_is_refused_without_touching_target(self):
        target = self.prepare_seedable_repo()
        lock = target / worktree_new.PROFILE / ".cargo-lock"
        outside = self.root / "outside-lock"
        lock.unlink()
        lock.symlink_to(outside)

        result = self.run_script("issue-1", "HEAD")

        self.assert_refused_without_creation(result, ".cargo-lock")
        self.assertFalse(outside.exists())

    def prepare_stale_seed(self):
        target = self.prepare_seedable_repo()
        (self.repo / "Cargo.lock").write_text("version = 3\n")
        self.git("add", "Cargo.lock")
        self.env["GIT_AUTHOR_DATE"] = f"@{NEW_EPOCH} +0000"
        self.env["GIT_COMMITTER_DATE"] = f"@{NEW_EPOCH} +0000"
        try:
            self.git("commit", "-q", "-m", "lockfile fixture")
        finally:
            self.env.pop("GIT_AUTHOR_DATE")
            self.env.pop("GIT_COMMITTER_DATE")
        return target

    def test_stale_seed_skips_copy_and_creates_no_cargo_lock_files(self):
        target = self.prepare_stale_seed()
        set_build_time(target, OLD_EPOCH)
        for name in worktree_new.CARGO_LOCK_FILES:
            (target / worktree_new.PROFILE / name).unlink()

        result = self.run_script("issue-1", "HEAD")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("not seeded (stale seed", result.stdout)
        self.assertIn(worktree_new.format_epoch(NEW_EPOCH), result.stdout)
        self.assertIn(worktree_new.format_epoch(OLD_EPOCH), result.stdout)
        self.assertIn("--force-seed", result.stdout)
        destination = self.worktrees / "issue-1"
        self.assertTrue(destination.is_dir())
        self.assertFalse((destination / "target").exists())
        self.assertIn(str(destination), self.git("worktree", "list", "--porcelain").stdout)
        for name in worktree_new.CARGO_LOCK_FILES:
            self.assertFalse((target / worktree_new.PROFILE / name).exists())
        self.assertEqual(self.git("branch", "--list", "zajca/worktree-new-tmp-*").stdout, "")

    def test_different_lockfile_blob_skips_a_time_fresh_seed(self):
        target = self.prepare_stale_seed()
        set_build_time(target, NEW_EPOCH + 100)
        (self.repo / "Cargo.lock").write_text("version = 4\n")

        result = self.run_script("issue-1", "HEAD")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("not seeded (stale seed: Cargo.lock on HEAD differs", result.stdout)
        self.assertIn("--force-seed", result.stdout)
        destination = self.worktrees / "issue-1"
        self.assertTrue(destination.is_dir())
        self.assertFalse((destination / "target").exists())
        self.assertIn(str(destination), self.git("worktree", "list", "--porcelain").stdout)


class SeededCreateTests(HarnessCase):
    def test_default_run_fetches_and_seeds_the_worktree_target(self):
        code, out, err = self.h.run("issue-1")
        self.assertEqual(code, 0, err)
        worktree = self.h.worktrees / "issue-1"
        commands = self.h.executor.commands
        self.assertIn(["git", "fetch", "origin", "main"], commands)
        creates = [c for c in commands if c[1:3] == ["branch", "--no-track"]]
        self.assertEqual(len(creates), 1)
        temp_branch = creates[0][3]
        self.assertTrue(temp_branch.startswith(TEMP_BRANCH), temp_branch)
        self.assertEqual(creates[0][4:], [BASE_COMMIT])
        adds = [c for c in commands if c[1:3] == ["worktree", "add"]]
        self.assertEqual(len(adds), 1)
        temp = adds[0][3]
        self.assertEqual(adds[0][4:], [temp_branch])
        move = ["git", "worktree", "move", temp, str(worktree)]
        rename = ["git", "branch", "-m", temp_branch, "zajca/issue-1"]
        self.assertLess(commands.index(move), commands.index(rename))
        self.assertEqual(commands[-1], rename)
        self.assertEqual(self.h.executor.temporary_branches(), [])
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
        self.assertEqual(sorted(p.name for p in self.h.worktrees.iterdir()),
                         ["issue-1"])
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
        self.assertEqual(list(self.h.worktrees.iterdir()), [])

    def test_separate_build_dir_fails(self):
        self.h.executor.layouts[self.h.repo] = (
            self.h.repo / "target", self.h.root / "build-dir")
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("build_directory", err)
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

    def test_file_at_a_predictable_probe_path_is_never_touched(self):
        self.h.worktrees.mkdir()
        foreign = self.h.worktrees / FOREIGN_PROBE_NAME
        foreign.write_text("someone else's file")
        for probe_fails in (True, False):
            with self.subTest(probe_fails=probe_fails):
                self.h.executor.probe_fails = probe_fails
                slug = f"issue-{int(probe_fails)}"
                code, _, err = self.h.run(slug)
                self.assertEqual(code, 1 if probe_fails else 0, err)
                self.assertEqual(foreign.read_text(), "someone else's file")
        leftovers = [p.name for p in self.h.worktrees.iterdir()
                     if p.name.startswith(worktree_new.PROBE_DIR_PREFIX)]
        self.assertEqual(leftovers, [])

    def test_probe_clones_into_a_private_directory(self):
        seen = []

        def record(source, dest):
            if not seen:
                seen.append((dest, dest.parent.stat().st_mode & 0o777))

        self.h.executor.on_copy = record
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 0, err)
        dest, mode = seen[0]
        self.assertTrue(
            dest.parent.name.startswith(worktree_new.PROBE_DIR_PREFIX))
        self.assertEqual(dest.parent.parent, self.h.worktrees)
        self.assertEqual(mode, 0o700)
        self.assertFalse(dest.parent.exists())

    def test_bare_common_dir_is_rejected(self):
        self.h.executor.common_dir = self.h.root / "bare.git"
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("non-bare main checkout", err)




class RollbackCase(HarnessCase):
    def assert_temp_branch_compare_and_deleted(self):
        """Rollback deleted exactly one ref: this run's temporary branch,
        compared against the commit it was created at."""
        deletions = self.h.executor.ref_deletions()
        self.assertEqual(len(deletions), 1, deletions)
        ref, expected = deletions[0][3:]
        self.assertTrue(ref.startswith(f"refs/heads/{TEMP_BRANCH}"), ref)
        self.assertEqual(expected, BASE_COMMIT)
        self.assertEqual(self.h.executor.temporary_branches(), [])


class RollbackTests(RollbackCase):
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
        self.assert_temp_branch_compare_and_deleted()

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


class WorktreeAddFailureTests(RollbackCase):
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
        self.assert_temp_branch_compare_and_deleted()
        self.assertFalse(self.h.executor.ran("git", "worktree", "remove"))

    def test_worktree_left_by_failed_hook_is_removed(self):
        err = self.run_failing_add("after-checkout")
        self.assertIn("post-checkout hook failed", err)
        removes = [c for c in self.h.executor.commands
                   if c[1:3] == ["worktree", "remove"]]
        self.assertEqual(len(removes), 1)
        self.assertTrue(Path(removes[0][-1]).name.startswith(TEMP_PREFIX))
        self.assert_temp_branch_compare_and_deleted()

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

    def test_change_between_listing_and_ref_deletion_is_left_alone(self):
        # Between rollback's last `git worktree list` and its ref deletion,
        # another process deletes and recreates the final branch name at
        # the same commit and checks it out in its own worktree.
        other = self.h.worktrees / "someone-else"

        def recreate_branch(command):
            if command[1:3] == ["update-ref", "-d"]:
                self.h.executor.branches.pop("zajca/issue-1", None)
                self.h.executor.branches["zajca/issue-1"] = BASE_COMMIT
                other.mkdir(parents=True, exist_ok=True)
                self.h.executor.registered[other] = "zajca/issue-1"

        self.h.executor.before_command = recreate_branch
        self.h.executor.copy_fails_for = "deps"
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("seeding failed", err)
        self.assertEqual(self.h.executor.branches.get("zajca/issue-1"),
                         BASE_COMMIT)
        self.assertEqual(self.h.executor.registered,
                         {other: "zajca/issue-1"})
        self.assert_temp_branch_compare_and_deleted()

    def after_add(self, action):
        """Run `action(temp path)` right after `git worktree add`."""
        def hook(command):
            if command[1:3] == ["worktree", "add"]:
                action(Path(command[3]))
        self.h.executor.after_command = hook

    def test_refused_plain_removal_keeps_files_and_branch(self):
        # Something wrote an untracked file into the temporary worktree:
        # the plain remove refuses, and nothing is deleted.
        self.after_add(lambda temp: (temp / "notes.txt").write_text("work"))
        self.h.executor.copy_fails_for = "deps"
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        (temp,) = self.h.executor.temporary_worktrees()
        (temp_branch,) = self.h.executor.temporary_branches()
        self.assertEqual((temp / "notes.txt").read_text(), "work")
        self.assertEqual(self.h.executor.registered, {temp: temp_branch})
        self.assertEqual(self.h.executor.branches[temp_branch], BASE_COMMIT)
        self.assertEqual(self.h.executor.ref_deletions(), [])
        self.assertIn(f"remove worktree {temp} manually (fatal: '{temp}' "
                      "contains modified or untracked files", err)
        self.assertIn(f"left branch {temp_branch} in place", err)

    def test_other_branch_at_the_temporary_path_is_left_alone(self):
        def swap_branch(temp):
            self.h.executor.branches["zajca/other"] = OTHER_COMMIT
            self.h.executor.registered[temp] = "zajca/other"

        self.after_add(swap_branch)
        self.h.executor.copy_fails_for = "deps"
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        (temp,) = self.h.executor.temporary_worktrees()
        (temp_branch,) = self.h.executor.temporary_branches()
        self.assertIn(f"left worktree {temp} in place: git lists it with "
                      f"refs/heads/zajca/other checked out, not {temp_branch}",
                      err)
        self.assertIn(f"left branch {temp_branch} in place", err)
        self.assertFalse(self.h.executor.ran("git", "worktree", "remove"))
        self.assertEqual(self.h.executor.ref_deletions(), [])
        self.assertEqual(self.h.executor.registered, {temp: "zajca/other"})
        self.assertTrue(temp.is_dir())

    def test_rollback_never_forces_a_removal(self):
        for mode in ("after-checkout", None):
            with self.subTest(mode=mode):
                self.h.executor.add_fails = mode
                self.h.executor.copy_fails_for = None if mode else "deps"
                code, _, err = self.h.run("issue-1")
                self.assertEqual(code, 1)
                self.assertIn("rolled back worktree", err)
        removes = [c for c in self.h.executor.commands
                   if c[1:3] == ["worktree", "remove"]]
        self.assertEqual(len(removes), 2)
        for command in self.h.executor.commands:
            self.assertNotIn("--force", command)

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
        self.assertIn(f"destination {worktree} appeared during the run", err)
        self.assertIn("rolled back", err)
        self.assert_other_worktree_untouched(worktree)
        self.assertEqual(self.h.executor.temporary_branches(), [])

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
            (temp_branch,) = self.h.executor.temporary_branches()
            self.h.executor.branches[temp_branch] = OTHER_COMMIT

        self.act_once_before(lambda c: c[1:3] == ["worktree", "add"],
                             move_branch)
        self.h.executor.add_fails = "before"
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("in place: it changed after creation", err)
        (temp_branch,) = self.h.executor.temporary_branches()
        self.assertEqual(self.h.executor.branches[temp_branch], OTHER_COMMIT)
        self.assertEqual(len(self.h.executor.ref_deletions()), 1)
        self.assertFalse(self.h.executor.ran("git", "branch", "-D"))

    def test_final_branch_taken_before_the_rename_keeps_the_worktree(self):
        worktree = self.h.worktrees / "issue-1"

        def take_branch():
            self.h.executor.branches["zajca/issue-1"] = OTHER_COMMIT

        self.act_once_before(lambda c: c[1:3] == ["branch", "-m"],
                             take_branch)
        code, out, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        (temp_branch,) = self.h.executor.temporary_branches()
        self.assertIn(f"created worktree {worktree} on the temporary branch "
                      f"{temp_branch}", err)
        self.assertIn("The worktree is kept", err)
        self.assertEqual(self.h.executor.registered, {worktree: temp_branch})
        self.assertTrue((worktree / "target" / "debug" / "deps").is_dir())
        self.assertEqual(self.h.executor.branches["zajca/issue-1"],
                         OTHER_COMMIT)
        self.assertEqual(self.h.executor.branches[temp_branch], BASE_COMMIT)
        self.assertFalse(self.h.executor.ran("git", "worktree", "remove"))
        self.assertEqual(self.h.executor.ref_deletions(), [])
        self.assertEqual(out, "")


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
        self.assertEqual(self.h.executor.temporary_branches(), [])
        temp_name = f"{TEMP_PREFIX}issue-1-{os.getpid()}-"
        seeded = [d for d in seen
                  if any(p.name.startswith(temp_name) for p in d.parents)]
        self.assertTrue(seeded, "the seed must land in the temporary path")

    def test_failure_leaves_no_temporary_worktree(self):
        self.h.executor.copy_fails_for = "incremental"
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertEqual(self.h.executor.temporary_worktrees(), [])
        self.assertEqual(self.h.executor.temporary_branches(), [])
        self.assertEqual(self.h.executor.registered, {})
        self.assertEqual(list(self.h.worktrees.iterdir()), [])



class StaleSeedTests(HarnessCase):
    def make_stale(self):
        """The lockfile changed on the base after the seed's last build."""
        set_build_time(self.h.target, OLD_EPOCH)
        self.h.executor.lockfile_log = f"{NEW_EPOCH}\n"

    def assert_seeded(self, code, out, err):
        self.assertEqual(code, 0, err)
        self.assertTrue(self.h.executor.ran("cp"))
        self.assertTrue(
            (self.h.worktrees / "issue-1/target/debug/deps").is_dir())
        self.assertIn("seeded ", out)
        self.assertNotIn("not seeded", out)

    def test_fresh_seed_is_seeded(self):
        self.assert_seeded(*self.h.run("issue-1"))
        self.assertTrue(self.h.executor.ran(
            "git", "log", "-1", "--first-parent", "--format=%ct", BASE_COMMIT,
            "--",
            "Cargo.lock"))

    def test_lockfile_commit_time_equal_to_build_time_is_fresh(self):
        set_build_time(self.h.target, NEW_EPOCH)
        self.h.executor.lockfile_log = f"{NEW_EPOCH}\n"
        self.assert_seeded(*self.h.run("issue-1"))

    def test_newest_of_the_two_activity_dirs_counts(self):
        self.make_stale()
        os.utime(self.h.target / "debug/deps", (NEW_EPOCH + 5, NEW_EPOCH + 5))
        self.assert_seeded(*self.h.run("issue-1"))

    def test_unknown_staleness_still_seeds(self):
        self.make_stale()
        for output in ("", "not-a-number\n"):
            with self.subTest(output=output):
                self.setUp()
                self.make_stale()
                self.h.executor.lockfile_log = output
                self.assert_seeded(*self.h.run("issue-1"))

    def test_force_seed_overrides_a_different_lockfile_blob(self):
        self.h.executor.base_blob = f"{'b' * 40}\n"
        self.assert_seeded(*self.h.run("--force-seed", "issue-1"))

    def test_unreadable_lockfile_blob_keeps_the_seed(self):
        cases = {
            "base rev-parse fails": {"base_blob_fails": True},
            "base output empty": {"base_blob": ""},
            "base output odd": {"base_blob": "not-an-id\n"},
            "main output empty": {"main_blob": ""},
            "main output odd": {"main_blob": "xyz\n"},
        }
        for label, settings in cases.items():
            with self.subTest(label):
                self.setUp()
                self.h.executor.base_blob = f"{'b' * 40}\n"
                for name, value in settings.items():
                    setattr(self.h.executor, name, value)
                self.assert_seeded(*self.h.run("issue-1"))

    def test_out_of_range_lockfile_time_keeps_the_seed(self):
        set_build_time(self.h.target, OLD_EPOCH)
        self.h.executor.lockfile_log = f"{OUT_OF_RANGE_EPOCH}\n"
        self.assert_seeded(*self.h.run("issue-1"))

    def test_out_of_range_build_time_keeps_the_seed(self):
        self.h.executor.lockfile_log = f"{NEW_EPOCH}\n"
        for name in worktree_new.BUILD_ACTIVITY_DIRS:
            path = self.h.target / worktree_new.PROFILE / name
            try:
                os.utime(path, (OUT_OF_RANGE_EPOCH, OUT_OF_RANGE_EPOCH))
            except (OSError, OverflowError) as error:
                self.skipTest(f"platform rejects the mtime: {error}")
        self.assert_seeded(*self.h.run("issue-1"))

    def test_force_seed_seeds_a_stale_seed(self):
        self.make_stale()
        code, out, err = self.h.run("--force-seed", "issue-1")
        self.assert_seeded(code, out, err)
        self.assertFalse(self.h.executor.ran("git", "log"))


class LockfileCommitTimeRealGitTests(unittest.TestCase):
    """`lockfile_commit_time` and `lockfile_differs` against a real
    repository."""

    T0, T1, T3 = 1_600_000_000, 1_600_001_000, 1_600_009_000

    def git(self, repo, *args, when):
        env = dict(os.environ, GIT_COMMITTER_DATE=f"{when} +0000",
                   GIT_AUTHOR_DATE=f"{when} +0000",
                   GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_SYSTEM=os.devnull)
        command = ["git", "-c", "commit.gpgsign=false",
                   "-c", "user.name=Test", "-c", "user.email=test@example.com",
                   *args]
        return subprocess.run(command, cwd=repo, env=env, check=True,
                              capture_output=True, text=True).stdout.strip()

    def test_merged_lockfile_change_counts_from_the_merge_commit(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            repo.mkdir()
            lock = repo / "Cargo.lock"
            self.git(repo, "init", "-q", "-b", "main", when=self.T0)
            lock.write_text("v0\n")
            self.git(repo, "add", "Cargo.lock", when=self.T0)
            self.git(repo, "commit", "-q", "-m", "initial", when=self.T0)
            self.git(repo, "checkout", "-q", "-b", "feature", when=self.T1)
            lock.write_text("v1\n")
            self.git(repo, "commit", "-q", "-am", "lock", when=self.T1)
            self.git(repo, "checkout", "-q", "main", when=self.T1)
            (repo / "other.txt").write_text("x\n")
            self.git(repo, "add", "other.txt", when=self.T1)
            self.git(repo, "commit", "-q", "-m", "other", when=self.T1)
            self.git(repo, "merge", "-q", "--no-ff", "-m", "merge", "feature",
                     when=self.T3)
            tip = self.git(repo, "rev-parse", "HEAD", when=self.T3)
            self.assertEqual(
                worktree_new.lockfile_commit_time(
                    tip, repo, worktree_new.execute), self.T3)

    def test_divergent_branch_lockfile_differs_from_the_main_checkout(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            repo.mkdir()
            lock = repo / "Cargo.lock"
            self.git(repo, "init", "-q", "-b", "main", when=self.T0)
            lock.write_text("A\n")
            self.git(repo, "add", "Cargo.lock", when=self.T0)
            self.git(repo, "commit", "-q", "-m", "initial", when=self.T0)
            main_tip = self.git(repo, "rev-parse", "HEAD", when=self.T0)
            self.git(repo, "checkout", "-q", "-b", "other", when=self.T0)
            lock.write_text("B\n")
            self.git(repo, "commit", "-q", "-am", "lock B", when=self.T0)
            other_tip = self.git(repo, "rev-parse", "HEAD", when=self.T0)
            self.git(repo, "checkout", "-q", "main", when=self.T0)
            self.assertEqual(lock.read_text(), "A\n")
            self.assertIs(worktree_new.lockfile_differs(
                other_tip, repo, worktree_new.execute), True)
            self.assertIs(worktree_new.lockfile_differs(
                main_tip, repo, worktree_new.execute), False)

    def test_missing_lockfile_says_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            repo.mkdir()
            self.git(repo, "init", "-q", "-b", "main", when=self.T0)
            (repo / "x").write_text("x\n")
            self.git(repo, "add", "x", when=self.T0)
            self.git(repo, "commit", "-q", "-m", "initial", when=self.T0)
            tip = self.git(repo, "rev-parse", "HEAD", when=self.T0)
            self.assertIsNone(worktree_new.lockfile_differs(
                tip, repo, worktree_new.execute))


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


class WorktreeListTests(HarnessCase):
    def test_path_with_a_newline_is_parsed_intact(self):
        odd = self.h.worktrees / "odd\nworktree issue-1"
        self.h.executor.registered[odd] = "zajca/odd"
        self.h.executor.registered[self.h.worktrees / "detached"] = None
        listed = worktree_new.list_worktrees(self.h.repo, self.h.executor)
        self.assertEqual(listed[odd.resolve()], "refs/heads/zajca/odd")
        self.assertIsNone(listed[(self.h.worktrees / "detached").resolve()])
        self.assertNotIn((self.h.worktrees / "odd").resolve(), listed)
        self.assertTrue(worktree_new.worktree_registered(
            odd, self.h.repo, self.h.executor))
        self.assertFalse(worktree_new.worktree_registered(
            self.h.worktrees / "issue-1", self.h.repo, self.h.executor))

    def test_real_execute_keeps_carriage_returns_and_newlines(self):
        # The running interpreter is the only external program used.
        result = worktree_new.execute(
            [sys.executable, "-c",
             "import sys; sys.stdout.buffer.write(b'a\\rb\\nc\\0')"],
            cwd=self.h.root)
        self.assertEqual(result.stdout, "a\rb\nc\0")


class LockTests(unittest.TestCase):
    def test_missing_profile_dir_is_a_seed_source_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(worktree_new.WorktreeError) as caught:
                with worktree_new.hold_cargo_locks(Path(tmp) / "debug"):
                    pass
            self.assertIn("no seed source", str(caught.exception))
            self.assertFalse((Path(tmp) / "debug").exists())

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
