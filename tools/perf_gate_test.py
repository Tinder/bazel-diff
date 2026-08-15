#!/usr/bin/env python3
"""Unit tests for the Kotlin-vs-Rust performance gate.

These tests never touch a real bazel-diff binary: the runner is exercised with
stub executables whose relative speed the test controls, which is the only way
to assert that the gate *fails* when Rust is not faster.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from perf_gate import (
    IMPLEMENTATIONS,
    STARTUP_WORKLOAD,
    ParityError,
    PreparedWorkload,
    Thresholds,
    WorkloadSpec,
    build_document,
    build_report,
    check_parity,
    default_workload_specs,
    describe_difference,
    evaluate,
    format_report,
    logic_seconds,
    main,
    normalize_output,
    paired_win_rate,
    parse_args,
    ratio,
    resolve_binaries,
    run_gate,
    run_rounds,
    select_workloads,
    summarize,
    time_command,
)
from perf_workload import (
    GraphSpec,
    HashFileSpec,
    build_dep_edges,
    build_hash_files,
    encode_delimited,
    encode_graph,
    encode_varint,
    hash_label,
    layer_of,
    package_name,
    upstream_packages,
    write_graph_fixture,
    write_hash_files,
    write_replay_bazel,
    write_workspace,
)


# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------


def decode_fields(payload: bytes) -> dict[int, list[object]]:
    """Decode a protobuf message into ``{field_number: [values]}``.

    Only the two wire types the fixture encoder emits are supported. Having an
    independent decoder here means the encoder is checked against the wire
    format rather than against itself.
    """
    fields: dict[int, list[object]] = {}
    index = 0
    while index < len(payload):
        key, index = decode_varint(payload, index)
        field_number, wire_type = key >> 3, key & 0x07
        if wire_type == 0:
            value, index = decode_varint(payload, index)
        elif wire_type == 2:
            length, index = decode_varint(payload, index)
            value = payload[index : index + length]
            index += length
        else:  # pragma: no cover - the encoder never emits other wire types
            raise AssertionError(f"unexpected wire type {wire_type}")
        fields.setdefault(field_number, []).append(value)
    return fields


def decode_varint(payload: bytes, index: int) -> tuple[int, int]:
    result = 0
    shift = 0
    while True:
        byte = payload[index]
        index += 1
        result |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return result, index
        shift += 7


def decode_delimited(payload: bytes) -> list[bytes]:
    messages = []
    index = 0
    while index < len(payload):
        length, index = decode_varint(payload, index)
        messages.append(payload[index : index + length])
        index += length
    return messages


def write_stub(path: Path, *, sleep_seconds: float, output: str = "", log: Path | None = None) -> Path:
    """Write a fake bazel-diff whose runtime and output the test controls.

    The stub writes ``output`` to whatever path it was handed last, which is
    where :class:`PreparedWorkload` puts the output argument.
    """
    lines = ["#!/usr/bin/env bash", "set -euo pipefail"]
    if log is not None:
        lines.append(f'echo "{path.name}" >> {log}')
    if sleep_seconds:
        lines.append(f"sleep {sleep_seconds}")
    if output:
        # Only the workloads that ask for output pass a path; for the others the last
        # argument is a flag such as --version, and writing to it would litter the
        # working directory with files named after flags.
        lines.append('last="${@: -1}"')
        lines.append('if [[ "$last" != -* ]]; then')
        lines.append('  printf %s ' + repr(output).replace("'", '"') + ' > "$last"')
        lines.append("fi")
    lines.append("exit 0")
    path.write_text("\n".join(lines) + "\n")
    path.chmod(0o755)
    return path


def stub_workload(name: str = "stub", *, comparison: str = "none") -> PreparedWorkload:
    return PreparedWorkload(
        name=name,
        description="stub workload",
        detail={},
        arguments=("--stub",),
        comparison=comparison,
        output_suffix=".txt",
    )


# --------------------------------------------------------------------------
# Workload fixtures: protobuf encoding
# --------------------------------------------------------------------------


class VarintTest(unittest.TestCase):
    def test_encodes_single_and_multi_byte_values(self):
        self.assertEqual(encode_varint(0), b"\x00")
        self.assertEqual(encode_varint(1), b"\x01")
        self.assertEqual(encode_varint(127), b"\x7f")
        self.assertEqual(encode_varint(128), b"\x80\x01")
        self.assertEqual(encode_varint(300), b"\xac\x02")

    def test_rejects_negative_values(self):
        with self.assertRaises(ValueError):
            encode_varint(-1)


class DelimitedStreamTest(unittest.TestCase):
    def test_frames_each_message_with_its_length(self):
        stream = encode_delimited([b"ab", b"cde"])
        self.assertEqual(stream, b"\x02ab\x03cde")
        self.assertEqual(decode_delimited(stream), [b"ab", b"cde"])


class EncodeGraphTest(unittest.TestCase):
    def test_emits_one_message_per_target(self):
        spec = GraphSpec(packages=4, rules_per_package=2, sources_per_rule=2, layers=2)
        messages = decode_delimited(encode_graph(spec))
        self.assertEqual(len(messages), spec.target_count)

    def test_is_deterministic(self):
        spec = GraphSpec(packages=3, layers=2)
        self.assertEqual(encode_graph(spec), encode_graph(spec))

    def test_target_messages_carry_the_expected_shape(self):
        spec = GraphSpec(packages=2, rules_per_package=1, sources_per_rule=1, layers=2)
        kinds: dict[int, list[dict[int, list[object]]]] = {}
        for message in decode_delimited(encode_graph(spec)):
            fields = decode_fields(message)
            discriminator = fields[1][0]
            inner_field = {1: 2, 2: 3, 3: 4}[discriminator]
            kinds.setdefault(discriminator, []).append(
                decode_fields(fields[inner_field][0])
            )

        source = kinds[2][0]
        self.assertEqual(source[1][0], b"//pkg/p00000:src_0_0.txt")

        rule = kinds[1][0]
        self.assertEqual(rule[1][0], b"//pkg/p00000:t0")
        self.assertEqual(rule[2][0], b"genrule")
        self.assertIn(b"//pkg/p00000:src_0_0.txt", rule[5])
        self.assertEqual(rule[6], [b"//pkg/p00000:t0.out"])
        # Every rule reports the .bzl frame that instantiated it, so both
        # implementations exercise their bzl-seed hashing.
        self.assertTrue(any(b".bzl:" in frame for frame in rule[13]))
        attribute_names = {decode_fields(attribute)[1][0] for attribute in rule[4]}
        self.assertIn(b"srcs", attribute_names)
        self.assertIn(b"tags", attribute_names)

        generated = kinds[3][0]
        self.assertEqual(generated[1][0], b"//pkg/p00000:t0.out")
        self.assertEqual(generated[2][0], b"//pkg/p00000:t0")

    def test_dependencies_only_point_at_lower_layers(self):
        spec = GraphSpec(packages=40, layers=5, fanin=3)
        for index in range(spec.packages):
            for upstream in upstream_packages(index, spec):
                self.assertEqual(
                    layer_of(upstream, spec), layer_of(index, spec) - 1, index
                )

    def test_first_layer_has_no_dependencies(self):
        spec = GraphSpec(packages=10, layers=5)
        self.assertEqual(upstream_packages(0, spec), [])


class GraphSpecTest(unittest.TestCase):
    def test_counts_rules_sources_and_generated_files(self):
        spec = GraphSpec(packages=5, rules_per_package=3, sources_per_rule=2)
        self.assertEqual(spec.rule_count, 15)
        self.assertEqual(spec.source_count, 30)
        self.assertEqual(spec.target_count, 60)

    def test_scaling_multiplies_packages_only(self):
        spec = GraphSpec(packages=100, rules_per_package=3).scaled(2.5)
        self.assertEqual(spec.packages, 250)
        self.assertEqual(spec.rules_per_package, 3)

    def test_scaling_never_produces_an_empty_graph(self):
        self.assertEqual(GraphSpec(packages=2).scaled(0.01).packages, 1)

    def test_rejects_invalid_specs(self):
        with self.assertRaises(ValueError):
            GraphSpec(packages=0)
        with self.assertRaises(ValueError):
            GraphSpec(packages=1, fanin=-1)
        with self.assertRaises(ValueError):
            GraphSpec(packages=1).scaled(0)


class WorkspaceTest(unittest.TestCase):
    def test_writes_every_file_the_graph_refers_to(self):
        spec = GraphSpec(packages=3, rules_per_package=2, sources_per_rule=2, bzl_files=2)
        with tempfile.TemporaryDirectory() as directory:
            root = write_workspace(Path(directory) / "ws", spec)
            self.assertTrue((root / "WORKSPACE").is_file())
            for package in range(spec.packages):
                package_dir = root / package_name(package)
                self.assertTrue((package_dir / "BUILD").is_file())
                for rule in range(spec.rules_per_package):
                    for source in range(spec.sources_per_rule):
                        self.assertTrue(
                            (package_dir / f"src_{rule}_{source}.txt").is_file()
                        )
            for index in range(spec.bzl_files):
                self.assertTrue((root / "defs" / f"macro_{index}.bzl").is_file())

    def test_source_files_have_the_requested_size(self):
        spec = GraphSpec(packages=1, rules_per_package=1, sources_per_rule=1, source_bytes=64)
        with tempfile.TemporaryDirectory() as directory:
            root = write_workspace(Path(directory) / "ws", spec)
            content = (root / package_name(0) / "src_0_0.txt").read_text()
            self.assertEqual(len(content.splitlines()[1]), 64)


class ReplayBazelTest(unittest.TestCase):
    def setUp(self):
        self._temp = tempfile.TemporaryDirectory()
        self.directory = Path(self._temp.name)
        self.fixture = self.directory / "targets.pb"
        self.fixture.write_bytes(b"fixture-bytes")
        self.replay = write_replay_bazel(
            self.directory / "bazel", self.fixture, self.directory / "output-base"
        )

    def tearDown(self):
        self._temp.cleanup()

    def _run(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(self.replay), *args], capture_output=True, text=True
        )

    def test_reports_a_bazel_version(self):
        self.assertIn("Build label:", self._run("version").stdout)

    def test_reports_no_bzlmod(self):
        self.assertNotEqual(self._run("mod", "graph").returncode, 0)

    def test_reports_the_output_base(self):
        self.assertEqual(
            self._run("info").stdout.strip(), str(self.directory / "output-base")
        )

    def test_replays_the_fixture_to_stdout(self):
        self.assertEqual(self._run("query", "//...").stdout, "fixture-bytes")

    def test_replays_the_fixture_to_an_output_file(self):
        destination = self.directory / "out.pb"
        for spelling in (
            ["--output_file", str(destination)],
            [f"--output_file={destination}"],
        ):
            destination.unlink(missing_ok=True)
            self._run("query", "//...", *spelling)
            self.assertEqual(destination.read_bytes(), b"fixture-bytes")

    def test_rejects_unknown_subcommands(self):
        self.assertEqual(self._run("build", "//...").returncode, 2)

    def test_ignores_startup_options_before_the_subcommand(self):
        result = self._run("--output_base=/tmp/whatever", "version")
        self.assertIn("Build label:", result.stdout)


class HashFileTest(unittest.TestCase):
    def test_hash_values_use_the_bazel_diff_grammar(self):
        starting, _ = build_hash_files(HashFileSpec(targets=8))
        for value in starting.values():
            overall, _, direct = value.partition("~")
            self.assertEqual(len(overall), 64)
            self.assertEqual(len(direct), 64)

    def test_final_revision_changes_adds_and_removes_targets(self):
        spec = HashFileSpec(
            targets=100, changed_ratio=0.1, added_ratio=0.02, removed_ratio=0.03
        )
        starting, final = build_hash_files(spec)
        removed = set(starting) - set(final)
        added = set(final) - set(starting)
        changed = {
            label
            for label in set(starting) & set(final)
            if starting[label] != final[label]
        }
        self.assertEqual(len(starting), 100)
        self.assertEqual(len(removed), 3)
        self.assertEqual(len(added), 2)
        self.assertEqual(len(changed), 10)

    def test_dep_edges_cover_every_label(self):
        spec = HashFileSpec(targets=20, deps_per_target=3)
        labels = [hash_label(index) for index in range(spec.targets)]
        edges = build_dep_edges(labels, spec)
        self.assertEqual(set(edges), set(labels))
        for dependencies in edges.values():
            self.assertEqual(len(dependencies), 3)
            self.assertTrue(set(dependencies) <= set(labels))

    def test_writes_hash_files_and_optional_dep_edges(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            starting, final, edges = write_hash_files(
                root, HashFileSpec(targets=10), with_dep_edges=True
            )
            self.assertEqual(len(json.loads(starting.read_text())), 10)
            self.assertTrue(final.is_file())
            self.assertIsNotNone(edges)
            self.assertTrue(edges.is_file())

            _, _, no_edges = write_hash_files(root, HashFileSpec(targets=4))
            self.assertIsNone(no_edges)

    def test_rejects_invalid_specs(self):
        with self.assertRaises(ValueError):
            HashFileSpec(targets=0)
        with self.assertRaises(ValueError):
            HashFileSpec(targets=1, changed_ratio=1.5)
        with self.assertRaises(ValueError):
            HashFileSpec(targets=1, deps_per_target=-1)
        with self.assertRaises(ValueError):
            HashFileSpec(targets=1).scaled(-1)

    def test_scaling_multiplies_target_count(self):
        self.assertEqual(HashFileSpec(targets=100).scaled(3).targets, 300)


class GraphFixtureTest(unittest.TestCase):
    def test_writes_the_encoded_stream(self):
        spec = GraphSpec(packages=2, layers=2)
        with tempfile.TemporaryDirectory() as directory:
            path = write_graph_fixture(Path(directory) / "nested" / "targets.pb", spec)
            self.assertEqual(path.read_bytes(), encode_graph(spec))


# --------------------------------------------------------------------------
# Command construction and output comparison
# --------------------------------------------------------------------------


class PreparedWorkloadTest(unittest.TestCase):
    def test_appends_a_positional_output_path(self):
        workload = PreparedWorkload(
            name="w", description="", detail={}, arguments=("generate-hashes",), comparison="hashes"
        )
        self.assertEqual(
            workload.command(Path("/bin/bazel-diff"), Path("/tmp/out.json")),
            ["/bin/bazel-diff", "generate-hashes", "/tmp/out.json"],
        )

    def test_appends_a_flagged_output_path(self):
        workload = PreparedWorkload(
            name="w",
            description="",
            detail={},
            arguments=("get-impacted-targets",),
            comparison="lines",
            output_argument=("-o",),
        )
        self.assertEqual(
            workload.command(Path("bazel-diff"), Path("out.txt")),
            ["bazel-diff", "get-impacted-targets", "-o", "out.txt"],
        )

    def test_omits_the_output_path_when_nothing_is_written(self):
        workload = stub_workload()
        self.assertEqual(
            workload.command(Path("bazel-diff"), None), ["bazel-diff", "--stub"]
        )

    def test_requires_an_output_path_when_output_is_compared(self):
        workload = stub_workload(comparison="lines")
        with self.assertRaises(ValueError):
            workload.command(Path("bazel-diff"), None)


class NormalizeOutputTest(unittest.TestCase):
    def setUp(self):
        self._temp = tempfile.TemporaryDirectory()
        self.directory = Path(self._temp.name)

    def tearDown(self):
        self._temp.cleanup()

    def _write(self, name: str, text: str) -> Path:
        path = self.directory / name
        path.write_text(text)
        return path

    def test_reads_flat_and_wrapped_hash_files_identically(self):
        flat = self._write("flat.json", json.dumps({"//:a": "x~y"}))
        wrapped = self._write("wrapped.json", json.dumps({"hashes": {"//:a": "x~y"}}))
        self.assertEqual(
            normalize_output("hashes", flat), normalize_output("hashes", wrapped)
        )

    def test_sorts_label_lists_so_ordering_is_not_a_difference(self):
        first = self._write("first.txt", "//:b\n//:a\n\n")
        second = self._write("second.txt", "//:a\n//:b\n")
        self.assertEqual(
            normalize_output("lines", first), normalize_output("lines", second)
        )

    def test_parses_json_documents(self):
        path = self._write("metrics.json", json.dumps({"//:a": {"distance": 1}}))
        self.assertEqual(normalize_output("json", path), {"//:a": {"distance": 1}})

    def test_returns_nothing_when_there_is_no_output(self):
        self.assertIsNone(normalize_output("none", self.directory / "missing"))

    def test_rejects_unknown_comparison_modes(self):
        with self.assertRaises(ValueError):
            normalize_output("bytes", self.directory / "missing")


class DescribeDifferenceTest(unittest.TestCase):
    def test_summarizes_map_differences(self):
        summary = describe_difference(
            "hashes", {"//:a": "1", "//:b": "2"}, {"//:a": "9", "//:c": "3"}
        )
        self.assertIn("1 kotlin-only keys", summary)
        self.assertIn("1 rust-only keys", summary)
        self.assertIn("1 differing values", summary)

    def test_summarizes_list_differences(self):
        summary = describe_difference("lines", ["//:a", "//:b"], ["//:a"])
        self.assertIn("1 kotlin-only entries", summary)

    def test_falls_back_for_other_shapes(self):
        self.assertEqual(describe_difference("json", "left", "right"), "outputs differ")


# --------------------------------------------------------------------------
# Statistics and thresholds
# --------------------------------------------------------------------------


class StatisticsTest(unittest.TestCase):
    def test_summarizes_samples(self):
        summary = summarize([1.0, 2.0, 3.0])
        self.assertEqual(summary["runs"], 3)
        self.assertEqual(summary["median_seconds"], 2.0)
        self.assertEqual(summary["min_seconds"], 1.0)
        self.assertEqual(summary["max_seconds"], 3.0)
        self.assertEqual(summary["mean_seconds"], 2.0)
        self.assertGreater(summary["stdev_seconds"], 0)

    def test_single_sample_has_no_deviation(self):
        self.assertEqual(summarize([1.5])["stdev_seconds"], 0.0)

    def test_rejects_empty_samples(self):
        with self.assertRaises(ValueError):
            summarize([])

    def test_win_rate_counts_strict_wins_per_round(self):
        self.assertEqual(paired_win_rate([2.0, 2.0], [1.0, 1.0]), 1.0)
        self.assertEqual(paired_win_rate([2.0, 1.0], [1.0, 1.0]), 0.5)
        self.assertEqual(paired_win_rate([1.0], [1.0]), 0.0)

    def test_win_rate_requires_paired_samples(self):
        with self.assertRaises(ValueError):
            paired_win_rate([1.0], [1.0, 2.0])
        with self.assertRaises(ValueError):
            paired_win_rate([], [])

    def test_ratio_is_kotlin_over_rust(self):
        self.assertEqual(ratio(2.0, 0.5), 4.0)
        self.assertIsNone(ratio(2.0, 0.0))

    def test_logic_seconds_subtracts_startup(self):
        self.assertEqual(logic_seconds(1.0, 0.25), 0.75)

    def test_logic_seconds_is_undefined_without_headroom(self):
        self.assertIsNone(logic_seconds(0.2, 0.25))
        self.assertIsNone(logic_seconds(1.0, None))


def _report(
    *,
    kotlin: list[float],
    rust: list[float],
    startup: dict[str, float] | None = None,
    rss: dict[str, list[int]] | None = None,
):
    return build_report(
        stub_workload(),
        {"kotlin": kotlin, "rust": rust},
        startup=startup,
        rss_samples=rss,
    )


class BuildReportTest(unittest.TestCase):
    def test_reports_medians_speedup_and_win_rate(self):
        report = _report(kotlin=[1.0, 1.2], rust=[0.2, 0.3])
        self.assertEqual(report.results["kotlin"]["median_seconds"], 1.1)
        self.assertEqual(report.speedup, 4.4)
        self.assertEqual(report.win_rate, 1.0)
        self.assertIsNone(report.logic["speedup"])

    def test_reports_startup_adjusted_speedup(self):
        report = _report(
            kotlin=[1.0], rust=[0.5], startup={"kotlin": 0.5, "rust": 0.25}
        )
        self.assertEqual(report.logic["kotlin_seconds"], 0.5)
        self.assertEqual(report.logic["rust_seconds"], 0.25)
        self.assertEqual(report.logic["speedup"], 2.0)

    def test_reports_peak_rss_when_sampled(self):
        report = _report(
            kotlin=[1.0], rust=[0.5], rss={"kotlin": [400, 600], "rust": [100, 200]}
        )
        self.assertEqual(report.rss["kotlin_median_bytes"], 500)
        self.assertEqual(report.rss["rust_median_bytes"], 150)
        self.assertEqual(report.rss["ratio"], 0.3)


class EvaluateTest(unittest.TestCase):
    def test_passes_when_rust_wins_every_round(self):
        report = evaluate(
            _report(
                kotlin=[1.0, 1.0], rust=[0.2, 0.2], startup={"kotlin": 0.3, "rust": 0.01}
            ),
            Thresholds(),
            is_startup=False,
        )
        self.assertTrue(report.passed)
        self.assertEqual(report.failures, [])

    def test_fails_when_rust_is_slower(self):
        report = evaluate(
            _report(kotlin=[0.2, 0.2], rust=[1.0, 1.0]), Thresholds(), is_startup=False
        )
        self.assertFalse(report.passed)
        self.assertTrue(any("median speedup" in failure for failure in report.failures))
        self.assertTrue(any("paired rounds" in failure for failure in report.failures))

    def test_fails_when_rust_loses_a_single_round(self):
        report = evaluate(
            _report(kotlin=[1.0, 0.1], rust=[0.2, 0.2]), Thresholds(), is_startup=False
        )
        self.assertFalse(report.passed)
        self.assertTrue(any("50%" in failure for failure in report.failures))

    def test_fails_when_only_start_up_explains_the_win(self):
        # Kotlin is faster once its JVM boot is subtracted: total wall time still
        # favours Rust, but the logic itself regressed.
        report = evaluate(
            _report(
                kotlin=[1.0, 1.0],
                rust=[0.9, 0.9],
                startup={"kotlin": 0.6, "rust": 0.01},
            ),
            Thresholds(),
            is_startup=False,
        )
        self.assertFalse(report.passed)
        self.assertTrue(
            any("startup-adjusted" in failure for failure in report.failures)
        )

    def test_notes_workloads_too_short_to_adjust(self):
        report = evaluate(
            _report(kotlin=[0.3], rust=[0.2], startup={"kotlin": 0.4, "rust": 0.3}),
            Thresholds(),
            is_startup=False,
        )
        self.assertTrue(report.passed)
        self.assertIn("too short", report.logic["note"])

    def test_start_up_workload_is_not_gated_on_logic(self):
        report = evaluate(
            _report(kotlin=[0.4], rust=[0.01]), Thresholds(), is_startup=True
        )
        self.assertTrue(report.passed)
        self.assertNotIn("note", report.logic)

    def test_honours_a_relaxed_win_rate(self):
        report = evaluate(
            _report(kotlin=[1.0, 0.1], rust=[0.2, 0.2]),
            Thresholds(min_win_rate=0.5, min_speedup=1.0),
            is_startup=False,
        )
        self.assertTrue(report.passed)

    def test_fails_when_rust_uses_too_much_memory(self):
        report = evaluate(
            _report(kotlin=[1.0], rust=[0.2], rss={"kotlin": [100], "rust": [400]}),
            Thresholds(max_rss_ratio=1.5),
            is_startup=False,
        )
        self.assertFalse(report.passed)
        self.assertTrue(any("peak RSS" in failure for failure in report.failures))


class ReportRenderingTest(unittest.TestCase):
    def test_table_marks_pass_and_failure(self):
        passing = evaluate(
            _report(kotlin=[1.0], rust=[0.1]), Thresholds(), is_startup=True
        )
        text = format_report([passing])
        self.assertIn("PASS", text)
        self.assertIn("rust is faster than kotlin", text)

        failing = evaluate(
            _report(kotlin=[0.1], rust=[1.0]), Thresholds(), is_startup=True
        )
        text = format_report([failing])
        self.assertIn("FAIL", text)
        self.assertNotIn("rust is faster than kotlin", text)

    def test_document_records_protocol_and_thresholds(self):
        report = evaluate(_report(kotlin=[1.0], rust=[0.1]), Thresholds(), is_startup=True)
        document = build_document(
            [report],
            {"kotlin": Path("/k"), "rust": Path("/r")},
            rounds=3,
            warmup_rounds=1,
            scale=2.0,
            thresholds=Thresholds(min_speedup=1.5),
        )
        self.assertTrue(document["passed"])
        self.assertEqual(document["protocol"]["rounds"], 3)
        self.assertEqual(document["protocol"]["scale"], 2.0)
        self.assertEqual(document["thresholds"]["min_speedup"], 1.5)
        self.assertEqual(document["workloads"][0]["name"], "stub")


# --------------------------------------------------------------------------
# Running commands
# --------------------------------------------------------------------------


class TimeCommandTest(unittest.TestCase):
    def test_returns_elapsed_seconds(self):
        with tempfile.TemporaryDirectory() as directory:
            stub = write_stub(Path(directory) / "slow", sleep_seconds=0.05)
            self.assertGreaterEqual(time_command([str(stub)]), 0.05)

    def test_raises_with_stderr_context_on_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            failing = Path(directory) / "failing"
            failing.write_text("#!/usr/bin/env bash\necho boom >&2\nexit 7\n")
            failing.chmod(0o755)
            with self.assertRaises(RuntimeError) as raised:
                time_command([str(failing)])
            self.assertIn("boom", str(raised.exception))
            self.assertIn("7", str(raised.exception))


class ParityTest(unittest.TestCase):
    def test_accepts_matching_outputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binaries = {
                name: write_stub(root / name, sleep_seconds=0, output="//:a\n//:b\n")
                for name in IMPLEMENTATIONS
            }
            check_parity(stub_workload(comparison="lines"), binaries, root)

    def test_rejects_diverging_outputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binaries = {
                "kotlin": write_stub(root / "kotlin", sleep_seconds=0, output="//:a\n"),
                "rust": write_stub(root / "rust", sleep_seconds=0, output="//:b\n"),
            }
            with self.assertRaises(ParityError) as raised:
                check_parity(stub_workload(comparison="lines"), binaries, root)
            self.assertIn("disagree", str(raised.exception))

    def test_skips_workloads_without_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binaries = {
                name: write_stub(root / name, sleep_seconds=0) for name in IMPLEMENTATIONS
            }
            check_parity(stub_workload(), binaries, root)


class RunRoundsTest(unittest.TestCase):
    def test_discards_warmup_rounds(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binaries = {
                name: write_stub(root / name, sleep_seconds=0) for name in IMPLEMENTATIONS
            }
            samples = run_rounds(
                stub_workload(), binaries, root, rounds=2, warmup_rounds=3
            )
            self.assertEqual(len(samples["kotlin"]), 2)
            self.assertEqual(len(samples["rust"]), 2)

    def test_alternates_which_implementation_runs_first(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / "order.log"
            binaries = {
                name: write_stub(root / name, sleep_seconds=0, log=log)
                for name in IMPLEMENTATIONS
            }
            run_rounds(stub_workload(), binaries, root, rounds=4, warmup_rounds=0)
            order = log.read_text().split()
            self.assertEqual(
                order,
                ["kotlin", "rust", "rust", "kotlin"] * 2,
            )

    def test_cleans_up_per_round_output_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binaries = {
                name: write_stub(root / name, sleep_seconds=0, output="//:a\n")
                for name in IMPLEMENTATIONS
            }
            run_rounds(
                stub_workload(comparison="lines"),
                binaries,
                root,
                rounds=2,
                warmup_rounds=0,
            )
            self.assertEqual(list(root.glob("stub-*.txt")), [])


# --------------------------------------------------------------------------
# Workload selection and binary resolution
# --------------------------------------------------------------------------


class WorkloadSelectionTest(unittest.TestCase):
    def test_default_set_covers_both_commands(self):
        names = [spec.name for spec in default_workload_specs(1.0)]
        self.assertIn(STARTUP_WORKLOAD, names)
        self.assertIn("generate-hashes-large", names)
        self.assertIn("get-impacted-targets", names)
        self.assertIn("get-impacted-targets-distances", names)

    def test_scale_grows_the_workloads(self):
        small = default_workload_specs(1.0)
        large = default_workload_specs(4.0)
        self.assertNotEqual(
            [spec.description for spec in small], [spec.description for spec in large]
        )

    def test_selection_keeps_the_start_up_baseline(self):
        specs = default_workload_specs(1.0)
        selected = select_workloads(specs, ["get-impacted-targets"])
        self.assertEqual(
            [spec.name for spec in selected],
            [STARTUP_WORKLOAD, "get-impacted-targets"],
        )

    def test_empty_selection_means_everything(self):
        specs = default_workload_specs(1.0)
        self.assertEqual(len(select_workloads(specs, [])), len(specs))

    def test_unknown_workload_is_rejected(self):
        with self.assertRaises(ValueError) as raised:
            select_workloads(default_workload_specs(1.0), ["nope"])
        self.assertIn("nope", str(raised.exception))


class PreparedDefaultWorkloadTest(unittest.TestCase):
    """Prepare the real workloads at a tiny scale and inspect what they will run."""

    def setUp(self):
        self._temp = tempfile.TemporaryDirectory()
        root = Path(self._temp.name)
        self.prepared = {}
        for spec in default_workload_specs(0.002):
            directory = root / spec.name
            directory.mkdir(parents=True, exist_ok=True)
            self.prepared[spec.name] = (spec.prepare(directory), directory)

    def tearDown(self):
        self._temp.cleanup()

    def test_generate_hashes_points_at_the_replay_shim_and_workspace(self):
        workload, directory = self.prepared["generate-hashes-small"]
        self.assertEqual(workload.arguments[0], "generate-hashes")
        self.assertIn("--excludeExternalTargets", workload.arguments)
        self.assertIn(str((directory / "bazel").resolve()), workload.arguments)
        self.assertIn(str((directory / "workspace").resolve()), workload.arguments)
        self.assertTrue((directory / "targets.pb").is_file())
        self.assertEqual(workload.comparison, "hashes")
        self.assertGreater(workload.detail["targets"], 0)

    def test_impacted_targets_pins_a_bazel_executable(self):
        # Without -b, the Kotlin CLI execs whatever `bazel` is on PATH (or fails when
        # there is none) while diffing, which is neither hermetic nor comparable.
        for name in ("get-impacted-targets", "get-impacted-targets-distances"):
            with self.subTest(workload=name):
                workload, directory = self.prepared[name]
                self.assertIn("-b", workload.arguments)
                self.assertIn(str((directory / "bazel").resolve()), workload.arguments)
                self.assertTrue((directory / "bazel").is_file())

    def test_distance_workload_passes_dependency_edges_and_reads_json(self):
        workload, directory = self.prepared["get-impacted-targets-distances"]
        self.assertIn("-d", workload.arguments)
        self.assertIn(str((directory / "dep-edges.json").resolve()), workload.arguments)
        self.assertEqual(workload.comparison, "json")

        plain, _ = self.prepared["get-impacted-targets"]
        self.assertNotIn("-d", plain.arguments)
        self.assertEqual(plain.comparison, "lines")

    def test_start_up_workload_only_asks_for_the_version(self):
        workload, _ = self.prepared[STARTUP_WORKLOAD]
        self.assertEqual(workload.arguments, ("--version",))
        self.assertFalse(workload.produces_output)


class ResolveBinariesTest(unittest.TestCase):
    def test_uses_explicit_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kotlin = write_stub(root / "kotlin", sleep_seconds=0)
            rust = write_stub(root / "rust", sleep_seconds=0)
            resolved = resolve_binaries(kotlin, rust)
            self.assertEqual(resolved["kotlin"], kotlin.resolve())
            self.assertEqual(resolved["rust"], rust.resolve())

    def test_requires_both_paths(self):
        with self.assertRaises(ValueError):
            resolve_binaries(Path("kotlin"), None)

    def test_rejects_missing_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kotlin = write_stub(root / "kotlin", sleep_seconds=0)
            with self.assertRaises(ValueError) as raised:
                resolve_binaries(kotlin, root / "absent")
            self.assertIn("does not exist", str(raised.exception))


# --------------------------------------------------------------------------
# End-to-end gate behaviour
# --------------------------------------------------------------------------


def _stub_specs() -> list[WorkloadSpec]:
    """Two stub workloads: the start-up baseline and one that writes output."""
    return [
        WorkloadSpec(
            name=STARTUP_WORKLOAD,
            description="stub start-up",
            prepare=lambda directory: PreparedWorkload(
                name=STARTUP_WORKLOAD,
                description="stub start-up",
                detail={},
                arguments=("--version",),
            ),
        ),
        WorkloadSpec(
            name="stub-work",
            description="stub work",
            prepare=lambda directory: PreparedWorkload(
                name="stub-work",
                description="stub work",
                detail={"kind": "stub"},
                arguments=("work",),
                comparison="lines",
                output_argument=("-o",),
                output_suffix=".txt",
            ),
        ),
    ]


class RunGateTest(unittest.TestCase):
    def _binaries(self, root: Path, *, kotlin_sleep: float, rust_sleep: float):
        return {
            "kotlin": write_stub(
                root / "kotlin", sleep_seconds=kotlin_sleep, output="//:a\n"
            ),
            "rust": write_stub(root / "rust", sleep_seconds=rust_sleep, output="//:a\n"),
        }

    def test_passes_when_rust_is_consistently_faster(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binaries = self._binaries(root, kotlin_sleep=0.2, rust_sleep=0.0)
            reports = run_gate(
                _stub_specs(),
                binaries,
                root / "fixtures",
                rounds=2,
                warmup_rounds=0,
                thresholds=Thresholds(),
            )
            self.assertTrue(all(report.passed for report in reports))
            self.assertEqual([report.name for report in reports], [STARTUP_WORKLOAD, "stub-work"])

    def test_fails_when_rust_is_slower(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binaries = self._binaries(root, kotlin_sleep=0.0, rust_sleep=0.2)
            reports = run_gate(
                _stub_specs(),
                binaries,
                root / "fixtures",
                rounds=2,
                warmup_rounds=0,
                thresholds=Thresholds(),
            )
            self.assertFalse(all(report.passed for report in reports))

    def test_stops_on_a_parity_mismatch(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binaries = {
                "kotlin": write_stub(root / "kotlin", sleep_seconds=0, output="//:a\n"),
                "rust": write_stub(root / "rust", sleep_seconds=0, output="//:b\n"),
            }
            with self.assertRaises(ParityError):
                run_gate(
                    _stub_specs(),
                    binaries,
                    root / "fixtures",
                    rounds=1,
                    warmup_rounds=0,
                    thresholds=Thresholds(),
                )


class ArgumentParsingTest(unittest.TestCase):
    def test_defaults_demand_a_win_every_round(self):
        args = parse_args([])
        self.assertEqual(args.rounds, 5)
        self.assertEqual(args.warmup_rounds, 1)
        self.assertEqual(args.min_speedup, 1.0)
        self.assertEqual(args.min_win_rate, 1.0)
        self.assertEqual(args.min_logic_speedup, 1.0)
        self.assertEqual(args.scale, 1.0)
        self.assertEqual(args.rss_runs, 0)

    def test_collects_repeated_workload_flags(self):
        args = parse_args(["--workload", "a", "--workload", "b"])
        self.assertEqual(args.workloads, ["a", "b"])

    def test_rejects_nonsense_values(self):
        for arguments in (
            ["--rounds", "0"],
            ["--warmup-rounds", "-1"],
            ["--scale", "0"],
            ["--rss-runs", "-1"],
            ["--min-win-rate", "1.5"],
            ["--max-rss-ratio", "2"],
        ):
            with self.subTest(arguments=arguments):
                with self.assertRaises(SystemExit):
                    parse_args(arguments)


class MainTest(unittest.TestCase):
    def test_lists_workloads(self):
        self.assertEqual(main(["--list-workloads"]), 0)

    def test_reports_missing_binary_pairs(self):
        self.assertEqual(main(["--kotlin-binary", "only-one"]), 2)

    def test_returns_zero_and_writes_json_when_rust_wins(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kotlin = write_stub(root / "kotlin", sleep_seconds=0.2)
            rust = write_stub(root / "rust", sleep_seconds=0.0)
            report = root / "report" / "perf.json"
            exit_code = main(
                [
                    "--kotlin-binary",
                    str(kotlin),
                    "--rust-binary",
                    str(rust),
                    "--workload",
                    STARTUP_WORKLOAD,
                    "--rounds",
                    "2",
                    "--warmup-rounds",
                    "0",
                    "--json",
                    str(report),
                    "--fixture-dir",
                    str(root / "fixtures"),
                ]
            )
            self.assertEqual(exit_code, 0)
            document = json.loads(report.read_text())
            self.assertTrue(document["passed"])
            self.assertEqual(len(document["workloads"]), 1)

    def test_returns_one_when_rust_is_slower(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kotlin = write_stub(root / "kotlin", sleep_seconds=0.0)
            rust = write_stub(root / "rust", sleep_seconds=0.2)
            exit_code = main(
                [
                    "--kotlin-binary",
                    str(kotlin),
                    "--rust-binary",
                    str(rust),
                    "--workload",
                    STARTUP_WORKLOAD,
                    "--rounds",
                    "2",
                    "--warmup-rounds",
                    "0",
                ]
            )
            self.assertEqual(exit_code, 1)

    def test_returns_two_when_a_binary_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            failing = root / "kotlin"
            failing.write_text("#!/usr/bin/env bash\nexit 3\n")
            failing.chmod(0o755)
            rust = write_stub(root / "rust", sleep_seconds=0.0)
            exit_code = main(
                [
                    "--kotlin-binary",
                    str(failing),
                    "--rust-binary",
                    str(rust),
                    "--workload",
                    STARTUP_WORKLOAD,
                    "--rounds",
                    "1",
                    "--warmup-rounds",
                    "0",
                ]
            )
            self.assertEqual(exit_code, 2)

    def test_rejects_unknown_workloads(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            kotlin = write_stub(root / "kotlin", sleep_seconds=0.0)
            rust = write_stub(root / "rust", sleep_seconds=0.0)
            exit_code = main(
                [
                    "--kotlin-binary",
                    str(kotlin),
                    "--rust-binary",
                    str(rust),
                    "--workload",
                    "does-not-exist",
                ]
            )
            self.assertEqual(exit_code, 2)


if __name__ == "__main__":
    unittest.main()
