"""Unit tests for the pure helpers in tools/perf/partial_graph_bench.py.

The harness itself needs git and bazel and is exercised by the partial-graph-bench workflow. What
is unit-testable is everything between a serve response and the step summary: the scenario edit
plans, reading the profile, comparing impacted sets, the per-pair accuracy and classifier checks,
the per-scenario medians, and the markdown rendering.

Run via Bazel:
    bazel test //tools/perf:partial_graph_bench_test
"""

import json
import tempfile
import unittest
from pathlib import Path

import partial_graph_bench as pg

META = {"deep_source": "pkg/p00045/src_0.txt", "wide_source": "pkg/p00001/src_0.txt", "packages": 60}


def pair(scenario="leaf_edit", expected="derived", mode="derived", **overrides):
    base = {
        "scenario": scenario,
        "iteration": 1,
        "leg": "warm",
        "from": "a" * 40,
        "to": "b" * 40,
        "expected_mode": expected,
        "baseline": {"to_s": 4.0, "total_s": 4.5, "impacted_count": 3},
        "partial": {"to_s": 0.2, "total_s": 0.4, "mode": mode, "impacted_count": 3},
        "verify": {"to_s": 4.2, "entry_identical": True, "entry_detail": ""},
        "impacted": {"match": True, "missing": [], "extra": [], "missing_count": 0, "extra_count": 0},
        "distances": {"match": True, "missing": [], "extra": [], "missing_count": 0, "extra_count": 0},
    }
    base.update(overrides)
    return base


class EditPlanTest(unittest.TestCase):
    def test_every_scenario_has_a_plan_with_distinct_content_per_tag(self):
        for scenario in pg.SCENARIOS:
            warm = pg.plan_edits(scenario, META, 1, "warm")
            cold = pg.plan_edits(scenario, META, 1, "cold")
            self.assertTrue(warm, scenario)
            self.assertNotEqual([(e.op, e.path, e.text) for e in warm], [(e.op, e.path, e.text) for e in cold], scenario)

    def test_scenarios_touch_the_files_they_claim(self):
        self.assertEqual([(e.op, e.path) for e in pg.plan_edits("leaf_edit", META, 0, "cold")], [("append", "pkg/p00045/src_0.txt")])
        self.assertEqual(pg.plan_edits("wide_edit", META, 0, "cold")[0].path, "pkg/p00001/src_0.txt")
        multi = pg.plan_edits("multi_edit", META, 0, "cold")
        self.assertEqual(len(multi), pg.MULTI_EDIT_FILES)
        self.assertEqual(len({e.path for e in multi}), pg.MULTI_EDIT_FILES)
        self.assertTrue(all(e.path.endswith("/src_1.txt") for e in multi))
        self.assertEqual([e.op for e in pg.plan_edits("exec_bit", META, 0, "cold")], ["chmod", "append"])
        self.assertEqual(pg.plan_edits("unmatched_edit", META, 0, "cold")[0].path, pg.UNMATCHED_FILE)
        chain = pg.plan_edits("chain", META, 0, "cold")
        self.assertEqual(chain[0].path, "pkg/p00045/src_2.txt", "chain edits a different file than leaf_edit")
        build = pg.plan_edits("build_edit", META, 0, "cold")
        self.assertEqual(build[0].path, "pkg/p00045/BUILD.bazel")
        self.assertTrue(build[0].text.startswith("#"))
        self.assertEqual(pg.plan_edits("module_edit", META, 0, "cold")[0].path, "MODULE.bazel")
        added = pg.plan_edits("add_file", META, 2, "warm")[0]
        self.assertEqual((added.op, added.path), ("add", "pkg/p00045/extra_warm_2.txt"))
        self.assertEqual([(e.op, e.path) for e in pg.plan_edits("delete_file", META, 0, "cold")],
                         [("delete", pg.UNMATCHED_FILE), ("append", "pkg/p00045/src_0.txt")])
        with self.assertRaises(ValueError):
            pg.plan_edits("nope", META, 0, "cold")

    def test_small_workloads_still_get_at_most_ten_multi_edits(self):
        plan = pg.plan_edits("multi_edit", {**META, "packages": 4}, 0, "cold")
        self.assertEqual([e.path for e in plan], [f"pkg/p0000{i}/src_1.txt" for i in range(4)])

    def test_apply_edit_round_trips_through_the_filesystem(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "pkg").mkdir()
            (root / "pkg" / "a.txt").write_text("one\n")
            pg.apply_edit(root, pg.Edit("append", "pkg/a.txt", "two\n"))
            self.assertEqual((root / "pkg" / "a.txt").read_text(), "one\ntwo\n")
            pg.apply_edit(root, pg.Edit("chmod", "pkg/a.txt"))
            self.assertTrue((root / "pkg" / "a.txt").stat().st_mode & 0o100)
            pg.apply_edit(root, pg.Edit("add", "docs/new.md", "x"))
            self.assertEqual((root / "docs" / "new.md").read_text(), "x")
            pg.apply_edit(root, pg.Edit("delete", "docs/new.md"))
            self.assertFalse((root / "docs" / "new.md").exists())
            with self.assertRaises(ValueError):
                pg.apply_edit(root, pg.Edit("rename", "pkg/a.txt"))


