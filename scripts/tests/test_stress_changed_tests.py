"""Integration scenarios for `scripts/stress-changed-tests` (stdlib only).

Every test drives the real script as a subprocess inside a disposable git
repository. A fake `cargo` on a private PATH answers `cargo metadata` and
`cargo nextest list` from files the test writes and records `cargo nextest
run`, so no test compiles anything. The stress run itself is the real
`scripts/stress-run` wrapping that fake cargo.

The selection rule under test (it is the one documented in the script's
docstring): a test is selected when any changed line lies in its range, from
its first doc comment or attribute to its closing brace. Comment edits inside
that range select it, a moved test is selected at its new place, a deleted
test is not selected, and a change outside every test range selects nothing.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

import hermetic_git  # noqa: F401  (isolates git from the developer's configuration)

SCRIPT = Path(__file__).resolve().parents[1] / "stress-changed-tests"
LINUX = sys.platform.startswith("linux")
WAIT_SECONDS = 60

FAKE_CARGO = r'''#!/usr/bin/env python3
"""Fake cargo: `metadata` and `nextest list` read canned files; `nextest run` is logged."""
import json, os, sys

argv = sys.argv[1:]
if argv[:1] == ["metadata"]:
    sys.stdout.write(open(os.environ["FAKE_METADATA"]).read())
elif argv[:2] == ["nextest", "list"]:
    with open(os.environ["FAKE_LOG"], "a") as stream:
        stream.write(json.dumps(argv) + "\n")
    sys.stdout.write(open(os.environ["FAKE_LIST"]).read())
elif argv[:2] == ["nextest", "run"]:
    with open(os.environ["FAKE_LOG"], "a") as stream:
        stream.write(json.dumps(argv) + "\n")
    sys.exit(int(os.environ.get("FAKE_RUN_RC", "0")))
else:
    sys.stderr.write("unexpected cargo call: %r\n" % argv)
    sys.exit(99)
'''

LIB_BASE = '''pub fn helper() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    #[test]
    fn untouched() {
        assert_eq!(1, 1);
    }

    #[test]
    fn body_edit() {
        let x = 1;
        assert_eq!(x, 1);
    }

    #[test]
    fn comment_edit() {
        // original comment
        assert!(true);
    }

    #[test]
    fn gets_deleted() {
        assert!(true);
    }

    #[test]
    fn gets_moved() {
        assert!(true);
    }

    #[test]
    fn middle_case() {
        assert!(true);
    }

    #[test]
    fn below_middle() {
        assert!(true);
    }
}
'''

FLOW_BASE = '''#[test]
fn flow_case() {
    assert!(true);
}
'''

CLI_ALL = '''#[path = "mod_a.rs"]
mod mod_a;
'''

CLI_MOD_A = '''#[test]
fn cli_case() {
    assert!(true);
}

mod nested {
    #[tokio::test(flavor = "multi_thread")]
    async fn deep_case() {
        assert!(true);
    }
}
'''


def git(root, *args):
    env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.invalid",
               GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.invalid",
               GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_SYSTEM=os.devnull,
               # A commit must not start detached maintenance that can outlive
               # the fixture and break its removal (#1251).
               GIT_CONFIG_COUNT="2",
               GIT_CONFIG_KEY_0="maintenance.auto", GIT_CONFIG_VALUE_0="false",
               GIT_CONFIG_KEY_1="gc.auto", GIT_CONFIG_VALUE_1="0")
    result = subprocess.run(["git", *args], cwd=root, capture_output=True, text=True, env=env)
    if result.returncode != 0:
        raise AssertionError(f"git {args} failed: {result.stderr}")
    return result.stdout


def listing(entries):
    """A `cargo nextest list` JSON for `{binary_id: {test: status or 'ignored'}}`."""
    suites = {}
    for binary_id, tests in entries.items():
        cases = {}
        for name, status in tests.items():
            cases[name] = {
                "kind": "test",
                "ignored": status == "ignored",
                "filter-match": {"status": "mismatch", "reason": "default-filter"}
                if status == "mismatch" else {"status": "matches"},
            }
        suites[binary_id] = {"testcases": cases}
    return json.dumps({"rust-suites": suites})


class Fixture(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="stress-changed-")).resolve()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.root = self.tmp / "repo"
        self.root.mkdir()
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        fake = self.bin / "cargo"
        fake.write_text(FAKE_CARGO)
        fake.chmod(0o755)
        self.log = self.tmp / "run.log"
        git(self.root, "init", "-q", "-b", "main")
        self.write("crates/demo/src/lib.rs", LIB_BASE)
        self.write("crates/demo/tests/flow.rs", FLOW_BASE)
        self.write("crates/cli/tests/all.rs", CLI_ALL)
        self.write("crates/cli/tests/mod_a.rs", CLI_MOD_A)
        self.write("crates/demo/src/orphan.rs", "#[test]\nfn orphan_case() {}\n")
        self.write("README.md", "fixture\n")
        git(self.root, "add", "-A")
        git(self.root, "commit", "-q", "-m", "base")
        git(self.root, "checkout", "-q", "-b", "feature")
        self.write_metadata()

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def edit(self, rel, old, new):
        path = self.root / rel
        text = path.read_text()
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1))

    def commit(self, message="change"):
        git(self.root, "add", "-A")
        git(self.root, "commit", "-q", "-m", message)

    def write_metadata(self, extra_targets=(), extra_packages=()):
        def target(name, kind, rel):
            return {"name": name, "kind": [kind], "test": True,
                    "src_path": str(self.root / rel)}

        packages = [
            {"name": "demo", "targets": [
                target("demo", "lib", "crates/demo/src/lib.rs"),
                target("flow", "test", "crates/demo/tests/flow.rs"),
                *extra_targets]},
            {"name": "pohunek-cli", "targets": [
                target("all", "test", "crates/cli/tests/all.rs")]},
            *extra_packages,
        ]
        (self.tmp / "metadata.json").write_text(json.dumps({"packages": packages}))

    def run_script(self, *extra, base="main", list_json=None, run_rc=0, profile="heavy",
                   extra_env=None):
        if list_json is not None:
            (self.tmp / "list.json").write_text(list_json)
        env = dict(os.environ, PATH=f"{self.bin}{os.pathsep}{os.environ['PATH']}",
                   FAKE_METADATA=str(self.tmp / "metadata.json"),
                   FAKE_LIST=str(self.tmp / "list.json"),
                   FAKE_LOG=str(self.log), FAKE_RUN_RC=str(run_rc))
        env.update(extra_env or {})
        argv = [sys.executable, str(SCRIPT), "--base", base, "--count", "3",
                "--deadline", "60s", "--profile", profile, *extra]
        if not LINUX:
            argv.append("--unpinned")
        return subprocess.run(argv, cwd=self.root, capture_output=True, text=True,
                              env=env, timeout=WAIT_SECONDS)

    def selection(self, *extra):
        result = self.run_script("--print", *extra)
        self.assertEqual(result.returncode, 0, result.stderr)
        names = []
        for line in result.stdout.splitlines():
            if line.startswith("filter:"):
                continue
            binary_id, name = line.split()[:2]
            names.append((binary_id, name))
        return sorted(names), result


class SelectionRule(Fixture):
    def test_an_added_test_is_selected(self):
        self.edit("crates/demo/src/lib.rs", "    #[test]\n    fn gets_moved() {",
                  "    #[test]\n    fn added_case() {\n        assert!(true);\n    }\n\n"
                  "    #[test]\n    fn gets_moved() {")
        self.commit()
        names, _ = self.selection()
        self.assertEqual(names, [("demo", "tests::added_case")])

    def test_a_changed_body_selects_only_that_test(self):
        self.edit("crates/demo/src/lib.rs", "assert_eq!(x, 1);", "assert_eq!(x + 0, 1);")
        self.commit()
        names, _ = self.selection()
        self.assertEqual(names, [("demo", "tests::body_edit")])

    def test_a_comment_edit_inside_a_test_selects_it(self):
        self.edit("crates/demo/src/lib.rs", "// original comment", "// reworded comment")
        self.commit()
        names, _ = self.selection()
        self.assertEqual(names, [("demo", "tests::comment_edit")])

    def test_a_deleted_line_inside_a_body_selects_the_test(self):
        self.edit("crates/demo/src/lib.rs", "        let x = 1;\n", "")
        self.commit()
        names, _ = self.selection()
        self.assertEqual(names, [("demo", "tests::body_edit")])

    def test_a_moved_test_is_selected_and_a_deleted_test_is_not(self):
        path = self.root / "crates/demo/src/lib.rs"
        text = path.read_text()
        moved = "    #[test]\n    fn gets_moved() {\n        assert!(true);\n    }\n"
        deleted = "    #[test]\n    fn gets_deleted() {\n        assert!(true);\n    }\n\n"
        self.assertIn(moved, text)
        self.assertIn(deleted, text)
        text = text.replace(deleted, "").replace("\n" + moved, "")
        text = text.replace(
            "    #[test]\n    fn untouched() {", moved + "\n    #[test]\n    fn untouched() {", 1)
        path.write_text(text)
        self.commit()
        names, _ = self.selection()
        self.assertEqual(names, [("demo", "tests::gets_moved")])

    def test_removing_the_first_attribute_line_selects_the_test(self):
        self.edit("crates/demo/src/lib.rs", "    #[test]\n    fn gets_moved() {",
                  "    #[ignore]\n    #[test]\n    fn gets_moved() {")
        self.commit("ignore")
        self.edit("crates/demo/src/lib.rs", "    #[ignore]\n", "")
        self.commit("enable")
        result = self.run_script("--print", base="HEAD~1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("tests::gets_moved", result.stdout)

    def test_a_change_outside_every_test_selects_nothing(self):
        self.edit("crates/demo/src/lib.rs", "pub fn helper() -> u32 {\n    1\n}",
                  "pub fn helper() -> u32 {\n    2\n}")
        self.commit()
        result = self.run_script("--print")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("nothing to stress", result.stdout)

    def test_uncommitted_and_untracked_changes_are_included(self):
        self.edit("crates/demo/src/lib.rs", "// original comment", "// working tree edit")
        self.write("crates/demo/tests/flow.rs", FLOW_BASE + "\n#[test]\nfn fresh_case() {}\n")
        self.write("crates/demo/tests/fresh.rs", "#[test]\nfn untracked_case() {}\n")
        self.write_metadata([{"name": "fresh", "kind": ["test"], "test": True,
                              "src_path": str(self.root / "crates/demo/tests/fresh.rs")}])
        names, _ = self.selection()
        self.assertEqual(names, [
            ("demo", "tests::comment_edit"),
            ("demo::flow", "fresh_case"),
            ("demo::fresh", "untracked_case"),
        ])

    def test_a_non_rust_change_selects_nothing(self):
        self.write("README.md", "changed\n")
        result = self.run_script("--print")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("nothing to stress", result.stdout)


class Identity(Fixture):
    def test_a_cli_integration_module_maps_into_the_all_binary(self):
        self.edit("crates/cli/tests/mod_a.rs", "fn cli_case() {\n    assert!(true);",
                  "fn cli_case() {\n    assert!(1 == 1);")
        self.edit("crates/cli/tests/mod_a.rs", "assert!(true);\n    }\n}",
                  "assert!(2 == 2);\n    }\n}")
        self.commit()
        names, result = self.selection()
        self.assertEqual(names, [
            ("pohunek-cli::all", "mod_a::cli_case"),
            ("pohunek-cli::all", "mod_a::nested::deep_case"),
        ])
        self.assertIn("binary(=all)", result.stdout)
        self.assertIn("test(=mod_a::cli_case)", result.stdout)
        self.assertIn("test(=mod_a::nested::deep_case)", result.stdout)

    def test_a_lib_test_and_an_integration_test_get_distinct_filters(self):
        self.edit("crates/demo/src/lib.rs", "// original comment", "// edit")
        self.edit("crates/demo/tests/flow.rs", "assert!(true);", "assert!(!false);")
        self.commit()
        names, result = self.selection()
        self.assertEqual(names, [("demo", "tests::comment_edit"), ("demo::flow", "flow_case")])
        self.assertIn("package(=demo) and kind(lib) and (test(=tests::comment_edit))",
                      result.stdout)
        self.assertIn("package(=demo) and kind(test) and binary(=flow) and (test(=flow_case))",
                      result.stdout)


class Unmappable(Fixture):
    def assert_fails_naming(self, name):
        result = self.run_script("--print")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(name, result.stderr)
        return result

    def test_a_test_in_a_file_no_target_reaches_fails_by_name(self):
        self.edit("crates/demo/src/orphan.rs", "fn orphan_case() {}", "fn orphan_case() { }  ")
        self.commit()
        result = self.assert_fails_naming("orphan_case")
        self.assertIn("no cargo target reaches", result.stderr)

    def test_a_test_inside_a_function_body_fails_by_name(self):
        self.edit("crates/demo/src/lib.rs", "pub fn helper() -> u32 {\n    1\n}",
                  "pub fn helper() -> u32 {\n    #[test]\n    fn buried_case() {}\n    1\n}")
        self.commit()
        self.assert_fails_naming("buried_case")

    def test_a_generated_case_attribute_fails_by_name(self):
        self.edit("crates/demo/src/lib.rs", "    #[test]\n    fn gets_moved() {",
                  "    #[rstest]\n    fn param_case() {}\n\n    #[test]\n    fn gets_moved() {")
        self.commit()
        self.assert_fails_naming("param_case")

    def test_one_unmappable_test_fails_even_with_mappable_ones(self):
        self.edit("crates/demo/src/lib.rs", "// original comment", "// edit")
        self.edit("crates/demo/src/orphan.rs", "fn orphan_case() {}", "fn orphan_case() { }  ")
        self.commit()
        result = self.run_script("--print")
        self.assertEqual(result.returncode, 1)
        self.assertIn("orphan_case", result.stderr)

    def test_an_ignored_test_is_reported_as_skipped(self):
        self.edit("crates/demo/src/lib.rs", "    #[test]\n    fn gets_moved() {",
                  "    #[test]\n    #[ignore = \"needs a real upstream\"]\n    fn slow_case() {}\n\n"
                  "    #[test]\n    fn gets_moved() {")
        self.commit()
        result = self.run_script("--print")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("slow_case", result.stderr)
        self.assertIn("skipped", result.stderr)
        self.assertIn("nothing to stress", result.stdout)


class Listing(Fixture):
    def setUp(self):
        super().setUp()
        self.edit("crates/demo/src/lib.rs", "// original comment", "// edit")
        self.commit()

    def test_a_test_missing_from_the_nextest_list_fails_by_name(self):
        result = self.run_script(list_json=listing({"demo": {"tests::other": "matches"}}))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("tests::comment_edit", result.stderr)
        self.assertIn("not built on this host", result.stderr)

    def test_a_test_excluded_by_the_profile_fails_by_name(self):
        result = self.run_script(
            list_json=listing({"demo": {"tests::comment_edit": "mismatch"}}), profile="fast")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("tests::comment_edit", result.stderr)
        self.assertIn("excluded by profile 'fast'", result.stderr)
        self.assertNotIn('"run"', self.log.read_text(), "no stress run may start")

    def test_the_stress_run_uses_the_exact_filter_without_retries(self):
        result = self.run_script(
            list_json=listing({"demo": {"tests::comment_edit": "matches"}}))
        self.assertEqual(result.returncode, 0, result.stderr)
        recorded = [json.loads(line) for line in self.log.read_text().splitlines()]
        self.assertEqual([r[1] for r in recorded], ["list", "run"])
        # The list and the run build with CI's feature set.
        for argv in recorded:
            self.assertIn("--all-features", argv)
        argv = recorded[1]
        self.assertEqual(argv[:7], ["nextest", "run", "--profile", "heavy", "--all-features",
                                    "-p", "demo"])
        self.assertEqual(argv[argv.index("--stress-count") + 1], "3")
        self.assertEqual(argv[argv.index("--retries") + 1], "0")
        self.assertIn("--no-tests=fail", argv)
        self.assertEqual(
            argv[argv.index("-E") + 1],
            "(package(=demo) and kind(lib) and (test(=tests::comment_edit)))")
        self.assertIn("cpus=", result.stderr)

    def test_a_failing_stress_run_fails_the_gate(self):
        result = self.run_script(
            list_json=listing({"demo": {"tests::comment_edit": "matches"}}), run_rc=100)
        self.assertEqual(result.returncode, 100, result.stderr)


MACRO_BLOCK = '''    macro_rules! gen_case {
        ($name:ident) => {
            #[test]
            fn $name() {
                assert!(true);
            }
        };
    }

    gen_case!(generated_one);

'''


class GeneratedTests(Fixture):
    def setUp(self):
        super().setUp()
        self.edit("crates/demo/src/lib.rs", "    #[test]\n    fn middle_case() {",
                  MACRO_BLOCK + "    #[test]\n    fn middle_case() {")
        self.commit("add macro")

    def test_a_changed_macro_template_fails_by_name(self):
        self.edit("crates/demo/src/lib.rs", "            fn $name() {\n                assert!(true);",
                  "            fn $name() {\n                assert!(1 == 1);")
        self.commit()
        self.assert_macro_failure_from("HEAD~1")

    def test_an_added_invocation_fails_by_name(self):
        self.edit("crates/demo/src/lib.rs", "    gen_case!(generated_one);",
                  "    gen_case!(generated_one);\n    gen_case!(generated_two);")
        self.commit()
        self.assert_macro_failure_from("HEAD~1")

    def test_an_unrelated_change_next_to_the_macro_is_not_a_failure(self):
        self.edit("crates/demo/src/lib.rs", "pub fn helper() -> u32 {\n    1\n}",
                  "pub fn helper() -> u32 {\n    3\n}")
        self.commit()
        result = self.run_script("--print", base="HEAD~1")
        self.assertEqual(result.returncode, 0, result.stderr)

    def assert_macro_failure_from(self, base):
        result = self.run_script("--print", base=base)
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("gen_case!", result.stderr)
        self.assertIn("generates tests", result.stderr)


class DeletionRule(Fixture):
    def test_deleting_a_middle_test_does_not_select_its_neighbour(self):
        self.edit("crates/demo/src/lib.rs",
                  "    #[test]\n    fn middle_case() {\n        assert!(true);\n    }\n\n", "")
        self.commit()
        result = self.run_script("--print")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("nothing to stress", result.stdout)

    def remove_attribute_line(self, attribute_line):
        """Add `attribute_line` above `middle_case` in one commit, remove it in the next."""
        marker = "    #[test]\n    fn middle_case() {"
        self.edit("crates/demo/src/lib.rs", marker, f"    {attribute_line}\n{marker}")
        self.commit("add attribute")
        self.edit("crates/demo/src/lib.rs", f"    {attribute_line}\n", "")
        self.commit("remove attribute")
        result = self.run_script("--print", base="HEAD~1")
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def test_removing_an_attribute_line_with_a_trailing_comment_selects_the_test(self):
        result = self.remove_attribute_line("#[ignore] // flaky on slow hosts")
        self.assertIn("tests::middle_case", result.stdout)

    def test_removing_an_attribute_with_a_url_in_its_string_selects_the_test(self):
        result = self.remove_attribute_line('#[ignore = "see https://example.invalid/issue/1"]')
        self.assertIn("tests::middle_case", result.stdout)

    def test_a_url_in_the_string_and_a_trailing_comment_are_both_handled(self):
        result = self.remove_attribute_line(
            '#[ignore = "see https://example.invalid/issue/1"] // reason')
        self.assertIn("tests::middle_case", result.stdout)

    def test_removing_an_attribute_whose_string_spans_lines_selects_the_test(self):
        result = self.remove_attribute_line(
            '#[ignore = "flaky, see\n    https://example.invalid/issue/1"]')
        self.assertIn("tests::middle_case", result.stdout)

    def test_removing_a_code_line_with_a_trailing_comment_selects_nothing_below(self):
        # Only whole attribute lines select the test below; a removed statement
        # that ends in a comment does not.
        self.edit("crates/demo/src/lib.rs", "pub fn helper() -> u32 {\n    1\n}",
                  "pub fn helper() -> u32 {\n    1\n}\n\nconst NOTE: u32 = 2; // note")
        self.commit("add const")
        self.edit("crates/demo/src/lib.rs", "\nconst NOTE: u32 = 2; // note", "")
        self.commit("remove const")
        result = self.run_script("--print", base="HEAD~1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("tests::", result.stdout)


class HostShare(Fixture):
    def setUp(self):
        super().setUp()
        self.edit("crates/demo/src/lib.rs", "// original comment", "// edit")
        self.write("crates/demo/tests/flow.rs", "#[cfg(all())]\n" + FLOW_BASE.replace(
            "assert!(true);", "assert!(!false);"))
        self.commit()

    def test_a_missing_test_in_a_file_without_a_cfg_still_fails_under_the_flag(self):
        # No cfg could have compiled the test out, so a missing name is a wrong identity.
        self.edit("crates/demo/tests/flow.rs", "#[cfg(all())]\n", "")
        self.commit("drop cfg")
        result = self.run_script(
            "--allow-missing-on-host",
            list_json=listing({"demo": {"tests::comment_edit": "matches"}}))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("flow_case", result.stderr)

    def test_a_test_the_host_does_not_build_fails_by_default(self):
        result = self.run_script(list_json=listing({"demo": {"tests::comment_edit": "matches"}}))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("flow_case", result.stderr)
        self.assertIn("--allow-missing-on-host", result.stderr)

    def test_the_opt_in_flag_names_it_and_stresses_the_rest(self):
        result = self.run_script(
            "--allow-missing-on-host",
            list_json=listing({"demo": {"tests::comment_edit": "matches"}}))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("NOT STRESSED on this host", result.stderr)
        self.assertIn("flow_case", result.stderr)
        argv = [json.loads(line) for line in self.log.read_text().splitlines()][-1]
        expression = argv[argv.index("-E") + 1]
        self.assertIn("tests::comment_edit", expression)
        self.assertNotIn("flow_case", expression)

    def test_nothing_runs_when_the_host_builds_none_of_them(self):
        self.gate_for_another_os("comment_edit")
        result = self.run_script("--allow-missing-on-host", list_json=listing({}))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("NOT STRESSED", result.stderr)
        self.assertIn("nothing stressed here", result.stdout)
        run_calls = [line for line in self.log.read_text().splitlines() if '"run"' in line]
        self.assertEqual(run_calls, [])


    def gate_for_another_os(self, test_name):
        """Mark `tests::<test_name>` in the demo lib as built only on another OS."""
        self.edit("crates/demo/src/lib.rs", f"    #[test]\n    fn {test_name}() {{",
                  f"    #[cfg(target_os = \"plan9\")]\n    #[test]\n    fn {test_name}() {{")
        self.commit("gate for another os")

    def test_a_cfg_test_only_module_fails_under_the_flag(self):
        # `#[cfg(test)] mod tests` is compiled in every test build, so a missing
        # name there is a wrong identity, not another host's share.
        result = self.run_script("--allow-missing-on-host", list_json=listing({}))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("NOT STRESSED on this host", result.stderr)
        self.assertIn("flow_case", result.stderr)
        self.assertIn("tests::comment_edit", result.stderr)
        self.assertIn("is not in the nextest list", result.stderr)

    def test_the_list_and_the_run_carry_the_same_package_set(self):
        self.edit("crates/cli/tests/mod_a.rs", "#[test]\nfn cli_case() {\n    assert!(true);",
                  "#[cfg(target_os = \"plan9\")]\n#[test]\nfn cli_case() {\n    assert!(false);")
        self.commit("gated cli test")
        result = self.run_script(
            "--allow-missing-on-host",
            list_json=listing({"demo": {"tests::comment_edit": "matches"},
                               "demo::flow": {"flow_case": "matches"}}))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("mod_a::cli_case", result.stderr)
        self.assertIn("NOT STRESSED", result.stderr)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        list_call = next(c for c in calls if c[:2] == ["nextest", "list"])
        run_call = next(c for c in calls if c[:2] == ["nextest", "run"])
        self.assertEqual(self.packages(list_call), ["demo", "pohunek-cli"])
        self.assertEqual(self.packages(run_call), self.packages(list_call))
        expression = run_call[run_call.index("-E") + 1]
        self.assertNotIn("cli_case", expression)

    @staticmethod
    def packages(argv):
        return [argv[i + 1] for i, word in enumerate(argv) if word == "-p"]


class DeclaringChain(Fixture):
    """Which `cfg` on the way to a test lets `--allow-missing-on-host` accept its absence."""

    def selected_missing_with_flag(self, base="HEAD~1"):
        return self.run_script("--allow-missing-on-host", base=base, list_json=listing({}))

    def test_a_parent_mod_declaration_for_another_os_is_not_stressed_here(self):
        self.write("crates/demo/src/gated.rs", "#[test]\nfn gated_case() {\n    assert!(true);\n}\n")
        self.edit("crates/demo/src/lib.rs", "pub fn helper()",
                  "#[cfg(target_os = \"plan9\")]\nmod gated;\n\npub fn helper()")
        self.commit("add gated module")
        self.edit("crates/demo/src/gated.rs", "assert!(true);", "assert!(1 == 1);")
        self.commit("edit gated test")
        result = self.selected_missing_with_flag()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("NOT STRESSED on this host", result.stderr)
        self.assertIn("gated::gated_case", result.stderr)

    def test_a_grandparent_cfg_reaches_a_nested_module(self):
        self.write("crates/demo/src/outer.rs", "mod inner;\n")
        self.write("crates/demo/src/outer/inner.rs",
                   "#[test]\nfn inner_case() {\n    assert!(true);\n}\n")
        self.edit("crates/demo/src/lib.rs", "pub fn helper()",
                  "#[cfg(unix)]\nmod outer;\n\npub fn helper()")
        self.commit("add nested modules")
        self.edit("crates/demo/src/outer/inner.rs", "assert!(true);", "assert!(1 == 1);")
        self.commit("edit nested test")
        result = self.selected_missing_with_flag()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("outer::inner::inner_case", result.stderr)

    def test_an_inner_cfg_attribute_of_the_file_counts(self):
        self.write("crates/demo/src/inner_cfg.rs",
                   "#![cfg(target_os = \"plan9\")]\n\n#[test]\nfn inner_cfg_case() {\n    assert!(true);\n}\n")
        self.edit("crates/demo/src/lib.rs", "pub fn helper()", "mod inner_cfg;\n\npub fn helper()")
        self.commit("add module")
        self.edit("crates/demo/src/inner_cfg.rs", "assert!(true);", "assert!(1 == 1);")
        self.commit("edit test")
        result = self.selected_missing_with_flag()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("inner_cfg::inner_cfg_case", result.stderr)

    def test_an_inline_module_for_another_os_counts(self):
        self.edit("crates/demo/src/lib.rs", "    #[test]\n    fn middle_case() {",
                  "    #[cfg(windows)]\n    mod windows_only {\n        #[test]\n"
                  "        fn win_case() {\n            assert!(true);\n        }\n    }\n\n"
                  "    #[test]\n    fn middle_case() {")
        self.commit("add inline module")
        self.edit("crates/demo/src/lib.rs", "fn win_case() {\n            assert!(true);",
                  "fn win_case() {\n            assert!(1 == 1);")
        self.commit("edit inline test")
        result = self.selected_missing_with_flag()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("tests::windows_only::win_case", result.stderr)

    def test_forms_that_only_add_test_do_not_count_but_any_with_test_does(self):
        for index, (predicate, accepted) in enumerate((
                ("all(test)", False), ("all(test, test)", False),
                ("any(test, target_os = \"plan9\")", True),
                ("all(test, unix)", True), ("not(test)", True))):
            with self.subTest(predicate=predicate):
                name = f"form_{index}"
                self.write(f"crates/demo/src/{name}.rs",
                           f"#[cfg({predicate})]\n#[test]\nfn {name}_case() {{\n    assert!(true);\n}}\n")
                self.edit("crates/demo/src/lib.rs", "pub fn helper()",
                          f"mod {name};\n\npub fn helper()")
                self.commit(f"add {name}")
                self.edit(f"crates/demo/src/{name}.rs", "assert!(true);", "assert!(1 == 1);")
                self.commit(f"edit {name}")
                result = self.selected_missing_with_flag()
                self.assertEqual(result.returncode, 0 if accepted else 1, result.stderr)
                self.assertIn(f"{name}::{name}_case", result.stderr)
                self.assertIn("NOT STRESSED" if accepted else "is not in the nextest list",
                              result.stderr)


class MultiLineAttribute(Fixture):
    def test_removing_a_multi_line_first_attribute_selects_the_test(self):
        self.edit("crates/demo/src/lib.rs", "    #[test]\n    fn middle_case() {",
                  "    #[cfg_attr(\n        target_os = \"macos\",\n        ignore = \"flaky\"\n"
                  "    )]\n    #[test]\n    fn middle_case() {")
        self.commit("gate")
        self.edit("crates/demo/src/lib.rs",
                  "    #[cfg_attr(\n        target_os = \"macos\",\n        ignore = \"flaky\"\n    )]\n", "")
        self.commit("enable")
        result = self.run_script("--print", base="HEAD~1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("tests::middle_case", result.stdout)


class BuildFeatures(Fixture):
    def feature_flags_for(self, profile):
        self.edit("crates/demo/src/lib.rs", "// original comment", "// edit")
        self.commit()
        result = self.run_script(
            list_json=listing({"demo": {"tests::comment_edit": "matches"}}), profile=profile)
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        return ["--all-features" in call for call in calls
                if call[:2] in (["nextest", "list"], ["nextest", "run"])]

    def test_a_linux_profile_builds_all_features_in_the_list_and_the_run(self):
        self.assertEqual(self.feature_flags_for("heavy"), [True, True])

    def test_the_macos_profile_builds_default_features_like_ci(self):
        self.assertEqual(self.feature_flags_for("macos"), [False, False])


class Arguments(Fixture):
    def test_base_count_deadline_and_profile_are_required(self):
        base = [sys.executable, str(SCRIPT)]
        full = ["--base", "main", "--count", "3", "--deadline", "60s", "--profile", "heavy"]
        for index, flag in ((0, "--base"), (2, "--count"), (4, "--deadline"), (6, "--profile")):
            argv = full[:index] + full[index + 2:]
            result = subprocess.run(base + argv, cwd=self.root, capture_output=True,
                                    text=True, timeout=WAIT_SECONDS)
            self.assertEqual(result.returncode, 2, flag)
            self.assertIn(flag, result.stderr)

    def test_help_names_the_lane_counts_and_the_rule(self):
        result = subprocess.run([sys.executable, str(SCRIPT), "--help"], capture_output=True,
                                text=True, timeout=WAIT_SECONDS)
        self.assertEqual(result.returncode, 0)
        for token in ("heavy", "50", "moved", "deleted"):
            self.assertIn(token, result.stdout)


def unit_slow_list():
    """Direct `entry_matches` probes: the daemon regex also matches
    a selected name only when the package guard allows it."""
    import importlib.machinery
    import importlib.util
    loader = importlib.machinery.SourceFileLoader(
        "stress_changed_unit",
        str(Path(__file__).resolve().parents[1] / "stress-changed-tests"))
    spec = importlib.util.spec_from_loader("stress_changed_unit", loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    daemon = module.Target("pohunek-daemon", "pohunek-daemon", "lib",
                           Path("crates/daemon/src/lib.rs"))
    regex = {"filter": "package(=pohunek-daemon) and test(/::running_loss::/)",
             "reason": "probe"}
    assert module.entry_matches(
        daemon, "session::reconcile::tests::running_loss::pinned_case", regex)
    demo = module.Target("demo", "demo", "lib", Path("crates/demo/src/lib.rs"))
    assert not module.entry_matches(
        demo, "session::reconcile::tests::running_loss::pinned_case", regex)
    return listing({
        "demo": {"tests::comment_edit": "matches",
                   "session::reconcile::tests::running_loss::pinned_case": "matches"}})


class SlowRunner(Fixture):
    """`--slow-runner` wraps the run in strace sync-delay injection (#2213)."""
    def setUp(self):
        super().setUp()
        (self.bin / "stress-run").unlink(missing_ok=True)
        fake_stress_run = self.bin / "stress-run"
        fake_stress_run.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "with open(os.environ['FAKE_LOG'], 'a') as stream:\n"
            "    stream.write(json.dumps(['stress-run'] + sys.argv[1:]) + '\\n')\n"
            "sys.exit(int(os.environ.get('FAKE_STRESS_RUN_RC', '0')))\n")
        fake_stress_run.chmod(0o755)
        fake_strace = self.bin / "strace"
        fake_strace.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "if sys.argv[1:] == ['-V']:\n"
            "    print('strace -- version 7.3 (fake)')\n"
            "    sys.exit(0)\n"
            "with open(os.environ['FAKE_LOG'], 'a') as stream:\n"
            "    stream.write(json.dumps(['strace'] + sys.argv[1:]) + '\\n')\n"
            "sys.exit(int(os.environ.get('FAKE_STRACE_RC', '0')))\n")
        fake_strace.chmod(0o755)
        self.edit("crates/demo/src/lib.rs", "// original comment", "// edit")
        self.commit()
        self.list_json = listing({"demo": {"tests::comment_edit": "matches"}})

    def slow_argv(self, *extra, list_json=None, **kwargs):
        listed_arg = self.list_json if list_json is None else list_json
        result = self.run_script("--slow-runner", *extra, list_json=listed_arg, **kwargs)
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        listed = next(call for call in calls if call[:2] == ["nextest", "list"])
        run = next(call for call in calls if call[0] == "stress-run")
        self.assertEqual(run[1:5], ["--deadline", "60s", "--cpus", "3"])
        self.assertIn("--", run)
        return (listed, run[run.index("strace"):]), result

    def untraced_filter_union(self):
        return " or ".join(
            entry["filter"] for entry in json.loads(
                (Path(__file__).resolve().parents[1]
                 / "stress-slow-runner-untraced.json").read_text()))

    def test_a_mixed_selection_runs_the_traced_and_the_untraced_run(self):
        listed = self.mixed_root()
        self.log.unlink(missing_ok=True)
        result = self.run_script("--slow-runner", list_json=listed)
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        runs = [call for call in calls if call[0] == "stress-run"]
        self.assertEqual(len(runs), 2)
        traced_run = next(call for call in runs if "strace" in call)
        untraced_run = next(call for call in runs if "strace" not in call)
        self.assertIn("and not", " ".join(traced_run))
        self.assertIn("running_loss", " ".join(untraced_run))
        self.assertIn("NOT SLOWED session::reconcile::tests::running_loss::pinned_case",
                      result.stderr)
        self.assertIn("SLOWED tests::comment_edit", result.stderr)

    def test_the_slow_run_wraps_the_same_nextest_argv_in_strace(self):
        (listed, run), result = self.slow_argv()
        self.assertEqual(listed[1], "list")
        self.assertEqual(run[0], "strace")
        self.assertEqual(
            run[:8],
            ["strace", "-f", "--seccomp-bpf", "-qq", "-o", "/dev/null",
             "--trace=fsync,fdatasync", "--inject=fsync,fdatasync:delay_enter=8300"])
        stress = [line for line in result.stderr.splitlines() if "stress-run" in line]
        self.assertEqual(len(stress), 1)
        self.assertIn("--cpus 3", stress[0])
        self.assertIn("strace", stress[0])
        self.assertIn("strace -- version", result.stderr)
        self.assertIn("--all-features", listed)
        argv = run[run.index("cargo"):]
        self.assertEqual(argv[:7], ["cargo", "nextest", "run", "--profile", "heavy",
                                    "--all-features", "-p"])
        self.assertEqual(argv[argv.index("--stress-count") + 1], "3")
        self.assertEqual(argv[argv.index("--retries") + 1], "0")
        self.assertEqual(
            argv[argv.index("-E") + 1],
            "((package(=demo) and kind(lib) and (test(=tests::comment_edit)))) "
            f"and not ({self.untraced_filter_union()})")
        self.assertIn("SLOWED", result.stderr)
        self.assertIn("tests::comment_edit", result.stderr)

    def mixed_root(self):
        daemon = self.root / "crates/daemon/src"
        daemon.mkdir(parents=True)
        (daemon / "lib.rs").write_text(
            "#[cfg(test)]\nmod session {\n"
            "    #[cfg(test)]\n    pub mod reconcile {\n"
            "        #[cfg(test)]\n        pub mod tests {\n"
            "            #[cfg(test)]\n            pub mod running_loss {\n"
            "                #[test]\n                pub fn pinned_case() {\n"
            "                    assert!(true);\n                }\n            }\n        }\n    }\n}\n")
        self.write_metadata(extra_packages=({
            "name": "pohunek-daemon", "targets": [{
                "name": "pohunek-daemon", "kind": ["lib"], "test": True,
                "src_path": str(daemon / "lib.rs")}]},))
        (daemon / "lib.rs").write_text(
            (daemon / "lib.rs").read_text() + "\n// touch running_loss\n")
        self.commit("add running_loss test")
        return listing({
            "demo": {"tests::comment_edit": "matches",
                       "session::reconcile::tests::running_loss::pinned_case": "matches"}})

    def test_the_untraced_matcher_pairs_package_and_pattern(self):
        unit_slow_list()

    def test_a_selection_on_the_list_runs_without_strace(self):
        listed = self.mixed_root()
        self.log.unlink(missing_ok=True)
        probe = self.run_script("--slow-runner", "--print", list_json=listed)
        self.assertEqual(probe.returncode, 0, probe.stderr)
        self.assertIn("NOT SLOWED", probe.stdout)
        self.assertIn("running_loss", probe.stdout)
        result = self.run_script("--slow-runner", list_json=listed)
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        runs = [call for call in calls if call[0] == "stress-run"]
        self.assertEqual(len(runs), 2)
        untraced_run = next(call for call in runs if "strace" not in call)
        self.assertIn("running_loss", " ".join(untraced_run))
        self.assertIn("NOT SLOWED session::reconcile::tests::running_loss::pinned_case",
                      result.stderr)

    def test_a_failing_slow_run_fails_the_gate(self):
        result = self.run_script("--slow-runner", list_json=self.list_json,
                                 extra_env={"FAKE_STRESS_RUN_RC": "100"})
        self.assertEqual(result.returncode, 100, result.stderr)

    def test_slow_runner_with_unpinned_is_a_usage_error(self):
        result = self.run_script("--slow-runner", "--unpinned", list_json=self.list_json)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("--unpinned", result.stderr)

    def test_slow_runner_without_strace_is_a_usage_error(self):
        (self.tmp / "list.json").write_text(self.list_json)
        (self.bin / "strace").write_text("#!/bin/sh\nexit 127\n")
        saved_path = os.environ["PATH"]
        cleared = os.pathsep.join(
            entry for entry in saved_path.split(os.pathsep)
            if Path(entry).resolve() != self.bin.resolve())
        env = dict(os.environ, PATH=f"{self.bin}{os.pathsep}{cleared}",
                   FAKE_METADATA=str(self.tmp / "metadata.json"),
                   FAKE_LIST=str(self.tmp / "list.json"),
                   FAKE_LOG=str(self.log), FAKE_RUN_RC="0")
        argv = [sys.executable, str(SCRIPT), "--base", "main", "--count", "3",
                "--deadline", "60s", "--profile", "heavy", "--slow-runner"]
        result = subprocess.run(argv, cwd=self.root, capture_output=True, text=True,
                                env=env, timeout=WAIT_SECONDS)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("strace", result.stderr)

    def test_slow_runner_deadline_overrun_names_ptrace(self):
        result = self.run_script("--slow-runner", list_json=self.list_json,
                                 extra_env={"FAKE_STRESS_RUN_RC": "124"})
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertIn("ptrace", result.stderr)

    def test_the_untraced_list_entries_carry_evidence(self):
        import importlib.machinery
        import importlib.util
        loader = importlib.machinery.SourceFileLoader(
            "stress_changed", str(Path(__file__).resolve().parents[1] / "stress-changed-tests"))
        spec = importlib.util.spec_from_loader("stress_changed", loader)
        module = importlib.util.module_from_spec(spec)
        loader.exec_module(module)
        entries = module.load_untraced_list()
        self.assertTrue(entries, "the untraced list must not be empty")
        for entry in entries:
            self.assertTrue(entry.get("filter"), entry)
            self.assertTrue(entry.get("reason"), entry)
            self.assertTrue(entry.get("evidence"), entry)


if __name__ == "__main__":
    unittest.main()
