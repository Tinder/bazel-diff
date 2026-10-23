"""Unit tests for tools/report_bazel_failures.py.

Run via Bazel:
    bazel test //tools:report_bazel_failures_test
"""

import json
import os
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO

from report_bazel_failures import (
    main,
    parse_bep,
    render_annotations,
    render_markdown,
    render_text,
)


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

RUSTC_STDERR = """\
error[E0425]: cannot find value `missing` in this scope
 --> src/lib.rs:42:5
  |
42 |     missing
  |     ^^^^^^^ not found in this scope

error: aborting due to 1 previous error
"""


def _event(ident, **payload):
    return json.dumps({"id": ident, **payload})


def _bep(*events):
    return "\n".join(events) + "\n"


def _test_result(label, status, log_uri, attempt=1, run=1, shard=1):
    return _event(
        {"testResult": {"label": label, "run": run, "shard": shard, "attempt": attempt, "configuration": {"id": "c"}}},
        testResult={
            "testActionOutput": [
                {"name": "test.log", "uri": log_uri},
                {"name": "test.xml", "uri": log_uri.replace("test.log", "test.xml")},
            ],
            "testAttemptDurationMillis": "400",
            "status": status,
        },
    )


def _test_summary(label, status):
    return _event({"testSummary": {"label": label, "configuration": {"id": "c"}}}, testSummary={"overallStatus": status})


def _action(label, stderr_uri, mnemonic="Rustc", exit_code=1, success=False):
    payload = {
        "exitCode": exit_code,
        "stderr": {"name": "stderr", "uri": stderr_uri},
        "label": label,
        "configuration": {"id": "c"},
        "type": mnemonic,
        "commandLine": ["bazel-out/process_wrapper", "rustc", "--very-long"],
        "failureDetail": {"message": f"{mnemonic} failed: error executing {mnemonic} command"},
    }
    if success:
        payload["success"] = True
    return _event({"actionCompleted": {"primaryOutput": "bazel-out/x", "label": label, "configuration": {"id": "c"}}}, action=payload)


FINISHED_FAIL = _event(
    {"buildFinished": {}},
    finished={
        "overallSuccess": False,
        "exitCode": {"name": "BUILD_FAILURE", "code": 1},
        "failureDetail": {"message": "Build did NOT complete successfully"},
    },
)

PASSING_TEST_BEP = _bep(
    _test_result("//src:rust_tests", "PASSED", "file:///tmp/rust_tests/test.log"),
    _test_summary("//src:rust_tests", "PASSED"),
    _event({"buildFinished": {}}, finished={"overallSuccess": True, "exitCode": {"name": "SUCCESS"}}),
)


def _logs(mapping):
    def read_log(path):
        return mapping.get(path)

    return read_log


