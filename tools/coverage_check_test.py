"""Unit tests for tools/coverage_check.py.

Run via Bazel:
    bazel test //tools:coverage_check_test
"""

import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from unittest.mock import patch

import coverage_check
from coverage_check import (
    FileCoverage,
    badge_color,
    filter_main_source,
    format_report,
    main,
    parse_lcov,
    write_badge_json,
)


# Mixed LCOV blob covering all the cases the check has to handle:
#   - one main-source file at 90% (9/10)
#   - one main-source file at 0%  (0/5)
#   - one TEST source that should be stripped entirely
SIMPLE_LCOV = """\
SF:cli/src/main/kotlin/com/bazel_diff/A.kt
LF:10
LH:9
end_of_record
SF:cli/src/main/kotlin/com/bazel_diff/B.kt
LF:5
LH:0
end_of_record
SF:cli/src/test/kotlin/com/bazel_diff/ATest.kt
LF:20
LH:20
end_of_record
"""


class ParseLcovTest(unittest.TestCase):
    def test_extracts_per_file_records(self):
        records = parse_lcov(SIMPLE_LCOV)
        self.assertEqual(len(records), 3)
        self.assertEqual(records[0].path, "cli/src/main/kotlin/com/bazel_diff/A.kt")
        self.assertEqual(records[0].lines_found, 10)
        self.assertEqual(records[0].lines_hit, 9)
        self.assertAlmostEqual(records[0].pct, 90.0)

    def test_ignores_non_line_lcov_fields(self):
        # FN/FNDA/DA/BRF lines should be skipped without disturbing the LF/LH read.
        text = (
            "SF:foo.kt\n"
            "FN:1,foo\n"
            "FNDA:0,foo\n"
            "FNF:1\n"
            "FNH:0\n"
            "DA:1,0\n"
            "DA:2,1\n"
            "LF:2\n"
            "LH:1\n"
            "BRF:0\n"
            "BRH:0\n"
            "end_of_record\n"
        )
        records = parse_lcov(text)
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0].lines_found, 2)
        self.assertEqual(records[0].lines_hit, 1)

    def test_empty_input_returns_no_records(self):
        self.assertEqual(parse_lcov(""), [])

    def test_record_without_terminator_is_skipped(self):
        # If end_of_record is missing, we drop the record rather than emit a half-
        # parsed FileCoverage. Otherwise a truncated LCOV blob would silently inflate
        # the line count.
        self.assertEqual(parse_lcov("SF:foo.kt\nLF:10\nLH:5\n"), [])

    def test_malformed_lf_falls_back_to_zero(self):
        # A garbage LF payload must not raise; the file is still recorded but with
        # zero instrumentable lines, contributing nothing to the totals.
        records = parse_lcov("SF:x\nLF:not_a_number\nLH:0\nend_of_record\n")
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0].lines_found, 0)

    def test_pct_zero_when_no_lines_found(self):
        self.assertEqual(FileCoverage("x", 0, 0).pct, 0.0)


class FilterMainSourceTest(unittest.TestCase):
    def test_keeps_only_matching_prefixes(self):
        records = parse_lcov(SIMPLE_LCOV)
        filtered = filter_main_source(records, ["cli/src/main/"])
        self.assertEqual(
            [r.path for r in filtered],
            [
                "cli/src/main/kotlin/com/bazel_diff/A.kt",
                "cli/src/main/kotlin/com/bazel_diff/B.kt",
            ],
        )

    def test_multiple_prefixes_are_unioned(self):
        records = [
            FileCoverage("cli/src/main/a.kt", 1, 1),
            FileCoverage("tools/coverage_check.py", 1, 1),
            FileCoverage("cli/src/test/a.kt", 1, 1),
        ]
        filtered = filter_main_source(
            records, ["cli/src/main/", "tools/coverage_check.py"]
        )
        self.assertEqual(
            [r.path for r in filtered],
            ["cli/src/main/a.kt", "tools/coverage_check.py"],
        )

    def test_empty_include_returns_empty(self):
        records = parse_lcov(SIMPLE_LCOV)
        self.assertEqual(filter_main_source(records, []), [])

    def test_drops_zero_line_records(self):
        # Bazel's Python coverage emits `LF:0`/`LH:0` for the .py files of a
        # py_test target when no coverage tool is configured; we treat those
        # as "no data" rather than "0% covered" to avoid tanking the threshold
        # on files that the toolchain just doesn't have instrumentation for.
        records = [
            FileCoverage("cli/src/main/has_data.kt", 10, 5),
            FileCoverage("cli/src/main/no_data.kt", 0, 0),
            FileCoverage("tools/coverage_check.py", 0, 0),
        ]
        filtered = filter_main_source(
            records, ["cli/src/main/", "tools/coverage_check.py"]
        )
        self.assertEqual([r.path for r in filtered], ["cli/src/main/has_data.kt"])


