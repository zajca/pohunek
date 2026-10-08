"""CLI integration coverage for the nextest JUnit job summary.

The captured reports under `fixtures/nextest-junit/` came from nextest
0.9.145 with a flaky test and a persistent failure under the heavy profile.
"""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "junit-flaky-summary"
FIXTURES = Path(__file__).resolve().parent / "fixtures" / "nextest-junit"
FLAKY_FAIL = FIXTURES / "flaky-result-fail.xml"
FLAKY_PASS = FIXTURES / "flaky-result-pass.xml"
FLAKY_NAME = "pohunek-paths::zz_flaky_sample flaky_first_attempt"
FAILED_NAME = "pohunek-paths::zz_flaky_sample always_fails"


def run_summary(*arguments, summary_path=None):
    """Run the same script entry point and environment used by CI."""
    environment = os.environ.copy()
    environment.pop("GITHUB_STEP_SUMMARY", None)
    if summary_path is not None:
        environment["GITHUB_STEP_SUMMARY"] = str(summary_path)
    return subprocess.run(
        [sys.executable, str(SCRIPT), "--label", "heavy", *map(str, arguments)],
        capture_output=True,
        text=True,
        check=False,
        env=environment,
    )


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


class JunitSummaryCliTests(unittest.TestCase):
    def test_flaky_and_failed_reports_are_classified_and_rendered(self):
        failed_run = run_summary("--stdout", FLAKY_FAIL)
        self.assertEqual(failed_run.returncode, 0, failed_run.stderr)
        self.assertIn("### Flaky and failed tests: heavy", failed_run.stdout)
        self.assertIn("3 test(s) in 1 report(s): 1 flaky, 1 failed.", failed_run.stdout)
        self.assertEqual(
            table_rows(failed_run.stdout, "| Flaky test |")[0][:3],
            [FLAKY_NAME, "2", "failure"],
        )
        self.assertEqual(
            table_rows(failed_run.stdout, "| Failed test |")[0][:2],
            [FAILED_NAME, "3"],
        )
        self.assertIn("zz_flaky_sample.rs:5:5", failed_run.stdout)

        passing_run = run_summary("--stdout", FLAKY_PASS)
        self.assertEqual(passing_run.returncode, 0, passing_run.stderr)
        self.assertIn("2 test(s) in 1 report(s): 1 flaky, 0 failed.", passing_run.stdout)
        self.assertEqual(
            table_rows(passing_run.stdout, "| Flaky test |")[0][:3],
            [FLAKY_NAME, "2", "pass"],
        )

    def test_malformed_missing_and_clean_reports_keep_the_job_summary_useful(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            malformed = root / "malformed.xml"
            malformed.write_text("<testsuites><testsuite>", encoding="utf-8")
            missing = root / "missing.xml"
            clean = root / "clean.xml"
            clean.write_text(
                '<testsuites><testsuite name="s">'
                '<testcase name="ok" classname="crate::bin" time="0.1"/>'
                "</testsuite></testsuites>",
                encoding="utf-8",
            )
            result = run_summary("--stdout", malformed, missing, clean, FLAKY_FAIL)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"unreadable JUnit report `{malformed}`", result.stdout)
        self.assertIn(f"missing JUnit report `{missing}`", result.stdout)
        self.assertIn("4 test(s) in 2 report(s): 1 flaky, 1 failed.", result.stdout)

    def test_empty_report_and_long_warning_path_are_visible(self):
        with tempfile.TemporaryDirectory() as directory:
            empty = Path(directory) / "empty.xml"
            empty.write_text("", encoding="utf-8")
            no_cases = Path(directory) / "no-cases.xml"
            no_cases.write_text(
                '<testsuites name="nextest-run" tests="0"/>', encoding="utf-8"
            )
            long_missing = Path("/", *(["d" * 100] * 4), "junit.xml")
            unusable = Path("/", "d" * 300, "junit.xml")
            result = run_summary(
                "--stdout", empty, no_cases, long_missing, unusable, FLAKY_PASS
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"unreadable JUnit report `{empty}`", result.stdout)
        self.assertIn(f"missing JUnit report `{long_missing}`", result.stdout)
        self.assertIn(f"report `{unusable}`", result.stdout)
        self.assertIn("2 test(s) in 2 report(s): 1 flaky, 0 failed.", result.stdout)

    def test_table_cells_escape_markup_and_bound_long_failure_messages(self):
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory) / "message.xml"
            report.write_text(
                '<testsuites><testsuite><testcase classname="danger | suite" '
                'name="row" time="0"><failure message="&lt;tag&gt; | '
                + "x" * 500
                + '"/></testcase></testsuite></testsuites>',
                encoding="utf-8",
            )
            result = run_summary("--stdout", report)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("danger \\| suite row", result.stdout)
        self.assertIn("&lt;tag&gt; \\|", result.stdout)
        self.assertNotIn("x" * 500, result.stdout)
        self.assertIn("…", result.stdout)

    def test_ci_appends_to_summary_and_stdout_needs_no_summary_path(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "summary.md"
            target.write_text("earlier step\n", encoding="utf-8")
            appended = run_summary(FLAKY_FAIL, summary_path=target)
            self.assertEqual(appended.returncode, 0, appended.stderr)
            self.assertTrue(
                target.read_text(encoding="utf-8").startswith(
                    "earlier step\n### Flaky and failed tests"
                )
            )
            printed = run_summary("--stdout", FLAKY_PASS)
            self.assertEqual(printed.returncode, 0, printed.stderr)
            self.assertIn(FLAKY_NAME, printed.stdout)
            self.assertEqual(printed.stderr, "")

    def test_missing_summary_environment_is_a_usage_error(self):
        result = run_summary(FLAKY_FAIL)
        self.assertEqual(result.returncode, 2)
        self.assertIn("GITHUB_STEP_SUMMARY is not set", result.stderr)


if __name__ == "__main__":
    unittest.main()
