#!/usr/bin/env python3
"""Renders a side-by-side Rust vs Kotlin comparison from two serve_stress.py metrics JSON files.

Usage:
    tools/serve_stress_compare.py --kotlin kotlin-metrics.json --rust rust-metrics.json

Prints a markdown table (per phase: req/s and p50/p95/p99 latency for each implementation, plus
the Rust/Kotlin ratio) to stdout. Missing/unreadable inputs degrade gracefully -- the table just
notes which side is unavailable, so a single failed matrix leg doesn't break the compare job.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def load(path: str) -> dict | None:
    if not path:
        return None
    p = Path(path)
    if not p.is_file():
        return None
    try:
        return json.loads(p.read_text())
    except (OSError, json.JSONDecodeError):
        return None


def phase_map(data: dict | None) -> dict:
    if not data:
        return {}
    return {ph["name"]: ph for ph in data.get("phases", [])}


def fmt_ratio(rust: float | None, kotlin: float | None) -> str:
    if not rust or not kotlin:
        return "—"
    return f"{rust / kotlin:.2f}x"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--kotlin", default="", help="path to the Kotlin leg's metrics JSON")
    ap.add_argument("--rust", default="", help="path to the Rust leg's metrics JSON")
    args = ap.parse_args()

    kotlin_data = load(args.kotlin)
    rust_data = load(args.rust)

    lines = ["## Rust vs Kotlin serve stress comparison", ""]
    if not kotlin_data and not rust_data:
        lines.append("Neither leg's metrics were available (both runs likely failed).")
        print("\n".join(lines))
        return 0
    if not kotlin_data:
        lines.append("⚠️ Kotlin leg metrics unavailable; showing Rust results only.")
    if not rust_data:
        lines.append("⚠️ Rust leg metrics unavailable; showing Kotlin results only.")
    lines.append("")

    kotlin_phases = phase_map(kotlin_data)
    rust_phases = phase_map(rust_data)
    names = list(dict.fromkeys(list(kotlin_phases) + list(rust_phases)))

    lines += [
        "| phase | kotlin req/s | rust req/s | req/s ratio | kotlin p95 (ms) | rust p95 (ms) | "
        "p95 ratio |",
        "|---|---|---|---|---|---|---|",
    ]
    for name in names:
        k = kotlin_phases.get(name)
        r = rust_phases.get(name)
        k_rps = k["rps"] if k else None
        r_rps = r["rps"] if r else None
        k_p95 = (k.get("latency_ms") or {}).get("p95") if k else None
        r_p95 = (r.get("latency_ms") or {}).get("p95") if r else None
        lines.append(
            f"| {name} | {k_rps if k_rps is not None else '—'} "
            f"| {r_rps if r_rps is not None else '—'} | {fmt_ratio(r_rps, k_rps)} "
            f"| {k_p95 if k_p95 is not None else '—'} | {r_p95 if r_p95 is not None else '—'} "
            f"| {fmt_ratio(r_p95, k_p95)} |")

    if kotlin_data and rust_data:
        lines += [
            "",
            f"Kotlin time-to-ready: {kotlin_data.get('time_to_ready_seconds', '—')}s — "
            f"Rust time-to-ready: {rust_data.get('time_to_ready_seconds', '—')}s.",
        ]

    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    sys.exit(main())
