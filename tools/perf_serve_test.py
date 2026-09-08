#!/usr/bin/env python3
"""Unit tests for the serve workload of the Kotlin-vs-Rust performance gate.

These tests never launch a real bazel-diff binary: ``serve`` is a long-running
HTTP server, so the stand-in here is a stub *script* that runs a tiny stdlib
``http.server`` answering ``/health`` and ``/impacted_targets`` with content
and timing the test controls -- the same "stub instead of real binary"
philosophy as ``perf_gate_test.py``, applied to a server instead of a
one-shot process.
"""

from __future__ import annotations

import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import perf_serve
from perf_gate import ParityError
from perf_serve import (
    ServerStartupError,
    ServeWorkloadSpec,
    check_serve_parity,
    default_serve_specs,
    latency_summary,
    measure_serve_workload,
    prepare_serve_fixture,
    run_requests,
    start_server,
)
from perf_workload import GraphSpec


def write_stub_server(
    path: Path,
    *,
    answer: str = '["//pkg/p00000:t0"]',
    ready_after_requests: int = 0,
    fail_health: bool = False,
) -> Path:
    """Write a tiny stdlib-only HTTP server standing in for ``binary serve``.

    ``ready_after_requests`` makes ``/health`` return 503 for that many probes
    before turning ready, so ``start_server``'s polling loop has something to
    poll. ``fail_health`` makes it never become ready, for the timeout path.
    """
    script = textwrap.dedent(
        f"""\
        #!/usr/bin/env python3
        import sys
        from http.server import BaseHTTPRequestHandler, HTTPServer

        port = None
        for i, a in enumerate(sys.argv):
            if a == "--port":
                port = int(sys.argv[i + 1])
        assert port is not None

        health_calls = [0]

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_GET(self):
                if self.path == "/health":
                    health_calls[0] += 1
                    if {fail_health!r} or health_calls[0] <= {ready_after_requests!r}:
                        self.send_response(503)
                        self.end_headers()
                    else:
                        self.send_response(200)
                        self.end_headers()
                elif self.path.startswith("/impacted_targets"):
                    body = {answer!r}.encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                else:
                    self.send_response(404)
                    self.end_headers()

        HTTPServer(("127.0.0.1", port), Handler).serve_forever()
        """
    )
    path.write_text(script)
    path.chmod(0o755)
    return path


class FreePortTest(unittest.TestCase):
    def test_returns_a_usable_port(self):
        port = perf_serve.free_port()
        self.assertGreater(port, 0)


