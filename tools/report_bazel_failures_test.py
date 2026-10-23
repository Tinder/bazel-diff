"""Unit tests for tools/report_bazel_failures.py.

Run via Bazel:
    bazel test //tools:report_bazel_failures_test
"""

import os
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO

import report_bazel_failures
from report_bazel_failures import main, parse_failures, render_annotations, render_text


RUST_TEST_LOG = """\
exec ${PAGER:-/usr/bin/less} "$0" || exit 1
Executing tests from //src:rust_tests
-----------------------------------------------------------------------------
running 2 tests
test hash::keeps_stable ... ok
test hash::rejects_workspace_path ... FAILED

failures:

---- hash::rejects_workspace_path stdout ----

thread 'hash::rejects_workspace_path' panicked at src/hash.rs:88:9:
assertion `left == right` failed
  left: "a"
 right: "b"
::error::this is test output, not an Actions command

failures:
    hash::rejects_workspace_path

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s
"""

RUST_BAZEL_LOG = """\
[5,041 / 5,099] Compiling src/google/protobuf/compiler/cpp/helpers.cc [for tool]
warning: 'weak' is deprecated [-Wdeprecated-declarations]
FAIL: //src:rust_tests (Exit 101) (see /tmp/rust_tests/test.log)
INFO: Found 1 target and 1 test target...
INFO: Build completed, 1 test FAILED, 2 total actions
//:rust_clippy_check                                                     PASSED in 0.1s
//src:rust_tests                                                         FAILED in 0.4s
  /tmp/rust_tests/test.log

Executed 2 out of 2 tests: 1 test passes and 1 fails locally.
"""

BUILD_LOG = """\
error[E0425]: cannot find value `missing` in this scope
 --> src/lib.rs:42:5
  |
42 |     missing
  |     ^^^^^^^ not found in this scope

ERROR: /home/runner/work/bazel-diff/bazel-diff/src/BUILD.bazel:20:12: Compiling Rust rlib //src:bazel_diff_lib (9 files) failed: (Exit 1): rustc failed: error executing command (from target //src:bazel_diff_lib) bazel-out/k8-opt-exec/bin/process_wrapper --huge
(cd /tmp/execroot && \\
  bazel-out/process_wrapper --very-long-command)
Use --verbose_failures to see the command lines of failed build steps.
INFO: Elapsed time: 4.200s, Critical Path: 1.10s
ERROR: Build did NOT complete successfully
"""

COVERAGE_LOG = """\
Overall main-source line coverage: 80.00% (8 / 10)
Threshold:                         90.00%
FAIL: coverage 80.00% is below threshold 90.00%.
ERROR: /repo/tools/BUILD:45:1: Running //tools:coverage-check failed: (Exit 1): coverage-check failed: error executing command
INFO: Elapsed time: 1.000s, Critical Path: 0.10s
"""

PYTHON_TEST_LOG = """\
======================================================================
FAIL: test_threshold (coverage_check_test.ParseLcovTest.test_threshold)
----------------------------------------------------------------------
Traceback (most recent call last):
  File "tools/coverage_check_test.py", line 12, in test_threshold
    self.assertEqual(1, 2)
AssertionError: 1 != 2
"""

GO_TEST_LOG = """\
--- FAIL: TestSample (0.00s)
    sample_test.go:10: got 1 want 2
FAIL
"""


def _logs(mapping):
    def read_log(path):
        return mapping.get(path)

    return read_log


