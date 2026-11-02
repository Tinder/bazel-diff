"""Unit tests for the pure helpers in tools/perf/perf_proof.py.

The harness itself builds bazel-diff and drives Bazel on generated workspaces, so it is exercised
by running it (`make perf-proof`), not here. What is unit-testable is everything between the raw
kernel accounting and the verdict: rusage normalisation, the `ps`//proc parsers, the Mann-Whitney
test, the per-metric and overall verdict rules, candidate-spec classification, hash-map diffing,
and the markdown rendering.

Run via Bazel:
    bazel test //tools/perf:perf_proof_test
"""

import json
import math
import os
import tempfile
import unittest
from pathlib import Path

import perf_proof as pp


class StartupOptionsTest(unittest.TestCase):
    def test_nohome_rc_is_always_first(self):
        self.assertEqual(pp.startup_options_for(""), ["--nohome_rc"])
        self.assertEqual(pp.startup_options_for("--host_jvm_args=-Xmx8g"), ["--nohome_rc", "--host_jvm_args=-Xmx8g"])
        self.assertEqual(pp.startup_options_for("--nohome_rc --foo"), ["--nohome_rc", "--foo"])

    def test_home_rc_is_refused(self):
        with self.assertRaises(SystemExit):
            pp.startup_options_for("--home_rc")
        with self.assertRaises(SystemExit):
            pp.startup_options_for("--host_jvm_args=-Xmx8g --home_rc=true")


class RusageTest(unittest.TestCase):
    def test_maxrss_units(self):
        self.assertEqual(pp.maxrss_to_bytes(4096, system="Darwin"), 4096)
        self.assertEqual(pp.maxrss_to_bytes(4096, system="Linux"), 4096 * 1024)

    def test_parse_ps_time(self):
        self.assertAlmostEqual(pp.parse_ps_time("0:09.81"), 9.81)
        self.assertAlmostEqual(pp.parse_ps_time("1:02:03.5"), 3723.5)
        self.assertAlmostEqual(pp.parse_ps_time("12.5"), 12.5)
        self.assertIsNone(pp.parse_ps_time(""))
        self.assertIsNone(pp.parse_ps_time("abc"))
        self.assertIsNone(pp.parse_ps_time("1:2:3:4"))

    def test_parse_proc_stat_cpu(self):
        # comm contains spaces and parens, as a Java server's can.
        line = "1234 (java (server)) S 1 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 30 0 1000 1 2 3"
        self.assertAlmostEqual(pp.parse_proc_stat_cpu(line, 100), 3.0)
        self.assertIsNone(pp.parse_proc_stat_cpu("garbage", 100))
        self.assertIsNone(pp.parse_proc_stat_cpu("1 (x) S 1", 100))

    def test_parse_proc_status_rss(self):
        text = "Name:\tbazel-diff\nVmHWM:\t  204800 kB\nVmRSS:\t  102400 kB\n"
        self.assertEqual(pp.parse_proc_status_rss(text), (102400 * 1024, 204800 * 1024))
        self.assertEqual(pp.parse_proc_status_rss(""), (None, None))


class MannWhitneyTest(unittest.TestCase):
    def test_empty(self):
        self.assertIsNone(pp.mann_whitney_p([], [1.0]))
        self.assertIsNone(pp.mann_whitney_p([1.0], []))

    def test_identical_samples_are_not_significant(self):
        self.assertAlmostEqual(pp.mann_whitney_p([1, 2, 3, 4, 5], [1, 2, 3, 4, 5]), 1.0)

    def test_disjoint_samples_exact(self):
        # n = m = 5: the two most extreme labellings out of C(10, 5) = 252.
        p = pp.mann_whitney_p([1, 2, 3, 4, 5], [6, 7, 8, 9, 10])
        self.assertAlmostEqual(p, 2 / 252)

    def test_symmetric(self):
        a, b = [1.0, 1.1, 0.9, 1.05], [1.2, 1.3, 1.25, 1.15]
        self.assertAlmostEqual(pp.mann_whitney_p(a, b), pp.mann_whitney_p(b, a))

    def test_normal_approximation_for_large_samples(self):
        a = [float(i) for i in range(12)]
        b = [float(i) + 20 for i in range(12)]
        p = pp.mann_whitney_p(a, b)
        self.assertLess(p, 0.001)
        same = pp.mann_whitney_p(a, list(a))
        self.assertGreater(same, 0.9)

    def test_ties(self):
        self.assertAlmostEqual(pp.mann_whitney_p([1, 1, 1], [1, 1, 1]), 1.0)
        p = pp.mann_whitney_p([1, 1, 2, 2], [3, 3, 4, 4])
        self.assertLess(p, 0.05)