class StartServerTest(unittest.TestCase):
    def test_waits_for_health_then_returns_handle(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            binary = write_stub_server(directory / "stub.py", ready_after_requests=2)
            fixture = prepare_serve_fixture(
                directory, ServeWorkloadSpec(name="t", description="t", graph=GraphSpec(packages=2))
            )
            handle = start_server(
                _python_stub(binary),
                fixture,
                directory / "cache",
                port=perf_serve.free_port(),
                ready_timeout=10.0,
            )
            try:
                self.assertGreater(handle.time_to_ready_seconds, 0)
            finally:
                handle.stop()

    def test_times_out_when_never_ready(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            binary = write_stub_server(directory / "stub.py", fail_health=True)
            fixture = prepare_serve_fixture(
                directory, ServeWorkloadSpec(name="t", description="t", graph=GraphSpec(packages=2))
            )
            with self.assertRaises(ServerStartupError):
                start_server(
                    _python_stub(binary),
                    fixture,
                    directory / "cache",
                    port=perf_serve.free_port(),
                    ready_timeout=1.0,
                )


class RunRequestsTest(unittest.TestCase):
    def test_collects_count_and_status(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            binary = write_stub_server(directory / "stub.py")
            fixture = prepare_serve_fixture(
                directory, ServeWorkloadSpec(name="t", description="t", graph=GraphSpec(packages=2))
            )
            handle = start_server(
                _python_stub(binary),
                fixture,
                directory / "cache",
                port=perf_serve.free_port(),
                ready_timeout=10.0,
            )
            try:
                samples = run_requests(handle, fixture, count=6, concurrency=3, timeout=10.0)
            finally:
                handle.stop()
            self.assertEqual(len(samples), 6)
            self.assertTrue(all(sample.status == 200 for sample in samples))
            self.assertTrue(all(sample.latency_seconds >= 0 for sample in samples))


class LatencySummaryTest(unittest.TestCase):
    def test_reports_median_and_percentiles(self):
        summary = latency_summary([0.1, 0.2, 0.3, 0.4, 0.5, 0.6])
        self.assertEqual(summary["runs"], 6)
        self.assertAlmostEqual(summary["median_seconds"], 0.35)
        self.assertIn("p90_seconds", summary)
        self.assertIn("p99_seconds", summary)

    def test_omits_percentiles_below_threshold(self):
        summary = latency_summary([0.1, 0.2])
        self.assertNotIn("p90_seconds", summary)

    def test_rejects_empty_samples(self):
        with self.assertRaises(ValueError):
            latency_summary([])


class CheckServeParityTest(unittest.TestCase):
    def test_passes_on_identical_answers(self):
        sample = perf_serve.RequestSample(status=200, body='["//a:b"]', latency_seconds=0.01)
        check_serve_parity([sample], [sample])

    def test_fails_on_mismatched_answers(self):
        kotlin = perf_serve.RequestSample(status=200, body='["//a:b"]', latency_seconds=0.01)
        rust = perf_serve.RequestSample(status=200, body='["//a:c"]', latency_seconds=0.01)
        with self.assertRaises(ParityError):
            check_serve_parity([kotlin], [rust])

    def test_fails_on_non_200(self):
        ok = perf_serve.RequestSample(status=200, body='[]', latency_seconds=0.01)
        bad = perf_serve.RequestSample(status=500, body='', latency_seconds=0.01)
        with self.assertRaises(ParityError):
            check_serve_parity([ok], [bad])


class MeasureServeWorkloadTest(unittest.TestCase):
    def test_produces_a_gateable_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            spec = ServeWorkloadSpec(
                name="serve-test",
                description="test",
                graph=GraphSpec(packages=2),
                request_count=4,
                concurrency=2,
                ready_timeout=10.0,
            )
            kotlin_binary = _python_stub(write_stub_server(directory / "kotlin_stub.py"))
            rust_binary = _python_stub(write_stub_server(directory / "rust_stub.py"))
            report = measure_serve_workload(
                spec,
                {"kotlin": kotlin_binary, "rust": rust_binary},
                directory,
                rounds=2,
            )
            self.assertEqual(report.name, "serve-test")
            self.assertIn("median_seconds", report.results["kotlin"])
            self.assertIn("median_seconds", report.results["rust"])
            self.assertIsNotNone(report.speedup)
            self.assertIn("speedup", report.logic)
            self.assertEqual(len(report.samples["kotlin"]), spec.request_count * 2)

    def test_raises_parity_error_on_disagreement(self):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            spec = ServeWorkloadSpec(
                name="serve-test",
                description="test",
                graph=GraphSpec(packages=2),
                request_count=2,
                concurrency=1,
                ready_timeout=10.0,
            )
            kotlin_binary = _python_stub(
                write_stub_server(directory / "kotlin_stub.py", answer='["//a:b"]')
            )
            rust_binary = _python_stub(
                write_stub_server(directory / "rust_stub.py", answer='["//a:c"]')
            )
            with self.assertRaises(ParityError):
                measure_serve_workload(
                    spec,
                    {"kotlin": kotlin_binary, "rust": rust_binary},
                    directory,
                    rounds=1,
                )


class DefaultServeSpecsTest(unittest.TestCase):
    def test_names_are_unique_and_scale_applies(self):
        specs = default_serve_specs(1.0)
        names = [spec.name for spec in specs]
        self.assertEqual(len(names), len(set(names)))
        scaled = default_serve_specs(0.1)
        self.assertLessEqual(scaled[0].graph.packages, specs[0].graph.packages)


def _python_stub(script: Path) -> Path:
    """Wrap a python stub script in a shim so it can be invoked as ``binary serve ...``.

    ``start_server`` invokes ``[binary, "serve", ...]`` positionally; the stub script
    itself ignores the leading "serve" token (argv[1]) and only looks for ``--port``.
    """
    wrapper = script.with_name(script.name + ".sh")
    wrapper.write_text(f"#!/usr/bin/env bash\nexec python3 {script} \"$@\"\n")
    wrapper.chmod(0o755)
    return wrapper


if __name__ == "__main__":
    unittest.main()
