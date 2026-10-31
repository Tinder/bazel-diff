"""Unit tests for the pure helpers in tools/perf/tango_compare.py.

The harness itself needs git, go, bazel and a live Tango server, so it is exercised by the
perf-tango-compare workflow rather than here. What *is* unit-testable is everything between the two
tools' raw output and the numbers in the step summary: decoding the Tango client's id-mapped JSON
stream (where gogo's `omitempty` drops zero ids and distances), decoding bazel-diff's impacted
list in both of its shapes, Go duration strings pulled from the Tango server log, the impacted-set
cross-check, the workload spec parser the workflow inputs feed, and the markdown rendering.

Run via Bazel:
    bazel test //tools/perf:tango_compare_test
"""

import json
import unittest

import tango_compare as tc


class ParseGoDurationTest(unittest.TestCase):
    def test_plain_seconds(self):
        self.assertAlmostEqual(tc.parse_go_duration("11.265275041s"), 11.265275041)

    def test_compound(self):
        self.assertAlmostEqual(tc.parse_go_duration("1m2.5s"), 62.5)
        self.assertAlmostEqual(tc.parse_go_duration("2h3m4s"), 2 * 3600 + 3 * 60 + 4)

    def test_sub_second_units(self):
        self.assertAlmostEqual(tc.parse_go_duration("110.849µs"), 110.849e-6)
        self.assertAlmostEqual(tc.parse_go_duration("250ms"), 0.25)
        self.assertAlmostEqual(tc.parse_go_duration("42ns"), 42e-9)

    def test_zero_and_sign(self):
        self.assertEqual(tc.parse_go_duration("0"), 0.0)
        self.assertEqual(tc.parse_go_duration("0s"), 0.0)
        self.assertAlmostEqual(tc.parse_go_duration("-1.5s"), -1.5)

    def test_rejects_non_durations(self):
        for bad in ("", "abc", "12", "1.5 s", "3x", "1s extra"):
            self.assertIsNone(tc.parse_go_duration(bad), bad)


class ParseTangoClientOutputTest(unittest.TestCase):
    STREAM = "\n".join(
        [
            # change_type 3 = CHANGED; the id-0 target has its `id` omitted entirely (gogo omitempty),
            # as is a distance of 0.
            json.dumps(
                {
                    "changed_targets": [
                        {"change_type": 3, "old_target": {"direct_dependencies": [1]}, "new_target": {"direct_dependencies": [1]}},
                        {"change_type": 3, "old_target": {"id": 1}, "new_target": {"id": 1}, "distance": 2},
                        {"change_type": 1, "new_target": {"id": 2}, "distance": 1},
                        {"change_type": 2, "old_target": {"id": 3}},
                    ]
                }
            ),
            # Metadata may arrive in several chunks; ids are JSON strings.
            json.dumps({"target_id_mapping": {"0": "//a:lib", "1": "//a:bin"}}),
            json.dumps({"target_id_mapping": {"2": "//b:new"}}),
            # Non-JSON noise (the client logs to stderr, but be tolerant anyway).
            "2026-10-15T12:03:12.865Z INFO client/client.go:156 Done.",
            "",
        ]
    )

    def test_resolves_labels_and_counts(self):
        parsed = tc.parse_tango_client_output(self.STREAM)
        # id 3 is never named by the metadata: kept as a sentinel, never dropped.
        self.assertEqual(parsed["labels"], ["//a:bin", "//a:lib", "//b:new", "<id:3>"])
        self.assertEqual(parsed["count"], 4)
        self.assertEqual(parsed["by_type"], {"new": 1, "deleted": 1, "changed": 2, "unknown": 0})
        self.assertEqual(parsed["distances"]["//a:lib"], 0)
        self.assertEqual(parsed["distances"]["//a:bin"], 2)

    def test_empty_stream(self):
        parsed = tc.parse_tango_client_output("")
        self.assertEqual(parsed["labels"], [])
        self.assertEqual(parsed["count"], 0)

    def test_prefers_new_target_id(self):
        stream = json.dumps({"changed_targets": [{"change_type": 3, "old_target": {"id": 7}, "new_target": {"id": 8}}]})
        stream += "\n" + json.dumps({"target_id_mapping": {"7": "//old", "8": "//new"}})
        self.assertEqual(tc.parse_tango_client_output(stream)["labels"], ["//new"])


class ParseImpactedTargetsTest(unittest.TestCase):
    def test_plain_label_list(self):
        self.assertEqual(tc.parse_impacted_targets('["//b:y", "//a:x", "//a:x"]'), ["//a:x", "//b:y"])

    def test_distance_objects(self):
        text = json.dumps(
            [
                {"label": "//a:x", "targetDistance": 0, "packageDistance": 0},
                {"label": "//b:y", "targetDistance": 1, "packageDistance": 1},
            ]
        )
        self.assertEqual(tc.parse_impacted_targets(text), ["//a:x", "//b:y"])

    def test_empty_file(self):
        self.assertEqual(tc.parse_impacted_targets(""), [])
        self.assertEqual(tc.parse_impacted_targets("[]"), [])


