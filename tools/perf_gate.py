#!/usr/bin/env python3
"""Performance gate: prove the Rust implementation beats the Kotlin one.

The Rust rewrite only earns its keep if it is *always* faster than the Kotlin
CLI it replaces. This runner turns that claim into a check that can fail:

1. build the hermetic workloads from :mod:`perf_workload` -- a synthetic
   ``streamed_proto`` fixture plus the workspace it describes, and hash-file
   pairs for the diff commands -- so both binaries do byte-identical work with
   no Bazel server, network or checkout involved;
2. run the two binaries **interleaved**, alternating which one goes first each
   round, so a machine that slows down mid-run penalizes both equally instead
   of whichever implementation happened to run second;
3. check that they produced the same answer before comparing how long they
   took -- a speedup over different work is not a speedup;
4. report, per workload, the median speedup, the *startup-adjusted* speedup
   (median wall time minus that implementation's own ``--version`` time, which
   is what isolates the hashing/diffing logic from JVM start-up), and the paired
   win rate -- how many of the interleaved rounds Rust actually won;
5. exit non-zero if any workload misses a threshold.

Defaults encode the project's rule: Rust must win every paired round
(``--min-win-rate 1.0``) and must not be slower on medians or on
startup-adjusted medians. Loosen them on noisy hardware; do not loosen them to
paper over a regression.

Example::

    bazel run -c opt //tools:perf-gate
    bazel run -c opt //tools:perf-gate -- --rounds 9 --scale 2 --json perf.json
    python3 tools/perf_gate.py \\
        --kotlin-binary bazel-bin/cli/bazel-diff \\
        --rust-binary bazel-bin/src/bazel-diff
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable, Sequence

from benchmark import (
    KOTLIN_RUNFILE,
    RUST_RUNFILE,
    load_hashes,
    measure_peak_rss,
)
from perf_workload import (
    GraphSpec,
    HashFileSpec,
    write_graph_fixture,
    write_hash_files,
    write_replay_bazel,
    write_workspace,
)

try:
    from python.runfiles import runfiles as _runfiles
except ImportError:  # pragma: no cover - exercised only outside Bazel
    _runfiles = None


IMPLEMENTATIONS = ("kotlin", "rust")

# Name of the workload whose median wall time is treated as each implementation's
# fixed process start-up cost (JVM boot for Kotlin, exec + clap for Rust).
STARTUP_WORKLOAD = "startup"


# --------------------------------------------------------------------------
# Workload description
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class PreparedWorkload:
    """A single measurable command, already materialized on disk.

    ``arguments`` are implementation-independent: both CLIs accept the same
    flags, which is precisely why they can be compared. The output path is
    appended last, positionally or behind ``output_argument``, so each run can
    write somewhere unique and the runner can diff the results.
    """

    name: str
    description: str
    detail: dict[str, Any]
    arguments: tuple[str, ...]
    comparison: str = "none"
    output_argument: tuple[str, ...] = ()
    output_suffix: str = ".json"
    cwd: Path | None = None

    @property
    def produces_output(self) -> bool:
        return self.comparison != "none"

    def command(self, binary: Path, output: Path | None) -> list[str]:
        command = [str(binary), *self.arguments]
        if self.produces_output:
            if output is None:
                raise ValueError(f"workload {self.name} requires an output path")
            command.extend([*self.output_argument, str(output)])
        return command


def normalize_output(comparison: str, path: Path) -> Any:
    """Read a run's output into a form two implementations can be compared on.

    Kotlin has historically written the hash map both flat and wrapped in a
    ``hashes`` object, and the impacted-target list is newline separated with no
    guaranteed order, so raw file bytes are the wrong comparison unit.
    """
    if comparison == "none":
        return None
    if comparison == "hashes":
        return load_hashes(path)
    if comparison == "json":
        return json.loads(path.read_text())
    if comparison == "lines":
        return sorted(line for line in path.read_text().splitlines() if line.strip())
    raise ValueError(f"unknown comparison mode: {comparison}")


def describe_difference(comparison: str, kotlin: Any, rust: Any) -> str:
    """Summarize *why* two outputs differ, without dumping thousands of lines."""
    if comparison in ("hashes", "json") and isinstance(kotlin, dict) and isinstance(rust, dict):
        kotlin_only = sorted(set(kotlin) - set(rust))
        rust_only = sorted(set(rust) - set(kotlin))
        mismatched = sorted(
            key for key in set(kotlin) & set(rust) if kotlin[key] != rust[key]
        )
        return (
            f"{len(kotlin_only)} kotlin-only keys {kotlin_only[:3]}, "
            f"{len(rust_only)} rust-only keys {rust_only[:3]}, "
            f"{len(mismatched)} differing values {mismatched[:3]}"
        )
    if isinstance(kotlin, list) and isinstance(rust, list):
        kotlin_only = sorted(set(kotlin) - set(rust))
        rust_only = sorted(set(rust) - set(kotlin))
        return (
            f"{len(kotlin_only)} kotlin-only entries {kotlin_only[:3]}, "
            f"{len(rust_only)} rust-only entries {rust_only[:3]}"
        )
    return "outputs differ"


# --------------------------------------------------------------------------
# Workload construction
# --------------------------------------------------------------------------


@dataclass(frozen=True)
class WorkloadSpec:
    """A workload before its fixtures exist; ``prepare`` materializes them."""

    name: str
    description: str
    prepare: Callable[[Path], PreparedWorkload]


def _startup_workload(directory: Path) -> PreparedWorkload:
    del directory
    return PreparedWorkload(
        name=STARTUP_WORKLOAD,
        description="process start-up floor (--version)",
        detail={"kind": "startup"},
        arguments=("--version",),
        comparison="none",
    )


def _prepare_generate_hashes(
    name: str, description: str, spec: GraphSpec, directory: Path
) -> PreparedWorkload:
    workspace = write_workspace(directory / "workspace", spec)
    fixture = write_graph_fixture(directory / "targets.pb", spec)
    replay = write_replay_bazel(
        directory / "bazel", fixture.resolve(), (directory / "output-base").resolve()
    )
    return PreparedWorkload(
        name=name,
        description=description,
        detail={
            "kind": "generate-hashes",
            "packages": spec.packages,
            "targets": spec.target_count,
            "rules": spec.rule_count,
            "source_files": spec.source_count,
            "fixture_bytes": fixture.stat().st_size,
        },
        arguments=(
            "generate-hashes",
            "-w",
            str(workspace.resolve()),
            "-b",
            str(replay.resolve()),
            f"--bazelStartupOptions=--output_base={(directory / 'output-base').resolve()}",
            "--excludeExternalTargets",
        ),
        comparison="hashes",
    )


def _prepare_impacted_targets(
    name: str,
    description: str,
    spec: HashFileSpec,
    directory: Path,
    *,
    with_dep_edges: bool,
) -> PreparedWorkload:
    starting, final, dep_edges = write_hash_files(
        directory, spec, with_dep_edges=with_dep_edges
    )
    # Even with --excludeExternalTargets, the Kotlin CLI constructs its Bazel client
    # while diffing (module-change detection) and fails outright if the executable is
    # missing. Both implementations therefore get the same replay shim, whose `mod`
    # answer is "not a bzlmod workspace" -- no server, no query, same code path on both
    # sides. The fixture is empty because nothing here should ever run a query.
    fixture = directory / "empty-targets.pb"
    fixture.write_bytes(b"")
    replay = write_replay_bazel(
        directory / "bazel", fixture.resolve(), (directory / "output-base").resolve()
    )
    arguments = [
        "get-impacted-targets",
        "--startingHashes",
        str(starting.resolve()),
        "--finalHashes",
        str(final.resolve()),
        "-w",
        str(directory.resolve()),
        "-b",
        str(replay.resolve()),
        # Pinned so neither implementation probes for bzlmod, which would make the
        # measurement depend on which side asked Bazel a question first.
        "--excludeExternalTargets",
    ]
    if dep_edges is not None:
        arguments.extend(["-d", str(dep_edges.resolve())])
    return PreparedWorkload(
        name=name,
        description=description,
        detail={
            "kind": "get-impacted-targets",
            "targets": spec.targets,
            "dep_edges": with_dep_edges,
            "deps_per_target": spec.deps_per_target if with_dep_edges else 0,
        },
        arguments=tuple(arguments),
        # With dependency edges the command emits JSON distance metrics; without
        # them it emits one label per line.
        comparison="json" if with_dep_edges else "lines",
        output_argument=("-o",),
        output_suffix=".json" if with_dep_edges else ".txt",
    )


def default_workload_specs(scale: float) -> list[WorkloadSpec]:
    """The workloads the gate measures by default, scaled by ``scale``.

    Sizes are picked so the Kotlin run is dominated by real work rather than by
    JVM start-up (otherwise the gate would only ever be measuring process
    launch), while keeping a full gate run to a few minutes on CI hardware.
    ``--scale`` multiplies the graph and hash-file sizes for deeper local runs.
    """
    small_graph = GraphSpec(packages=400).scaled(scale)
    large_graph = GraphSpec(packages=2000).scaled(scale)
    diff_spec = HashFileSpec(targets=150_000).scaled(scale)
    distance_spec = HashFileSpec(targets=40_000, deps_per_target=4).scaled(scale)
    return [
        WorkloadSpec(
            name=STARTUP_WORKLOAD,
            description="process start-up floor (--version)",
            prepare=_startup_workload,
        ),
        WorkloadSpec(
            name="generate-hashes-small",
            description=f"hash a {small_graph.target_count}-target graph",
            prepare=lambda directory: _prepare_generate_hashes(
                "generate-hashes-small",
                f"hash a {small_graph.target_count}-target graph",
                small_graph,
                directory,
            ),
        ),
        WorkloadSpec(
            name="generate-hashes-large",
            description=f"hash a {large_graph.target_count}-target graph",
            prepare=lambda directory: _prepare_generate_hashes(
                "generate-hashes-large",
                f"hash a {large_graph.target_count}-target graph",
                large_graph,
                directory,
            ),
        ),
        WorkloadSpec(
            name="get-impacted-targets",
            description=f"diff {diff_spec.targets} hashed targets",
            prepare=lambda directory: _prepare_impacted_targets(
                "get-impacted-targets",
                f"diff {diff_spec.targets} hashed targets",
                diff_spec,
                directory,
                with_dep_edges=False,
            ),
        ),
        WorkloadSpec(
            name="get-impacted-targets-distances",
            description=f"diff {distance_spec.targets} targets with distance metrics",
            prepare=lambda directory: _prepare_impacted_targets(
                "get-impacted-targets-distances",
                f"diff {distance_spec.targets} targets with distance metrics",
                distance_spec,
                directory,
                with_dep_edges=True,
            ),
        ),
    ]


def select_workloads(
    specs: Sequence[WorkloadSpec], names: Sequence[str]
) -> list[WorkloadSpec]:
    """Filter ``specs`` down to ``names``, always keeping the start-up baseline.

    The start-up workload is not optional: every other workload's
    startup-adjusted number is derived from it.
    """
    if not names:
        return list(specs)
    known = {spec.name for spec in specs}
    unknown = sorted(set(names) - known)
    if unknown:
        raise ValueError(
            f"unknown workload(s): {', '.join(unknown)}; available: {', '.join(sorted(known))}"
        )
    wanted = set(names) | {STARTUP_WORKLOAD}
    return [spec for spec in specs if spec.name in wanted]


# --------------------------------------------------------------------------
# Measurement
# --------------------------------------------------------------------------


def time_command(
    command: Sequence[str], *, cwd: Path | None = None, env: dict[str, str] | None = None
) -> float:
    """Run ``command`` to completion and return its wall time in seconds.

    Wall time of the whole process is deliberate: start-up is a real cost a user
    pays on every invocation. The startup-adjusted metric elsewhere in this
    module is what separates that cost from the hashing and diffing logic.
    """
    start = time.perf_counter()
    process = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    elapsed = time.perf_counter() - start
    if process.returncode != 0:
        tail = "\n".join(process.stderr.splitlines()[-20:])
        raise RuntimeError(
            f"command failed ({process.returncode}): {' '.join(command)}\n{tail}"
        )
    return elapsed


def check_parity(
    workload: PreparedWorkload,
    binaries: dict[str, Path],
    directory: Path,
    env: dict[str, str] | None = None,
) -> None:
    """Fail unless both implementations produce the same answer for ``workload``."""
    if not workload.produces_output:
        return
    outputs: dict[str, Any] = {}
    for implementation in IMPLEMENTATIONS:
        path = directory / f"parity-{workload.name}-{implementation}{workload.output_suffix}"
        time_command(
            workload.command(binaries[implementation], path),
            cwd=workload.cwd,
            env=env,
        )
        outputs[implementation] = normalize_output(workload.comparison, path)
    if outputs["kotlin"] != outputs["rust"]:
        raise ParityError(
            f"{workload.name}: implementations disagree "
            f"({describe_difference(workload.comparison, outputs['kotlin'], outputs['rust'])})"
        )


class ParityError(RuntimeError):
    """Raised when the two implementations do not compute the same result."""


def run_rounds(
    workload: PreparedWorkload,
    binaries: dict[str, Path],
    directory: Path,
    *,
    rounds: int,
    warmup_rounds: int,
    env: dict[str, str] | None = None,
) -> dict[str, list[float]]:
    """Time both implementations over interleaved rounds.

    Each round runs both binaries back to back and alternates which one starts,
    so any drift in machine speed (thermal throttling, a noisy neighbour on a CI
    runner) lands on both implementations rather than biasing one of them. The
    first ``warmup_rounds`` rounds are discarded: they pay for cold page cache
    and, on the Kotlin side, a cold JIT.
    """
    samples: dict[str, list[float]] = {name: [] for name in IMPLEMENTATIONS}
    for round_index in range(warmup_rounds + rounds):
        order = (
            IMPLEMENTATIONS
            if round_index % 2 == 0
            else tuple(reversed(IMPLEMENTATIONS))
        )
        for implementation in order:
            output = (
                directory
                / f"{workload.name}-{implementation}-{round_index}{workload.output_suffix}"
                if workload.produces_output
                else None
            )
            elapsed = time_command(
                workload.command(binaries[implementation], output),
                cwd=workload.cwd,
                env=env,
            )
            if output is not None:
                output.unlink(missing_ok=True)
            if round_index >= warmup_rounds:
                samples[implementation].append(elapsed)
    return samples


def sample_peak_rss(
    workload: PreparedWorkload,
    binaries: dict[str, Path],
    directory: Path,
    *,
    runs: int,
    env: dict[str, str],
) -> dict[str, list[int]]:
    """Sample peak process-tree RSS in separate, untimed runs."""
    samples: dict[str, list[int]] = {name: [] for name in IMPLEMENTATIONS}
    for implementation in IMPLEMENTATIONS:
        for run in range(runs):
            output = (
                directory / f"rss-{workload.name}-{implementation}-{run}{workload.output_suffix}"
                if workload.produces_output
                else None
            )
            samples[implementation].append(
                measure_peak_rss(
                    workload.command(binaries[implementation], output),
                    cwd=workload.cwd or Path.cwd(),
                    env=env,
                )
            )
            if output is not None:
                output.unlink(missing_ok=True)
    return samples


# --------------------------------------------------------------------------
# Statistics and thresholds
# --------------------------------------------------------------------------


def summarize(samples: Sequence[float]) -> dict[str, float]:
    """Reduce timing samples to the statistics the report and gate use."""
    if not samples:
        raise ValueError("cannot summarize an empty sample set")
    return {
        "runs": len(samples),
        "median_seconds": round(statistics.median(samples), 6),
        "mean_seconds": round(statistics.fmean(samples), 6),
        "min_seconds": round(min(samples), 6),
        "max_seconds": round(max(samples), 6),
        "stdev_seconds": (
            round(statistics.stdev(samples), 6) if len(samples) > 1 else 0.0
        ),
    }


def paired_win_rate(kotlin: Sequence[float], rust: Sequence[float]) -> float:
    """Fraction of interleaved rounds in which Rust was strictly faster."""
    if len(kotlin) != len(rust):
        raise ValueError("paired comparison requires equal sample counts")
    if not kotlin:
        raise ValueError("paired comparison requires at least one round")
    wins = sum(1 for left, right in zip(kotlin, rust) if right < left)
    return wins / len(kotlin)


def ratio(kotlin_seconds: float, rust_seconds: float) -> float | None:
    """Kotlin/Rust ratio: >1 means Rust is faster. ``None`` if undefined."""
    if rust_seconds <= 0:
        return None
    return round(kotlin_seconds / rust_seconds, 3)


def logic_seconds(total_seconds: float, startup_seconds: float | None) -> float | None:
    """Time attributable to the workload itself, with start-up subtracted.

    ``None`` when there is no start-up baseline, or when the workload is so
    short that subtracting start-up leaves nothing measurable -- in that case
    the number would be noise dressed up as a metric.
    """
    if startup_seconds is None:
        return None
    remainder = total_seconds - startup_seconds
    if remainder <= 0:
        return None
    return round(remainder, 6)


@dataclass(frozen=True)
class Thresholds:
    """The bar Rust has to clear. Defaults say "faster, every single round"."""

    min_speedup: float = 1.0
    min_win_rate: float = 1.0
    min_logic_speedup: float = 1.0
    max_rss_ratio: float | None = None


@dataclass
class WorkloadReport:
    name: str
    description: str
    detail: dict[str, Any]
    results: dict[str, dict[str, float]]
    samples: dict[str, list[float]]
    speedup: float | None
    logic: dict[str, Any]
    win_rate: float
    rss: dict[str, Any] | None = None
    failures: list[str] = field(default_factory=list)

    @property
    def passed(self) -> bool:
        return not self.failures


def evaluate(
    report: WorkloadReport, thresholds: Thresholds, *, is_startup: bool
) -> WorkloadReport:
    """Attach threshold failures to ``report``.

    The start-up workload is measured (every other workload's adjusted number
    depends on it) but only its win rate and speedup are gated -- there is no
    "logic" left in it once start-up is subtracted from start-up.
    """
    failures: list[str] = []
    if report.speedup is None:
        failures.append("rust wall time was not measurable")
    elif report.speedup < thresholds.min_speedup:
        failures.append(
            f"median speedup {report.speedup}x is below the required "
            f"{thresholds.min_speedup}x"
        )
    if report.win_rate < thresholds.min_win_rate:
        failures.append(
            f"rust won {report.win_rate:.0%} of paired rounds, below the required "
            f"{thresholds.min_win_rate:.0%}"
        )
    if not is_startup:
        logic_speedup = report.logic.get("speedup")
        if logic_speedup is None:
            # Not a failure: a workload can be too small for the adjustment to
            # mean anything. The report says so rather than inventing a number.
            report.logic.setdefault(
                "note", "workload too short to separate from process start-up"
            )
        elif logic_speedup < thresholds.min_logic_speedup:
            failures.append(
                f"startup-adjusted speedup {logic_speedup}x is below the required "
                f"{thresholds.min_logic_speedup}x"
            )
    if thresholds.max_rss_ratio is not None and report.rss:
        rss_ratio = report.rss.get("ratio")
        if rss_ratio is not None and rss_ratio > thresholds.max_rss_ratio:
            failures.append(
                f"rust peak RSS is {rss_ratio}x kotlin's, above the allowed "
                f"{thresholds.max_rss_ratio}x"
            )
    report.failures = failures
    return report


def build_report(
    workload: PreparedWorkload,
    samples: dict[str, list[float]],
    *,
    startup: dict[str, float] | None,
    rss_samples: dict[str, list[int]] | None = None,
) -> WorkloadReport:
    """Turn raw samples into the per-workload report the gate evaluates."""
    results = {name: summarize(samples[name]) for name in IMPLEMENTATIONS}
    kotlin_logic = logic_seconds(
        results["kotlin"]["median_seconds"], startup["kotlin"] if startup else None
    )
    rust_logic = logic_seconds(
        results["rust"]["median_seconds"], startup["rust"] if startup else None
    )
    logic: dict[str, Any] = {
        "kotlin_seconds": kotlin_logic,
        "rust_seconds": rust_logic,
        "speedup": (
            ratio(kotlin_logic, rust_logic)
            if kotlin_logic is not None and rust_logic is not None
            else None
        ),
    }
    rss: dict[str, Any] | None = None
    if rss_samples:
        kotlin_rss = round(statistics.median(rss_samples["kotlin"]))
        rust_rss = round(statistics.median(rss_samples["rust"]))
        rss = {
            "kotlin_median_bytes": kotlin_rss,
            "rust_median_bytes": rust_rss,
            "ratio": round(rust_rss / kotlin_rss, 3) if kotlin_rss else None,
            "samples": {name: list(rss_samples[name]) for name in IMPLEMENTATIONS},
        }
    return WorkloadReport(
        name=workload.name,
        description=workload.description,
        detail=dict(workload.detail),
        results=results,
        samples={name: [round(value, 6) for value in samples[name]] for name in IMPLEMENTATIONS},
        speedup=ratio(
            results["kotlin"]["median_seconds"], results["rust"]["median_seconds"]
        ),
        logic=logic,
        win_rate=paired_win_rate(samples["kotlin"], samples["rust"]),
        rss=rss,
    )


# --------------------------------------------------------------------------
# Orchestration
# --------------------------------------------------------------------------


def resolve_binaries(kotlin: Path | None, rust: Path | None) -> dict[str, Path]:
    """Resolve both binaries from flags, or from Bazel runfiles when unset."""
    if kotlin and rust:
        for label, path in (("kotlin", kotlin), ("rust", rust)):
            if not path.is_file():
                raise ValueError(f"{label} binary does not exist: {path}")
        return {"kotlin": kotlin.resolve(), "rust": rust.resolve()}
    if kotlin or rust:
        raise ValueError("provide both --kotlin-binary and --rust-binary, or neither")
    if _runfiles is None:
        raise ValueError(
            "Bazel runfiles are unavailable; provide both --kotlin-binary and --rust-binary"
        )
    resolver = _runfiles.Create()
    if resolver is None:
        raise ValueError(
            "Bazel runfiles are unavailable; provide both --kotlin-binary and --rust-binary"
        )
    resolved: dict[str, Path] = {}
    for name, logical_path in (("kotlin", KOTLIN_RUNFILE), ("rust", RUST_RUNFILE)):
        location = resolver.Rlocation(logical_path, source_repo="")
        if not location or not Path(location).is_file():
            raise ValueError(f"binary is missing from runfiles: {logical_path}")
        resolved[name] = Path(location)
    return resolved


def run_gate(
    specs: Sequence[WorkloadSpec],
    binaries: dict[str, Path],
    root: Path,
    *,
    rounds: int,
    warmup_rounds: int,
    thresholds: Thresholds,
    rss_runs: int = 0,
    env: dict[str, str] | None = None,
) -> list[WorkloadReport]:
    """Prepare, measure and evaluate every workload, in order."""
    environment = env if env is not None else os.environ.copy()
    startup: dict[str, float] | None = None
    reports: list[WorkloadReport] = []
    for spec in specs:
        directory = root / spec.name
        directory.mkdir(parents=True, exist_ok=True)
        workload = spec.prepare(directory)
        check_parity(workload, binaries, directory, env=environment)
        samples = run_rounds(
            workload,
            binaries,
            directory,
            rounds=rounds,
            warmup_rounds=warmup_rounds,
            env=environment,
        )
        rss_samples = (
            sample_peak_rss(
                workload, binaries, directory, runs=rss_runs, env=environment
            )
            if rss_runs
            else None
        )
        is_startup = workload.name == STARTUP_WORKLOAD
        report = build_report(
            workload,
            samples,
            startup=None if is_startup else startup,
            rss_samples=rss_samples,
        )
        if is_startup:
            startup = {
                name: report.results[name]["median_seconds"] for name in IMPLEMENTATIONS
            }
        reports.append(evaluate(report, thresholds, is_startup=is_startup))
    return reports


def build_document(
    reports: Sequence[WorkloadReport],
    binaries: dict[str, Path],
    *,
    rounds: int,
    warmup_rounds: int,
    scale: float,
    thresholds: Thresholds,
) -> dict[str, Any]:
    """Assemble the machine-readable report."""
    return {
        "schema_version": 1,
        "passed": all(report.passed for report in reports),
        "environment": {
            "platform": platform.platform(),
            "logical_cpu_count": os.cpu_count(),
            "python": platform.python_version(),
        },
        "binaries": {name: str(path) for name, path in binaries.items()},
        "protocol": {
            "rounds": rounds,
            "warmup_rounds": warmup_rounds,
            "scale": scale,
            "interleaved": "both binaries run every round, alternating which goes first",
            "parity": "outputs compared before timings are reported",
            "startup_adjustment": (
                "each implementation's own --version median is subtracted to isolate "
                "hashing/diffing logic from process start-up"
            ),
        },
        "thresholds": {
            "min_speedup": thresholds.min_speedup,
            "min_win_rate": thresholds.min_win_rate,
            "min_logic_speedup": thresholds.min_logic_speedup,
            "max_rss_ratio": thresholds.max_rss_ratio,
        },
        "workloads": [
            {
                "name": report.name,
                "description": report.description,
                "detail": report.detail,
                "results": report.results,
                "samples": report.samples,
                "speedup": report.speedup,
                "logic": report.logic,
                "win_rate": round(report.win_rate, 4),
                "rss": report.rss,
                "passed": report.passed,
                "failures": report.failures,
            }
            for report in reports
        ],
    }


def format_report(reports: Sequence[WorkloadReport]) -> str:
    """Render the human-facing table plus the verdict."""
    lines = [
        f"{'workload':32} {'kotlin':>10} {'rust':>10} {'speedup':>9} "
        f"{'logic':>9} {'wins':>6}  status",
        "-" * 92,
    ]
    for report in reports:
        kotlin = report.results["kotlin"]["median_seconds"]
        rust = report.results["rust"]["median_seconds"]
        speedup = f"{report.speedup}x" if report.speedup is not None else "n/a"
        logic_speedup = report.logic.get("speedup")
        logic = f"{logic_speedup}x" if logic_speedup is not None else "n/a"
        status = "PASS" if report.passed else "FAIL"
        lines.append(
            f"{report.name:32} {kotlin:9.3f}s {rust:9.3f}s {speedup:>9} "
            f"{logic:>9} {report.win_rate:>5.0%}  {status}"
        )
    lines.append("")
    for report in reports:
        for failure in report.failures:
            lines.append(f"FAIL {report.name}: {failure}")
    if all(report.passed for report in reports):
        lines.append("PASS: rust is faster than kotlin on every measured workload")
    return "\n".join(lines)


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kotlin-binary", type=Path)
    parser.add_argument("--rust-binary", type=Path)
    parser.add_argument(
        "--workload",
        action="append",
        default=[],
        dest="workloads",
        help="measure only this workload; repeat for several (start-up is always measured)",
    )
    parser.add_argument(
        "--list-workloads",
        action="store_true",
        help="print the available workloads and exit",
    )
    parser.add_argument(
        "--rounds",
        type=int,
        default=5,
        help="measured interleaved rounds per workload (default: 5)",
    )
    parser.add_argument(
        "--warmup-rounds",
        type=int,
        default=1,
        help="discarded rounds before measurement (default: 1)",
    )
    parser.add_argument(
        "--scale",
        type=float,
        default=1.0,
        help="multiply workload sizes (default: 1.0)",
    )
    parser.add_argument(
        "--min-speedup",
        type=float,
        default=1.0,
        help="required kotlin/rust median wall-time ratio (default: 1.0)",
    )
    parser.add_argument(
        "--min-win-rate",
        type=float,
        default=1.0,
        help="required fraction of paired rounds rust must win (default: 1.0)",
    )
    parser.add_argument(
        "--min-logic-speedup",
        type=float,
        default=1.0,
        help="required startup-adjusted speedup (default: 1.0)",
    )
    parser.add_argument(
        "--rss-runs",
        type=int,
        default=0,
        help="untimed peak-RSS samples per implementation (default: 0, Linux only)",
    )
    parser.add_argument(
        "--max-rss-ratio",
        type=float,
        help="fail if rust's median peak RSS exceeds this multiple of kotlin's",
    )
    parser.add_argument(
        "--fixture-dir",
        type=Path,
        help="keep generated fixtures here instead of a temporary directory",
    )
    parser.add_argument("--json", type=Path, help="write the full report as JSON")
    args = parser.parse_args(argv)
    if args.rounds < 1:
        parser.error("--rounds must be at least 1")
    if args.warmup_rounds < 0:
        parser.error("--warmup-rounds must not be negative")
    if args.scale <= 0:
        parser.error("--scale must be positive")
    if args.rss_runs < 0:
        parser.error("--rss-runs must not be negative")
    if not 0 <= args.min_win_rate <= 1:
        parser.error("--min-win-rate must be within [0, 1]")
    if args.max_rss_ratio is not None and args.rss_runs < 1:
        parser.error("--max-rss-ratio requires --rss-runs")
    return args


def main(argv: Sequence[str]) -> int:
    args = parse_args(argv)
    specs = default_workload_specs(args.scale)
    if args.list_workloads:
        for spec in specs:
            print(f"{spec.name:32} {spec.description}")
        return 0
    thresholds = Thresholds(
        min_speedup=args.min_speedup,
        min_win_rate=args.min_win_rate,
        min_logic_speedup=args.min_logic_speedup,
        max_rss_ratio=args.max_rss_ratio,
    )
    try:
        selected = select_workloads(specs, args.workloads)
        binaries = resolve_binaries(args.kotlin_binary, args.rust_binary)
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    def gate(root: Path) -> list[WorkloadReport]:
        return run_gate(
            selected,
            binaries,
            root,
            rounds=args.rounds,
            warmup_rounds=args.warmup_rounds,
            thresholds=thresholds,
            rss_runs=args.rss_runs,
        )

    try:
        if args.fixture_dir:
            args.fixture_dir.mkdir(parents=True, exist_ok=True)
            reports = gate(args.fixture_dir)
        else:
            with tempfile.TemporaryDirectory(prefix="bazel-diff-perf-gate-") as temp:
                reports = gate(Path(temp))
    except (ParityError, RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    print(format_report(reports))
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        document = build_document(
            reports,
            binaries,
            rounds=args.rounds,
            warmup_rounds=args.warmup_rounds,
            scale=args.scale,
            thresholds=thresholds,
        )
        args.json.write_text(json.dumps(document, indent=2) + "\n")
        print(f"\nwrote {args.json}")
    return 0 if all(report.passed for report in reports) else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
