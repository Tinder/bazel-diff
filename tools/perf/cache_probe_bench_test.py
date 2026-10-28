"""Unit tests for the pure helpers in tools/perf/cache_probe_bench.py.

The harness itself needs git, go, bazel, bazel-remote and the dummy executor, so it is exercised
by the cache-probe-bench workflow rather than here. What *is* unit-testable is everything between
Bazel's raw output and the numbers in the step summary: decoding --remote_grpc_log without a proto
runtime, turning lookups into per-label verdicts, telling require-cached denials from real errors,
classifying `bazel test` result lines, the rdeps attribution query, the accuracy math, and the
markdown rendering.

Run via Bazel:
    bazel test //tools/perf:cache_probe_bench_test
"""

import unittest

import cache_probe_bench as cp


# --- a tiny protobuf encoder, enough to build LogEntry fixtures the way Bazel frames them ---


def _varint(n: int) -> bytes:
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def _field(fno: int, wt: int, payload: bytes) -> bytes:
    return _varint((fno << 3) | wt) + payload


def _bytes_field(fno: int, data: bytes) -> bytes:
    return _field(fno, 2, _varint(len(data)) + data)


def log_entry(method: str, label: str = "", mnemonic: str = "", code: int = 0, config: str = "cfg") -> bytes:
    """One remote_logging.LogEntry: metadata(1){mnemonic(5), target_id(6), configuration_id(7)},
    status(2){code(1)}, method_name(3)."""
    meta = _bytes_field(5, mnemonic.encode()) + _bytes_field(6, label.encode()) + _bytes_field(7, config.encode())
    status = _field(1, 0, _varint(code)) if code else b""
    entry = _bytes_field(1, meta) + (_bytes_field(2, status) if status else b"") + _bytes_field(3, method.encode())
    return _varint(len(entry)) + entry


GAR = cp.GET_ACTION_RESULT


class GrpcLogTest(unittest.TestCase):
    def test_decodes_hits_misses_and_counts_other_methods(self):
        data = (
            log_entry("build.bazel.remote.execution.v2.Capabilities/GetCapabilities")
            + log_entry(GAR, "//a:lib", "Genrule", code=0)
            + log_entry(GAR, "//a:t", "TestRunner", code=0)
            + log_entry(GAR, "//a:t", "TestRunner", code=0)
            + log_entry(GAR, "//b:lib", "Genrule", code=5)
            + log_entry("google.bytestream.ByteStream/Read")
        )
        parsed = cp.parse_grpc_log(data)
        self.assertEqual(parsed["methods"][GAR], 4)
        self.assertEqual(parsed["methods"]["google.bytestream.ByteStream/Read"], 1)
        self.assertEqual(
            [(lk["label"], lk["mnemonic"], lk["code"]) for lk in parsed["lookups"]],
            [("//a:lib", "Genrule", 0), ("//a:t", "TestRunner", 0), ("//a:t", "TestRunner", 0), ("//b:lib", "Genrule", 5)],
        )
        self.assertEqual(parsed["lookups"][0]["config"], "cfg")

    def test_empty_log(self):
        self.assertEqual(cp.parse_grpc_log(b""), {"methods": {}, "lookups": []})

    def test_multibyte_varint_lengths(self):
        # A label long enough that the entry's length prefix needs two varint bytes.
        label = "//" + "p" * 200 + ":t"
        parsed = cp.parse_grpc_log(log_entry(GAR, label, "TestRunner", code=5) + log_entry(GAR, "//x:y", "Genrule"))
        self.assertEqual([lk["label"] for lk in parsed["lookups"]], [label, "//x:y"])


class VerdictTest(unittest.TestCase):
    def test_any_miss_makes_the_label_a_miss_and_double_lookups_are_fine(self):
        lookups = [
            {"label": "//a:t", "mnemonic": "TestRunner", "code": 0, "config": ""},
            {"label": "//a:t", "mnemonic": "TestRunner", "code": 0, "config": ""},
            {"label": "//b:lib", "mnemonic": "Genrule", "code": 0, "config": ""},
            {"label": "//b:lib", "mnemonic": "CppCompile", "code": 5, "config": ""},
            {"label": "", "mnemonic": "Genrule", "code": 5, "config": ""},
        ]
        v = cp.verdict(lookups)
        self.assertEqual(v["misses"], {"//b:lib": "CppCompile"})
        self.assertEqual(v["hits"], ["//a:t"])
        self.assertEqual(v["looked_up"], 2)


class ProbeErrorsTest(unittest.TestCase):
    def test_denials_are_expected_other_errors_are_not(self):
        stderr = "\n".join(
            [
                "ERROR: /ws/BUILD:4:8: Executing genrule //:shared failed: (Exit 1): Action must be cached due to --experimental_remote_require_cached but it is not",
                "ERROR: /ws/BUILD:18:8: Testing //:t failed: (Exit 1): Action must be cached due to --experimental_remote_require_cached but it is not",
                "ERROR: Build did NOT complete successfully",
                "INFO: Elapsed time: 0.6s",
            ]
        )
        self.assertEqual(cp.probe_unexpected_errors(stderr), [])
        bad = stderr + "\nERROR: /ws/BUILD:1:1: no such package 'nope'"
        self.assertEqual(cp.probe_unexpected_errors(bad), ["ERROR: /ws/BUILD:1:1: no such package 'nope'"])