class FormatReportTest(unittest.TestCase):
    def test_includes_overall_summary(self):
        records = [FileCoverage("a.kt", 10, 9), FileCoverage("b.kt", 10, 1)]
        out = format_report(records, total_lh=10, total_lf=20, threshold=90.0)
        self.assertIn("Overall main-source line coverage: 50.00% (10 / 20)", out)
        self.assertIn("Threshold:                         90.00%", out)

    def test_sorts_worst_covered_first(self):
        records = [FileCoverage("a.kt", 10, 9), FileCoverage("b.kt", 10, 1)]
        out = format_report(records, total_lh=10, total_lf=20, threshold=90.0)
        a_idx = out.index("a.kt")
        b_idx = out.index("b.kt")
        self.assertLess(b_idx, a_idx, "Worst-covered file should sort first")

    def test_empty_records_still_prints_summary(self):
        # No records => zeroed-out summary, but no crash. The main() path treats
        # this as exit-2 separately; format_report itself stays well-defined.
        out = format_report([], total_lh=0, total_lf=0, threshold=90.0)
        self.assertIn("Overall main-source line coverage: 0.00% (0 / 0)", out)


class MainTest(unittest.TestCase):
    def _write_lcov(self, content: str) -> str:
        fd, path = tempfile.mkstemp(suffix=".dat")
        with os.fdopen(fd, "w") as f:
            f.write(content)
        self.addCleanup(os.remove, path)
        return path

    def _run_main(self, argv):
        """Invoke main() with stdout/stderr captured. Returns (exit_code, stdout, stderr)."""
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            rc = main(argv)
        return rc, out.getvalue(), err.getvalue()

    def test_passes_when_above_threshold(self):
        path = self._write_lcov(SIMPLE_LCOV)
        # SIMPLE_LCOV main-source lines: 9 hit / 15 total = 60.00%
        rc, stdout, _ = self._run_main([path, "--threshold", "50"])
        self.assertEqual(rc, 0)
        self.assertIn("PASS", stdout)
        self.assertIn("60.00%", stdout)

    def test_fails_when_below_threshold(self):
        path = self._write_lcov(SIMPLE_LCOV)
        rc, _, stderr = self._run_main([path, "--threshold", "90"])
        self.assertEqual(rc, 1)
        self.assertIn("FAIL", stderr)

    def test_threshold_boundary_is_inclusive(self):
        # 9/15 = 60.00% exactly; threshold=60 must PASS, threshold=60.01 must FAIL.
        path = self._write_lcov(SIMPLE_LCOV)
        self.assertEqual(self._run_main([path, "--threshold", "60"])[0], 0)
        self.assertEqual(self._run_main([path, "--threshold", "60.01"])[0], 1)

    def test_missing_lcov_file_exits_2(self):
        rc, _, stderr = self._run_main(["/does/not/exist.dat"])
        self.assertEqual(rc, 2)
        self.assertIn("not found", stderr)

    def test_no_main_source_lines_exits_2(self):
        # Only a test source -- nothing matches the default include prefix.
        path = self._write_lcov(
            "SF:cli/src/test/kotlin/com/bazel_diff/ATest.kt\n"
            "LF:5\nLH:5\nend_of_record\n"
        )
        rc, _, stderr = self._run_main([path, "--include", "cli/src/main/"])
        self.assertEqual(rc, 2)
        self.assertIn("no instrumented production-source lines", stderr)

    def test_default_include_covers_self_when_instrumented(self):
        # The default include prefix list contains tools/coverage_check.py so that
        # the script's own py_test coverage counts towards the threshold when
        # Python instrumentation is configured. We simulate the instrumented case
        # with non-zero LF/LH; the zero-line case (no Python coverage tool) is
        # covered by FilterMainSourceTest.test_drops_zero_line_records.
        path = self._write_lcov(
            "SF:tools/coverage_check.py\nLF:10\nLH:10\nend_of_record\n"
        )
        rc, stdout, _ = self._run_main([path])
        self.assertEqual(rc, 0)
        self.assertIn("tools/coverage_check.py", stdout)

    def test_environment_threshold_override(self):
        # $COVERAGE_THRESHOLD provides the default when --threshold is not passed.
        path = self._write_lcov(SIMPLE_LCOV)
        os.environ["COVERAGE_THRESHOLD"] = "55"
        try:
            self.assertEqual(self._run_main([path])[0], 0)
        finally:
            del os.environ["COVERAGE_THRESHOLD"]


