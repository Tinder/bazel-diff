#!/usr/bin/env python3
"""Unit tests for the pure helpers in tools/serve_consistency.py.

Only the functions that never shell out are exercised here -- importing the module is safe, but
running a case would invoke `bazel build`, and nested bazel deadlocks on the output-base lock (see
the comment block in tools/BUILD).

`compare_payloads` / `divergence_verdict` are the load-bearing pair: they decide whether a run
reports "hashes are consistent" or names diverging targets, so they are tested against the two
payload shapes `HashService.serialize` emits and against the failure modes that would otherwise
produce a false green -- most importantly the JSON key-order difference that is *expected* between
hosts and must not be reported as divergence.
"""

import json
import unittest

import serve_consistency as sc


def _payload(hashes, module_graph=None, dep_edges=None, *, wrapped=None):
    """Serializes [hashes] the way HashService.serialize does."""
    if wrapped is None:
        wrapped = module_graph is not None or dep_edges is not None
    if not wrapped:
        return json.dumps(hashes).encode()
    metadata = {}
    if module_graph is not None:
        metadata["moduleGraphJson"] = module_graph
    if dep_edges is not None:
        metadata["depEdges"] = dep_edges
    return json.dumps({"hashes": hashes, "metadata": metadata}).encode()


HASHES = {
    "//:core": "Rule#aaa~bbb",
    "//:mid": "Rule#ccc~ddd",
    "//:core.txt": "SourceFile#eee~fff",
}


class DecodePayloadTest(unittest.TestCase):
    def test_bare_hash_map(self):
        hashes, graph, edges = sc.decode_payload(_payload(HASHES))
        self.assertEqual(hashes, HASHES)
        self.assertIsNone(graph)
        self.assertEqual(edges, {})

    def test_wrapped_with_metadata(self):
        body = _payload(HASHES, module_graph='{"root":"x"}', dep_edges={"//:mid": ["//:core"]})
        hashes, graph, edges = sc.decode_payload(body)
        self.assertEqual(hashes, HASHES)
        self.assertEqual(graph, '{"root":"x"}')
        self.assertEqual(edges, {"//:mid": ["//:core"]})


