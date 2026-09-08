#!/usr/bin/env python3
"""Serve-command workload for the Kotlin/Rust performance gate.

``perf_gate.py``'s existing workloads all share one shape: run a binary once
to completion, time it, diff its output file. The ``serve`` subcommand is a
long-running HTTP query server -- start it, wait for ``/health``, fire many
requests, then kill it -- which does not fit that model at all. This module
is the parallel pipeline for it: build a real (but tiny, hermetic) git
workspace, start both servers against it, verify they answer identically,
then measure request latency and time-to-ready for each.

Results are folded into the same :class:`perf_gate.WorkloadReport` shape the
rest of the gate uses, so ``evaluate``, ``format_report`` and
``build_document`` need no serve-specific branches.
"""

from __future__ import annotations

import contextlib
import json
import socket
import statistics
import subprocess
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Sequence

from perf_gate import (
    IMPLEMENTATIONS,
    ParityError,
    WorkloadReport,
    paired_win_rate,
    ratio,
)
from perf_workload import GraphSpec, write_git_workspace

# Requests below this count make percentile math meaningless -- guard rather
# than report noise dressed up as p99.
MIN_REQUESTS_FOR_PERCENTILES = 5


@dataclass(frozen=True)
class ServeWorkloadSpec:
    """Shape of one serve scenario: a small git workspace plus a request load."""

    name: str
    description: str
    graph: GraphSpec
    request_count: int = 40
    concurrency: int = 4
    ready_timeout: float = 30.0
    request_timeout: float = 30.0

    def __post_init__(self) -> None:
        if self.request_count < 1:
            raise ValueError(f"request_count must be at least 1, got {self.request_count}")
        if self.concurrency < 1:
            raise ValueError(f"concurrency must be at least 1, got {self.concurrency}")


@dataclass(frozen=True)
class ServeFixture:
    """A materialized git workspace both servers query against."""

    name: str
    repo: Path
    from_sha: str
    to_sha: str


def prepare_serve_fixture(directory: Path, spec: ServeWorkloadSpec) -> ServeFixture:
    """Materialize the git repo once; both implementations query the same fixture."""
    repo, shas = write_git_workspace(directory / "repo", spec.graph, revisions=2)
    return ServeFixture(name=spec.name, repo=repo, from_sha=shas[0], to_sha=shas[-1])


# --------------------------------------------------------------------------
# Server lifecycle
# --------------------------------------------------------------------------


