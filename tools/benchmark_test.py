#!/usr/bin/env python3

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

from benchmark import (
    HyperfineResult,
    compare_hashes,
    _injected_binaries,
    _resolve_binaries,
    load_hashes,
    load_hyperfine_results,
    speedup,
    summarize,
    wall_time_comparison,
)


class LoadHashesTest(unittest.TestCase):
    def test_loads_flat_and_wrapped_formats(self):
        with tempfile.TemporaryDirectory() as directory:
            flat = Path(directory) / "flat.json"
            wrapped = Path(directory) / "wrapped.json"
            flat.write_text(json.dumps({"//:a": "one"}))
            wrapped.write_text(json.dumps({"hashes": {"//:a": "one"}}))

            self.assertEqual(load_hashes(flat), {"//:a": "one"})
            self.assertEqual(load_hashes(wrapped), {"//:a": "one"})

    def test_rejects_non_string_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bad.json"
            path.write_text(json.dumps({"//:a": 1}))
            with self.assertRaises(ValueError):
                load_hashes(path)


class CompareHashesTest(unittest.TestCase):
    def _write(self, directory: str, name: str, value: object) -> Path:
        path = Path(directory) / name
        path.write_text(json.dumps(value))
        return path

    def test_reports_exact_parity_across_formats(self):
        with tempfile.TemporaryDirectory() as directory:
            kotlin = self._write(directory, "kotlin.json", {"//:a": "one"})
            rust = self._write(
                directory, "rust.json", {"hashes": {"//:a": "one"}}
            )
            self.assertTrue(compare_hashes(kotlin, rust)["equal"])

    def test_reports_label_and_digest_differences(self):
        with tempfile.TemporaryDirectory() as directory:
            kotlin = self._write(
                directory,
                "kotlin.json",
                {"//:kotlin": "one", "//:shared": "old"},
            )
            rust = self._write(
                directory,
                "rust.json",
                {"//:rust": "one", "//:shared": "new"},
            )
            result = compare_hashes(kotlin, rust)
            self.assertFalse(result["equal"])
            self.assertEqual(result["kotlin_only_count"], 1)
            self.assertEqual(result["rust_only_count"], 1)
            self.assertEqual(result["digest_mismatch_count"], 1)


class SummaryTest(unittest.TestCase):
    def test_summarizes_medians_and_speedup(self):
        kotlin = summarize(
            HyperfineResult(
                command="kotlin-cold",
                mean=3.0,
                stddev=1.0,
                median=3.0,
                minimum=2.0,
                maximum=4.0,
                times=(4.0, 2.0, 3.0),
            ),
            [400, 200, 300],
        )
        rust = summarize(
            HyperfineResult(
                command="rust-cold",
                mean=1.5,
                stddev=0.5,
                median=1.5,
                minimum=1.0,
                maximum=2.0,
                times=(2.0, 1.0, 1.5),
            ),
            [100, 50, 75],
        )
        self.assertEqual(kotlin["median_wall_seconds"], 3.0)
        self.assertEqual(kotlin["median_peak_rss_bytes"], 300)
        self.assertEqual(
            speedup(kotlin, rust),
            {
                "wall_time_ratio": 2.0,
                "wall_time_reduction_pct": 50.0,
                "peak_rss_ratio": 4.0,
                "peak_rss_reduction_pct": 75.0,
            },
        )
        self.assertEqual(wall_time_comparison(speedup(kotlin, rust)), "2.0x faster")

    def test_describes_slowdown_without_calling_it_faster(self):
        comparison = {"wall_time_ratio": 0.8}
        self.assertEqual(wall_time_comparison(comparison), "1.25x slower")


class HyperfineResultTest(unittest.TestCase):
    def _write(self, directory: str, value: object) -> Path:
        path = Path(directory) / "hyperfine.json"
        path.write_text(json.dumps(value))
        return path

    def test_loads_named_hyperfine_result(self):
        with tempfile.TemporaryDirectory() as directory:
            path = self._write(
                directory,
                {
                    "results": [
                        {
                            "command": "rust-warm",
                            "mean": 1.5,
                            "stddev": 0.25,
                            "median": 1.4,
                            "min": 1.2,
                            "max": 1.9,
                            "times": [1.2, 1.9],
                            "memory_usage_byte": [10, 20],
                            "exit_codes": [0, 0],
                        }
                    ]
                },
            )

            result = load_hyperfine_results(path)["rust-warm"]

            self.assertEqual(result.times, (1.2, 1.9))
            self.assertEqual(result.median, 1.4)

    def test_rejects_failed_hyperfine_run(self):
        with tempfile.TemporaryDirectory() as directory:
            path = self._write(
                directory,
                {
                    "results": [
                        {
                            "command": "rust-warm",
                            "mean": 1.0,
                            "stddev": None,
                            "median": 1.0,
                            "min": 1.0,
                            "max": 1.0,
                            "times": [1.0],
                            "memory_usage_byte": [10],
                            "exit_codes": [1],
                        }
                    ]
                },
            )

            with self.assertRaises(ValueError):
                load_hyperfine_results(path)


class BinaryResolutionTest(unittest.TestCase):
    def test_bazel_target_injects_real_binaries(self):
        if "TEST_SRCDIR" not in os.environ:
            self.skipTest("requires Bazel runfiles")
        kotlin, rust = _injected_binaries()
        self.assertTrue(kotlin.is_file())
        self.assertTrue(rust.is_file())

    def test_resolves_injected_binaries_from_runfiles(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kotlin = root / "kotlin"
            rust = root / "rust"
            kotlin.write_text("kotlin")
            rust.write_text("rust")
            resolver = mock.Mock()
            resolver.Rlocation.side_effect = [str(kotlin), str(rust)]

            runfiles_module = mock.Mock()
            runfiles_module.Create.return_value = resolver
            with mock.patch("benchmark._runfiles", runfiles_module):
                self.assertEqual(_injected_binaries(), (kotlin, rust))

            resolver.Rlocation.assert_has_calls(
                [
                    mock.call("_main/cli/bazel-diff", source_repo=""),
                    mock.call("_main/src/bazel-diff", source_repo=""),
                ]
            )

    def test_explicit_binary_overrides_still_require_a_pair(self):
        args = SimpleNamespace(
            kotlin_binary=Path("kotlin"),
            rust_binary=Path("rust"),
        )
        self.assertEqual(
            _resolve_binaries(args),
            (Path("kotlin").resolve(), Path("rust").resolve()),
        )
        args.rust_binary = None
        with self.assertRaises(ValueError):
            _resolve_binaries(args)

    def test_missing_runfiles_reports_actionable_error(self):
        with mock.patch("benchmark._runfiles", None):
            with self.assertRaisesRegex(ValueError, "provide both"):
                _injected_binaries()


if __name__ == "__main__":
    unittest.main()