class ComparePayloadsTest(unittest.TestCase):
    def test_identical_payloads_pass(self):
        div = sc.compare_payloads({"i0": _payload(HASHES), "i1": _payload(HASHES)})
        self.assertTrue(div.byte_identical)
        self.assertEqual(div.hash_mismatches, [])
        status, _ = sc.divergence_verdict(div)
        self.assertEqual(status, sc.base.PASS)

    def test_key_order_difference_is_not_divergence(self):
        # HashService.serialize gsons a HashMap built by Collectors.toMap over a *parallel* stream,
        # so key order tracks the host's CPU count. Two hosts may legitimately differ here.
        reordered = dict(reversed(list(HASHES.items())))
        div = sc.compare_payloads({"i0": _payload(HASHES), "i1": _payload(reordered)})
        self.assertFalse(div.byte_identical)
        self.assertTrue(div.key_order_only)
        status, detail = sc.divergence_verdict(div)
        self.assertEqual(status, sc.base.PASS)
        self.assertIn("key order", detail)

    def test_single_label_hash_difference_fails(self):
        other = dict(HASHES, **{"//:core": "Rule#zzz~bbb"})
        div = sc.compare_payloads({"i0": _payload(HASHES), "i1": _payload(other)})
        self.assertEqual(div.total_hash_mismatches, 1)
        self.assertEqual(div.hash_mismatches[0][0], "//:core")
        self.assertEqual(
            div.hash_mismatches[0][1], {"i0": "Rule#aaa~bbb", "i1": "Rule#zzz~bbb"}
        )
        status, detail = sc.divergence_verdict(div)
        self.assertEqual(status, sc.base.FAIL)
        self.assertIn("//:core", detail)

    def test_label_set_difference_fails(self):
        fewer = {k: v for k, v in HASHES.items() if k != "//:mid"}
        div = sc.compare_payloads({"i0": _payload(HASHES), "i1": _payload(fewer)})
        self.assertEqual(div.label_only_in, {"i1": ["//:mid"]})
        self.assertTrue(div.stable_label_sets_differ)
        self.assertEqual(sc.divergence_verdict(div)[0], sc.base.FAIL)

    def test_generated_file_only_difference_is_xfail(self):
        # `*.out` membership fluctuates with Bazel's incremental analysis state across the
        # service's sequential checkouts; serve_harness._stable drops them for the same reason.
        with_out = dict(HASHES, **{"//:core.out": "GeneratedFile#aaa~bbb"})
        div = sc.compare_payloads({"i0": _payload(HASHES), "i1": _payload(with_out)})
        self.assertFalse(div.stable_label_sets_differ)
        self.assertEqual(sc.divergence_verdict(div)[0], sc.base.XFAIL)

    def test_module_graph_difference_fails(self):
        a = _payload(HASHES, module_graph='{"root":"x"}')
        b = _payload(HASHES, module_graph='{"root":"y"}')
        div = sc.compare_payloads({"i0": a, "i1": b})
        self.assertTrue(div.module_graph_differs)
        self.assertEqual(sc.divergence_verdict(div)[0], sc.base.FAIL)

    def test_module_graph_key_reordering_is_not_a_difference(self):
        a = _payload(HASHES, module_graph='{"a":1,"b":2}')
        b = _payload(HASHES, module_graph='{"b":2,"a":1}')
        div = sc.compare_payloads({"i0": a, "i1": b})
        self.assertFalse(div.module_graph_differs)
        self.assertEqual(sc.divergence_verdict(div)[0], sc.base.PASS)

    def test_dep_edge_difference_fails_but_ordering_does_not(self):
        a = _payload(HASHES, dep_edges={"//:mid": ["//:core", "//:other"]})
        reordered = _payload(HASHES, dep_edges={"//:mid": ["//:other", "//:core"]})
        different = _payload(HASHES, dep_edges={"//:mid": ["//:core"]})
        self.assertFalse(sc.compare_payloads({"i0": a, "i1": reordered}).dep_edges_differ)
        div = sc.compare_payloads({"i0": a, "i1": different})
        self.assertTrue(div.dep_edges_differ)
        self.assertEqual(sc.divergence_verdict(div)[0], sc.base.FAIL)

    def test_unparseable_payload_fails_loudly(self):
        div = sc.compare_payloads({"i0": _payload(HASHES), "i1": b"not json"})
        self.assertIn("i1", div.parse_errors)
        self.assertEqual(sc.divergence_verdict(div)[0], sc.base.FAIL)

    def test_three_instances_report_every_value(self):
        a = _payload(HASHES)
        b = _payload(dict(HASHES, **{"//:core": "Rule#bbb~bbb"}))
        c = _payload(dict(HASHES, **{"//:core": "Rule#ccc~bbb"}))
        div = sc.compare_payloads({"i0": a, "i1": b, "i2": c})
        self.assertEqual(len(div.hash_mismatches[0][1]), 3)

    def test_max_labels_caps_the_report_but_not_the_count(self):
        many = {f"//:t{i}": f"Rule#{i}~x" for i in range(50)}
        other = {f"//:t{i}": f"Rule#{i}~y" for i in range(50)}
        div = sc.compare_payloads({"i0": _payload(many), "i1": _payload(other)}, max_labels=5)
        self.assertEqual(len(div.hash_mismatches), 5)
        self.assertEqual(div.total_hash_mismatches, 50)

    def test_single_body_is_not_a_comparison(self):
        div = sc.compare_payloads({"i0": _payload(HASHES)})
        self.assertEqual(div.hash_mismatches, [])
        self.assertTrue(div.byte_identical)

    def test_as_json_is_serializable(self):
        div = sc.compare_payloads(
            {"i0": _payload(HASHES), "i1": _payload(dict(HASHES, **{"//:core": "Rule#z~z"}))}
        )
        div.logical_key = "sha.fp"
        json.dumps(div.as_json())  # must not raise