def free_port() -> int:
    """Ask the OS for a free port. There is a small bind/release race, same as
    the one already accepted by the Rust e2e harness -- Rust's ``serve`` has no
    ``--portFile`` equivalent to Kotlin's, so both implementations are driven
    through a pre-picked port rather than an OS-assigned ephemeral one."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def http_call(
    port: int, path: str, *, method: str = "GET", timeout: float = 30.0
) -> tuple[int | None, str]:
    """Issue one HTTP request against a local serve instance.

    Returns ``(status, body)``; ``status`` is ``None`` on a transport error
    (connection refused, timeout) rather than raising, since a refused
    connection during start-up polling is expected, not exceptional.
    """
    url = f"http://127.0.0.1:{port}{path}"
    request = urllib.request.Request(url, method=method)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as error:
        with error:
            return error.code, error.read().decode("utf-8", "replace")
    except (urllib.error.URLError, OSError):
        return None, ""


@dataclass
class ServerHandle:
    process: subprocess.Popen
    port: int
    time_to_ready_seconds: float

    def stop(self) -> None:
        with contextlib.suppress(Exception):
            self.process.terminate()
            try:
                self.process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=8)


class ServerStartupError(RuntimeError):
    """Raised when a server never becomes ready. Not a perf signal -- a hard failure."""


def start_server(
    binary: Path,
    fixture: ServeFixture,
    cache_dir: Path,
    *,
    port: int,
    ready_timeout: float,
    env: dict[str, str] | None = None,
) -> ServerHandle:
    """Spawn ``binary serve`` against ``fixture`` and block until ``/health`` is ready."""
    cache_dir.mkdir(parents=True, exist_ok=True)
    args = [
        str(binary),
        "serve",
        "-w",
        str(fixture.repo),
        "-b",
        "bazel",
        "--cacheDir",
        str(cache_dir),
        "--port",
        str(port),
        "--no-initial-fetch",
        "--excludeExternalTargets",
    ]
    start = time.perf_counter()
    process = subprocess.Popen(
        args,
        cwd=fixture.repo,
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    deadline = start + ready_timeout
    while time.perf_counter() < deadline:
        if process.poll() is not None:
            raise ServerStartupError(
                f"{binary.name} serve exited before becoming ready (code {process.returncode})"
            )
        status, _ = http_call(port, "/health", timeout=2.0)
        if status == 200:
            return ServerHandle(
                process=process,
                port=port,
                time_to_ready_seconds=time.perf_counter() - start,
            )
        time.sleep(0.1)
    process.terminate()
    with contextlib.suppress(subprocess.TimeoutExpired):
        process.wait(timeout=8)
    raise ServerStartupError(f"{binary.name} serve did not become ready within {ready_timeout}s")


# --------------------------------------------------------------------------
# Load generation
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class RequestSample:
    status: int | None
    body: str
    latency_seconds: float


def run_requests(
    handle: ServerHandle,
    fixture: ServeFixture,
    *,
    count: int,
    concurrency: int,
    timeout: float,
) -> list[RequestSample]:
    """Fire ``count`` ``/impacted_targets`` requests at ``handle`` and time each one."""
    path = f"/impacted_targets?from={fixture.from_sha}&to={fixture.to_sha}"

    def one(_: int) -> RequestSample:
        start = time.perf_counter()
        status, body = http_call(handle.port, path, timeout=timeout)
        return RequestSample(status=status, body=body, latency_seconds=time.perf_counter() - start)

    with ThreadPoolExecutor(max_workers=concurrency) as pool:
        return list(pool.map(one, range(count)))


def check_serve_parity(
    kotlin_samples: Sequence[RequestSample], rust_samples: Sequence[RequestSample]
) -> None:
    """Fail unless both implementations answered every request the same way."""
    for implementation, samples in (("kotlin", kotlin_samples), ("rust", rust_samples)):
        failures = [sample for sample in samples if sample.status != 200]
        if failures:
            raise ParityError(
                f"serve: {implementation} returned {len(failures)}/{len(samples)} "
                f"non-200 responses (first: {failures[0].status})"
            )
    kotlin_answer = _parsed_response(kotlin_samples[0].body)
    rust_answer = _parsed_response(rust_samples[0].body)
    if kotlin_answer != rust_answer:
        raise ParityError(
            "serve: implementations disagree on /impacted_targets "
            f"(kotlin={sorted(set(kotlin_answer) - set(rust_answer))[:3]} only, "
            f"rust={sorted(set(rust_answer) - set(kotlin_answer))[:3]} only)"
        )


def _parsed_response(body: str) -> Any:
    data = json.loads(body)
    if isinstance(data, list):
        return sorted(data)
    return data


# --------------------------------------------------------------------------
# Percentiles
# --------------------------------------------------------------------------


def _percentile(sorted_values: Sequence[float], fraction: float) -> float:
    """Linear-interpolated percentile over an already-sorted sequence."""
    if len(sorted_values) == 1:
        return sorted_values[0]
    position = (len(sorted_values) - 1) * fraction
    lower, upper = int(position), min(int(position) + 1, len(sorted_values) - 1)
    if lower == upper:
        return sorted_values[lower]
    return sorted_values[lower] + (sorted_values[upper] - sorted_values[lower]) * (position - lower)


def latency_summary(values: Sequence[float]) -> dict[str, float]:
    """Reduce request latencies (seconds) to the statistics the report and gate use."""
    if not values:
        raise ValueError("cannot summarize an empty sample set")
    ordered = sorted(values)
    summary = {
        "runs": len(ordered),
        "median_seconds": round(statistics.median(ordered), 6),
        "mean_seconds": round(statistics.fmean(ordered), 6),
        "min_seconds": round(ordered[0], 6),
        "max_seconds": round(ordered[-1], 6),
        "stdev_seconds": round(statistics.stdev(ordered), 6) if len(ordered) > 1 else 0.0,
    }
    if len(ordered) >= MIN_REQUESTS_FOR_PERCENTILES:
        summary["p90_seconds"] = round(_percentile(ordered, 0.90), 6)
        summary["p99_seconds"] = round(_percentile(ordered, 0.99), 6)
    return summary


# --------------------------------------------------------------------------
# Orchestration
# --------------------------------------------------------------------------


def measure_serve_workload(
    spec: ServeWorkloadSpec,
    binaries: dict[str, Path],
    directory: Path,
    *,
    rounds: int,
    env: dict[str, str] | None = None,
) -> WorkloadReport:
    """Run ``rounds`` interleaved start/measure/stop cycles for both servers."""
    fixture = prepare_serve_fixture(directory, spec)
    latencies: dict[str, list[float]] = {name: [] for name in IMPLEMENTATIONS}
    startup: dict[str, list[float]] = {name: [] for name in IMPLEMENTATIONS}
    round_medians: dict[str, list[float]] = {name: [] for name in IMPLEMENTATIONS}
    parity_checked = False
    kotlin_samples: list[RequestSample] | None = None
    rust_samples: list[RequestSample] | None = None

    for round_index in range(rounds):
        order = IMPLEMENTATIONS if round_index % 2 == 0 else tuple(reversed(IMPLEMENTATIONS))
        round_samples: dict[str, list[RequestSample]] = {}
        for implementation in order:
            cache_dir = directory / f"cache-{implementation}-{round_index}"
            handle = start_server(
                binaries[implementation],
                fixture,
                cache_dir,
                port=free_port(),
                ready_timeout=spec.ready_timeout,
                env=env,
            )
            try:
                samples = run_requests(
                    handle,
                    fixture,
                    count=spec.request_count,
                    concurrency=spec.concurrency,
                    timeout=spec.request_timeout,
                )
            finally:
                handle.stop()
            round_samples[implementation] = samples
            startup[implementation].append(handle.time_to_ready_seconds)
            latencies[implementation].extend(sample.latency_seconds for sample in samples)
            round_medians[implementation].append(
                statistics.median(sample.latency_seconds for sample in samples)
            )
        if not parity_checked:
            kotlin_samples, rust_samples = round_samples["kotlin"], round_samples["rust"]
            parity_checked = True

    check_serve_parity(kotlin_samples, rust_samples)

    results = {name: latency_summary(latencies[name]) for name in IMPLEMENTATIONS}
    startup_results = {
        name: round(statistics.median(startup[name]), 6) for name in IMPLEMENTATIONS
    }
    speedup = ratio(results["kotlin"]["median_seconds"], results["rust"]["median_seconds"])
    startup_speedup = ratio(startup_results["kotlin"], startup_results["rust"])
    win_rate = paired_win_rate(round_medians["kotlin"], round_medians["rust"])

    return WorkloadReport(
        name=spec.name,
        description=spec.description,
        detail={
            "kind": "serve",
            "packages": spec.graph.packages,
            "request_count": spec.request_count,
            "concurrency": spec.concurrency,
            "rounds": rounds,
        },
        results=results,
        samples={name: [round(v, 6) for v in latencies[name]] for name in IMPLEMENTATIONS},
        speedup=speedup,
        logic={
            "kotlin_seconds": startup_results["kotlin"],
            "rust_seconds": startup_results["rust"],
            "speedup": startup_speedup,
            "note": "startup here means server time-to-ready (/health 200), not request latency",
        },
        win_rate=win_rate,
    )


def default_serve_specs(scale: float) -> list[ServeWorkloadSpec]:
    """The serve workloads the gate measures by default, scaled by ``scale``."""
    small_graph = GraphSpec(packages=20).scaled(scale)
    large_graph = GraphSpec(packages=100).scaled(scale)
    return [
        ServeWorkloadSpec(
            name="serve-small",
            description=f"serve queries against a {small_graph.target_count}-target workspace",
            graph=small_graph,
        ),
        ServeWorkloadSpec(
            name="serve-large",
            description=f"serve queries against a {large_graph.target_count}-target workspace",
            graph=large_graph,
            request_count=80,
        ),
    ]
