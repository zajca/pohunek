"""Scenarios for scripts/ci-test-selection (stdlib only).

Each scenario builds a Git repository whose `selector.py` stands in for the
`xtask` selector: it knows a set of path prefixes (its rules), selects every
test for any other path, and writes the `affected ci` output files with the
revision it was built from. Its `affected sparse` prints one pattern per
tracked file of the named packages' directories, after a comment naming that
revision. A private `cargo` on `$CARGO` builds it by copying
`selector.py` of the checked-out tree to `target/debug/xtask`, so the binary
that runs is the one of whatever revision the script checked out to build.
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "ci-test-selection"

# Runs the script's own code with module constants overridden, so a timeout
# scenario does not wait for the production limits.
DRIVER = textwrap.dedent(
    """\
    import importlib.machinery, importlib.util, json, sys
    path, overrides = sys.argv[1], json.loads(sys.argv[2])
    loader = importlib.machinery.SourceFileLoader("ci_test_selection", path)
    module = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
    loader.exec_module(module)
    for name, value in overrides.items():
        setattr(module, name, value)
    sys.argv = [path, *sys.argv[3:]]
    sys.exit(module.main())
    """
)

FAKE_CARGO = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    import os, shutil, sys
    if sys.argv[1:] != ["build", "--locked", "-p", "xtask", "--bin", "xtask"]:
        sys.exit(f"unexpected cargo call {sys.argv[1:]}")
    if not os.path.exists("selector.py"):
        sys.exit("error: package ID specification `xtask` did not match any packages")
    os.makedirs("target/debug", exist_ok=True)
    shutil.copy("selector.py", "target/debug/xtask")
    os.chmod("target/debug/xtask", 0o755)
    """
)

