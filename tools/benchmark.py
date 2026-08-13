#!/usr/bin/env python3
"""Benchmark Kotlin and Rust bazel-diff on the same real Bazel workspace.

Hyperfine owns all wall-time measurements and statistics. By default, the runner
captures Bazel's ``streamed_proto`` once and measures only bazel-diff processing.
Pass ``--include-bazel`` for the secondary Bazel-inclusive cold/warm benchmark:

* cold: shut down that implementation's Bazel server, then generate hashes;
* warm: shut down and prime that server outside the timed command, then generate
  hashes again.

Peak process-tree RSS is sampled in separate, untimed runs for bazel-diff and its
attached children, including the Kotlin JVM and Bazel client. Bazel's long-lived
server daemon detaches from that tree and is intentionally excluded. Output hashes
from every measured iteration are normalized and compared because older Kotlin
releases wrote a flat map while newer releases may wrap it in a ``hashes`` object.

Example:

    bazel run -c opt //tools:benchmark -- \
      --workspace /tmp/bazel \
      --bazel /path/to/bazelisk \
      --iterations 10 \
      --warmup 3 \
      --rss-runs 5 \
      --json /tmp/bazel-diff-benchmark.json
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Sequence

try:
    from python.runfiles import runfiles as _runfiles
except ImportError:
    _runfiles = None


KOTLIN_RUNFILE = "_main/cli/bazel-diff"
RUST_RUNFILE = "_main/src/bazel-diff"


@dataclass(frozen=True)
class HyperfineResult:
    command: str
    mean: float
    stddev: float | None
    median: float
    minimum: float
    maximum: float
    times: tuple[float, ...]


def _run(
    command: Sequence[str],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    process = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if process.returncode != 0:
        stderr_tail = "\n".join(process.stderr.splitlines()[-40:])
        raise RuntimeError(
            f"command failed ({process.returncode}): {' '.join(command)}\n{stderr_tail}"
        )
    return process


def _children_by_parent() -> dict[int, list[int]]:
    children: dict[int, list[int]] = {}
    for process_dir in Path("/proc").iterdir():
        if not process_dir.name.isdigit():
            continue
        try:
            fields = (process_dir / "stat").read_text().split()
            parent_pid = int(fields[3])
        except (
            FileNotFoundError,
            PermissionError,
            ProcessLookupError,
            IndexError,
            ValueError,
        ):
            continue
        children.setdefault(parent_pid, []).append(int(process_dir.name))
    return children


def _process_tree(root_pid: int) -> set[int]:
    children = _children_by_parent()
    result: set[int] = set()
    pending = [root_pid]
    while pending:
        pid = pending.pop()
        if pid in result:
            continue
        result.add(pid)
        pending.extend(children.get(pid, ()))
    return result


def _rss_bytes(pid: int) -> int:
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) * 1024
    except (FileNotFoundError, PermissionError, ProcessLookupError, ValueError):
        pass
    return 0


def _tree_rss_bytes(root_pid: int) -> int:
    return sum(_rss_bytes(pid) for pid in _process_tree(root_pid))


def measure_peak_rss(
    command: Sequence[str],
    *,
    cwd: Path,
    env: dict[str, str],
    poll_interval_seconds: float = 0.01,
) -> int:
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8") as stderr_file:
        process = subprocess.Popen(
            command,
            cwd=cwd,
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=stderr_file,
            text=True,
        )
        peak_rss = 0
        while process.poll() is None:
            peak_rss = max(peak_rss, _tree_rss_bytes(process.pid))
            time.sleep(poll_interval_seconds)
        process.wait()
        peak_rss = max(peak_rss, _tree_rss_bytes(process.pid))
        stderr_file.seek(0)
        stderr = stderr_file.read()
    if process.returncode != 0:
        stderr_tail = "\n".join(stderr.splitlines()[-40:])
        raise RuntimeError(
            f"command failed ({process.returncode}): {' '.join(command)}\n{stderr_tail}"
        )
    return peak_rss


def load_hashes(path: Path) -> dict[str, str]:
    value = json.loads(path.read_text())
    hashes = value.get("hashes", value)
    if not isinstance(hashes, dict) or not all(
        isinstance(label, str) and isinstance(digest, str)
        for label, digest in hashes.items()
    ):
        raise ValueError(f"{path} does not contain a target-to-hash map")
    return hashes


def hash_summary(path: Path) -> dict[str, Any]:
    hashes = load_hashes(path)
    canonical = json.dumps(hashes, sort_keys=True, separators=(",", ":")).encode()
    return {
        "target_count": len(hashes),
        "canonical_sha256": hashlib.sha256(canonical).hexdigest(),
        "file_bytes": path.stat().st_size,
    }


def compare_hashes(kotlin_path: Path, rust_path: Path) -> dict[str, Any]:
    kotlin = load_hashes(kotlin_path)
    rust = load_hashes(rust_path)
    kotlin_labels = set(kotlin)
    rust_labels = set(rust)
    common = kotlin_labels & rust_labels
    mismatched = sorted(label for label in common if kotlin[label] != rust[label])
    return {
        "equal": kotlin == rust,
        "kotlin_only_count": len(kotlin_labels - rust_labels),
        "rust_only_count": len(rust_labels - kotlin_labels),
        "digest_mismatch_count": len(mismatched),
        "kotlin_only_sample": sorted(kotlin_labels - rust_labels)[:10],
        "rust_only_sample": sorted(rust_labels - kotlin_labels)[:10],
        "digest_mismatch_sample": mismatched[:10],
    }


def median(values: Sequence[float | int]) -> float:
    return float(statistics.median(values))


def load_hyperfine_results(path: Path) -> dict[str, HyperfineResult]:
    document = json.loads(path.read_text())
    raw_results = document.get("results")
    if not isinstance(raw_results, list):
        raise ValueError(f"{path} does not contain Hyperfine results")
    results: dict[str, HyperfineResult] = {}
    for raw in raw_results:
        if not isinstance(raw, dict):
            raise ValueError(f"{path} contains an invalid Hyperfine result")
        command = raw.get("command")
        times = raw.get("times")
        exit_codes = raw.get("exit_codes")
        if (
            not isinstance(command, str)
            or not isinstance(times, list)
            or not times
            or not all(isinstance(value, (int, float)) for value in times)
            or not isinstance(exit_codes, list)
            or len(exit_codes) != len(times)
            or any(code != 0 for code in exit_codes)
        ):
            raise ValueError(f"{path} contains an invalid Hyperfine result for {command!r}")
        results[command] = HyperfineResult(
            command=command,
            mean=float(raw["mean"]),
            stddev=(
                float(raw["stddev"]) if raw.get("stddev") is not None else None
            ),
            median=float(raw["median"]),
            minimum=float(raw["min"]),
            maximum=float(raw["max"]),
            times=tuple(float(value) for value in times),
        )
    return results


def summarize(result: HyperfineResult, peak_rss_bytes: Sequence[int]) -> dict[str, Any]:
    return {
        "mean_wall_seconds": round(result.mean, 6),
        "stddev_wall_seconds": (
            round(result.stddev, 6) if result.stddev is not None else None
        ),
        "median_wall_seconds": round(result.median, 6),
        "min_wall_seconds": round(result.minimum, 6),
        "max_wall_seconds": round(result.maximum, 6),
        "median_peak_rss_bytes": round(median(peak_rss_bytes)),
        "wall_seconds": [round(value, 6) for value in result.times],
        "peak_rss_bytes": list(peak_rss_bytes),
    }


def speedup(kotlin: dict[str, Any], rust: dict[str, Any]) -> dict[str, Any]:
    kotlin_wall = kotlin["median_wall_seconds"]
    rust_wall = rust["median_wall_seconds"]
    kotlin_rss = kotlin["median_peak_rss_bytes"]
    rust_rss = rust["median_peak_rss_bytes"]
    return {
        "wall_time_ratio": round(kotlin_wall / rust_wall, 2) if rust_wall else None,
        "wall_time_reduction_pct": (
            round(100 * (kotlin_wall - rust_wall) / kotlin_wall, 1)
            if kotlin_wall
            else None
        ),
        "peak_rss_ratio": round(kotlin_rss / rust_rss, 2) if rust_rss else None,
        "peak_rss_reduction_pct": (
            round(100 * (kotlin_rss - rust_rss) / kotlin_rss, 1)
            if kotlin_rss
            else None
        ),
    }


def wall_time_comparison(comparison: dict[str, Any]) -> str:
    ratio = comparison["wall_time_ratio"]
    if ratio is None:
        return "wall-time ratio unavailable"
    if ratio >= 1:
        return f"{ratio}x faster"
    return f"{round(1 / ratio, 2)}x slower"


def _git(workspace: Path, *args: str) -> str:
    return _run(["git", "-C", str(workspace), *args]).stdout.strip()


def _bazel_version(bazel: Path, workspace: Path) -> str:
    output = _run([str(bazel), "--batch", "version"], cwd=workspace).stdout
    return next(
        (
            line[len("Build label: ") :]
            for line in output.splitlines()
            if line.startswith("Build label: ")
        ),
        output.strip(),
    )


def _shutdown(bazel: Path, workspace: Path, output_base: Path) -> None:
    subprocess.run(
        [str(bazel), f"--output_base={output_base}", "shutdown"],
        cwd=workspace,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def _shutdown_command(bazel: Path, output_base: Path) -> list[str]:
    return [str(bazel), f"--output_base={output_base}", "shutdown"]


def _injected_binaries() -> tuple[Path, Path]:
    resolver = _runfiles.Create() if _runfiles is not None else None
    if resolver is None:
        raise ValueError(
            "Bazel runfiles are unavailable; provide both --kotlin-binary and --rust-binary"
        )
    artifacts = []
    for logical_path in (KOTLIN_RUNFILE, RUST_RUNFILE):
        resolved = resolver.Rlocation(logical_path, source_repo="")
        if not resolved or not Path(resolved).is_file():
            raise ValueError(f"benchmark artifact is missing from runfiles: {logical_path}")
        artifacts.append(Path(resolved))
    return artifacts[0], artifacts[1]


def _resolve_binaries(args: argparse.Namespace) -> tuple[Path, Path]:
    if args.kotlin_binary and args.rust_binary:
        return args.kotlin_binary.resolve(), args.rust_binary.resolve()
    if args.kotlin_binary or args.rust_binary:
        raise ValueError("provide both --kotlin-binary and --rust-binary, or neither")
    return _injected_binaries()


def _capture_streamed_proto(
    bazel: Path,
    workspace: Path,
    output: Path,
    environment: dict[str, str],
) -> None:
    command = [
        str(bazel),
        "query",
        "//...:all-targets",
        "--output=streamed_proto",
        "--proto:instantiation_stack",
        "--order_output=no",
    ]
    with output.open("wb") as stdout, tempfile.TemporaryFile() as stderr:
        process = subprocess.run(
            command,
            cwd=workspace,
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
        )
        if process.returncode != 0:
            stderr.seek(0)
            tail = stderr.read().decode(errors="replace").splitlines()[-40:]
            raise RuntimeError(
                f"command failed ({process.returncode}): {' '.join(command)}\n"
                + "\n".join(tail)
            )


def _write_replay_bazel(path: Path, fixture: Path, output_base: Path) -> None:
    output_base.mkdir(parents=True)
    script = f"""#!/usr/bin/env bash