class ParseRustTestFailureTest(unittest.TestCase):
    def test_reads_test_log_and_ignores_passing_targets(self):
        report = parse_failures(RUST_BAZEL_LOG, read_log=_logs({"/tmp/rust_tests/test.log": RUST_TEST_LOG}))
        self.assertEqual([failure.label for failure in report.failures], ["//src:rust_tests"])
        failure = report.failures[0]
        self.assertEqual(failure.detail, "FAILED in 0.4s")
        self.assertIn("panicked at src/hash.rs:88:9", failure.excerpt)
        self.assertNotIn("exec ${PAGER", failure.excerpt)
        self.assertNotIn("keeps_stable", failure.excerpt)
        self.assertEqual(failure.file, "src/hash.rs")
        self.assertEqual(failure.line, 88)
        self.assertIn("Executed 2 out of 2 tests: 1 test passes and 1 fails locally.", report.notes)
        self.assertTrue(any(note.startswith("INFO: Build completed, 1 test FAILED") for note in report.notes))

    def test_falls_back_to_bazel_testlogs(self):
        seen = []

        def read_log(path):
            seen.append(path)
            if path == "bazel-testlogs/src/rust_tests/test.log":
                return RUST_TEST_LOG
            return None

        report = parse_failures(RUST_BAZEL_LOG, read_log=read_log)
        self.assertEqual(seen[0], "/tmp/rust_tests/test.log")
        self.assertIn("bazel-testlogs/src/rust_tests/test.log", seen)
        self.assertIn("panicked at src/hash.rs:88:9", report.failures[0].excerpt)

    def test_reports_unreadable_test_log_path(self):
        report = parse_failures(RUST_BAZEL_LOG, read_log=lambda path: None)
        self.assertEqual(report.failures[0].excerpt, "test log: /tmp/rust_tests/test.log")

    def test_canonical_main_repo_label(self):
        text = "@@//src:rust_tests                                                         FAILED in 0.2s\n"
        report = parse_failures(text, read_log=lambda path: None)
        self.assertEqual(report.failures[0].label, "@@//src:rust_tests")
        self.assertEqual(report.failures[0].detail, "FAILED in 0.2s")

    def test_cached_failure(self):
        text = "//src:rust_tests                                          (cached) FAILED in 0.1s\n"
        report = parse_failures(text, read_log=lambda path: None)
        self.assertEqual(report.failures[0].label, "//src:rust_tests")
        self.assertEqual(report.failures[0].detail, "FAILED in 0.1s")

    def test_carriage_return_progress_does_not_hide_the_failure(self):
        text = "[1 / 2] Compiling\rFAIL: //src:rust_tests (Exit 101) (see /tmp/rust_tests/test.log)\n"
        report = parse_failures(text, read_log=_logs({"/tmp/rust_tests/test.log": RUST_TEST_LOG}))
        self.assertEqual(report.failures[0].label, "//src:rust_tests")
        self.assertIn("panicked at", report.failures[0].excerpt)


class ParseBuildFailureTest(unittest.TestCase):
    def test_keeps_compiler_diagnostic_and_drops_the_command(self):
        report = parse_failures(BUILD_LOG, read_log=lambda path: None)
        self.assertEqual(len(report.failures), 1)
        failure = report.failures[0]
        self.assertEqual(failure.label, "//src:bazel_diff_lib")
        self.assertEqual(failure.file, "src/lib.rs")
        self.assertEqual(failure.line, 42)
        self.assertIn("error[E0425]: cannot find value `missing`", failure.excerpt)
        self.assertIn("failed: (Exit 1)", failure.excerpt)
        self.assertNotIn("process_wrapper", failure.excerpt)
        self.assertNotIn("(cd /tmp/execroot", failure.excerpt)
        self.assertIn("ERROR: Build did NOT complete successfully", report.notes)

    def test_summary_line_alone_is_not_a_failure(self):
        report = parse_failures("ERROR: Build did NOT complete successfully\n", read_log=lambda path: None)
        self.assertEqual(report.failures, [])
        self.assertEqual(report.notes, ["ERROR: Build did NOT complete successfully"])

    def test_drops_bazel_run_wrapper_when_the_program_printed_fail(self):
        report = parse_failures(COVERAGE_LOG, read_log=lambda path: None)
        self.assertEqual(len(report.failures), 1)
        failure = report.failures[0]
        self.assertEqual(failure.detail, "FAIL: coverage 80.00% is below threshold 90.00%.")
        self.assertIn("Overall main-source line coverage: 80.00%", failure.excerpt)
        self.assertNotIn("Running //tools:coverage-check", failure.excerpt)


class ParseOtherTestLogsTest(unittest.TestCase):
    def test_python_unittest_traceback(self):
        text = "//tools:coverage_check_test                                          FAILED in 0.3s\n  /tmp/py/test.log\n"
        report = parse_failures(text, read_log=_logs({"/tmp/py/test.log": PYTHON_TEST_LOG}))
        self.assertIn("Traceback (most recent call last):", report.failures[0].excerpt)
        self.assertIn("AssertionError: 1 != 2", report.failures[0].excerpt)

    def test_go_failure(self):
        text = "//tools/go/sample:sample_test                                        FAILED in 0.1s\n  /tmp/go/test.log\n"
        report = parse_failures(text, read_log=_logs({"/tmp/go/test.log": GO_TEST_LOG}))
        self.assertIn("--- FAIL: TestSample (0.00s)", report.failures[0].excerpt)
        self.assertIn("got 1 want 2", report.failures[0].excerpt)

    def test_timeout(self):
        text = "//tools:slow                                                         TIMEOUT in 300.0s\n"
        report = parse_failures(text, read_log=lambda path: None)
        self.assertEqual(report.failures[0].label, "//tools:slow")
        self.assertEqual(report.failures[0].detail, "TIMEOUT in 300.0s")

    def test_keeps_failure_order(self):
        text = "\n".join(
            [
                "//src:rust_tests                                                    FAILED in 0.4s",
                "//tools:coverage_check_test                                         FAILED in 0.2s",
            ]
        )
        report = parse_failures(text, read_log=lambda path: None)
        self.assertEqual(
            [failure.label for failure in report.failures],
            ["//src:rust_tests", "//tools:coverage_check_test"],
        )

    def test_passing_log_has_no_failures(self):
        text = "//src:rust_tests                                                     PASSED in 0.1s\nINFO: Build completed successfully, 1 total action\n"
        report = parse_failures(text, read_log=lambda path: None)
        self.assertEqual(report.failures, [])
        self.assertEqual(report.notes, [])