class CompareLabelSetsTest(unittest.TestCase):
    def test_match(self):
        c = tc.compare_label_sets(["//a", "//b"], ["//b", "//a"])
        self.assertTrue(c["match"])
        self.assertEqual((c["bazel_diff_count"], c["tango_count"], c["common_count"]), (2, 2, 2))
        self.assertEqual(c["only_bazel_diff"], [])
        self.assertEqual(c["only_tango"], [])

    def test_mismatch_lists_both_sides_sorted(self):
        c = tc.compare_label_sets(["//a", "//c", "//b"], ["//a", "//d"])
        self.assertFalse(c["match"])
        self.assertEqual(c["only_bazel_diff"], ["//b", "//c"])
        self.assertEqual(c["only_tango"], ["//d"])
        self.assertEqual(c["common_count"], 1)


class ParseSyntheticSpecTest(unittest.TestCase):
    def test_full_spec(self):
        self.assertEqual(tc.parse_synthetic_spec("huge:8000:5:40"), ("huge", {"packages": 8000, "targets_per_package": 5, "layers": 40}))

    def test_layers_default_scales_with_packages(self):
        name, params = tc.parse_synthetic_spec("mid:1500:4")
        self.assertEqual(name, "mid")
        self.assertEqual(params["layers"], 40)  # capped
        self.assertEqual(tc.parse_synthetic_spec("s:50:2")[1]["layers"], 5)
        self.assertEqual(tc.parse_synthetic_spec("s:5:2")[1]["layers"], 1)  # floor

    def test_rejects_bad_specs(self):
        import argparse

        for bad in ("x", "x:1", "x:1:2:3:4", "x:a:2", "x:0:2", "x:1:-2", "bad name:1:2", ":1:2"):
            with self.assertRaises(argparse.ArgumentTypeError, msg=bad):
                tc.parse_synthetic_spec(bad)

    def test_presets_are_well_formed(self):
        for name, params in tc.SYNTHETIC_PRESETS.items():
            self.assertRegex(name, r"^[a-z]+$")
            self.assertGreater(params["packages"], 0)
            self.assertGreater(params["targets_per_package"], 0)
            self.assertGreater(params["layers"], 0)


class SummarizeSamplesTest(unittest.TestCase):
    def test_median_ignores_failures(self):
        s = tc.summarize_samples([{"ok": True, "total_s": 3.0}, {"ok": False, "total_s": 99.0}, {"ok": True, "total_s": 1.0}])
        self.assertEqual(s["runs"], 3)
        self.assertEqual(s["ok"], 2)
        self.assertEqual(s["median_s"], 2.0)
        self.assertEqual((s["min_s"], s["max_s"]), (1.0, 3.0))

    def test_empty(self):
        s = tc.summarize_samples([])
        self.assertEqual(s["runs"], 0)
        self.assertIsNone(s["median_s"])


class ExtractServerTimingsTest(unittest.TestCase):
    def test_pulls_last_request(self):
        lines = [
            'INFO controller/getchangedtargets.go:335 GetChangedTargets: Both graphs fetched {"repository": "ws", "graph_fetch_duration": "11.263438083s"}',
            'INFO controller/getchangedtargets.go:154 GetChangedTargets: Successfully processed request {"repository": "ws", "send_duration": "110.849µs", "total_duration": "11.265275041s"}',
        ]
        t = tc.extract_server_timings(lines)
        self.assertAlmostEqual(t["server_total_s"], 11.265275041)
        self.assertAlmostEqual(t["server_graph_fetch_s"], 11.263438083)

    def test_absent(self):
        self.assertEqual(tc.extract_server_timings([]), {"server_total_s": None, "server_graph_fetch_s": None})


def _report(match: bool = True) -> dict:
    correctness = tc.compare_label_sets(["//a", "//b"], ["//a", "//b"] if match else ["//a", "//z"])
    return {
        "iterations": 3,
        "environment": {
            "bazel_diff_version": "bazel-diff 39.0.0",
            "repo_sha": "1d081a478522b574df1147ca42c0ea8f38569161",
            "tango_sha": "41480a27e50d3a86dc25cd4d773bbe1b60474cdb",
            "tango_ref": "41480a27e50d3a86dc25cd4d773bbe1b60474cdb",
            "tango_pool_size": 1,
            "bazel_version": "8.7.0",
            "cpu_count": 4,
            "mem_total_gib": 15.7,
            "platform": "Linux",
        },
        "workloads": [
            {
                "name": "small",
                "base_sha": "aaaaaaaaaaaaaaaaaaaa",
                "head_sha": "bbbbbbbbbbbbbbbbbbbb",
                "meta": {"rule_targets": 1600},
                "results": {
                    "bazel_diff": {
                        "cold": {"median_s": 20.0},
                        "warm": {"median_s": 4.0},
                        "diff_only": {"median_s": 0.1},
                        "cold_stages": {"checkout_base": 0.01, "generate_base": 15.0, "checkout_head": 0.01, "generate_head": 4.9, "diff": 0.08},
                    },
                    "tango": {"cold": {"median_s": 10.0}, "warm": {"median_s": 2.0}, "cached": {"median_s": 0.01}},
                },
                "correctness": correctness,
            }
        ],
        "failures": [],
    }