set -euo pipefail
fixture={shlex.quote(str(fixture))}
output_base={shlex.quote(str(output_base))}
args=("$@")
command=
command_index=-1
for index in "${{!args[@]}}"; do
  case "${{args[$index]}}" in
    version|mod|info|query)
      command="${{args[$index]}}"
      command_index=$index
      break
      ;;
  esac
done
case "$command" in
  version)
    echo 'Build label: 9.2.0'
    ;;
  mod)
    exit 1
    ;;
  info)
    echo "$output_base"
    ;;
  query)
    output_file=
    for ((index = command_index + 1; index < ${{#args[@]}}; index++)); do
      case "${{args[$index]}}" in
        --output_file)
          ((index += 1))
          output_file="${{args[$index]}}"
          ;;
        --output_file=*)
          output_file="${{args[$index]#--output_file=}}"
          ;;
      esac
    done
    if [[ -n "$output_file" ]]; then
      cp "$fixture" "$output_file"
    else
      cat "$fixture"
    fi
    ;;
  *)
    echo "unsupported replay invocation: $*" >&2
    exit 2
    ;;
esac
"""
    path.write_text(script)
    path.chmod(0o755)


def _generate_command(
    command_prefix: Sequence[str],
    bazel: Path,
    workspace: Path,
    output_base: Path,
    output: Path,
    extra_args: Sequence[str],
) -> list[str]:
    return [
        *command_prefix,
        "generate-hashes",
        "-w",
        str(workspace),
        "-b",
        str(bazel),
        f"--bazelStartupOptions=--output_base={output_base}",
        "--excludeExternalTargets",
        *extra_args,
        str(output),
    ]


def _shell_command(command: Sequence[str]) -> str:
    return " ".join(shlex.quote(part) for part in command)


def _iteration_output(prefix: Path) -> str:
    return f"{shlex.quote(str(prefix))}-${{HYPERFINE_ITERATION}}.json"


def _hyperfine_command(command: Sequence[str], output_prefix: Path) -> str:
    return f"{_shell_command(command)} {_iteration_output(output_prefix)}"


def _run_hyperfine(
    *,
    hyperfine: Path,
    workspace: Path,
    environment: dict[str, str],
    runs: int,
    warmup: int,
    json_path: Path,
    commands: Sequence[tuple[str, str, str]],
) -> dict[str, HyperfineResult]:
    invocation = [
        str(hyperfine),
        "--style",
        "basic",
        "--runs",
        str(runs),
        "--warmup",
        str(warmup),
        "--export-json",
        str(json_path),
    ]
    for _, prepare, _ in commands:
        invocation.extend(["--prepare", prepare])
    for name, _, command in commands:
        invocation.extend(["--command-name", name, command])
    _run(invocation, cwd=workspace, env=environment)
    results = load_hyperfine_results(json_path)
    expected = {name for name, _, _ in commands}
    if set(results) != expected:
        raise RuntimeError(
            f"Hyperfine returned commands {sorted(results)}, expected {sorted(expected)}"
        )
    if any(len(result.times) != runs for result in results.values()):
        raise RuntimeError(f"Hyperfine did not execute exactly {runs} measured runs")
    return results


def _warm_server(
    command_prefix: Sequence[str],
    bazel: Path,
    workspace: Path,
    output_base: Path,
    output: Path,
    extra_args: Sequence[str],
    environment: dict[str, str],
) -> None:
    _run(
        _generate_command(
            command_prefix,
            bazel,
            workspace,
            output_base,
            output,
            extra_args,
        ),
        cwd=workspace,
        env=environment,
    )


def _sample_rss(
    *,
    commands: dict[str, list[str]],
    bazel: Path,
    workspace: Path,
    output_bases: dict[str, Path],
    extra_args: Sequence[str],
    environment: dict[str, str],
    runs: int,
    temp_path: Path,
) -> tuple[dict[str, dict[str, list[int]]], dict[str, dict[str, Path]]]:
    samples = {
        implementation: {"cold": [], "warm": []}
        for implementation in ("kotlin", "rust")
    }
    outputs: dict[str, dict[str, Path]] = {}
    for implementation in ("kotlin", "rust"):
        output_base = output_bases[implementation]
        outputs[implementation] = {}
        for phase in ("cold", "warm"):
            for iteration in range(runs):
                _shutdown(bazel, workspace, output_base)
                if phase == "warm":
                    _warm_server(
                        commands[implementation],
                        bazel,
                        workspace,
                        output_base,
                        temp_path / f"rss-{implementation}-{iteration}-warmup.json",
                        extra_args,
                        environment,
                    )
                output = temp_path / f"rss-{implementation}-{phase}-{iteration}.json"
                command = _generate_command(
                    commands[implementation],
                    bazel,
                    workspace,
                    output_base,
                    output,
                    extra_args,
                )
                samples[implementation][phase].append(
                    measure_peak_rss(command, cwd=workspace, env=environment)
                )
                outputs[implementation][phase] = output
    return samples, outputs


def _benchmark_replay(
    *,
    args: argparse.Namespace,
    commands: dict[str, list[str]],
    workspace: Path,
    bazel: Path,
    hyperfine: Path,
    environment: dict[str, str],
    temp_path: Path,
) -> tuple[
    dict[str, dict[str, dict[str, Any]]],
    dict[str, Any],
    dict[str, dict[str, Any]],
    dict[str, Any],
]:
    fixture = (
        args.streamed_proto.resolve()
        if args.streamed_proto
        else temp_path / "targets.pb"
    )
    if not args.streamed_proto:
        _capture_streamed_proto(bazel, workspace, fixture, environment)
    replay = temp_path / "bazel-replay"
    replay_output_base = temp_path / "replay-output-base"
    _write_replay_bazel(replay, fixture, replay_output_base)
    prefixes = {
        implementation: temp_path / f"timing-{implementation}-processing"
        for implementation in ("kotlin", "rust")
    }
    hyperfine_commands = []
    for implementation in ("kotlin", "rust"):
        measured = _generate_command(
            commands[implementation],
            replay,
            workspace,
            replay_output_base,
            Path(),
            args.bazel_diff_arg,
        )
        measured.pop()
        hyperfine_commands.append(
            (
                implementation,
                "true",
                _hyperfine_command(measured, prefixes[implementation]),
            )
        )
    timing_results = _run_hyperfine(
        hyperfine=hyperfine,
        workspace=workspace,
        environment=environment,
        runs=args.iterations,
        warmup=args.warmup,
        json_path=temp_path / "hyperfine.json",
        commands=hyperfine_commands,
    )
    iteration_parity = []
    for iteration in range(args.iterations):
        parity = compare_hashes(
            Path(f"{prefixes['kotlin']}-{iteration}.json"),
            Path(f"{prefixes['rust']}-{iteration}.json"),
        )
        iteration_parity.append(parity)
        if not parity["equal"]:
            raise RuntimeError(
                f"Kotlin and Rust replay outputs differ in iteration "
                f"{iteration + 1}: {parity}"
            )
    rss_samples: dict[str, list[int]] = {"kotlin": [], "rust": []}
    rss_outputs: dict[str, Path] = {}
    for implementation in ("kotlin", "rust"):
        for iteration in range(args.rss_runs):
            output = temp_path / f"rss-{implementation}-{iteration}.json"
            command = _generate_command(
                commands[implementation],
                replay,
                workspace,
                replay_output_base,
                output,
                args.bazel_diff_arg,
            )
            rss_samples[implementation].append(
                measure_peak_rss(command, cwd=workspace, env=environment)
            )
            rss_outputs[implementation] = output
    summaries = {
        implementation: {
            "processing": summarize(
                timing_results[implementation], rss_samples[implementation]
            )
        }
        for implementation in ("kotlin", "rust")
    }
    outputs = {
        implementation: hash_summary(rss_outputs[implementation])
        for implementation in ("kotlin", "rust")
    }
    fixture_bytes = fixture.read_bytes()
    protocol = {
        "timing": "Hyperfine exact-run benchmark against a captured streamed_proto",
        "capture": (
            str(args.streamed_proto.resolve())
            if args.streamed_proto
            else "captured once before measurement with bazel query"
        ),
        "rss": "separate process-tree samples; not included in Hyperfine timings",
        "fixture_bytes": len(fixture_bytes),
        "fixture_sha256": hashlib.sha256(fixture_bytes).hexdigest(),
        "fixed_args": ["--excludeExternalTargets"],
    }
    return summaries, iteration_parity[-1], outputs, protocol


def _benchmark_end_to_end(
    *,
    args: argparse.Namespace,
    commands: dict[str, list[str]],
    workspace: Path,
    bazel: Path,
    hyperfine: Path,
    environment: dict[str, str],
    temp_path: Path,
) -> tuple[
    dict[str, dict[str, dict[str, Any]]],
    dict[str, Any],
    dict[str, dict[str, Any]],
    dict[str, Any],
]:
    output_bases = {
        "kotlin": temp_path / "output-base-kotlin",
        "rust": temp_path / "output-base-rust",
    }
    timing_prefixes = {
        implementation: {
            phase: temp_path / f"timing-{implementation}-{phase}"
            for phase in ("cold", "warm")
        }
        for implementation in ("kotlin", "rust")
    }
    hyperfine_commands: list[tuple[str, str, str]] = []
    for implementation in ("kotlin", "rust"):
        output_base = output_bases[implementation]
        shutdown = _shell_command(_shutdown_command(bazel, output_base))
        for phase in ("cold", "warm"):
            name = f"{implementation}-{phase}"
            prepare = shutdown
            if phase == "warm":
                warmup_command = _generate_command(
                    commands[implementation],
                    bazel,
                    workspace,
                    output_base,
                    temp_path / f"timing-{implementation}-server-warmup.json",
                    args.bazel_diff_arg,
                )
                prepare = f"{prepare} && {_shell_command(warmup_command)}"
            measured = _generate_command(
                commands[implementation],
                bazel,
                workspace,
                output_base,
                Path(),
                args.bazel_diff_arg,
            )
            measured.pop()
            hyperfine_commands.append(
                (
                    name,
                    prepare,
                    _hyperfine_command(
                        measured, timing_prefixes[implementation][phase]
                    ),
                )
            )
    try:
        timing_results = _run_hyperfine(
            hyperfine=hyperfine,
            workspace=workspace,
            environment=environment,
            runs=args.iterations,
            warmup=args.warmup,
            json_path=temp_path / "hyperfine.json",
            commands=hyperfine_commands,
        )
        iteration_parity = []
        for iteration in range(args.iterations):
            phase_outputs = {
                implementation: {
                    phase: Path(
                        f"{timing_prefixes[implementation][phase]}-{iteration}.json"
                    )
                    for phase in ("cold", "warm")
                }
                for implementation in ("kotlin", "rust")
            }
            for implementation in ("kotlin", "rust"):
                cold_warm_parity = compare_hashes(
                    phase_outputs[implementation]["cold"],
                    phase_outputs[implementation]["warm"],
                )
                if not cold_warm_parity["equal"]:
                    raise RuntimeError(
                        f"{implementation} cold and warm outputs differ: "
                        f"{cold_warm_parity}"
                    )
            for phase in ("cold", "warm"):
                parity = compare_hashes(
                    phase_outputs["kotlin"][phase],
                    phase_outputs["rust"][phase],
                )
                iteration_parity.append(parity)
                if not parity["equal"]:
                    raise RuntimeError(
                        f"Kotlin and Rust {phase} outputs differ in "
                        f"iteration {iteration + 1}: {parity}"
                    )
        rss_samples, rss_outputs = _sample_rss(
            commands=commands,
            bazel=bazel,
            workspace=workspace,
            output_bases=output_bases,
            extra_args=args.bazel_diff_arg,
            environment=environment,
            runs=args.rss_runs,
            temp_path=temp_path,
        )
        summaries = {
            implementation: {
                phase: summarize(
                    timing_results[f"{implementation}-{phase}"],
                    rss_samples[implementation][phase],
                )
                for phase in ("cold", "warm")
            }
            for implementation in ("kotlin", "rust")
        }
        outputs = {
            implementation: hash_summary(rss_outputs[implementation]["warm"])
            for implementation in ("kotlin", "rust")
        }
    finally:
        for implementation in ("kotlin", "rust"):
            _shutdown(bazel, workspace, output_bases[implementation])
    protocol = {
        "timing": "Hyperfine exact-run benchmark with per-command prepare hooks",
        "cold": "implementation-specific Bazel server shut down before every timed run",
        "warm": "server shut down and primed by an unmeasured generate-hashes before every timed run",
        "rss": "separate process-tree samples; not included in Hyperfine timings",
        "fixed_args": [
            "--excludeExternalTargets",
            "--bazelStartupOptions=<implementation-specific output base>",
        ],
    }
    return summaries, iteration_parity[-1], outputs, protocol


def benchmark(args: argparse.Namespace) -> dict[str, Any]:
    if not Path("/proc").is_dir():
        raise ValueError("peak RSS sampling requires Linux /proc")
    workspace = args.workspace.resolve()
    bazel = args.bazel.resolve()
    hyperfine = args.hyperfine.resolve()
    environment = os.environ.copy()
    environment.setdefault("CARGO_BAZEL_ISOLATED", "false")
    environment.setdefault("CARGO_HOME", str(Path.home() / ".cargo"))

    kotlin_artifact, rust_artifact = _resolve_binaries(args)

    workspace_commit = _git(workspace, "rev-parse", "HEAD")
    dirty = bool(_git(workspace, "status", "--porcelain"))
    if dirty:
        raise ValueError(f"benchmark workspace is dirty: {workspace}")

    with tempfile.TemporaryDirectory(prefix="bazel-diff-benchmark-") as temp:
        temp_path = Path(temp)
        commands = {
            "kotlin": [str(kotlin_artifact)],
            "rust": [str(rust_artifact)],
        }
        if not args.include_bazel:
            summaries, parity, output_summaries, protocol = _benchmark_replay(
                args=args,
                commands=commands,
                workspace=workspace,
                bazel=bazel,
                hyperfine=hyperfine,
                environment=environment,
                temp_path=temp_path,
            )
            phases = ("processing",)
            iteration_parity = [parity]
        else:
            summaries, parity, output_summaries, protocol = _benchmark_end_to_end(
                args=args,
                commands=commands,
                workspace=workspace,
                bazel=bazel,
                hyperfine=hyperfine,
                environment=environment,
                temp_path=temp_path,
            )
            phases = ("cold", "warm")
            iteration_parity = [parity]

    return {
        "schema_version": 2,
        "workspace": {
            "path": str(workspace),
            "git_commit": workspace_commit,
            "remote": _git(workspace, "remote", "get-url", "origin"),
        },
        "environment": {
            "platform": platform.platform(),
            "logical_cpu_count": os.cpu_count(),
            "bazel": str(bazel),
            "bazel_version": _bazel_version(bazel, workspace),
            "hyperfine": str(hyperfine),
            "hyperfine_version": _run([str(hyperfine), "--version"]).stdout.strip(),
            "peak_rss_scope": (
                "bazel-diff process tree, including Kotlin JVM and Bazel client; "
                "excluding detached Bazel server daemon"
            ),
        },
        "binaries": {
            "kotlin": str(kotlin_artifact),
            "rust": str(rust_artifact),
        },
        "iterations": args.iterations,
        "benchmark_mode": "end-to-end" if args.include_bazel else "replay",
        "phases": list(phases),
        "warmup_runs": args.warmup,
        "rss_runs": args.rss_runs,
        "bazel_diff_args": args.bazel_diff_arg,
        "protocol": protocol,
        "results": summaries,
        "comparison": {
            phase: speedup(summaries["kotlin"][phase], summaries["rust"][phase])
            for phase in phases
        },
        "outputs": output_summaries,
        "hash_parity": {
            **parity,
            "all_iterations_equal": all(
                result["equal"] for result in iteration_parity
            ),
        },
    }


def print_report(report: dict[str, Any]) -> None:
    print(
        f"workspace: {report['workspace']['remote']} "
        f"@ {report['workspace']['git_commit']}"
    )
    print(f"Bazel: {report['environment']['bazel_version']}")
    print()
    print("phase  implementation  median wall  median peak RSS")
    for phase in report["phases"]:
        for implementation in ("kotlin", "rust"):
            result = report["results"][implementation][phase]
            print(
                f"{phase:5}  {implementation:14}  "
                f"{result['median_wall_seconds']:10.3f}s  "
                f"{result['median_peak_rss_bytes'] / 2**20:14.1f} MiB"
            )
        comparison = report["comparison"][phase]
        print(
            f"       Rust: {wall_time_comparison(comparison)}, "
            f"{comparison['peak_rss_reduction_pct']}% less peak RSS"
        )
    parity = report["hash_parity"]
    print()
    print(
        "hash parity: "
        + ("exact" if parity["equal"] else f"DIFFERENT ({parity})")
    )


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--bazel", type=Path, default=Path("bazel"))
    parser.add_argument("--hyperfine", type=Path, default=Path("hyperfine"))
    parser.add_argument(
        "--include-bazel",
        action="store_true",
        help="benchmark Bazel-inclusive cold/warm runs instead of pure replay processing",
    )
    parser.add_argument(
        "--streamed-proto",
        type=Path,
        help="reuse an existing streamed_proto fixture instead of capturing one",
    )
    parser.add_argument("--kotlin-binary", type=Path)
    parser.add_argument("--rust-binary", type=Path)
    parser.add_argument(
        "--iterations",
        type=int,
        default=10,
        help="exact Hyperfine runs per implementation and phase",
    )
    parser.add_argument(
        "--warmup",
        type=int,
        default=3,
        help="Hyperfine warmup runs per implementation and phase",
    )
    parser.add_argument(
        "--rss-runs",
        type=int,
        default=5,
        help="separate process-tree peak RSS samples per implementation and phase",
    )
    parser.add_argument(
        "--bazel-diff-arg",
        action="append",
        default=[],
        help="extra generate-hashes argument; repeat for multiple arguments",
    )
    parser.add_argument("--json", type=Path)
    args = parser.parse_args(argv)
    if args.iterations < 1:
        parser.error("--iterations must be at least 1")
    if args.warmup < 0:
        parser.error("--warmup must not be negative")
    if args.rss_runs < 1:
        parser.error("--rss-runs must be at least 1")
    if args.include_bazel and args.streamed_proto:
        parser.error("--streamed-proto cannot be combined with --include-bazel")
    for attribute, label in (("bazel", "Bazel"), ("hyperfine", "Hyperfine")):
        executable = getattr(args, attribute)
        if executable.is_absolute():
            continue
        resolved = shutil.which(str(executable))
        if not resolved:
            parser.error(f"{label} executable not found: {executable}")
        setattr(args, attribute, Path(resolved))
    return args


def main(argv: Sequence[str]) -> int:
    args = parse_args(argv)
    report = benchmark(args)
    print_report(report)
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(report, indent=2) + "\n")
        print(f"\nwrote {args.json}")
    return 0 if report["hash_parity"]["equal"] else 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