SELECTOR = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    import json, os, subprocess, sys, time
    REVISION = {revision!r}
    KNOWN = {known!r}
    SCHEMA = {schema!r}
    args = sys.argv[1:]
    git = lambda *a: subprocess.check_output(["git", *a], text=True)
    if args[:2] == ["affected", "sparse"] and {has_sparse!r}:
        names = [args[i + 1] for i, a in enumerate(args) if a == "--package"]
        files = git("ls-files").split()
        print(f"# {{REVISION}}")
        print("".join(f"/{{f}}\\n" for f in files if f.split("/")[0] in names), end="")
        sys.exit(0)
    if args[:2] != ["affected", "ci"] or not {has_ci!r}:
        sys.exit("error: unrecognized subcommand")
    time.sleep({ci_delay!r})
    base = args[args.index("--base") + 1]
    out = args[args.index("--out") + 1]
    merge_base = git("merge-base", base, "HEAD").strip()
    files = git("diff", "--name-only", merge_base, "HEAD").split()
    unknown = [f for f in files if not f.startswith(tuple(KNOWN))]
    packages = sorted({{f.split("/")[0] for f in files if f not in unknown}})
    mode = "full" if unknown else ("packages" if packages else "none")
    filterset = " | ".join(f"package(={{p}})" for p in packages) if mode == "packages" else None
    selection = {{
        "schema_version": SCHEMA, "base": base, "merge_base": merge_base, "mode": mode,
        "docs_only": False, "script_tests": mode == "full", "sdk_gates": mode == "full",
        "filterset": filterset, "full_causes": [f"{{f}}: unmapped" for f in unknown],
        "packages": [{{"name": p}} for p in packages], "files": files, "selector_revision": REVISION,
    }}
    os.makedirs(out, exist_ok=True)
    flags = {{k: str(selection[k]).lower() for k in ("docs_only", "script_tests", "sdk_gates")}}
    with open(os.path.join(out, "outputs.env"), "w") as f:
        f.write(f"mode={{mode}}\\n" + "".join(f"{{k}}={{v}}\\n" for k, v in flags.items())
                + f"filterset={{filterset or ''}}\\n")
    with open(os.path.join(out, "summary.md"), "w") as f:
        f.write(f"### Test selection\\n\\nMode: **{{mode}}**\\n")
    with open(os.path.join(out, "selection.json"), "w") as f:
        json.dump(selection, f)
    if {outputs_env!r} is not None:
        with open(os.path.join(out, "outputs.env"), "w") as f:
            f.write({outputs_env!r})
    """
)

# Built at a merge base whose untracked output blocks the checkout back to HEAD:
# the build leaves `b/y`, which HEAD tracks and the merge base does not.
UNTRACKED_OUTPUT_CARGO = FAKE_CARGO + (
    "os.makedirs('b', exist_ok=True)\n"
    "open('b/y', 'w').write('left behind\\n')\n"
)

# The build sleeps past any timeout a scenario sets.
SLOW_CARGO = FAKE_CARGO.replace("import os, shutil, sys\n", "import os, shutil, sys, time\ntime.sleep(3)\n", 1)


def selector(revision, known, schema=1, has_ci=True, has_sparse=True, ci_delay=0,
             outputs_env=None):
    """A base selector; `ci_delay` seconds pass before `affected ci` answers and
    `outputs_env`, when set, replaces the `outputs.env` it writes."""
    return SELECTOR.format(
        revision=revision, known=list(known), schema=schema, has_ci=has_ci,
        has_sparse=has_sparse, ci_delay=ci_delay, outputs_env=outputs_env,
    )


class Repo:
    """A repository on `main` with one base commit, checked out on `topic`."""

    def __init__(self, root, base_files):
        self.root = root
        self.dir = root / "repo"
        self.dir.mkdir()
        bin_dir = root / "bin"
        bin_dir.mkdir()
        cargo = bin_dir / "cargo"
        cargo.write_text(FAKE_CARGO)
        cargo.chmod(0o755)
        self.env = {
            "PATH": os.environ["PATH"],
            "HOME": str(root),
            "CARGO": str(cargo),
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "test", "GIT_AUTHOR_EMAIL": "test@example.invalid",
            "GIT_COMMITTER_NAME": "test", "GIT_COMMITTER_EMAIL": "test@example.invalid",
        }
        self.git("init", "--quiet", "--initial-branch=main")
        (self.dir / ".gitignore").write_text("target/\n")
        self.commit(base_files, "base")
        self.git("checkout", "--quiet", "-b", "topic")

    def git(self, *args):
        return subprocess.run(
            ["git", *args], cwd=self.dir, env=self.env, check=True,
            capture_output=True, text=True,
        ).stdout.strip()

    def commit(self, files, message):
        for path, text in files.items():
            target = self.dir / path
            if text is None:
                target.unlink()
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(text)
        self.git("add", "--all")
        self.git("commit", "--quiet", "-m", message)

    def invoke(self, *args, constants=None):
        """Runs the script, or with `constants` the script's code with those
        module constants overridden; the result is kept in `self.result`."""
        out = self.out = self.root / "out"
        if constants is None:
            command = [str(SCRIPT), *args, "--out", str(out)]
        else:
            command = [sys.executable, "-c", DRIVER, str(SCRIPT), json.dumps(constants),
                       *args, "--out", str(out)]
        self.result = subprocess.run(
            command, cwd=self.dir, env=self.env, check=False, capture_output=True, text=True,
        )
        return self.result

    def run(self, *args, constants=None):
        result = self.invoke(*args, constants=constants)
        out = self.out
        assert result.returncode == 0, result.stderr + result.stdout
        selection = json.loads((out / "selection.json").read_text())
        outputs = dict(
            line.split("=", 1) for line in (out / "outputs.env").read_text().splitlines()
        )
        return selection, outputs, (out / "summary.md").read_text()


class CiTestSelectionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def repo(self, base_selector):
        files = {"a/x": "x\n"}
        if base_selector is not None:
            files["selector.py"] = base_selector
        return Repo(self.root, files)

    def assert_head_restored(self, repo, head):
        self.assertEqual(repo.git("rev-parse", "HEAD"), head)
        self.assertEqual(repo.git("status", "--porcelain", "--untracked-files=no"), "")

    def test_the_base_selector_judges_a_path_only_the_head_rules_know(self):
        repo = self.repo(selector("base", ["a/", "selector.py"]))
        repo.commit({"selector.py": selector("head", ["a/", "b/"]), "b/y": "y\n"}, "teach b/")
        head = repo.git("rev-parse", "HEAD")
        selection, outputs, summary = repo.run("--base", "main")
        self.assertEqual(selection["selector_revision"], "base")
        self.assertEqual(outputs["mode"], "full")
        self.assertEqual(selection["full_causes"], ["b/y: unmapped"])
        self.assertIn("Selector built from the merge base", summary)
        self.assert_head_restored(repo, head)

    def test_the_base_selector_narrows_a_change_it_knows(self):
        # The head selector knows nothing, so it would select every test.
        repo = self.repo(selector("base", ["a/", "selector.py"]))
        repo.commit({"selector.py": selector("head", []), "a/x": "changed\n"}, "change a/")
        head = repo.git("rev-parse", "HEAD")
        selection, outputs, _ = repo.run("--base", "main")
        self.assertEqual(selection["selector_revision"], "base")
        self.assertEqual(outputs["mode"], "packages")
        self.assertEqual(outputs["filterset"], "package(=a) | package(=selector.py)")
        self.assert_head_restored(repo, head)

    def test_the_base_selector_writes_the_sparse_tree_of_a_narrowed_selection(self):
        repo = self.repo(selector("base", ["a/", "b/", "selector.py"]))
        repo.commit({"selector.py": selector("head", ["a/", "b/"], has_sparse=False),
                     "a/x": "changed\n", "b/y": "y\n"}, "change a/")
        _, outputs, summary = repo.run("--base", "main")
        self.assertEqual(outputs["mode"], "packages")
        # The base selector's patterns, of the head tree's files.
        self.assertEqual(
            (repo.out / "sparse.txt").read_text(), "# base\n/a/x\n/b/y\n/selector.py\n"
        )
        self.assertIn("run from a sparse tree", summary)

    def test_a_base_selector_without_a_sparse_tree_runs_the_selection_from_the_full_tree(self):
        repo = self.repo(selector("base", ["a/"], has_sparse=False))
        repo.commit({"a/x": "changed\n"}, "change a/")
        _, outputs, summary = repo.run("--base", "main")
        self.assertEqual(outputs["mode"], "packages")
        self.assertFalse((repo.out / "sparse.txt").exists())
        self.assertIn("run from the full tree", summary)
        self.assertIn("::warning::The merge base's selector gave no sparse tree", repo.result.stdout)

    def test_a_full_selection_writes_no_sparse_tree(self):
        repo = self.repo(selector("base", ["a/"]))
        repo.commit({"c/z": "z\n"}, "change an unmapped path")
        (self.root / "out").mkdir()
        (self.root / "out" / "sparse.txt").write_text("/stale\n")
        _, outputs, _ = repo.run("--base", "main")
        self.assertEqual(outputs["mode"], "full")
        self.assertFalse((repo.out / "sparse.txt").exists())
        (self.root / "out" / "sparse.txt").write_text("/stale\n")
        repo.run("--full", "push to main runs every test")
        self.assertFalse((repo.out / "sparse.txt").exists())

    def test_a_base_selector_without_the_ci_command_runs_every_test(self):
        repo = self.repo(selector("base", ["a/"], has_ci=False))
        repo.commit({"a/x": "changed\n"}, "change a/")
        selection, outputs, _ = repo.run("--base", "main")
        self.assertEqual(outputs["mode"], "full")
        self.assertIn("the merge base's selector failed", selection["full_causes"][0])
        self.assertIn("::warning::every test runs", repo.result.stdout)

    def test_a_base_that_does_not_build_runs_every_test(self):
        repo = self.repo(None)
        head_files = {"selector.py": selector("head", ["a/"]), "a/x": "changed\n"}
        repo.commit(head_files, "add the selector")
        head = repo.git("rev-parse", "HEAD")
        selection, outputs, _ = repo.run("--base", "main")
        self.assertEqual(outputs["mode"], "full")
        self.assertIn("did not build", selection["full_causes"][0])
        self.assert_head_restored(repo, head)

    def test_another_schema_version_runs_every_test(self):
        repo = self.repo(selector("base", ["a/"], schema=2))
        repo.commit({"a/x": "changed\n"}, "change a/")
        selection, outputs, _ = repo.run("--base", "main")
        self.assertEqual(outputs["mode"], "full")
        self.assertEqual(
            selection["full_causes"],
            ["the merge base's selector wrote schema version 2, not 1"],
        )

    def test_a_base_without_a_merge_base_runs_every_test(self):
        repo = self.repo(selector("base", ["a/"]))
        repo.git("checkout", "--quiet", "--orphan", "unrelated")
        repo.commit({"c/z": "z\n"}, "unrelated history")
        repo.git("checkout", "--quiet", "topic")
        selection, outputs, _ = repo.run("--base", "unrelated")
        self.assertEqual(outputs["mode"], "full")
        self.assertIn("no merge base of unrelated and HEAD", selection["full_causes"][0])

    def assert_every_test_runs_because(self, repo, cause):
        self.assertEqual(repo.result.returncode, 0, repo.result.stderr)
        selection = json.loads((repo.out / "selection.json").read_text())
        self.assertEqual(selection["mode"], "full")
        self.assertEqual(selection["full_causes"], [cause])
        self.assertIn(f"::warning::every test runs: {cause}", repo.result.stdout)

    def inconsistent_outputs_repo(self, outputs_env):
        repo = self.repo(selector("base", ["a/"], outputs_env=outputs_env))
        repo.commit({"a/x": "changed\n"}, "change a/")
        return repo

    def test_a_base_selector_whose_mode_disagrees_with_its_selection_runs_every_test(self):
        repo = self.inconsistent_outputs_repo(
            "mode=none\ndocs_only=false\nscript_tests=false\nsdk_gates=false\nfilterset=package(=a)\n"
        )
        repo.invoke("--base", "main")
        self.assert_every_test_runs_because(
            repo, "the merge base's selector wrote an inconsistent selection"
        )

    def test_a_base_selector_whose_flag_disagrees_with_its_selection_runs_every_test(self):
        repo = self.inconsistent_outputs_repo(
            "mode=packages\ndocs_only=false\nscript_tests=true\nsdk_gates=false\nfilterset=package(=a)\n"
        )
        repo.invoke("--base", "main")
        self.assert_every_test_runs_because(
            repo, "the merge base's selector wrote an inconsistent selection"
        )

    def test_a_base_selector_with_a_malformed_outputs_line_runs_every_test(self):
        # A line without `=` is no key, so the keys no longer match the schema.
        repo = self.inconsistent_outputs_repo(
            "mode=packages\ndocs_only=false\nscript_tests=false\nsdk_gates=false\nfilterset\n"
        )
        repo.invoke("--base", "main")
        self.assert_every_test_runs_because(
            repo, "the merge base's selector wrote an inconsistent selection"
        )

    def test_a_base_selector_that_builds_too_slowly_runs_every_test(self):
        repo = self.repo(selector("base", ["a/"]))
        repo.commit({"a/x": "changed\n"}, "change a/")
        head = repo.git("rev-parse", "HEAD")
        slow = self.root / "bin" / "cargo"
        slow.write_text(SLOW_CARGO)
        repo.run("--base", "main", constants={"BUILD_TIMEOUT_SECONDS": 1})
        self.assert_every_test_runs_because(
            repo, "the merge base's selector did not build within 1 s"
        )
        self.assert_head_restored(repo, head)

    def test_a_base_selector_that_selects_too_slowly_runs_every_test(self):
        repo = self.repo(selector("base", ["a/"], ci_delay=3))
        repo.commit({"a/x": "changed\n"}, "change a/")
        head = repo.git("rev-parse", "HEAD")
        repo.run("--base", "main", constants={"SELECT_TIMEOUT_SECONDS": 1})
        self.assert_every_test_runs_because(
            repo, "the merge base's selector did not finish within 1 s"
        )
        self.assert_head_restored(repo, head)

    def test_a_dirty_checkout_runs_every_test_without_building_the_base(self):
        repo = self.repo(selector("base", ["a/"]))
        repo.commit({"a/y": "y\n"}, "add a/y")
        # Tracked in both commits, so the base checkout would carry it along.
        (repo.dir / "a" / "x").write_text("uncommitted\n")
        repo.invoke("--base", "main")
        self.assert_every_test_runs_because(
            repo, "the checkout has uncommitted changes, so the merge base cannot be built"
        )
        self.assertEqual((repo.dir / "a" / "x").read_text(), "uncommitted\n")
        self.assertFalse((repo.dir / "target").exists())

    def test_a_checkout_that_cannot_return_to_head_fails_and_leaves_no_selection(self):
        repo = self.repo(selector("base", ["a/", "selector.py"]))
        repo.commit({"b/y": "y\n"}, "add b/y")
        (self.root / "bin" / "cargo").write_text(UNTRACKED_OUTPUT_CARGO)
        stale = self.root / "out"
        stale.mkdir()
        (stale / "selection.json").write_text("{}\n")
        result = repo.invoke("--base", "main")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("::error::ci-test-selection: the checkout did not return to", result.stderr)
        for name in ("selection.json", "outputs.env", "summary.md", "sparse.txt"):
            self.assertFalse((repo.out / name).exists(), name)

    def test_a_merge_base_that_cannot_be_checked_out_runs_every_test(self):
        repo = self.repo(selector("base", ["a/", "selector.py"]))
        repo.commit({"a/x": None}, "remove a/x")
        head = repo.git("rev-parse", "HEAD")
        # An untracked file where the merge base tracks one blocks its checkout.
        (repo.dir / "a" / "x").write_text("untracked\n")
        selection, _, summary = repo.run("--base", "main")
        self.assertEqual(selection["mode"], "full")
        [cause] = selection["full_causes"]
        self.assertIn("could not be checked out", cause)
        self.assertIn("could not be checked out", summary)
        self.assert_head_restored(repo, head)
        self.assertEqual((repo.dir / "a" / "x").read_text(), "untracked\n")

    def test_a_full_selection_names_its_cause_in_every_output(self):
        repo = self.repo(selector("base", ["a/"]))
        selection, outputs, summary = repo.run("--full", "push to main runs every test")
        self.assertEqual(selection["schema_version"], 1)
        self.assertEqual(selection["full_causes"], ["push to main runs every test"])
        self.assertEqual(
            outputs,
            {"mode": "full", "docs_only": "false", "script_tests": "true",
             "sdk_gates": "true", "filterset": ""},
        )
        self.assertIn("push to main runs every test", summary)


if __name__ == "__main__":
    unittest.main()