class VerdictTest(unittest.TestCase):
    def test_improved(self):
        cmp = pp.compare_samples([10, 10.2, 9.9, 10.1, 10.0], [8.0, 8.1, 7.9, 8.2, 8.0], threshold_pct=3, alpha=0.05)
        self.assertEqual(cmp["verdict"], pp.VERDICT_IMPROVED)
        self.assertAlmostEqual(cmp["delta_pct"], -20.0, places=5)
        self.assertLess(cmp["p_value"], 0.05)

    def test_regressed(self):
        cmp = pp.compare_samples([8.0, 8.1, 7.9, 8.2, 8.0], [10, 10.2, 9.9, 10.1, 10.0], threshold_pct=3, alpha=0.05)
        self.assertEqual(cmp["verdict"], pp.VERDICT_REGRESSED)

    def test_unchanged_within_threshold(self):
        cmp = pp.compare_samples([100, 100, 100, 100], [101, 101, 101, 101], threshold_pct=3, alpha=0.05)
        self.assertEqual(cmp["verdict"], pp.VERDICT_UNCHANGED)
        self.assertAlmostEqual(cmp["delta_pct"], 1.0)

    def test_inconclusive_when_noisy(self):
        # Medians differ by 10% but the samples overlap heavily.
        cmp = pp.compare_samples([10, 14, 8, 12], [11, 9, 15, 7], threshold_pct=3, alpha=0.05)
        self.assertEqual(cmp["verdict"], pp.VERDICT_INCONCLUSIVE)

    def test_insufficient(self):
        self.assertEqual(pp.compare_samples([1.0], [2.0], 3, 0.05)["verdict"], pp.VERDICT_INSUFFICIENT)
        self.assertEqual(pp.compare_samples([], [], 3, 0.05)["verdict"], pp.VERDICT_INSUFFICIENT)
        # Two iterations: the best possible p is 2/6, so a 20% gap is neither confirmed nor dismissed.
        self.assertEqual(pp.compare_samples([10, 10], [8, 8], 3, 0.05)["verdict"], pp.VERDICT_INSUFFICIENT)
        # ... but a gap inside the threshold is still "unchanged".
        self.assertEqual(pp.compare_samples([10, 10], [10.1, 10.1], 3, 0.05)["verdict"], pp.VERDICT_UNCHANGED)

    def test_minimum_iterations(self):
        self.assertAlmostEqual(pp.min_achievable_p(5, 5), 2 / 252)
        self.assertEqual(pp.min_achievable_p(0, 3), 1.0)
        self.assertEqual(pp.iterations_for_alpha(0.05), 4)
        self.assertEqual(pp.iterations_for_alpha(0.01), 5)

    def test_overall(self):
        mk = lambda v: {"verdict": v}  # noqa: E731
        self.assertEqual(pp.overall_verdict([mk(pp.VERDICT_IMPROVED), mk(pp.VERDICT_REGRESSED)]), "REGRESSED")
        self.assertEqual(pp.overall_verdict([mk(pp.VERDICT_IMPROVED), mk(pp.VERDICT_UNCHANGED)]), "IMPROVED")
        self.assertEqual(pp.overall_verdict([mk(pp.VERDICT_UNCHANGED), mk(pp.VERDICT_UNCHANGED)]), "NO CHANGE")
        self.assertEqual(pp.overall_verdict([mk(pp.VERDICT_UNCHANGED), mk(pp.VERDICT_INCONCLUSIVE)]), "INCONCLUSIVE")
        self.assertEqual(pp.overall_verdict([mk(pp.VERDICT_UNCHANGED), mk(pp.VERDICT_INSUFFICIENT)]), "INSUFFICIENT DATA")
        self.assertEqual(pp.overall_verdict([mk(pp.VERDICT_IMPROVED), mk(pp.VERDICT_INSUFFICIENT)]), "IMPROVED")
        self.assertEqual(pp.overall_verdict([]), "INCONCLUSIVE")