class TestFailureTest(unittest.TestCase):
    def test_reads_the_test_log_of_a_failed_test(self):
        bep = _bep(
            _test_result("//src:rust_tests", "FAILED", "file:///tmp/rust_tests/test.log"),
            _test_summary("//src:rust_tests", "FAILED"),
            FINISHED_FAIL,
        )
        report = parse_bep(bep, read_log=_logs({"/tmp/rust_tests/test.log": RUST_TEST_LOG}))
        self.assertEqual([failure.label for failure in report.failures], ["//src:rust_tests"])
        failure = report.failures[0]
        self.assertEqual(failure.detail, "FAILED")
        self.assertEqual(failure.logs, [("test.log", "/tmp/rust_tests/test.log")])
        self.assertIn("panicked at src/hash.rs:88:9", failure.excerpt)
        self.assertNotIn("exec ${PAGER", failure.excerpt)
        self.assertNotIn("Executing tests from", failure.excerpt)
        # The finished message only repeats the failure above, so it is dropped.
        self.assertEqual(report.notes, ["build finished: BUILD_FAILURE"])

    def test_passing_and_flaky_tests_are_not_failures(self):
        report = parse_bep(PASSING_TEST_BEP, read_log=lambda path: None)
        self.assertEqual(report.failures, [])
        self.assertEqual(report.notes, ["build finished: SUCCESS"])

        flaky = _bep(
            _test_result("//src:rust_tests", "FAILED", "file:///tmp/a1/test.log", attempt=1),
            _test_result("//src:rust_tests", "PASSED", "file:///tmp/a2/test.log", attempt=2),
            _test_summary("//src:rust_tests", "FLAKY"),
            _action("//src:other", "file:///tmp/stderr-1"),
        )
        report = parse_bep(flaky, read_log=lambda path: None)
        self.assertEqual([failure.label for failure in report.failures], ["//src:other"])

    def test_every_failed_attempt_and_shard_is_listed(self):
        bep = _bep(
            _test_result("//src:rust_tests", "FAILED", "file:///tmp/s1/test.log", shard=1),
            _test_result("//src:rust_tests", "TIMEOUT", "file:///tmp/s2/test.log", shard=2, attempt=2),
            _test_summary("//src:rust_tests", "FAILED"),
        )
        report = parse_bep(bep, read_log=lambda path: None)
        self.assertEqual([failure.detail for failure in report.failures], ["FAILED", "TIMEOUT (shard 2, attempt 2)"])

    def test_unreadable_test_log_still_lists_its_path(self):
        bep = _bep(_test_result("//src:rust_tests", "FAILED", "file:///tmp/rust_tests/test.log"))
        report = parse_bep(bep, read_log=lambda path: None)
        failure = report.failures[0]
        self.assertEqual(failure.logs, [("test.log", "/tmp/rust_tests/test.log")])
        self.assertEqual(failure.excerpt, "")
        self.assertIn("test.log: /tmp/rust_tests/test.log", render_text(report))

    def test_remote_log_uri_is_shown_but_not_opened(self):
        opened = []

        def read_log(path):
            opened.append(path)
            return None

        bep = _bep(_test_result("//src:rust_tests", "FAILED", "bytestream://cache.example/blobs/abc/12"))
        report = parse_bep(bep, read_log=read_log)
        self.assertEqual(opened, [])
        self.assertEqual(report.failures[0].logs, [("test.log", "bytestream://cache.example/blobs/abc/12")])


class ActionFailureTest(unittest.TestCase):
    def test_reads_the_stderr_of_a_failed_action(self):
        bep = _bep(_action("//src:bazel_diff_lib", "file:///tmp/actions/stderr-7"), FINISHED_FAIL)
        report = parse_bep(bep, read_log=_logs({"/tmp/actions/stderr-7": RUSTC_STDERR}))
        self.assertEqual(len(report.failures), 1)
        failure = report.failures[0]
        self.assertEqual(failure.label, "//src:bazel_diff_lib")
        self.assertEqual(failure.detail, "Rustc failed (exit 1)")
        self.assertEqual(failure.logs, [("stderr", "/tmp/actions/stderr-7")])
        self.assertIn("error[E0425]: cannot find value `missing`", failure.excerpt)
        self.assertNotIn("process_wrapper", failure.excerpt)

    def test_successful_actions_are_ignored(self):
        bep = _bep(_action("//src:ok", "file:///tmp/actions/stderr-1", success=True))
        report = parse_bep(bep, read_log=lambda path: RUSTC_STDERR)
        self.assertEqual(report.failures, [])

    def test_missing_stderr_falls_back_to_the_failure_message(self):
        bep = _bep(_action("//src:bazel_diff_lib", "file:///tmp/actions/stderr-7"))
        report = parse_bep(bep, read_log=lambda path: None)
        self.assertEqual(report.failures[0].excerpt, "Rustc failed: error executing Rustc command")

    def test_stdout_and_stderr_are_both_shown(self):
        event = json.loads(_action("//src:tool", "file:///tmp/err"))
        event["action"]["stdout"] = {"name": "stdout", "uri": "file:///tmp/out"}
        report = parse_bep(json.dumps(event), read_log=_logs({"/tmp/err": "bad thing", "/tmp/out": "progress"}))
        self.assertEqual(report.failures[0].logs, [("stderr", "/tmp/err"), ("stdout", "/tmp/out")])
        self.assertEqual(report.failures[0].excerpt, "[stderr]\nbad thing\n\n[stdout]\nprogress")

    def test_windows_file_uri(self):
        bep = _bep(_action("//src:lib", "file:///D:/a/_temp/stderr-1"))
        report = parse_bep(bep, read_log=lambda path: None)
        path = report.failures[0].logs[0][1]
        self.assertIn(path, ("D:\\a\\_temp\\stderr-1", "/D:/a/_temp/stderr-1"))

    def test_keeps_failure_order_actions_first(self):
        bep = _bep(
            _test_result("//tools:t", "FAILED", "file:///tmp/t/test.log"),
            _action("//src:a", "file:///tmp/a"),
            _action("//src:b", "file:///tmp/b"),
        )
        report = parse_bep(bep, read_log=lambda path: None)
        self.assertEqual([failure.label for failure in report.failures], ["//src:a", "//src:b", "//tools:t"])