class RenderTest(unittest.TestCase):
    def test_annotation_escapes_colons_and_newlines(self):
        report = parse_failures(RUST_BAZEL_LOG, read_log=_logs({"/tmp/rust_tests/test.log": RUST_TEST_LOG}))
        annotation = render_annotations(report)
        self.assertIn("file=src/hash.rs,line=88,", annotation)
        self.assertIn("title=//src%3Arust_tests FAILED in 0.4s::", annotation)
        self.assertIn("%0A", annotation)
        self.assertNotIn("\nthread", annotation.split("::error", 1)[1])

    def test_text_report_neutralizes_workflow_commands(self):
        report = parse_failures(RUST_BAZEL_LOG, read_log=_logs({"/tmp/rust_tests/test.log": RUST_TEST_LOG}))
        text = render_text(report)
        self.assertIn("----- bazel failure summary -----", text)
        self.assertIn("//src:rust_tests FAILED in 0.4s", text)
        self.assertIn(" ::error::this is test output", text)
        self.assertNotIn("\n::error::this is test output", text)

    def test_limits_the_number_of_printed_failures(self):
        lines = [f"//pkg:t{i}                                                          FAILED in 0.1s" for i in range(12)]
        report = parse_failures("\n".join(lines), read_log=lambda path: None)
        text = render_text(report)
        self.assertIn("//pkg:t0 FAILED in 0.1s", text)
        self.assertIn("//pkg:t9 FAILED in 0.1s", text)
        self.assertNotIn("//pkg:t10 FAILED in 0.1s", text)
        self.assertIn("2 more failures", text)
        self.assertIn("//pkg:t10", text)


class MainTest(unittest.TestCase):
    def test_reads_a_real_test_log_and_writes_the_job_summary(self):
        with tempfile.TemporaryDirectory() as directory:
            log_path = os.path.join(directory, "test.log")
            with open(log_path, "w", encoding="utf-8") as handle:
                handle.write(RUST_TEST_LOG)
            bazel_log = os.path.join(directory, "bazel.log")
            with open(bazel_log, "w", encoding="utf-8") as handle:
                handle.write(
                    "FAIL: //src:rust_tests (Exit 101) (see "
                    + log_path
                    + ")\n//src:rust_tests                                                         FAILED in 0.4s\n  "
                    + log_path
                    + "\n"
                )
            summary = os.path.join(directory, "summary.md")
            stdout = StringIO()
            with redirect_stdout(stdout):
                previous = os.environ.get("GITHUB_STEP_SUMMARY")
                os.environ["GITHUB_STEP_SUMMARY"] = summary
                try:
                    status = main([bazel_log])
                finally:
                    if previous is None:
                        os.environ.pop("GITHUB_STEP_SUMMARY", None)
                    else:
                        os.environ["GITHUB_STEP_SUMMARY"] = previous
            self.assertEqual(status, 0)
            self.assertIn("panicked at src/hash.rs:88:9", stdout.getvalue())
            self.assertIn("::error ", stdout.getvalue())
            with open(summary, encoding="utf-8") as handle:
                markdown = handle.read()
            self.assertIn("## Bazel failures", markdown)
            self.assertIn("panicked at src/hash.rs:88:9", markdown)

    def test_unstructured_failure_uses_the_log_tail(self):
        with tempfile.TemporaryDirectory() as directory:
            bazel_log = os.path.join(directory, "bazel.log")
            with open(bazel_log, "w", encoding="utf-8") as handle:
                handle.write("[1 / 9] Compiling something\nfetch failed: 500 from GitHub Releases\n")
            stdout = StringIO()
            with redirect_stdout(stdout):
                status = main([bazel_log])
            self.assertEqual(status, 0)
            rendered = stdout.getvalue()
            self.assertIn("command failed", rendered)
            self.assertIn("fetch failed: 500 from GitHub Releases", rendered)
            self.assertNotIn("Compiling something", rendered)

    def test_missing_log_is_an_error(self):
        stderr = StringIO()
        with redirect_stdout(StringIO()), redirect_stderr(stderr):
            status = main(["/no/such/bazel.log"])
        self.assertEqual(status, 2)
        self.assertIn("cannot read", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