class ClassifyCandidateTest(unittest.TestCase):
    def test_rev_and_worktree(self):
        self.assertEqual(pp.classify_candidate(None), "rev")
        self.assertEqual(pp.classify_candidate("HEAD"), "rev")
        self.assertEqual(pp.classify_candidate("my-branch"), "rev")
        self.assertEqual(pp.classify_candidate("WORKTREE"), "worktree")
        self.assertEqual(pp.classify_candidate("worktree"), "worktree")

    def test_patch_must_exist(self):
        with tempfile.TemporaryDirectory() as d:
            patch = Path(d) / "fix.patch"
            self.assertEqual(pp.classify_candidate(str(patch)), "rev")
            patch.write_text("diff --git a/x b/x\n")
            self.assertEqual(pp.classify_candidate(str(patch)), "patch")
            diff = Path(d) / "fix.DIFF"
            diff.write_text("")
            self.assertEqual(pp.classify_candidate(str(diff)), "patch")


class JsonDictDiffTest(unittest.TestCase):
    def test_identical(self):
        d = pp.json_dict_diff('{"//a": "1", "//b": "2"}', '{"//b": "2", "//a": "1"}')
        self.assertTrue(d["identical"])

    def test_differences(self):
        d = pp.json_dict_diff('{"//a": "1", "//b": "2"}', '{"//a": "9", "//c": "3"}')
        self.assertFalse(d["identical"])
        self.assertEqual(d["changed"], 1)
        self.assertEqual(d["only_in_reference"], 1)
        self.assertEqual(d["only_in_run"], 1)
        self.assertEqual(d["examples"]["changed"], ["//a"])

    def test_parse_error(self):
        self.assertIn("parse_error", pp.json_dict_diff("{", "{}"))


class FormattingTest(unittest.TestCase):
    def test_metric_formats(self):
        self.assertEqual(pp.fmt_metric(None, "cpu_s"), "n/a")
        self.assertEqual(pp.fmt_metric(1.23456, "cpu_s"), "1.235s")
        self.assertEqual(pp.fmt_metric(12.3456, "wall_s"), "12.35s")
        self.assertEqual(pp.fmt_metric(100 * 1024 * 1024, "peak_rss_bytes"), "100.0 MiB")
        self.assertEqual(pp.fmt_delta(-12.345), "-12.3%")
        self.assertEqual(pp.fmt_delta(None), "n/a")
        self.assertEqual(pp.fmt_p(0.0001), "<0.001")
        self.assertEqual(pp.fmt_p(0.04), "0.040")


def _report(verdict="IMPROVED", consistent=True):
    cmp_fast = pp.compare_samples([10, 10.2, 9.9, 10.1, 10.0], [8.0, 8.1, 7.9, 8.2, 8.0], 3, 0.05)
    cmp_same = pp.compare_samples([100, 100, 100, 100, 100], [100, 100, 100, 100, 100], 3, 0.05)
    comparisons = {s: {"cpu_s": cmp_fast, "peak_rss_bytes": cmp_same, "wall_s": cmp_same, "bazel_cpu_s": cmp_same}
                   for s in pp.STAGE_NAMES}
    return {
        "verdict": verdict,
        "verdict_reasons": ["small/generate-hashes @base bazel-diff CPU (user+sys) -20.0% (p=0.008)"],
        "outputs_consistent": consistent,
        "changeset": {
            "base": {"describe": "abc123 base", "commit": "abc123", "tree": "t1", "version": "bazel-diff 70.0.1", "build": "bazel, 100s"},
            "candidate": {"describe": "def456 faster", "commit": "def456", "tree": "t2", "version": "bazel-diff 70.0.1", "build": "bazel, 60s"},
        },
        "environment": {"platform": "macOS", "cpu_count": 8, "mem_total_gib": 32, "bazel_version": "8.8.2", "python": "3.12"},
        "config": {"iterations": 5, "threshold_pct": 3.0, "alpha": 0.05, "verdict_metrics": ["cpu_s", "peak_rss_bytes"],
                   "allow_output_diff": False},
        "workloads": [{
            "name": "small", "kind": "synthetic", "meta": {"packages": 400, "rule_targets": 2000},
            "comparisons": comparisons,
            "correctness": {"base_hashes": True, "head_hashes": True, "dep_edges": True, "impacted": consistent, "impacted_count": 42},
        }],
        "failures": [],
    }