class AnalysisFailureTest(unittest.TestCase):
    def test_console_errors_and_aborted_targets_are_notes(self):
        bep = _bep(
            _event({"progress": {"opaqueCount": 3}}, progress={"stderr": "Loading: 0 packages loaded\nERROR: no such package 'nope': BUILD file not found\nERROR: Skipping '//nope:x': no such package 'nope'\n"}),
            _event({"pattern": {"pattern": ["//nope:x"]}}, aborted={"reason": "LOADING_FAILURE", "description": "no such package 'nope': BUILD file not found"}),
            _event({"targetCompleted": {"label": "//nope:x"}}, aborted={"reason": "ANALYSIS_FAILURE"}),
            _event(
                {"buildFinished": {}},
                finished={"exitCode": {"name": "PARSING_FAILURE", "code": 1}, "failureDetail": {"message": "BUILD file not found"}},
            ),
        )
        report = parse_bep(bep, read_log=lambda path: None)
        self.assertEqual(report.failures, [])
        self.assertEqual(
            report.notes,
            [
                "ERROR: no such package 'nope': BUILD file not found",
                "ERROR: Skipping '//nope:x': no such package 'nope'",
                "//nope:x aborted: LOADING_FAILURE: no such package 'nope': BUILD file not found",
                "//nope:x aborted: ANALYSIS_FAILURE",
                "build finished: PARSING_FAILURE: BUILD file not found",
            ],
        )
        text = render_text(report)
        self.assertIn("names no failed action or test", text)
        self.assertIn("//nope:x aborted: LOADING_FAILURE", text)

    def test_console_errors_are_dropped_when_an_action_failed(self):
        bep = _bep(
            _event({"progress": {}}, progress={"stderr": "ERROR: /repo/BUILD:1:1: Compiling //src:lib failed: (Exit 1)\n"}),
            _action("//src:lib", "file:///tmp/err"),
        )
        report = parse_bep(bep, read_log=lambda path: None)
        self.assertEqual(report.notes, [])

    def test_malformed_lines_are_counted_not_fatal(self):
        bep = "not json\n" + _action("//src:lib", "file:///tmp/err") + "\n{\"truncated\": \n"
        report = parse_bep(bep, read_log=lambda path: None)
        self.assertEqual([failure.label for failure in report.failures], ["//src:lib"])
        self.assertEqual(report.notes, ["2 unreadable line(s) in the build event file were skipped."])

    def test_empty_file(self):
        report = parse_bep("", read_log=lambda path: None)
        self.assertEqual(report.failures, [])
        self.assertIn("names no failed action or test", render_text(report))