class HtmlReportTest(unittest.TestCase):
    """Cover the --html flag without requiring genhtml to actually be installed."""

    def _write_lcov(self, content: str) -> str:
        fd, path = tempfile.mkstemp(suffix=".dat")
        with os.fdopen(fd, "w") as f:
            f.write(content)
        self.addCleanup(os.remove, path)
        return path

    def _run_main(self, argv):
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            rc = main(argv)
        return rc, out.getvalue(), err.getvalue()

    def test_html_flag_invokes_genhtml_with_correct_args(self):
        # Mock both `which` (so the function doesn't reject the call) and `run`
        # (so we don't actually invoke a subprocess). The test asserts the
        # contract: genhtml is invoked with the LCOV file, --output-directory,
        # and --quiet, then the absolute index.html path is printed.
        path = self._write_lcov(SIMPLE_LCOV)
        out_dir = tempfile.mkdtemp()
        self.addCleanup(lambda: os.rmdir(out_dir) if os.path.isdir(out_dir) else None)

        with patch.object(coverage_check.shutil, "which", return_value="/usr/bin/genhtml") as mock_which, \
             patch.object(coverage_check.subprocess, "run") as mock_run:
            mock_run.return_value = subprocess.CompletedProcess(args=[], returncode=0)
            rc, stdout, _ = self._run_main(
                [path, "--threshold", "50", "--html", out_dir]
            )

        self.assertEqual(rc, 0)
        mock_which.assert_called_once_with("genhtml")
        mock_run.assert_called_once_with(
            ["genhtml", path, "--output-directory", out_dir, "--quiet"],
            check=True,
        )
        # The absolute index.html path is surfaced so the user can open it
        # directly from their terminal.
        self.assertIn(os.path.abspath(os.path.join(out_dir, "index.html")), stdout)

    def test_html_flag_errors_clearly_when_genhtml_missing(self):
        # When genhtml is absent we exit 2 with a clear install-hint message,
        # even if the threshold itself would have passed -- the user
        # explicitly asked for HTML output and we couldn't produce it.
        path = self._write_lcov(SIMPLE_LCOV)
        out_dir = tempfile.mkdtemp()
        self.addCleanup(lambda: os.rmdir(out_dir) if os.path.isdir(out_dir) else None)

        with patch.object(coverage_check.shutil, "which", return_value=None):
            rc, _, stderr = self._run_main(
                [path, "--threshold", "50", "--html", out_dir]
            )

        self.assertEqual(rc, 2)
        self.assertIn("genhtml not found", stderr)
        self.assertIn("brew install lcov", stderr)

    def test_html_flag_errors_when_genhtml_returns_nonzero(self):
        path = self._write_lcov(SIMPLE_LCOV)
        out_dir = tempfile.mkdtemp()
        self.addCleanup(lambda: os.rmdir(out_dir) if os.path.isdir(out_dir) else None)

        with patch.object(coverage_check.shutil, "which", return_value="/usr/bin/genhtml"), \
             patch.object(coverage_check.subprocess, "run",
                          side_effect=subprocess.CalledProcessError(returncode=3, cmd=["genhtml"])):
            rc, _, stderr = self._run_main(
                [path, "--threshold", "50", "--html", out_dir]
            )

        self.assertEqual(rc, 2)
        self.assertIn("genhtml exited with code 3", stderr)

    def test_html_generated_even_when_threshold_fails(self):
        # The interactive-investigation case: someone below threshold needs the
        # HTML report to see WHERE the gaps are. We render the HTML and THEN
        # return exit 1 for the threshold failure.
        path = self._write_lcov(SIMPLE_LCOV)
        out_dir = tempfile.mkdtemp()
        self.addCleanup(lambda: os.rmdir(out_dir) if os.path.isdir(out_dir) else None)

        with patch.object(coverage_check.shutil, "which", return_value="/usr/bin/genhtml"), \
             patch.object(coverage_check.subprocess, "run") as mock_run:
            mock_run.return_value = subprocess.CompletedProcess(args=[], returncode=0)
            rc, stdout, stderr = self._run_main(
                [path, "--threshold", "95", "--html", out_dir]
            )

        self.assertEqual(rc, 1)
        mock_run.assert_called_once()  # HTML still produced
        self.assertIn("HTML report:", stdout)
        self.assertIn("FAIL", stderr)


