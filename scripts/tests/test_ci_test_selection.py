"""Scenarios for scripts/ci-test-selection (stdlib only).

Each scenario builds a Git repository whose `selector.py` stands in for the
`xtask` selector: it knows a set of path prefixes (its rules), selects every
test for any other path, and writes the `affected ci` output files with the
revision it was built from. A private `cargo` on `$CARGO` builds it by copying
`selector.py` of the checked-out tree to `target/debug/xtask`, so the binary
that runs is the one of whatever revision the script checked out to build.
"""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "ci-test-selection"

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
    import json, os, subprocess, sys
    REVISION = {revision!r}
    KNOWN = {known!r}
    SCHEMA = {schema!r}
    args = sys.argv[1:]
    if args[:2] != ["affected", "ci"] or not {has_ci!r}:
        sys.exit("error: unrecognized subcommand")
    base = args[args.index("--base") + 1]
    out = args[args.index("--out") + 1]
    git = lambda *a: subprocess.check_output(["git", *a], text=True)
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
        "packages": packages, "files": files, "selector_revision": REVISION,
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
    """
)


def selector(revision, known, schema=1, has_ci=True):
    return SELECTOR.format(revision=revision, known=list(known), schema=schema, has_ci=has_ci)


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

    def run(self, *args):
        out = self.root / "out"
        result = subprocess.run(
            [str(SCRIPT), *args, "--out", str(out)], cwd=self.dir, env=self.env,
            check=False, capture_output=True, text=True,
        )
        self.result = result
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