class ProfileTest(unittest.TestCase):
    def test_decode_body_accepts_json_and_text(self):
        self.assertEqual(pg.decode_body(b'{"a": 1}'), {"a": 1})
        self.assertEqual(pg.decode_body(b"OK"), {"text": "OK"})
        self.assertEqual(pg.decode_body(b""), {})
        self.assertEqual(pg.decode_body(b"[1]"), {"value": [1]})

    def test_retrieval_and_mode(self):
        profile = {"hashRetrievals": [{"cacheHit": True}, {"cacheHit": False, "generation": {"mode": "derived"}}]}
        self.assertEqual(pg.observed_mode(pg.retrieval(profile, 0)), "cached")
        self.assertEqual(pg.observed_mode(pg.retrieval(profile, 1)), "derived")
        self.assertEqual(pg.observed_mode({"cacheHit": False, "generation": {}}), "default")
        self.assertEqual(pg.observed_mode({"cacheHit": False, "generation": {"mode": "full"}}), "full")
        self.assertEqual(pg.retrieval(profile, 5), {})
        self.assertEqual(pg.retrieval({}, 0), {})
        self.assertEqual(pg.retrieval(None, 0), {})

    def test_label_sets_cover_plain_and_distance_bodies(self):
        self.assertEqual(pg.label_set({"impactedTargets": ["//a:a", "//b:b"]}), {"//a:a", "//b:b"})
        self.assertEqual(
            pg.label_set({"impactedTargets": [{"label": "//a:a", "targetDistance": 0, "packageDistance": 0}]}),
            {"//a:a@0/0"},
        )
        self.assertEqual(pg.label_set({}), set())

    def test_compare_sets_reports_both_directions(self):
        result = pg.compare_sets({"//a:a", "//b:b"}, {"//b:b", "//c:c"})
        self.assertFalse(result["match"])
        self.assertEqual((result["missing"], result["extra"]), (["//a:a"], ["//c:c"]))
        self.assertTrue(pg.compare_sets({"//a:a"}, {"//a:a"})["match"])


class CheckPairTest(unittest.TestCase):
    def test_clean_pair_has_no_failures(self):
        self.assertEqual(pg.check_pair(pair()), [])
        self.assertEqual(pg.check_pair(pair("build_edit", "full", "full")), [])
        self.assertEqual(pg.check_pair(pair("history", None, "full")), [], "real history has no expected mode")

    def test_mode_mismatch_is_reported_with_the_reason(self):
        failures = pg.check_pair(pair("leaf_edit", "derived", "full", partial={"mode": "full", "fallback_reason": "x changed", "to_s": 1}))
        self.assertEqual(len(failures), 1)
        self.assertIn("expected a derived generation", failures[0])
        self.assertIn("x changed", failures[0])
        self.assertIn("leaf_edit#1/warm", failures[0])
        failures = pg.check_pair(pair("build_edit", "full", "derived"))
        self.assertIn("expected a full generation", failures[0])

    def test_set_entry_and_unmatched_failures(self):
        bad_sets = pair(impacted={"match": False, "missing": ["//a:a"], "extra": [], "missing_count": 1, "extra_count": 0},
                        distances={"match": False, "missing": [], "extra": ["//a:a@1/1"], "missing_count": 0, "extra_count": 1})
        failures = pg.check_pair(bad_sets)
        self.assertEqual(len(failures), 2)
        self.assertIn("impacted sets differ", failures[0])
        self.assertIn("distance-annotated sets differ", failures[1])
        failures = pg.check_pair(pair(verify={"entry_identical": False, "entry_detail": "3 hashes differ"}))
        self.assertEqual(len(failures), 1)
        self.assertIn("3 hashes differ", failures[0])
        self.assertEqual(pg.check_pair(pair(verify={"entry_identical": None, "entry_detail": "missing"})), [])
        failures = pg.check_pair(pair("unmatched_edit", baseline={"to_s": 1, "impacted_count": 2}))
        self.assertIn("outside the graph impacted 2", failures[0])

    def test_errors_short_circuit(self):
        failures = pg.check_pair(pair(baseline={"error": "boom"}, partial={"mode": "full"}))
        self.assertEqual(failures, ["leaf_edit#1/warm: baseline failed: boom"])


