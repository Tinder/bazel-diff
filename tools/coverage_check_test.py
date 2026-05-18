"""Unit tests for tools/coverage_check.py.

Run via Bazel:
    bazel test //tools:coverage_check_test
"""

import io
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout

from coverage_check import (
    FileCoverage,
    filter_main_source,
    format_report,
    main,
    parse_lcov,
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


if __name__ == "__main__":
    unittest.main()
