"""Scenarios for scripts/red-on-old (stdlib only).

The real script runs in a throwaway git repository that holds the real
`scripts/stress-changed-tests` (its test-range parser) and a tiny "crate" whose
behaviour is the content of `crates/demo/src/lib.rs`. A private `cargo` on
`$CARGO` stands in for nextest: it reads that file in the tree it runs in, so the
old tree (production file restored to the merge base) and the new tree give
different results, and it writes the JUnit report nextest would write. `$HEAVY_RUN`
is a pass-through.

Filtersets the stand-in knows: `red` fails while the tree holds `broken`, `green`
always passes, `empty` selects no test. A tree whose `lib.rs` says `nobuild` does
not compile, and every filter needs `crates/demo/tests/scenarios.rs` (test code
the old tree must keep).
"""

import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

import hermetic_git  # noqa: F401  (isolates git from the developer's configuration)

SCRIPTS = Path(__file__).resolve().parents[1]
RED_ON_OLD = SCRIPTS / "red-on-old"
STRESS = SCRIPTS / "stress-changed-tests"

LIB = "crates/demo/src/lib.rs"
SCENARIOS = "crates/demo/tests/scenarios.rs"
MIXED = "crates/demo/src/mixed.rs"

FAKE_CARGO = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    import os, sys
    args = sys.argv[1:]
    sources = []
    for directory, _, names in os.walk("crates/demo/src"):
        for name in sorted(names):
            path = os.path.join(directory, name)
            if path != "crates/demo/src/lib.rs":
                sources.append(path + "=" + open(path).read().split("\\n")[0])
    with open(os.environ["FAKE_CALLS"], "a") as calls:
        calls.write(os.getcwd() + " cargo " + " ".join(args) + " | " + " ".join(sorted(sources)) + "\\n")
    lib = open("crates/demo/src/lib.rs").read()
    if args[0] == "build":
        if "nobuild" in lib:
            sys.exit("error: could not compile `demo`")
        sys.exit(0)
    target = os.environ.get("CARGO_TARGET_DIR", "target")
    if os.environ.get("FAKE_STORE_IN_WORKSPACE"):
        target = "target"
    junit = os.path.join(target, "nextest", "ci", "junit.xml")
    os.makedirs(os.path.dirname(junit), exist_ok=True)
    if args[:2] != ["nextest", "run"]:
        sys.exit(f"unexpected cargo call {args}")
    if "nobuild" in lib or "notestbuild" in lib:
        sys.exit("error: could not compile `demo` (test)")
    filterset = args[args.index("-E") + 1]
    if "empty" in filterset or not os.path.exists("crates/demo/tests/scenarios.rs"):
        print("error: no tests to run", file=sys.stderr)
        sys.exit(4)
    broken = "broken" in lib
    failing = "red" in filterset and broken
    body = f'<failure message="assertion failed"/>' if failing else ""
    with open(junit, "w") as report:
        report.write(
            '<testsuites><testsuite name="demo"><testcase name="the_scenario" '
            f'classname="demo::scenarios">{body}</testcase></testsuite></testsuites>'
        )
    sys.exit(100 if failing else 0)
    """
)

HEAVY_RUN = textwrap.dedent(
    """\
    #!/bin/sh
    exec "$@"
    """
)

MIXED_OLD = textwrap.dedent(
    """\
    pub fn value() -> u8 {
        1
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn value_is_one() {
            assert_eq!(super::value(), 1);
        }
    }
    """
)
MIXED_NEW = MIXED_OLD.replace("        1\n", "        2\n").replace("super::value(), 1", "super::value(), 2")


class RedOnOldScenarios(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.base = Path(tmp.name)
        self.repo = self.base / "repo"
        self.scratch = self.base / "scratch"
        self.bin = self.base / "bin"
        self.calls = self.base / "calls.log"
        for directory in (self.repo, self.scratch, self.bin):
            directory.mkdir()
        self.calls.write_text("")
        for name, text in (("cargo", FAKE_CARGO), ("heavy-run", HEAVY_RUN)):
            (self.bin / name).write_text(text)
            (self.bin / name).chmod(0o755)
        self.write("scripts/stress-changed-tests", STRESS.read_text())
        self.write("scripts/red-on-old", RED_ON_OLD.read_text())
        (self.repo / "scripts/red-on-old").chmod(0o755)
        self.write(LIB, "broken\n")
        self.write(MIXED, MIXED_OLD)
        self.write("docs/page.md", "docs\n")
        self.git("init", "-q", "-b", "main")
        self.commit("initial")
        self.git("checkout", "-q", "-b", "fix")
        self.environment = dict(
            os.environ,
            CARGO=str(self.bin / "cargo"),
            HEAVY_RUN=str(self.bin / "heavy-run"),
            FAKE_CALLS=str(self.calls),
        )
        self.environment.pop("CARGO_TARGET_DIR", None)

    def write(self, relative, text):
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def git(self, *args):
        # A commit must not start a detached `maintenance.auto` / `gc.auto` run that
        # outlives the fixture directory (#1251).
        result = subprocess.run(
            ["git", "-c", "maintenance.auto=false", "-c", "gc.auto=0", "-C", str(self.repo), *args],
            check=True, capture_output=True, text=True,
        )
        return result.stdout

    def commit(self, message):
        self.git("add", "-A")
        self.git("-c", "commit.gpgsign=false", "commit", "-q", "-m", message)

    def fix(self, lib="fixed\n"):
        """The fix commit: production code plus a scenario file."""
        self.write(LIB, lib)
        self.write(SCENARIOS, "// scenarios\n")
        self.commit("fix")

    def red_on_old(self, *extra, environment=None):
        return subprocess.run(
            [sys.executable, str(RED_ON_OLD), "--root", str(self.repo), "--base", "main",
             "--scratch-root", str(self.scratch), *extra],
            capture_output=True, text=True, check=False, env=dict(self.environment, **(environment or {})),
        )

    def rows(self, result):
        """The table rows by row id; each is the list of its four cells."""
        found = {}
        for line in result.stdout.splitlines():
            cells = [cell.strip() for cell in line.strip("|").split("|")]
            if len(cells) == 4 and cells[0] not in ("row", "---"):
                found[cells[0]] = cells
        return found


class RedOldPassingNew(RedOnOldScenarios):
    def test_a_scenario_that_fails_on_old_code_and_passes_on_new_code_is_accepted(self):
        self.fix()
        result = self.red_on_old("--rows", "R1", "--scenario", "R1=test(red)", "--package", "demo")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            self.rows(result)["R1"],
            ["R1", "`test(red)`", "FAIL (demo::scenarios the_scenario)", "PASS"],
        )
        self.assertIn("| row | scenario | old code | new code |", result.stdout)
        self.assertIn("old tree builds cold", result.stderr)

    def test_the_report_is_found_when_nextest_stores_it_under_the_workspace_target(self):
        self.fix()
        shared = self.base / "shared-target"
        (shared / "debug" / "deps").mkdir(parents=True)
        result = self.red_on_old(
            "--rows", "R1", "--scenario", "R1=test(red)", "--package", "demo",
            environment={"CARGO_TARGET_DIR": str(shared), "FAKE_STORE_IN_WORKSPACE": "1"},
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.rows(result)["R1"][2:], ["FAIL (demo::scenarios the_scenario)", "PASS"])
        self.assertIn("old tree", result.stderr)

    def test_a_scenario_that_passes_on_both_trees_is_not_red_and_is_rejected(self):
        self.fix()
        result = self.red_on_old("--rows", "R1", "--scenario", "R1=test(green)", "--package", "demo")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(self.rows(result)["R1"][2:], ["PASS (not red)", "PASS"])

    def test_a_row_without_a_scenario_or_manual_proof_is_missing_and_rejected(self):
        self.fix()
        result = self.red_on_old(
            "--rows", "R1,R2", "--scenario", "R1=test(red)", "--package", "demo",
        )
        self.assertEqual(result.returncode, 1)
        rows = self.rows(result)
        self.assertEqual(rows["R2"], ["R2", "-", "MISSING", "MISSING"])
        self.assertEqual(rows["R1"][2], "FAIL (demo::scenarios the_scenario)")

    def test_the_old_tree_keeps_test_code_and_restores_the_production_file(self):
        self.fix()
        result = self.red_on_old("--rows", "R1", "--scenario", "R1=test(red)", "--package", "demo")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        old_runs = [call for call in self.calls.read_text().splitlines() if "red-on-old-" in call]
        self.assertTrue(old_runs)


class OldTreeThatDoesNotBuild(RedOnOldScenarios):
    def test_a_build_failure_is_never_a_red_result(self):
        self.git("checkout", "-q", "main")
        self.write(LIB, "nobuild\n")
        self.commit("main does not build")
        self.git("checkout", "-q", "-b", "fix2")
        self.fix()
        result = self.red_on_old("--rows", "R1", "--scenario", "R1=test(red)", "--package", "demo")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(self.rows(result)["R1"][2:], ["BUILD FAIL (not a red result)", "PASS"])

    def test_a_test_build_failure_after_a_good_binary_build_is_never_a_red_result(self):
        self.git("checkout", "-q", "main")
        self.write(LIB, "notestbuild\n")
        self.commit("main binaries build, its tests do not")
        self.git("checkout", "-q", "-b", "fix2")
        self.fix()
        result = self.red_on_old("--rows", "R1", "--scenario", "R1=test(red)", "--package", "demo")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(self.rows(result)["R1"][2:], ["BUILD FAIL (not a red result)", "PASS"])

    def test_a_manual_row_replaces_the_old_run_and_is_accepted(self):
        self.git("checkout", "-q", "main")
        self.write(LIB, "nobuild\n")
        self.commit("main does not build")
        self.git("checkout", "-q", "-b", "fix2")
        self.fix()
        result = self.red_on_old(
            "--rows", "R1", "--scenario", "R1=test(red)", "--manual", "R1=reverted the guard by hand",
            "--package", "demo",
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.rows(result)["R1"][2:], ["manual: reverted the guard by hand", "PASS"])
        self.assertFalse([call for call in self.calls.read_text().splitlines() if "red-on-old-" in call])


class FilterThatSelectsNothing(RedOnOldScenarios):
    def test_a_filter_that_selects_no_test_is_a_failure_and_never_a_red_result(self):
        self.fix()
        result = self.red_on_old("--rows", "R1", "--scenario", "R1=test(empty)", "--package", "demo")
        self.assertEqual(result.returncode, 1)
        cell = "FAIL (no test selected; not a red result)"
        self.assertEqual(self.rows(result)["R1"][2:], [cell, cell])


class ProductionFileThatAlsoChangesTestCode(RedOnOldScenarios):
    def mixed_fix(self):
        self.write(LIB, "fixed\nmod mixed;\n")
        self.write(MIXED, MIXED_NEW)
        self.write(SCENARIOS, "// scenarios\n")
        self.commit("fix with an inline test")

    def test_the_script_refuses_until_every_row_has_a_manual_proof(self):
        self.mixed_fix()
        result = self.red_on_old("--rows", "R1", "--scenario", "R1=test(red)", "--package", "demo")
        self.assertEqual(result.returncode, 2)
        self.assertIn(MIXED, result.stderr)
        self.assertIn("--manual", result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(self.calls.read_text(), "")

    def test_manual_rows_print_how_the_old_behaviour_was_reproduced(self):
        self.mixed_fix()
        result = self.red_on_old(
            "--rows", "R1", "--scenario", "R1=test(green)", "--manual", "R1=set value() back to 1 by hand",
            "--package", "demo",
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            self.rows(result)["R1"],
            ["R1", "`test(green)`", "manual: set value() back to 1 by hand", "PASS"],
        )
        self.assertFalse([call for call in self.calls.read_text().splitlines() if "red-on-old-" in call])

    def test_a_manual_row_without_a_scenario_is_rejected_because_nothing_ran_on_the_new_code(self):
        self.mixed_fix()
        result = self.red_on_old("--rows", "R1", "--manual", "R1=set value() back to 1 by hand")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(
            self.rows(result)["R1"][2:],
            ["manual: set value() back to 1 by hand", "NOT RUN (a manual row needs a --scenario for the new code)"],
        )

    def test_a_change_to_production_lines_outside_the_test_module_is_restored_automatically(self):
        self.write(MIXED, MIXED_OLD.replace("        1\n", "        2\n", 1).replace("super::value(), 1", "super::value(), 1"))
        self.write(SCENARIOS, "// scenarios\n")
        self.write(LIB, "fixed\nmod mixed;\n")
        self.commit("fix a production line only")
        result = self.red_on_old("--rows", "R1", "--scenario", "R1=test(red)", "--package", "demo")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


class ModulesThatOnlyATestBuildCompiles(RedOnOldScenarios):
    """A `mod` declaration decides whether a changed source file is test code."""

    def old_tree_runs(self):
        return [call for call in self.calls.read_text().splitlines() if "red-on-old-" in call and " nextest " in call]

    def run_fix(self, lib_old, lib_new, files_old, files_new, *extra):
        for path, text in files_old.items():
            self.write(f"crates/demo/src/{path}", text)
        self.write(LIB, lib_old)
        self.commit("main with the modules")
        self.git("checkout", "-q", "-b", "fix2")
        for path, text in files_new.items():
            self.write(f"crates/demo/src/{path}", text)
        self.write(LIB, lib_new)
        self.write(SCENARIOS, "// scenarios\n")
        self.commit("fix")
        return self.red_on_old("--rows", "R1", "--scenario", "R1=test(red)", "--package", "demo", *extra)

    def test_the_old_tree_keeps_a_helper_declared_by_a_cfg_test_module(self):
        self.git("checkout", "-q", "main")
        result = self.run_fix(
            "broken\n#[cfg(test)]\nmod test_support;\n", "fixed\n#[cfg(test)]\nmod test_support;\n",
            {"test_support.rs": "old helper\n"}, {"test_support.rs": "new helper\n"},
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        runs = self.old_tree_runs()
        self.assertTrue(runs)
        self.assertTrue(all("crates/demo/src/test_support.rs=new helper" in run for run in runs), runs)

    def test_the_old_tree_restores_a_helper_that_a_production_module_declares(self):
        self.git("checkout", "-q", "main")
        result = self.run_fix(
            "broken\nmod helper;\n", "fixed\nmod helper;\n",
            {"helper.rs": "old helper\n"}, {"helper.rs": "new helper\n"},
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        runs = self.old_tree_runs()
        self.assertTrue(runs)
        self.assertTrue(all("crates/demo/src/helper.rs=old helper" in run for run in runs), runs)

    def test_every_file_below_a_cfg_test_directory_module_is_test_code(self):
        self.git("checkout", "-q", "main")
        result = self.run_fix(
            "broken\n#[cfg(test)]\nmod scenarios_dir;\n", "fixed\n#[cfg(test)]\nmod scenarios_dir;\n",
            {"scenarios_dir/mod.rs": "mod leaf;\n", "scenarios_dir/leaf.rs": "old leaf\n"},
            {"scenarios_dir/mod.rs": "mod leaf;\n", "scenarios_dir/leaf.rs": "new leaf\n"},
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        runs = self.old_tree_runs()
        self.assertTrue(runs)
        self.assertTrue(all("crates/demo/src/scenarios_dir/leaf.rs=new leaf" in run for run in runs), runs)

    def test_a_path_attribute_names_the_file_of_a_cfg_test_module(self):
        self.git("checkout", "-q", "main")
        declaration = '#[cfg(test)]\n#[path = "support/impl_file.rs"]\nmod helper;\n'
        result = self.run_fix(
            "broken\n" + declaration, "fixed\n" + declaration,
            {"support/impl_file.rs": "old helper\n"}, {"support/impl_file.rs": "new helper\n"},
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        runs = self.old_tree_runs()
        self.assertTrue(runs)
        self.assertTrue(all("crates/demo/src/support/impl_file.rs=new helper" in run for run in runs), runs)

    def test_a_file_whose_declaration_cannot_be_read_is_refused_until_every_row_is_manual(self):
        self.git("checkout", "-q", "main")
        result = self.run_fix(
            "broken\n", "fixed\n", {"orphan.rs": "old\n"}, {"orphan.rs": "new\n"},
        )
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("crates/demo/src/orphan.rs", result.stderr)
        self.assertIn("--manual", result.stderr)
        self.assertEqual(result.stdout, "")
        self.assertEqual(self.calls.read_text(), "")

    def test_a_manual_row_accepts_a_file_whose_declaration_cannot_be_read(self):
        self.git("checkout", "-q", "main")
        result = self.run_fix(
            "broken\n", "fixed\n", {"orphan.rs": "old\n"}, {"orphan.rs": "new\n"},
            "--manual", "R1=set the value back by hand",
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.rows(result)["R1"][2:], ["manual: set the value back by hand", "PASS"])

    def test_a_module_that_a_host_cfg_may_compile_out_is_never_taken_for_production(self):
        self.git("checkout", "-q", "main")
        result = self.run_fix(
            "broken\n#[cfg(any(target_os = \"macos\", test))]\nmod layout;\n",
            "fixed\n#[cfg(any(target_os = \"macos\", test))]\nmod layout;\n",
            {"layout.rs": "old\n"}, {"layout.rs": "new\n"},
        )
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("crates/demo/src/layout.rs", result.stderr)


class ScratchWorktreeCleanup(RedOnOldScenarios):
    def scratch_entries(self):
        return sorted(path.name for path in self.scratch.iterdir())

    def test_only_the_created_worktree_is_removed_and_a_sibling_survives(self):
        (self.scratch / "keep-me").mkdir()
        (self.scratch / "keep-me" / "file").write_text("x")
        self.fix()
        result = self.red_on_old("--rows", "R1", "--scenario", "R1=test(red)", "--package", "demo")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.scratch_entries(), ["keep-me"])
        self.assertTrue((self.scratch / "keep-me" / "file").exists())
        self.assertNotIn("red-on-old-", self.git("worktree", "list"))

    def test_the_worktree_is_removed_when_the_old_tree_fails(self):
        self.git("checkout", "-q", "main")
        self.write(LIB, "nobuild\n")
        self.commit("main does not build")
        self.git("checkout", "-q", "-b", "fix2")
        self.fix()
        result = self.red_on_old("--rows", "R1", "--scenario", "R1=test(red)", "--package", "demo")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(self.scratch_entries(), [])
        self.assertNotIn("red-on-old-", self.git("worktree", "list"))


class ExemptionAndRefusals(RedOnOldScenarios):
    def test_a_diff_that_changes_no_crate_file_is_exempt(self):
        self.write("docs/page.md", "docs, changed\n")
        self.commit("docs only")
        result = self.red_on_old("--rows", "R1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("exempt", result.stdout)
        self.assertNotIn("| row |", result.stdout)

    def test_a_dirty_tree_is_refused(self):
        self.fix()
        self.write(LIB, "dirty\n")
        result = self.red_on_old("--rows", "R1", "--scenario", "R1=test(red)", "--package", "demo")
        self.assertEqual(result.returncode, 2)
        self.assertIn("not clean", result.stderr)

    def test_a_missing_heavy_run_is_refused_without_a_default(self):
        self.fix()
        environment = {key: value for key, value in self.environment.items() if key != "HEAVY_RUN"}
        result = subprocess.run(
            [sys.executable, str(RED_ON_OLD), "--root", str(self.repo), "--base", "main",
             "--scratch-root", str(self.scratch), "--rows", "R1", "--scenario", "R1=test(red)",
             "--package", "demo"],
            capture_output=True, text=True, check=False, env=environment,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("$HEAVY_RUN is required", result.stderr)

    def test_a_scenario_for_an_unknown_row_is_refused(self):
        self.fix()
        result = self.red_on_old("--rows", "R1", "--scenario", "R9=test(red)", "--package", "demo")
        self.assertEqual(result.returncode, 2)
        self.assertIn("R9 is not in --rows", result.stderr)


if __name__ == "__main__":
    unittest.main()