class SummaryTest(unittest.TestCase):
    def workload(self):
        return {
            "name": "tiny",
            "meta": {"rule_targets": 240},
            "aborted": None,
            "pairs": [
                pair(leg="cold", iteration=0, baseline={"to_s": 9.0, "total_s": 9.5, "impacted_count": 3}),
                pair(leg="warm", iteration=1),
                pair(leg="warm", iteration=2, baseline={"to_s": 6.0, "total_s": 6.5, "impacted_count": 3}),
                pair("build_edit", "full", "full", partial={"to_s": 4.1, "total_s": 4.3, "mode": "full", "fallback_reason": "graph-defining file"}),
            ],
        }

    def test_summarize_takes_medians_per_leg(self):
        summary = pg.summarize_workload(self.workload())
        self.assertEqual((summary["derived"], summary["pairs"]), (3, 4))
        rows = {row["scenario"]: row for row in summary["rows"]}
        leaf = rows["leaf_edit"]
        self.assertEqual(leaf["baseline_cold_s"], 9.0)
        self.assertEqual(leaf["baseline_warm_s"], 5.0)
        self.assertEqual(leaf["partial_to_s"], 0.2)
        self.assertEqual(leaf["verify_full_s"], 4.2)
        self.assertEqual(leaf["observed_modes"], ["derived"])
        self.assertTrue(leaf["impacted_match"] and leaf["entry_identical"])
        self.assertEqual(leaf["failures"], 0)
        build = rows["build_edit"]
        self.assertIsNone(build["baseline_cold_s"])
        self.assertEqual(build["observed_modes"], ["full"])

    def test_render_summary_is_markdown_with_failures(self):
        wl = self.workload()
        wl["summary"] = pg.summarize_workload(wl)
        report = {
            "iterations": 2,
            "environment": {"bazel_diff_version": "bazel-diff 68.0.0", "repo_sha": "abc123def4567890", "bazel_version": "8.8.2",
                            "cpu_count": 4, "mem_total_gib": 16, "platform": "Linux"},
            "workloads": [wl],
            "failures": ["tiny: leaf_edit#1/warm: impacted sets differ"],
        }
        text = pg.render_summary(report)
        self.assertIn("### tiny (240 rule targets)", text)
        self.assertIn("Derived 3 of 4 revision pairs.", text)
        self.assertIn("| leaf_edit | derived | derived | 9.00s | 5.00s | 0.20s | 0.40s | 4.20s | 25.00x | yes | yes |", text)
        self.assertIn("| build_edit | full | full | n/a |", text)
        self.assertIn("### Failures (1)", text)
        self.assertIn("- tiny: leaf_edit#1/warm: impacted sets differ", text)
        report["failures"] = []
        wl["aborted"] = "partial: serve exited"
        text = pg.render_summary(report)
        self.assertIn("No accuracy or classifier failures.", text)
        self.assertIn("**Aborted:** partial: serve exited", text)
        self.assertIn("Iterations: 2.", text)
        json.dumps(report)


class CacheEntryTest(unittest.TestCase):
    def test_compare_entries_explains_differences(self):
        with tempfile.TemporaryDirectory() as tmp:
            left, right = Path(tmp) / "partial", Path(tmp) / "verify"
            left.mkdir()
            right.mkdir()
            tree = "t" * 40
            self.assertEqual(pg.compare_entries(left, right, tree)[0], None)
            entry = {"hashes": {"//a:a": "Rule#1~1", "//b:b": "Rule#2~2"}, "metadata": {"partialGraph": {"version": 1}}}
            (left / f"{tree}.fp.json").write_text(json.dumps(entry))
            (right / f"{tree}.fp.json").write_text(json.dumps(entry))
            self.assertEqual(pg.compare_entries(left, right, tree), (True, ""))
            entry["hashes"]["//b:b"] = "Rule#3~3"
            entry["metadata"]["partialGraph"]["version"] = 2
            (right / f"{tree}.fp.json").write_text(json.dumps(entry))
            identical, detail = pg.compare_entries(left, right, tree)
            self.assertFalse(identical)
            self.assertIn("1 hashes differ", detail)
            self.assertIn("//b:b", detail)
            self.assertIn("metadata differs: True", detail)
            (right / f"{tree}.fp.json").write_text("not json")
            self.assertEqual(pg.compare_entries(left, right, tree), (False, "entries are not comparable JSON"))
            self.assertIsNone(pg.cache_entry(left, "u" * 40))


class ParserTest(unittest.TestCase):
    def test_defaults_and_flags(self):
        args = pg.build_parser().parse_args([])
        self.assertEqual(args.iterations, 2)
        self.assertTrue(args.require_match)
        self.assertFalse(args.no_verify)
        args = pg.build_parser().parse_args(["--quick", "--scenario", "leaf_edit", "--scenario", "build_edit",
                                            "--no-require-match", "--serve-arg=--cacheMemoryMax=1GB",
                                            "--synthetic", "custom:10:2:3"])
        self.assertTrue(args.quick)
        self.assertEqual(args.scenario, ["leaf_edit", "build_edit"])
        self.assertFalse(args.require_match)
        self.assertEqual(args.serve_arg, ["--cacheMemoryMax=1GB"])
        self.assertEqual(args.synthetic[0][0], "custom")
        with self.assertRaises(SystemExit):
            pg.build_parser().parse_args(["--scenario", "nope"])


if __name__ == "__main__":
    unittest.main()