class RenderSummaryTest(unittest.TestCase):
    def test_tables_and_ratios(self):
        md = tc.render_summary(_report())
        self.assertIn("| small | 1600 | cold | 20.00s | 10.00s | 0.50x |", md)
        self.assertIn("| small | 1600 | warm | 4.00s | 2.00s | 0.50x |", md)
        self.assertIn("| small | 1600 | diff only | 0.10s | 0.01s | 0.10x |", md)
        self.assertIn("| small | 0.01s | 15.00s | 0.01s | 4.90s | 0.08s |", md)
        self.assertIn("| small | `aaaaaaaaaa..bbbbbbbbbb` | 2 | 2 | 2 | yes |", md)
        self.assertIn("Tango `41480a27e50d`", md)
        self.assertNotIn("Failures", md)
        self.assertNotIn("<details>", md)

    def test_mismatch_is_loud_and_listed(self):
        md = tc.render_summary(_report(match=False))
        self.assertIn("| **NO** |", md)
        self.assertIn("<details><summary>small: labels the tools disagree on</summary>", md)
        self.assertIn("- `//b`", md)
        self.assertIn("- `//z`", md)

    def test_missing_numbers_render_as_na(self):
        report = _report()
        report["workloads"][0]["results"]["tango"]["cold"] = {"median_s": None}
        report["workloads"][0]["correctness"] = None
        report["failures"] = [{"workload": "small", "tool": "tango", "phase": "cold", "iteration": 1, "error": "boom"}]
        md = tc.render_summary(report)
        self.assertIn("| small | 1600 | cold | 20.00s | n/a | n/a |", md)
        self.assertIn("| n/a |", md)
        self.assertIn("### Failures", md)
        self.assertIn("**small / tango / cold #1**: boom", md)


class FormattingTest(unittest.TestCase):
    def test_fmt_helpers(self):
        self.assertEqual(tc.fmt_s(None), "n/a")
        self.assertEqual(tc.fmt_s(1.234), "1.23s")
        self.assertEqual(tc.fmt_ratio(2.0, 4.0), "0.50x")
        self.assertEqual(tc.fmt_ratio(None, 4.0), "n/a")
        self.assertEqual(tc.fmt_ratio(2.0, 0.0), "n/a")


class RenderTangoConfigTest(unittest.TestCase):
    def test_one_repository_per_workload(self):
        from pathlib import Path

        wls = [
            tc.Workload(name="small", kind="synthetic", path=Path("/w/small"), base_sha="a", head_sha="b"),
            tc.Workload(name="skylib", kind="repo", path=Path("/w/skylib"), base_sha="c", head_sha="d"),
        ]
        cfg = tc.render_tango_config(wls, "/usr/bin/bazelisk", Path("/w/tango-run/workspaces"), pool_size=2, query_timeout_s=1200)
        self.assertEqual(cfg.count("- remote:"), 2)
        self.assertIn('remote: "/w/small"', cfg)
        self.assertIn('repository_id: "skylib"', cfg)
        self.assertIn('bazel_command_path: "/usr/bin/bazelisk"', cfg)
        self.assertIn("max_worker_pool_size: 2", cfg)
        self.assertIn("query_timeout_seconds: 1200", cfg)
        self.assertIn('workspaces_root_path: "/w/tango-run/workspaces"', cfg)


class ParserTest(unittest.TestCase):
    def test_workflow_argv_parses(self):
        argv = [
            "--bazel", "/home/runner/go/bin/bazelisk",
            "--bazel-diff", "bazel-bin/src/bazel-diff", "--skip-build",
            "--workload", "small", "--workload", "medium", "--workload", "large",
            "--iterations", "3",
            "--tango-ref", tc.TANGO_PINNED_REF,
            "--metrics-out", "tango-compare-metrics.json",
            "--summary-out", "tango-compare-summary.md",
        ]
        args = tc.build_parser().parse_args(argv)
        self.assertEqual(args.workload, ["small", "medium", "large"])
        self.assertEqual(args.iterations, 3)
        self.assertTrue(args.skip_build)

    def test_synthetic_option_uses_spec_parser(self):
        args = tc.build_parser().parse_args(["--synthetic", "huge:8000:5"])
        self.assertEqual(args.synthetic, [("huge", {"packages": 8000, "targets_per_package": 5, "layers": 40})])


if __name__ == "__main__":
    unittest.main()