class RenderSummaryTest(unittest.TestCase):
    def test_headline_and_tables(self):
        md = pp.render_summary(_report())
        self.assertIn("**Verdict: IMPROVED**", md)
        self.assertIn("abc123 base → def456 faster", md)
        self.assertIn("| small | identical | identical | identical | identical | 42 |", md)
        self.assertIn("## small (400 packages, 2000 rule targets)", md)
        self.assertIn("| generate-hashes @base, cold Bazel server | bazel-diff CPU (user+sys) | 10.00s | 8.000s | -20.0% | 0.008 | **improved** |", md)
        # Non-verdict metrics are italicised context.
        self.assertIn("| wall time | 100.00s | 100.00s | +0.0% | 1.000 | _unchanged_ |", md)
        self.assertNotIn("Outputs differ", md)

    def test_insufficient_data_names_the_iteration_count(self):
        r = _report(verdict="INSUFFICIENT DATA")
        r["config"]["iterations"] = 2
        md = pp.render_summary(r)
        self.assertIn("**Verdict: INSUFFICIENT DATA**", md)
        self.assertIn("2 iterations cannot reach α=0.05; run at least --iterations 4", md)

    def test_output_mismatch_is_called_out(self):
        md = pp.render_summary(_report(verdict="IMPROVED", consistent=False))
        self.assertIn("Outputs differ between base and candidate", md)
        self.assertIn("**DIFFERS**", md)

    def test_failures_section(self):
        r = _report()
        r["failures"] = [{"workload": "small", "variant": "candidate", "iteration": 0, "stage": "impacted", "error": "exit 1"}]
        md = pp.render_summary(r)
        self.assertIn("## Failures", md)
        self.assertIn("`small` candidate iteration 0 impacted: exit 1", md)


class ParserTest(unittest.TestCase):
    def test_defaults(self):
        args = pp.build_parser().parse_args([])
        self.assertIsNone(args.iterations)
        self.assertEqual(args.builder, "bazel")
        self.assertEqual(args.threshold, 5.0)
        self.assertEqual(args.bazel_startup_options, "")
        self.assertEqual(args.build_bazel_startup_options, "")
        self.assertIsNone(args.workload)
        wls = pp.select_workloads(args)
        self.assertEqual([w[0] for w in wls], ["small", "medium", "large"])

    def test_quick_and_custom(self):
        args = pp.build_parser().parse_args(["--quick", "--synthetic", "mine:10:2", "--repo-url", "https://x/y.git"])
        wls = pp.select_workloads(args)
        self.assertEqual([(w[0], w[1]) for w in wls], [("tiny", "synthetic"), ("mine", "synthetic"), ("repo", "repo")])
        self.assertEqual(wls[1][2]["packages"], 10)

    def test_patch_repeatable(self):
        args = pp.build_parser().parse_args(["--patch", "a.patch", "--patch", "b.patch"])
        self.assertEqual(args.patch, ["a.patch", "b.patch"])


class MeasureProcessTest(unittest.TestCase):
    """measure_process only needs a child process, so it is cheap enough to exercise for real."""

    def test_accounts_for_a_real_child(self):
        with tempfile.TemporaryDirectory() as d:
            logp = Path(d) / "run.log"
            m = pp.measure_process(
                ["python3", "-c", "x = bytearray(64 * 1024 * 1024); print('hi')"], None, logp, timeout_s=60)
            self.assertTrue(m.ok)
            self.assertEqual(m.returncode, 0)
            self.assertGreater(m.wall_s, 0)
            self.assertGreaterEqual(m.cpu_s, 0)
            self.assertGreater(m.peak_rss_bytes, 64 * 1024 * 1024)
            self.assertEqual(logp.read_text().strip(), "hi")
            self.assertIsNone(m.bazel_cpu_s)

    def test_failure_and_timeout(self):
        with tempfile.TemporaryDirectory() as d:
            m = pp.measure_process(["python3", "-c", "import sys; print('boom'); sys.exit(3)"], None, Path(d) / "f.log", 60)
            self.assertFalse(m.ok)
            self.assertEqual(m.returncode, 3)
            self.assertIn("boom", m.error)
            m = pp.measure_process(["python3", "-c", "import time; time.sleep(30)"], None, Path(d) / "t.log", timeout_s=0.5)
            self.assertFalse(m.ok)
            self.assertTrue(m.timed_out)
            self.assertLess(m.wall_s, 10)


if __name__ == "__main__":
    unittest.main()