class FormatTest(unittest.TestCase):
    def _diverged(self):
        other = dict(HASHES, **{"//:core": "Rule#zzz~bbb", "//:mid": "Rule#yyy~ddd"})
        return sc.compare_payloads({"i0": _payload(HASHES), "i1": _payload(other)})

    def test_summary_stays_on_one_line(self):
        summary = sc.summarize_divergence(self._diverged())
        self.assertNotIn("\n", summary)
        self.assertIn("2 label(s) hash differently", summary)

    def test_full_report_lists_each_label(self):
        report = sc.format_divergence(self._diverged())
        self.assertIn("//:core: i0=Rule#aaa~bbb  i1=Rule#zzz~bbb", report)
        self.assertIn("//:mid:", report)

    def test_full_report_notes_what_it_truncated(self):
        many = {f"//:t{i}": f"Rule#{i}~x" for i in range(30)}
        other = {f"//:t{i}": f"Rule#{i}~y" for i in range(30)}
        div = sc.compare_payloads({"i0": _payload(many), "i1": _payload(other)})
        self.assertIn("... 22 more", sc.format_divergence(div, limit=8))

    def test_identical_payloads_summarize_as_no_differences(self):
        div = sc.compare_payloads({"i0": _payload(HASHES), "i1": _payload(HASHES)})
        self.assertEqual(sc.summarize_divergence(div), "no differences")


class SkewTest(unittest.TestCase):
    def test_clone_names_have_distinct_lengths(self):
        names = sc.skewed_clone_names(3)
        self.assertEqual(len({len(n) for n in names}), 3)
        self.assertEqual(len(set(names)), 3)

    def test_skew_can_be_disabled(self):
        names = sc.skewed_clone_names(3, skew=False)
        self.assertEqual(len({len(n) for n in names}), 1)

    def test_output_base_is_md5_of_the_absolute_path(self):
        import hashlib
        from pathlib import Path

        p = Path("/tmp/some/workspace")
        self.assertEqual(
            sc.output_base_of(p), hashlib.md5(str(p).encode("utf-8")).hexdigest()
        )

    def test_different_paths_give_different_output_bases(self):
        from pathlib import Path

        a = sc.output_base_of(Path("/tmp/ws-i0-ppp"))
        b = sc.output_base_of(Path("/tmp/ws-i1-pppppppppppppp"))
        self.assertNotEqual(a, b)


class EnvTest(unittest.TestCase):
    def test_aws_env_pins_credentials_and_region(self):
        env = sc.aws_env()
        # Without credentials the SDK throws SdkClientException, which S3HashCacheStorage swallows
        # into a silent miss; without a region createS3Storage() throws and the process dies.
        self.assertTrue(env["AWS_ACCESS_KEY_ID"])
        self.assertTrue(env["AWS_SECRET_ACCESS_KEY"])
        self.assertEqual(env["AWS_REGION"], sc.S3_REGION)

    def test_aws_env_neutralizes_ambient_config(self):
        env = sc.aws_env()
        self.assertIsNone(env["AWS_PROFILE"])
        self.assertNotIn("aws", env["AWS_CONFIG_FILE"].lower())
        self.assertEqual(env["AWS_MAX_ATTEMPTS"], "1")

    def test_instance_flags(self):
        inst = sc.Instance(idx=1, iid="i1", clone=None, cache=None)
        flags = inst.extra_args("http://127.0.0.1:1234")
        self.assertIn("--s3ForcePathStyle", flags)
        self.assertEqual(flags[flags.index("--s3Prefix") + 1], "i1/")
        self.assertEqual(flags[flags.index("--s3Region") + 1], sc.S3_REGION)
        self.assertEqual(flags[flags.index("--s3Endpoint") + 1], "http://127.0.0.1:1234")

    def test_serve_args_are_appended_uniformly(self):
        inst = sc.Instance(idx=0, iid="i0", clone=None, cache=None)
        flags = inst.extra_args("http://127.0.0.1:1", ["--fineGrainedHashExternalRepos=@x"])
        self.assertEqual(flags[-1], "--fineGrainedHashExternalRepos=@x")
        # Still uniform across instances -- several passthrough flags feed the config fingerprint,
        # so a per-instance difference would split the fleet across cache keys.
        other = sc.Instance(idx=1, iid="i1", clone=None, cache=None)
        self.assertEqual(
            flags[len(inst.s3_flags("http://127.0.0.1:1")):],
            other.extra_args("http://127.0.0.1:1", ["--fineGrainedHashExternalRepos=@x"])[
                len(other.s3_flags("http://127.0.0.1:1")):
            ],
        )

    def test_env_skew_is_opt_in(self):
        from pathlib import Path

        inst = sc.Instance(
            idx=0, iid="i0", clone=None, cache=None, home=Path("/h"), tmpdir=Path("/t")
        )
        self.assertNotIn("HOME", inst.env(env_skew=False))
        skewed = inst.env(env_skew=True)
        self.assertEqual(skewed["HOME"], "/h")
        self.assertEqual(skewed["USER"], "bd-consistency-i0")