class BadgeColorTest(unittest.TestCase):
    def test_color_bands_track_the_gate(self):
        # Boundaries: <60 red, <75 orange, <90 yellow, >=90 brightgreen.
        # The 90% boundary is the project's enforced gate; coverage at-or-above
        # it must read brightgreen so the badge confirms the gate is met.
        self.assertEqual(badge_color(0.0), "red")
        self.assertEqual(badge_color(59.99), "red")
        self.assertEqual(badge_color(60.0), "orange")
        self.assertEqual(badge_color(74.99), "orange")
        self.assertEqual(badge_color(75.0), "yellow")
        self.assertEqual(badge_color(89.99), "yellow")
        self.assertEqual(badge_color(90.0), "brightgreen")
        self.assertEqual(badge_color(100.0), "brightgreen")


class WriteBadgeJsonTest(unittest.TestCase):
    def _read_json(self, path: str) -> dict:
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f)

    def test_writes_shields_endpoint_schema(self):
        fd, path = tempfile.mkstemp(suffix=".json")
        os.close(fd)
        self.addCleanup(os.remove, path)

        write_badge_json(path, 92.345)
        payload = self._read_json(path)

        # https://shields.io/endpoint requires schemaVersion=1 plus label/message/color.
        self.assertEqual(payload["schemaVersion"], 1)
        self.assertEqual(payload["label"], "coverage")
        self.assertEqual(payload["message"], "92.3%")
        self.assertEqual(payload["color"], "brightgreen")

    def test_creates_parent_directory(self):
        # The CI step writes into the repo root; we shouldn't require the caller
        # to pre-create intermediate directories when they pass a nested path.
        tmp = tempfile.mkdtemp()
        self.addCleanup(lambda: __import__("shutil").rmtree(tmp))
        path = os.path.join(tmp, "nested", "deeper", "badge.json")

        write_badge_json(path, 50.0)

        self.assertTrue(os.path.isfile(path))
        self.assertEqual(self._read_json(path)["color"], "red")

    def test_below_threshold_still_writes(self):
        # write_badge_json itself doesn't gate on the threshold — the caller does.
        # We want the badge to surface a regression rather than freeze on the
        # last-good value, so a sub-90% number renders honestly (yellow here).
        fd, path = tempfile.mkstemp(suffix=".json")
        os.close(fd)
        self.addCleanup(os.remove, path)

        write_badge_json(path, 80.0)
        payload = self._read_json(path)

        self.assertEqual(payload["message"], "80.0%")
        self.assertEqual(payload["color"], "yellow")


class MainBadgeJsonTest(unittest.TestCase):
    def _write_lcov(self, content: str) -> str:
        fd, path = tempfile.mkstemp(suffix=".dat")
        with os.fdopen(fd, "w") as f:
            f.write(content)
        self.addCleanup(os.remove, path)
        return path

    def _run_main(self, argv):
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            rc = main(argv)
        return rc, out.getvalue(), err.getvalue()

    def test_badge_json_emitted_when_threshold_passes(self):
        path = self._write_lcov(SIMPLE_LCOV)
        fd, badge_path = tempfile.mkstemp(suffix=".json")
        os.close(fd)
        self.addCleanup(os.remove, badge_path)

        rc, _, _ = self._run_main(
            [path, "--threshold", "50", "--badge-json", badge_path]
        )

        self.assertEqual(rc, 0)
        with open(badge_path) as f:
            payload = json.load(f)
        # SIMPLE_LCOV main-source = 9 hit / 15 total = 60.0%
        self.assertEqual(payload["message"], "60.0%")

    def test_badge_json_emitted_when_threshold_fails(self):
        # A sub-threshold run must still update the badge — otherwise a regression
        # silently keeps the old badge value and the README lies. The exit code
        # still fails the gate; only the badge artifact is decoupled.
        path = self._write_lcov(SIMPLE_LCOV)
        fd, badge_path = tempfile.mkstemp(suffix=".json")
        os.close(fd)
        self.addCleanup(os.remove, badge_path)

        rc, _, _ = self._run_main(
            [path, "--threshold", "90", "--badge-json", badge_path]
        )

        self.assertEqual(rc, 1)
        with open(badge_path) as f:
            payload = json.load(f)
        self.assertEqual(payload["message"], "60.0%")
        self.assertEqual(payload["color"], "orange")


if __name__ == "__main__":
    unittest.main()