class TestLinesTest(unittest.TestCase):
    def test_classifies_bazel_test_result_lines(self):
        text = "\n".join(
            [
                "INFO: Build completed, 1 test FAILED, 6 total actions",
                "//:test_a                                                       (cached) PASSED in 0.3s",
                "//:test_b                                                                PASSED in 0.3s",
                "//:test_c                                                                FAILED in 0.3s",
                "//:test_d                                                       FAILED TO BUILD",
                "//:test_e                                                       NO STATUS",
                "//pkg/p00001:t0                                                 (cached) PASSED in 0.0s",
                "Executed 2 out of 6 tests: 2 tests pass and 1 fails to build.",
            ]
        )
        got = cp.parse_test_lines(text)
        self.assertEqual(got["executed"], ["//:test_b", "//:test_c"])
        self.assertEqual(got["cached"], ["//:test_a", "//pkg/p00001:t0"])
        self.assertEqual(got["not_built"], ["//:test_d", "//:test_e"])
        self.assertEqual(got["other"], [])


class AttributionAndAccuracyTest(unittest.TestCase):
    def test_rdeps_query_is_one_set_expression(self):
        self.assertEqual(cp.rdeps_query(["//b:lib", "//a:lib", "//b:lib"]), "tests(rdeps(//..., set(//a:lib //b:lib)))")

    def test_accuracy_sound_and_precise(self):
        a = cp.accuracy({"//a:t", "//b:t"}, {"//a:t", "//b:t"})
        self.assertEqual((a["recall"], a["precision"], a["missed"], a["extra_count"]), (1.0, 1.0, [], 0))

    def test_accuracy_overreport_keeps_recall(self):
        a = cp.accuracy({"//a:t"}, {"//a:t", "//b:t", "//c:t", "//d:t"})
        self.assertEqual((a["recall"], a["precision"], a["extra_count"]), (1.0, 0.25, 3))

    def test_accuracy_missed_test_breaks_recall(self):
        a = cp.accuracy({"//a:t", "//b:t"}, {"//a:t"})
        self.assertEqual((a["recall"], a["missed"]), (0.5, ["//b:t"]))

    def test_accuracy_empty_truth(self):
        self.assertEqual(cp.accuracy(set(), set())["precision"], 1.0)
        self.assertEqual(cp.accuracy(set(), {"//a:t"})["precision"], 0.0)

    def test_precise_affected_is_own_test_misses_only(self):
        probe = {"verdict": {"misses": {"//a:t": "TestRunner", "//b:lib": "Genrule", "//x:t": "TestRunner"}}}
        self.assertEqual(cp.precise_affected(probe, {"//a:t", "//b:t"}), ["//a:t"])


class RenderSummaryTest(unittest.TestCase):
    def test_renders_tables_and_flags_unsound_selectors(self):
        report = {
            "environment": {"bazel_version": "8.8.2", "bazel_diff_version": "67.0.3", "bazel_remote": "v2.6.4", "cpu_count": 4, "platform": "Linux x86_64"},
            "workloads": [
                {
                    "name": "tiny", "params": {"packages": 60}, "rule_targets": 300, "test_targets": 120,
                    "seed": {"seed_s": 12.5, "verified": True, "misses": 0},
                    "scenarios": [
                        {
                            "name": "leaf_edit", "truth": {"count": 7, "time_s": 3.0},
                            "accuracy": {
                                "bazel_diff": cp.accuracy({"//a:t"} | {f"//x:{i}" for i in range(6)}, {"//a:t"} | {f"//x:{i}" for i in range(13)}),
                                "overreport": cp.accuracy({"//a:t"}, {"//a:t", "//b:t"}),
                                "precise": cp.accuracy({"//a:t", "//b:t"}, {"//a:t"}),
                            },
                            "timing": {
                                k: cp.summarize([1.0, 2.0, 3.0])
                                for k in ("bazel_diff_cold", "bazel_diff_warm", "probe_cold", "probe_cold_single", "probe_warm", "precise")
                            },
                            "probe_breakdown": {"pass1_s": 0.5, "pass2_s": 1.0, "parse_s": 0.01, "attribute_s": 0.2, "frontier": 1, "lookups": 300},
                        }
                    ],
                }
            ],
            "failures": [{"where": "tiny/seed", "error": "boom"}],
            "unsound": ["tiny/leaf_edit: precise missed 1 test(s)"],
        }
        md = cp.render_summary(report)
        self.assertIn("### tiny: 60 packages, 300 rule targets, 120 tests", md)
        self.assertIn("verified (0 misses)", md)
        self.assertIn("| leaf_edit | 7 | 14 (50% precise) | 2 (50% precise) | 1 (100% precise) **MISSED 1** |", md)
        self.assertIn("| leaf_edit | 3.00s | 2.00s | 2.00s | 2.00s | 2.00s | 2.00s | 2.00s |", md)
        self.assertIn("frontier 1 misses, 300 lookups", md)
        self.assertIn("- **tiny/seed**: boom", md)
        self.assertIn("### Soundness failures", md)


class SummarizeTest(unittest.TestCase):
    def test_median_min_max(self):
        s = cp.summarize([3.0, 1.0, 2.0])
        self.assertEqual((s["runs"], s["median_s"], s["min_s"], s["max_s"]), (3, 2.0, 1.0, 3.0))
        self.assertEqual(cp.summarize([])["median_s"], None)


if __name__ == "__main__":
    unittest.main()
