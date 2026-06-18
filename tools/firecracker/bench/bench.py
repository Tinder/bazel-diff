#!/usr/bin/env python3
"""Cold-vs-warm analysis-time benchmark for the Firecracker snapshot RFC.

This measures the *addressable* win of a Firecracker snapshot without needing
Firecracker itself. The insight: a restored snapshot gives you a Bazel server
whose Skyframe graph is already loaded and whose external repos are already
fetched — i.e. a **warm** server. So:

    snapshot-restore consume  ~=  warm `generate-hashes`  +  restore overhead

and the per-PR win a snapshot can capture is bounded by:

    win  =  cold `generate-hashes`  -  warm `generate-hashes`

The cold number is what bazel-diff costs today on every PR (fresh server,
full graph load). The warm number is what it would cost if the server were
already warm — exactly the snapshot consume path. This script measures both
on a real generated workspace and reports the delta.

It also asserts the warm output is byte-identical to the cold output, which is
the RFC's core correctness claim (an incorrect affected set is worse than none).

Usage:
    python3 bench.py \
        --workspace /tmp/benchbig \
        --base-sha <sha> --target-sha <sha> \
        --bazel /usr/local/bin/bazel \
        --bazel-diff /tmp/bazel_diff \
        --iterations 3 \
        [--json out.json]
"""

from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import sys
import time
from pathlib import Path


def _run(cmd: list[str], cwd: str | None = None) -> None:
    # Capture stderr so a failing bazel/bazel-diff surfaces its diagnostics
    # instead of failing silently.
    proc = subprocess.run(
        cmd,
        cwd=cwd,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    if proc.returncode != 0:
        tail = "\n".join(proc.stderr.splitlines()[-25:])
        raise RuntimeError(
            f"command failed (exit {proc.returncode}): {' '.join(cmd)}\n{tail}"
        )


def _git_checkout(workspace: str, sha: str) -> None:
    _run(["git", "-C", workspace, "checkout", "--quiet", "--force", sha])


def _shutdown(bazel: str, workspace: str) -> None:
    subprocess.run(
        [bazel, "shutdown"],
        cwd=workspace,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def _generate_hashes(bazel_diff: str, bazel: str, workspace: str, out: str) -> float:
    t0 = time.time()
    _run(
        [
            bazel_diff,
            "generate-hashes",
            "-w",
            workspace,
            "-b",
            bazel,
            out,
        ]
    )
    return time.time() - t0


def benchmark(
    workspace: str,
    base_sha: str,
    target_sha: str,
    bazel: str,
    bazel_diff: str,
    iterations: int,
    tmp: Path,
) -> dict:
    cold_base = []
    cold_consume = []
    warm_consume = []

    base_json = str(tmp / "base.json")
    warm_json = str(tmp / "target_warm.json")
    cold_json = str(tmp / "target_cold.json")

    correctness_ok = True

    for it in range(iterations):
        # --- record-time warmup: cold server on base SHA ---
        _git_checkout(workspace, base_sha)
        _shutdown(bazel, workspace)
        cold_base.append(_generate_hashes(bazel_diff, bazel, workspace, base_json))

        # --- consume the warm server: checkout target, incremental re-analysis.
        # The server is still warm from the base run; this models a restore.
        _git_checkout(workspace, target_sha)
        warm_consume.append(_generate_hashes(bazel_diff, bazel, workspace, warm_json))

        # --- consume cold: what a PR pays today (no snapshot) ---
        _shutdown(bazel, workspace)
        cold_consume.append(_generate_hashes(bazel_diff, bazel, workspace, cold_json))

        # correctness: warm output must equal cold output
        if Path(warm_json).read_bytes() != Path(cold_json).read_bytes():
            correctness_ok = False

        print(
            f"  iter {it + 1}/{iterations}: "
            f"cold_base={cold_base[-1]:.1f}s  "
            f"warm_consume={warm_consume[-1]:.1f}s  "
            f"cold_consume={cold_consume[-1]:.1f}s",
            file=sys.stderr,
        )

    def med(xs: list[float]) -> float:
        return round(statistics.median(xs), 2)

    cold_med = med(cold_consume)
    warm_med = med(warm_consume)
    win = round(cold_med - warm_med, 2)
    pct = round(100 * win / cold_med, 1) if cold_med else 0.0

    return {
        "workspace": workspace,
        "base_sha": base_sha,
        "target_sha": target_sha,
        "iterations": iterations,
        "cold_base_warmup_s": med(cold_base),
        "cold_consume_s": cold_med,
        "warm_consume_s": warm_med,
        "win_s": win,
        "win_pct": pct,
        "correctness_warm_equals_cold": correctness_ok,
        "raw": {
            "cold_base": cold_base,
            "warm_consume": warm_consume,
            "cold_consume": cold_consume,
        },
    }


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--workspace", required=True)
    p.add_argument("--base-sha", required=True)
    p.add_argument("--target-sha", required=True)
    p.add_argument("--bazel", default="bazel")
    p.add_argument("--bazel-diff", required=True)
    p.add_argument("--iterations", type=int, default=3)
    p.add_argument("--json", help="write full report JSON here")
    args = p.parse_args(argv)

    tmp = Path(args.workspace).parent / "_bench_tmp"
    tmp.mkdir(exist_ok=True)

    print("Running cold-vs-warm benchmark...", file=sys.stderr)
    report = benchmark(
        workspace=args.workspace,
        base_sha=args.base_sha,
        target_sha=args.target_sha,
        bazel=args.bazel,
        bazel_diff=args.bazel_diff,
        iterations=args.iterations,
        tmp=tmp,
    )

    print("\n=== Analysis-time win (median over "
          f"{report['iterations']} iters) ===")
    print(f"  cold consume (today):        {report['cold_consume_s']}s")
    print(f"  warm consume (snapshot ~=):  {report['warm_consume_s']}s")
    print(f"  win:                         {report['win_s']}s "
          f"({report['win_pct']}% faster)")
    print(f"  correctness (warm == cold):  "
          f"{'OK' if report['correctness_warm_equals_cold'] else 'FAILED'}")

    if args.json:
        Path(args.json).write_text(json.dumps(report, indent=2))
        print(f"\nwrote {args.json}")

    if not report["correctness_warm_equals_cold"]:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