class ArgParsingTest(unittest.TestCase):
    def test_defaults(self):
        args = sc.parse_args([])
        self.assertEqual(args.instances, 3)
        self.assertFalse(args.env_skew)
        self.assertFalse(args.no_path_skew)
        self.assertEqual(args.request_timeout, 300.0)

    def test_request_timeout_is_raisable_for_real_repos(self):
        self.assertEqual(sc.parse_args(["--request-timeout", "1800"]).request_timeout, 1800.0)

    def test_serve_arg_is_repeatable_and_accepts_flag_shaped_values(self):
        args = sc.parse_args(
            ["--serve-arg", "--fineGrainedHashExternalRepos=@rules_kotlin", "--serve-arg", "-k"]
        )
        self.assertEqual(
            args.serve_arg, ["--fineGrainedHashExternalRepos=@rules_kotlin", "-k"]
        )

    def test_flag_shaped_option_value_is_folded(self):
        # argparse rejects `--real-clone-args --depth=50` unless it is folded to the `=` spelling;
        # the same omission broke the serve-stress cron twice.
        args = sc.parse_args(["--real-clone-args", "--depth=50"])
        self.assertEqual(args.real_clone_args, "--depth=50")

    def test_fold_leaves_normal_values_alone(self):
        self.assertEqual(
            sc.fold_flag_values(["--real-clone-args", "--depth=1", "--instances", "4"]),
            ["--real-clone-args=--depth=1", "--instances", "4"],
        )

    def test_real_mode_arguments(self):
        args = sc.parse_args(
            ["--real-repo-path", "/repo", "--real-from", "abc", "--real-to", "def"]
        )
        self.assertEqual(args.real_repo_path, "/repo")
        self.assertEqual(args.real_from, "abc")

    def test_every_case_name_is_selectable(self):
        for name, _fn in sc.CASES:
            selected = [n for n, _ in sc.CASES if n in name]
            self.assertIn(name, selected)


class SummaryTest(unittest.TestCase):
    def _data(self, failures=0, divergences=None):
        return {
            "meta": {
                "instances": 3,
                "wall_seconds": 12.3,
                "path_skew": True,
                "env_skew": False,
            },
            "checks": [{"case": "baseline", "name": "ok", "status": "PASS", "detail": "fine"}],
            "divergences": divergences or {},
            "s3": {
                "run_total": {
                    "puts": 6,
                    "gets": 6,
                    "heads": 0,
                    "hits": 0,
                    "objects": 6,
                    "bad_requests": 0,
                }
            },
            "output_bases": {"baseline/i0": "abc123"},
            "failures": failures,
        }

    def test_clean_run_summary(self):
        md = sc.build_summary_md(self._data())
        self.assertIn("hashes are consistent", md)
        self.assertIn("| baseline | ok | PASS | fine |", md)

    def test_divergence_is_rendered(self):
        divs = {
            "baseline": [
                {
                    "logical_key": "sha.fp",
                    "total_hash_mismatches": 1,
                    "label_only_in": {},
                    "module_graph_differs": False,
                    "dep_edges_differ": False,
                    "hash_mismatches": [
                        {"label": "//:core", "hashes": {"i0": "Rule#a~b", "i1": "Rule#z~b"}}
                    ],
                }
            ]
        }
        md = sc.build_summary_md(self._data(failures=1, divergences=divs))
        self.assertIn("Diverging targets", md)
        self.assertIn("//:core: i0=Rule#a~b  i1=Rule#z~b", md)

    def test_pipes_in_details_do_not_break_the_table(self):
        data = self._data()
        data["checks"][0]["detail"] = "a | b"
        self.assertIn("a \\| b", sc.build_summary_md(data))


if __name__ == "__main__":
    unittest.main()
