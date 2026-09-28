"""Regression checks for the worktree-new helper (stdlib only).

No test reaches a real git, cargo, or cp subprocess and none needs btrfs:
the executor is injected everywhere and emulates those commands inside a
temporary directory. Cargo lock contention is exercised with real `flock`
calls on temporary files.
"""

import contextlib
import fcntl
import importlib.machinery
import importlib.util
import io
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "worktree-new"
LOADER = importlib.machinery.SourceFileLoader("worktree_new", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
worktree_new = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(worktree_new)

# Error text of GNU cp when FICLONE crosses filesystems (tmpfs, other btrfs).
CROSS_DEVICE = "cp: failed to clone: Invalid cross-device link"


class FakeExecutor:
    """Emulates the git/cargo/cp commands worktree-new runs."""

    def __init__(self, repo):
        self.common_dir = repo / ".git"
        self.commands = []
        self.branches = {"main"}
        self.refs = {"origin/main", "HEAD"}
        self.probe_fails = False
        self.copy_fails_for = None
        # How `git worktree add` fails: None (it succeeds), "before" (nothing
        # created), "after-branch" (branch created, checkout failed), or
        # "after-checkout" (a post-checkout hook failed on a full worktree).
        self.add_fails = None
        self.registered = set()
        # Per-root override of `cargo metadata` target/build directories.
        self.layouts = {}

    def ran(self, *prefix):
        """True when some command starts with `prefix`."""
        return any(tuple(c[: len(prefix)]) == prefix for c in self.commands)

    def __call__(self, command, cwd, check=True):
        command = [str(part) for part in command]
        self.commands.append(command)
        returncode, stdout, stderr = self.dispatch(command, Path(cwd))
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
            target, build = self.layouts.get(
                cwd, (cwd / "target", cwd / "target")
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
            return (0 if ref in self.refs else 1), "", ""
        if args[:2] == ["worktree", "add"]:
            branch, path = args[3], Path(args[4])
            if self.add_fails == "before":
                return 128, "", "fatal: invalid reference"
            self.branches.add(branch)
            if self.add_fails == "after-branch":
                return 128, "", "fatal: could not create work tree dir"
            path.mkdir()
            (path / "Cargo.toml").write_text("[workspace]\n")
            self.registered.add(path)
            if self.add_fails == "after-checkout":
                return 1, "", "error: post-checkout hook failed"
            return 0, "", ""
        if args[:2] == ["worktree", "list"]:
            lines = [f"worktree {self.common_dir.parent}"]
            lines += [f"worktree {path}" for path in sorted(self.registered)]
            return 0, "\n\n".join(lines) + "\n", ""
        if args[:2] == ["worktree", "remove"]:
            path = Path(args[-1])
            if path not in self.registered:
                return 128, "", f"fatal: '{path}' is not a working tree"
            shutil.rmtree(path)
            self.registered.discard(path)
            return 0, "", ""
        if args[:2] == ["branch", "-D"]:
            if args[2] not in self.branches:
                return 1, "", f"error: branch '{args[2]}' not found"
            self.branches.discard(args[2])
            return 0, "", ""
        raise AssertionError(f"unexpected git {args}")

    def cp(self, args):
        assert args[:2] == ["-a", "--reflink=always"], args
        source, dest = Path(args[2]), Path(args[3])
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
        self.repo.mkdir()
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
        self.assertIn(["git", "fetch", "origin", "main"], self.h.executor.commands)
        self.assertIn(
            ["git", "worktree", "add", "-b", "zajca/issue-1", str(worktree),
             "origin/main"],
            self.h.executor.commands,
        )
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
        self.h.executor.refs.add("zajca/base")
        code, _, err = self.h.run("issue-1", "zajca/base")
        self.assertEqual(code, 0, err)
        self.assertFalse(self.h.executor.ran("git", "fetch"))
        self.assertEqual(
            [c for c in self.h.executor.commands if c[1:3] == ["worktree", "add"]][0][-1],
            "zajca/base",
        )

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
        self.h.executor.branches.add("zajca/issue-1")
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

    def test_bare_common_dir_is_rejected(self):
        self.h.executor.common_dir = self.h.root / "bare.git"
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("non-bare main checkout", err)


class RollbackTests(HarnessCase):
    def test_copy_failure_removes_worktree_and_branch(self):
        self.h.executor.copy_fails_for = "deps"
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("seeding failed", err)
        self.assertIn("rolled back", err)
        self.assertFalse((self.h.worktrees / "issue-1").exists())
        self.assertNotIn("zajca/issue-1", self.h.executor.branches)
        self.assertIn(["git", "branch", "-D", "zajca/issue-1"],
                      self.h.executor.commands)

    def test_worktree_with_overridden_layout_is_rolled_back(self):
        worktree = self.h.worktrees / "issue-1"
        self.h.executor.layouts[worktree] = (
            self.h.root / "elsewhere", self.h.root / "elsewhere")
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 1)
        self.assertIn("rolled back", err)
        self.assertFalse(worktree.exists())
        self.assertNotIn("zajca/issue-1", self.h.executor.branches)


class WorktreeAddFailureTests(HarnessCase):
    def run_failing_add(self, mode, *argv):
        self.h.executor.add_fails = mode
        code, _, err = self.h.run(*argv, "issue-1")
        self.assertEqual(code, 1)
        self.assertIn("git worktree add failed", err)
        self.assertNotIn("manually", err)
        self.assertFalse((self.h.worktrees / "issue-1").exists())
        self.assertNotIn("zajca/issue-1", self.h.executor.branches)
        self.assertEqual(self.h.executor.registered, set())
        return err

    def test_branch_left_by_failed_checkout_is_deleted(self):
        self.run_failing_add("after-branch")
        self.assertIn(["git", "branch", "-D", "zajca/issue-1"],
                      self.h.executor.commands)
        self.assertFalse(self.h.executor.ran("git", "worktree", "remove"))

    def test_worktree_left_by_failed_hook_is_removed(self):
        err = self.run_failing_add("after-checkout")
        self.assertIn("post-checkout hook failed", err)
        self.assertTrue(self.h.executor.ran("git", "worktree", "remove"))
        self.assertIn(["git", "branch", "-D", "zajca/issue-1"],
                      self.h.executor.commands)

    def test_failure_before_creation_rolls_back_nothing(self):
        err = self.run_failing_add("before")
        self.assertIn("nothing to roll back", err)
        self.assertFalse(self.h.executor.ran("git", "branch", "-D"))
        self.assertFalse(self.h.executor.ran("git", "worktree", "remove"))

    def test_no_seed_failure_is_rolled_back_too(self):
        self.run_failing_add("after-checkout", "--no-seed")

    def test_unregistered_leftover_dir_is_reported_not_deleted(self):
        worktree = self.h.worktrees / "issue-1"
        worktree.mkdir(parents=True)
        (worktree / "keep").write_text("unknown owner")
        removed, problems = worktree_new.rollback(
            self.h.repo, worktree, "zajca/issue-1", self.h.executor)
        self.assertEqual(removed, [])
        self.assertEqual(len(problems), 1)
        self.assertIn("not a registered worktree", problems[0])
        self.assertTrue((worktree / "keep").exists())

    def test_retry_after_rollback_succeeds(self):
        self.run_failing_add("after-checkout")
        self.h.executor.add_fails = None
        code, _, err = self.h.run("issue-1")
        self.assertEqual(code, 0, err)


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


class LockTests(unittest.TestCase):
    def test_missing_lock_files_are_skipped(self):
        with tempfile.TemporaryDirectory() as tmp:
            with worktree_new.hold_cargo_locks(Path(tmp)):
                pass

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
