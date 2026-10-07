"""Regression checks for the JUnit flaky-test job summary (stdlib only).

The fixtures under `fixtures/nextest-junit/` are unedited nextest 0.9.145
reports from a throwaway test that failed on attempt 1 and passed on attempt
2, next to a test that failed every attempt, run under profile.heavy
(`retries = 2`). `flaky-result-fail.xml` used the repository policy;
`flaky-result-pass.xml` overrode it with `NEXTEST_FLAKY_RESULT=pass`.
"""

import contextlib
import importlib.machinery
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "junit-flaky-summary"
FIXTURES = Path(__file__).resolve().parent / "fixtures" / "nextest-junit"
FLAKY_FAIL = FIXTURES / "flaky-result-fail.xml"
FLAKY_PASS = FIXTURES / "flaky-result-pass.xml"
LOADER = importlib.machinery.SourceFileLoader("junit_flaky_summary", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
summary = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(summary)

FLAKY_NAME = "pohunek-paths::zz_flaky_sample flaky_first_attempt"
FAILED_NAME = "pohunek-paths::zz_flaky_sample always_fails"


def table_rows(markdown, header):
    """Data rows of the Markdown table whose header row starts with `header`."""
    lines = markdown.splitlines()
    start = next(index for index, line in enumerate(lines) if line.startswith(header))
    rows = []
    for line in lines[start + 2:]:
        if not line.startswith("|"):
            break
        rows.append([cell.strip() for cell in line.strip("|").split(" | ")])
    return rows


class ClassifyTests(unittest.TestCase):
    def test_flaky_result_fail_report(self):
        result, problem = summary.load(FLAKY_FAIL)
        self.assertIsNone(problem)
        total, flaky, failed = result
        self.assertEqual(total, 3)
        self.assertEqual([record["name"] for record in flaky], [FLAKY_NAME])
        self.assertEqual(flaky[0]["attempts"], 2)
        self.assertTrue(flaky[0]["fails_run"])
        self.assertIn("zz_flaky_sample.rs:5:5", flaky[0]["message"])
        # A persistent failure is not flaky: three attempts, all failed.
        self.assertEqual([record["name"] for record in failed], [FAILED_NAME])
        self.assertEqual(failed[0]["attempts"], 3)

    def test_flaky_result_pass_report_is_still_listed_as_flaky(self):
        result, problem = summary.load(FLAKY_PASS)
        self.assertIsNone(problem)
        total, flaky, failed = result
        self.assertEqual(total, 2)
        self.assertEqual([record["name"] for record in flaky], [FLAKY_NAME])
        self.assertFalse(flaky[0]["fails_run"])
        self.assertEqual(failed, [])

    def test_malformed_report_is_a_problem(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "junit.xml"
            path.write_text("<testsuites><testsuite>")
            result, problem = summary.load(path)
        self.assertIsNone(result)
        self.assertIn("unreadable JUnit report", problem)


class RenderTests(unittest.TestCase):
    def test_render_lists_flaky_and_failed_tables(self):
        markdown = summary.render("heavy", [FLAKY_FAIL])
        self.assertIn("### Flaky and failed tests: heavy", markdown)
        self.assertIn("3 test(s) in 1 report(s): 1 flaky, 1 failed.", markdown)
        flaky = table_rows(markdown, "| Flaky test |")
        self.assertEqual(len(flaky), 1)
        self.assertEqual(flaky[0][:3], [FLAKY_NAME, "2", "failure"])
        failed = table_rows(markdown, "| Failed test |")
        self.assertEqual(len(failed), 1)
        self.assertEqual(failed[0][:2], [FAILED_NAME, "3"])

    def test_render_marks_a_flaky_pass_as_counted_pass(self):
        flaky = table_rows(summary.render("heavy", [FLAKY_PASS]), "| Flaky test |")
        self.assertEqual(flaky[0][:3], [FLAKY_NAME, "2", "pass"])

    def test_render_clean_report_has_no_tables(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "junit.xml"
            path.write_text(
                '<testsuites><testsuite name="s">'
                '<testcase name="ok" classname="crate::bin" time="0.1"/>'
                "</testsuite></testsuites>"
            )
            markdown = summary.render("unit", [path])
        self.assertIn("1 test(s) in 1 report(s): 0 flaky, 0 failed.", markdown)
        self.assertNotIn("| Flaky test |", markdown)
        self.assertNotIn("| Failed test |", markdown)

    def test_render_empty_file_is_reported_as_unreadable(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "junit.xml"
            path.write_text("")
            markdown = summary.render("heavy", [path])
        self.assertIn(f"**Warning:** unreadable JUnit report `{path}`", markdown)
        self.assertIn("0 test(s) in 0 report(s): 0 flaky, 0 failed.", markdown)

    def test_render_report_without_testcases(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "junit.xml"
            path.write_text('<testsuites name="nextest-run" tests="0"/>')
            markdown = summary.render("heavy", [path])
        self.assertNotIn("**Warning:**", markdown)
        self.assertIn("0 test(s) in 1 report(s): 0 flaky, 0 failed.", markdown)
        self.assertNotIn("| Flaky test |", markdown)

    def test_render_reports_missing_file_next_to_valid_ones(self):
        missing = FIXTURES / "does-not-exist.xml"
        markdown = summary.render("heavy", [FLAKY_FAIL, missing])
        self.assertIn(f"**Warning:** missing JUnit report `{missing}`", markdown)
        self.assertIn("3 test(s) in 1 report(s)", markdown)

    def test_warning_keeps_a_long_path_whole(self):
        # Longer than MESSAGE_LIMIT, with every component a valid file name.
        missing = Path("/", *(["d" * 100] * 4), "junit.xml")
        markdown = summary.render("heavy", [missing])
        self.assertIn(f"**Warning:** missing JUnit report `{missing}`", markdown)

    def test_path_that_cannot_be_inspected_is_reported(self):
        # One component over NAME_MAX: stat() fails with ENAMETOOLONG.
        unusable = Path("/", "d" * 300, "junit.xml")
        markdown = summary.render("heavy", [unusable, FLAKY_FAIL])
        self.assertIn("**Warning:**", markdown)
        self.assertIn("3 test(s) in 1 report(s)", markdown)

    def test_cell_escapes_table_and_html_syntax(self):
        self.assertEqual(summary.cell("a | b\n<c>"), "a \\| b &lt;c&gt;")

    def test_cell_truncates_long_messages(self):
        rendered = summary.cell("x" * (summary.MESSAGE_LIMIT * 2))
        self.assertEqual(len(rendered), summary.MESSAGE_LIMIT)


class MainTests(unittest.TestCase):
    def test_appends_to_step_summary(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "summary.md"
            target.write_text("earlier step\n")
            code = summary.main(
                ["--label", "heavy", str(FLAKY_FAIL)],
                environ={summary.SUMMARY_ENV: str(target)},
            )
            written = target.read_text()
        self.assertEqual(code, 0)
        self.assertTrue(written.startswith("earlier step\n### Flaky and failed tests"))

    def test_missing_report_still_exits_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "summary.md"
            code = summary.main(
                ["--label", "heavy", str(Path(directory) / "junit.xml")],
                environ={summary.SUMMARY_ENV: str(target)},
            )
            written = target.read_text()
        self.assertEqual(code, 0)
        self.assertIn("missing JUnit report", written)

    def test_stdout_mode_ignores_step_summary(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            code = summary.main(["--stdout", "--label", "heavy", str(FLAKY_FAIL)], environ={})
        self.assertEqual(code, 0)
        self.assertIn(FLAKY_NAME, output.getvalue())

    def test_unset_step_summary_is_a_usage_error(self):
        with contextlib.redirect_stderr(io.StringIO()) as stderr:
            with self.assertRaises(SystemExit) as raised:
                summary.main(["--label", "heavy", str(FLAKY_FAIL)], environ={})
        self.assertEqual(raised.exception.code, 2)
        self.assertIn("GITHUB_STEP_SUMMARY is not set", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