class RenderTest(unittest.TestCase):
    def _report(self):
        bep = _bep(_test_result("//src:rust_tests", "FAILED", "file:///tmp/rust_tests/test.log"))
        return parse_bep(bep, read_log=_logs({"/tmp/rust_tests/test.log": RUST_TEST_LOG}))

    def test_annotation_escapes_colons_and_newlines(self):
        annotation = render_annotations(self._report())
        self.assertTrue(annotation.startswith("::error title=//src%3Arust_tests%3A FAILED::"))
        self.assertIn("%0A", annotation)
        self.assertNotIn("\nthread", annotation)
        # Only property values escape colons; the message keeps them.
        self.assertIn("panicked at src/hash.rs:88:9", annotation)

    def test_annotation_keeps_the_end_of_a_long_log(self):
        bep = _bep(_action("//src:lib", "file:///tmp/err"))
        report = parse_bep(bep, read_log=lambda path: "\n".join(f"line {i}" for i in range(150)) + "\nfinal error")
        annotation = render_annotations(report)
        self.assertIn("final error", annotation)
        self.assertLess(len(annotation), 4_500)

    def test_text_report_neutralizes_workflow_commands(self):
        text = render_text(self._report())
        self.assertIn("----- bazel failure summary -----", text)
        self.assertIn("//src:rust_tests: FAILED", text)
        self.assertIn("  test.log: /tmp/rust_tests/test.log", text)
        self.assertIn(" ::error::this is test output", text)
        self.assertNotIn("\n::error::this is test output", text)

    def test_markdown_fences_the_log(self):
        markdown = render_markdown(self._report())
        self.assertIn("## Bazel failures", markdown)
        self.assertIn("### `//src:rust_tests: FAILED`", markdown)
        self.assertIn("```text\n", markdown)
        self.assertIn("panicked at src/hash.rs:88:9", markdown)

    def test_long_logs_keep_the_tail(self):
        bep = _bep(_action("//src:lib", "file:///tmp/err"))
        report = parse_bep(bep, read_log=lambda path: "\n".join(f"line {i}" for i in range(500)))
        excerpt = report.failures[0].excerpt
        self.assertTrue(excerpt.startswith("… (300 earlier line(s) omitted)\n"))
        self.assertIn("line 499", excerpt)
        self.assertNotIn("line 299\n", excerpt)

    def test_limits_the_number_of_printed_failures(self):
        bep = _bep(*[_action(f"//pkg:t{i}", f"file:///tmp/{i}") for i in range(12)])
        report = parse_bep(bep, read_log=lambda path: None)
        text = render_text(report)
        self.assertIn("//pkg:t0: Rustc failed (exit 1)", text)
        self.assertIn("//pkg:t9: Rustc failed (exit 1)", text)
        self.assertNotIn("//pkg:t10: Rustc", text)
        self.assertIn("2 more failures", text)
        self.assertIn("//pkg:t10, //pkg:t11", text)


class MainTest(unittest.TestCase):
    def test_reads_real_files_and_writes_the_job_summary(self):
        with tempfile.TemporaryDirectory() as directory:
            log_path = os.path.join(directory, "test.log")
            with open(log_path, "w", encoding="utf-8") as handle:
                handle.write(RUST_TEST_LOG)
            bep_path = os.path.join(directory, "bep.json")
            with open(bep_path, "w", encoding="utf-8") as handle:
                handle.write(
                    _bep(
                        _test_result("//src:rust_tests", "FAILED", "file://" + log_path),
                        _test_summary("//src:rust_tests", "FAILED"),
                        FINISHED_FAIL,
                    )
                )
            summary = os.path.join(directory, "summary.md")
            stdout = StringIO()
            previous = os.environ.get("GITHUB_STEP_SUMMARY")
            os.environ["GITHUB_STEP_SUMMARY"] = summary
            try:
                with redirect_stdout(stdout):
                    status = main([bep_path])
            finally:
                if previous is None:
                    os.environ.pop("GITHUB_STEP_SUMMARY", None)
                else:
                    os.environ["GITHUB_STEP_SUMMARY"] = previous
            self.assertEqual(status, 0)
            self.assertIn("panicked at src/hash.rs:88:9", stdout.getvalue())
            self.assertIn("::error title=", stdout.getvalue())
            with open(summary, encoding="utf-8") as handle:
                markdown = handle.read()
            self.assertIn("## Bazel failures", markdown)
            self.assertIn("panicked at src/hash.rs:88:9", markdown)

    def test_missing_bep_file_is_an_error(self):
        stderr = StringIO()
        with redirect_stdout(StringIO()), redirect_stderr(stderr):
            status = main(["/no/such/bep.json"])
        self.assertEqual(status, 2)
        self.assertIn("cannot read", stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
