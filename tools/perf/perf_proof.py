#!/usr/bin/env python3
"""Changeset performance proof for bazel-diff.

This is the one tool a change's author runs to show that the change made bazel-diff faster or
leaner (or at least not slower or fatter). It takes a changeset -- two revisions, a revision plus
patches, or the uncommitted working tree -- builds a bazel-diff binary from each side, runs both
binaries on Bazel workspaces of several sizes, and measures what the *bazel-diff process* costs on
each: CPU time (user + system), peak resident memory, and wall time, with the Bazel server's own
CPU reported alongside. It then says, per workload, stage and metric, whether the candidate is
better, worse or indistinguishable from the base, and whether both sides produced byte-identical
hashes and the same impacted set.

Why a dedicated tool rather than `time` and a screenshot:

- **Interleaved A/B.** Every iteration runs base and candidate back to back, and the order flips
  each iteration, so thermal throttling, page-cache state and whatever else drifts over a run
  lands on both sides equally.
- **Process-exact numbers.** Each stage is reaped with ``os.wait4``, so CPU seconds and peak RSS
  are the kernel's accounting for the bazel-diff process tree (bazel-diff plus the Bazel client it
  spawns), not a sampled approximation. The Bazel *server* is a daemon outside that tree; its CPU
  is read from its pid separately so a change that alters the query shape still shows up.
- **A statistical verdict.** Medians are compared and a two-sided Mann-Whitney U test decides
  whether a difference beyond the noise threshold is real. "Improved" needs both a difference
  larger than ``--threshold`` percent and ``p < --alpha``; a large but noisy difference is
  reported as inconclusive, not as a win.
- **Correctness is part of the proof.** A faster binary that produces different hashes has
  changed behaviour, not performance. Outputs from every run are compared against the first base
  run; a mismatch fails the proof unless ``--allow-output-diff`` says the change is meant to
  move the hashes.

The flow measured per workload and iteration is the README recipe, split into stages:

    git checkout <base>;  bazel shutdown
    1. generate-hashes                       (cold Bazel server)
    2. generate-hashes                       (warm server; bazel-diff's own cost is clearest here)
    git checkout <head>
    3. generate-hashes --depEdgesFile        (warm server, dependency tracking on)
    4. get-impacted-targets --depEdgesFile   (no Bazel involved)

Workloads are the synthetic genrule workspaces of tools/firecracker/bench/gen_project.py (the
presets the nightly benches use: tiny, small, medium, large, xlarge) and, optionally, a real
repository and commit pair.

Usage (from a checkout; the harness builds from git, your tree is never modified):

    # Prove HEAD against its merge-base with master on small + medium + large, 5 iterations.
    python3 tools/perf/perf_proof.py

    # Explicit revisions, or a patch series on top of a base, or the uncommitted working tree.
    python3 tools/perf/perf_proof.py --base v70.0.0 --candidate my-branch
    python3 tools/perf/perf_proof.py --base master --patch 0001-faster.patch --patch 0002.patch
    python3 tools/perf/perf_proof.py --candidate WORKTREE

    # Smoke run (tiny workload, 2 iterations), or more statistical power on one size.
    python3 tools/perf/perf_proof.py --quick
    python3 tools/perf/perf_proof.py --workload large --iterations 10

    # Measure the harness's own noise floor: the base binary against itself.
    python3 tools/perf/perf_proof.py --aa

Every Bazel invocation the harness makes, builds included, runs with ``--nohome_rc``; it is not
configurable, because a run that a developer's ``~/.bazelrc`` can shape is not reproducible.

Builds happen in a worktree under ``~/.cache/bazel-diff-perf-proof/`` that persists between runs,
so only the first run on a machine pays a cold Bazel build; later runs rebuild incrementally.

The harness is deliberately not a `py_binary`: it builds bazel-diff with `bazel build`, which
would deadlock on the output-base lock if the harness were itself launched via `bazel run`.
Python 3.9+, standard library only. See tools/perf/README.md.
"""

from __future__ import annotations

import argparse
import json
import math
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from dataclasses import dataclass, field
from itertools import combinations
from pathlib import Path
from typing import Any, Callable, Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))
import tango_compare as tc  # noqa: E402  (shared workload/subprocess helpers)

REPO_ROOT = tc.REPO_ROOT
BUILD_TARGET = tc.BUILD_TARGET

# Stage name -> description. Order is the order they run in per flow.
STAGES: list[tuple[str, str]] = [
    ("gen_base_cold", "generate-hashes @base, cold Bazel server"),
    ("gen_base_warm", "generate-hashes @base, warm Bazel server"),
    ("gen_head_deps", "generate-hashes @head --depEdgesFile, warm server"),
    ("impacted", "get-impacted-targets --depEdgesFile (no Bazel)"),
]
STAGE_NAMES = [s for s, _ in STAGES]
STAGE_LABELS = dict(STAGES)

# Metric key -> (label, unit). Lower is better for every metric.
METRICS: dict[str, tuple[str, str]] = {
    "cpu_s": ("bazel-diff CPU (user+sys)", "s"),
    "peak_rss_bytes": ("bazel-diff peak RSS", "MiB"),
    "user_s": ("  of which user", "s"),
    "sys_s": ("  of which sys", "s"),
    "wall_s": ("wall time", "s"),
    "bazel_cpu_s": ("Bazel server CPU", "s"),
}
# The metrics the overall verdict is built from. Wall time is reported but excluded by default:
# on the generate-hashes stages it is dominated by `bazel query`, whose variance has nothing to
# do with the change under test. `--verdict-metrics` overrides.
DEFAULT_VERDICT_METRICS = ("cpu_s", "peak_rss_bytes")

VERDICT_IMPROVED = "improved"
VERDICT_REGRESSED = "regressed"
VERDICT_UNCHANGED = "unchanged"
VERDICT_INCONCLUSIVE = "inconclusive"
VERDICT_INSUFFICIENT = "insufficient"

EXIT_OK = 0
EXIT_FAILURE = 1
EXIT_OUTPUT_MISMATCH = 2
EXIT_REGRESSED = 3
EXIT_NO_IMPROVEMENT = 4

# Exact Mann-Whitney enumeration is used while C(n+m, n) is at most this; above it the normal
# approximation (with tie and continuity corrections) is used.
EXACT_MAX_COMBINATIONS = 20000

RSS_SAMPLE_INTERVAL_S = 0.05

# Every Bazel invocation the harness makes, builds included, starts with this. A developer's
# ~/.bazelrc can change compilation flags, add a build event service, or alter query behaviour,
# so without it two runs on two machines are not comparable. It is not configurable.
MANDATORY_STARTUP_OPTIONS = ["--nohome_rc"]

log = tc.log
vlog = tc.vlog


# ----------------------------------------------------------------------------------------------
# Pure helpers (unit-tested in perf_proof_test.py; nothing here shells out)
# ----------------------------------------------------------------------------------------------


def startup_options_for(extra: str) -> list[str]:
    """The mandatory startup options followed by the user's extra ones. Anything that would
    re-enable the home rc is refused, since it would make the run irreproducible."""
    out = list(MANDATORY_STARTUP_OPTIONS)
    for opt in extra.split():
        if opt == "--home_rc" or opt.startswith("--home_rc="):
            raise SystemExit(f"{opt} is not allowed: the harness always runs Bazel with --nohome_rc "
                             "so a ~/.bazelrc cannot shape the measurement")
        if opt not in out:
            out.append(opt)
    return out


def maxrss_to_bytes(ru_maxrss: int, system: Optional[str] = None) -> int:
    """`ru_maxrss` is bytes on macOS and kibibytes everywhere else."""
    system = system or platform.system()
    return int(ru_maxrss) if system == "Darwin" else int(ru_maxrss) * 1024


def parse_ps_time(text: str) -> Optional[float]:
    """Parse the `[[H:]M:]S[.ss]` CPU time `ps -o utime`/`stime` prints (BSD/macOS)."""
    text = text.strip()
    if not text:
        return None
    parts = text.split(":")
    try:
        nums = [float(p) for p in parts]
    except ValueError:
        return None
    if not 1 <= len(nums) <= 3:
        return None
    total = 0.0
    for n in nums:
        total = total * 60 + n
    return total


def parse_proc_stat_cpu(stat_line: str, clk_tck: int) -> Optional[float]:
    """utime + stime (fields 14 and 15 of /proc/<pid>/stat) in seconds."""
    rparen = stat_line.rfind(")")
    if rparen < 0:
        return None
    fields = stat_line[rparen + 1:].split()
    # After the comm field, index 0 is `state` (field 3), so utime (14) is index 11.
    if len(fields) < 13:
        return None
    try:
        return (int(fields[11]) + int(fields[12])) / float(clk_tck)
    except ValueError:
        return None


def parse_proc_status_rss(status_text: str) -> tuple[Optional[int], Optional[int]]:
    """(VmRSS, VmHWM) in bytes from /proc/<pid>/status."""
    rss = hwm = None
    for line in status_text.splitlines():
        if line.startswith("VmRSS:"):
            rss = int(line.split()[1]) * 1024
        elif line.startswith("VmHWM:"):
            hwm = int(line.split()[1]) * 1024
    return rss, hwm


def median(xs: list[float]) -> Optional[float]:
    return statistics.median(xs) if xs else None


def _ranks(values: list[float]) -> list[float]:
    """Average ranks (1-based) with ties sharing the mean rank."""
    order = sorted(range(len(values)), key=lambda i: values[i])
    ranks = [0.0] * len(values)
    i = 0
    while i < len(order):
        j = i
        while j + 1 < len(order) and values[order[j + 1]] == values[order[i]]:
            j += 1
        avg = (i + j + 2) / 2.0
        for k in range(i, j + 1):
            ranks[order[k]] = avg
        i = j + 1
    return ranks


def mann_whitney_p(a: list[float], b: list[float]) -> Optional[float]:
    """Two-sided Mann-Whitney U p-value for H0: the two samples come from one distribution.

    Exact (by enumerating every labelling of the pooled sample) while the number of labellings is
    small, otherwise the normal approximation with tie and continuity corrections. Returns None
    when either sample is empty.
    """
    n, m = len(a), len(b)
    if n == 0 or m == 0:
        return None
    pooled = list(a) + list(b)
    ranks = _ranks(pooled)
    rank_sum_a = sum(ranks[:n])
    u_obs = rank_sum_a - n * (n + 1) / 2.0
    mu = n * m / 2.0
    dev_obs = abs(u_obs - mu)
    total = n + m
    if math.comb(total, n) <= EXACT_MAX_COMBINATIONS:
        hits = 0
        count = 0
        for idx in combinations(range(total), n):
            count += 1
            u = sum(ranks[i] for i in idx) - n * (n + 1) / 2.0
            if abs(u - mu) >= dev_obs - 1e-9:
                hits += 1
        return hits / count
    # Normal approximation.
    tie_term = 0.0
    seen: dict[float, int] = {}
    for v in pooled:
        seen[v] = seen.get(v, 0) + 1
    for t in seen.values():
        tie_term += t ** 3 - t
    var = n * m / 12.0 * ((total + 1) - tie_term / (total * (total - 1)))
    if var <= 0:
        return 1.0
    z = max(dev_obs - 0.5, 0.0) / math.sqrt(var)
    return math.erfc(z / math.sqrt(2))


def min_achievable_p(n: int, m: int) -> float:
    """The smallest two-sided p the exact Mann-Whitney test can produce for sample sizes n and m
    (the two most extreme labellings out of C(n+m, n))."""
    if n == 0 or m == 0:
        return 1.0
    return min(1.0, 2.0 / math.comb(n + m, n))


def iterations_for_alpha(alpha: float) -> int:
    """Fewest equal-sized iterations whose extreme outcome is significant at alpha."""
    n = 1
    while min_achievable_p(n, n) >= alpha:
        n += 1
    return n


def compare_samples(base: list[float], cand: list[float], threshold_pct: float, alpha: float) -> dict[str, Any]:
    """Medians, relative change and the verdict for one (stage, metric). Lower is better."""
    mb, mc = median(base), median(cand)
    delta_pct: Optional[float] = None
    if mb is not None and mc is not None and mb > 0:
        delta_pct = (mc / mb - 1.0) * 100.0
    p = mann_whitney_p(base, cand)
    if len(base) < 2 or len(cand) < 2 or delta_pct is None:
        verdict = VERDICT_INSUFFICIENT
    elif abs(delta_pct) < threshold_pct:
        verdict = VERDICT_UNCHANGED
    elif min_achievable_p(len(base), len(cand)) >= alpha:
        # No outcome of this many samples could be significant at alpha, so the difference can
        # neither be confirmed nor dismissed: more iterations are needed, not a coin flip.
        verdict = VERDICT_INSUFFICIENT
    elif p is not None and p < alpha:
        verdict = VERDICT_IMPROVED if delta_pct < 0 else VERDICT_REGRESSED
    else:
        verdict = VERDICT_INCONCLUSIVE
    return {
        "n_base": len(base),
        "n_candidate": len(cand),
        "base_median": mb,
        "candidate_median": mc,
        "base_min": min(base) if base else None,
        "base_max": max(base) if base else None,
        "candidate_min": min(cand) if cand else None,
        "candidate_max": max(cand) if cand else None,
        "delta_pct": delta_pct,
        "p_value": p,
        "verdict": verdict,
    }


def overall_verdict(comparisons: list[dict[str, Any]]) -> str:
    """Fold the per-(workload, stage, metric) verdicts that drive the decision into one word.

    Any regression wins (a change that speeds up one size and slows down another is not a proven
    improvement); then any improvement; then too few samples to judge a difference; then
    all-unchanged; otherwise inconclusive (differences beyond the threshold that the samples do
    not confirm).
    """
    verdicts = {c["verdict"] for c in comparisons}
    if VERDICT_REGRESSED in verdicts:
        return "REGRESSED"
    if VERDICT_IMPROVED in verdicts:
        return "IMPROVED"
    if VERDICT_INSUFFICIENT in verdicts:
        return "INSUFFICIENT DATA"
    if verdicts and verdicts <= {VERDICT_UNCHANGED}:
        return "NO CHANGE"
    return "INCONCLUSIVE"


def classify_candidate(spec: Optional[str]) -> str:
    """'worktree' for the uncommitted tree, 'patch' for an existing .patch/.diff file, else 'rev'."""
    if spec is None:
        return "rev"
    if spec.upper() == "WORKTREE":
        return "worktree"
    p = Path(spec)
    if p.suffix.lower() in (".patch", ".diff") and p.is_file():
        return "patch"
    return "rev"


def json_dict_diff(ref_text: str, run_text: str, limit: int = 5) -> dict[str, Any]:
    """Describe how two JSON objects (hash maps) differ: keys only on one side, keys whose value moved."""
    try:
        ref = json.loads(ref_text)
        run = json.loads(run_text)
    except json.JSONDecodeError as e:
        return {"parse_error": str(e)}
    if not isinstance(ref, dict) or not isinstance(run, dict):
        return {"identical": ref == run, "note": "not JSON objects"}
    only_ref = sorted(set(ref) - set(run))
    only_run = sorted(set(run) - set(ref))
    changed = sorted(k for k in ref.keys() & run.keys() if ref[k] != run[k])
    return {
        "identical": not (only_ref or only_run or changed),
        "only_in_reference": len(only_ref),
        "only_in_run": len(only_run),
        "changed": len(changed),
        "examples": {
            "only_in_reference": only_ref[:limit],
            "only_in_run": only_run[:limit],
            "changed": changed[:limit],
        },
    }


def fmt_metric(value: Optional[float], metric: str) -> str:
    if value is None:
        return "n/a"
    if metric == "peak_rss_bytes":
        return f"{value / (1024 * 1024):.1f} MiB"
    return f"{value:.3f}s" if value < 10 else f"{value:.2f}s"


def fmt_delta(delta_pct: Optional[float]) -> str:
    if delta_pct is None:
        return "n/a"
    return f"{delta_pct:+.1f}%"


def fmt_p(p: Optional[float]) -> str:
    if p is None:
        return "n/a"
    return "<0.001" if p < 0.001 else f"{p:.3f}"


def _verdict_badge(v: str) -> str:
    return {
        VERDICT_IMPROVED: "**improved**",
        VERDICT_REGRESSED: "**REGRESSED**",
        VERDICT_UNCHANGED: "unchanged",
        VERDICT_INCONCLUSIVE: "inconclusive",
        VERDICT_INSUFFICIENT: "insufficient data",
    }.get(v, v)


def render_summary(report: dict[str, Any]) -> str:
    """GitHub-flavored markdown: the verdict, the changeset, correctness, then a table per workload."""
    cs = report["changeset"]
    env = report["environment"]
    cfg = report["config"]
    out: list[str] = []
    out.append(f"# bazel-diff perf proof: {cs['base']['describe']} → {cs['candidate']['describe']}")
    out.append("")
    headline = report["verdict"]
    reasons = list(report.get("verdict_reasons", []))
    if headline == "INSUFFICIENT DATA":
        reasons.append(
            f"{cfg['iterations']} iterations cannot reach α={cfg['alpha']:g}; run at least "
            f"--iterations {iterations_for_alpha(cfg['alpha'])}"
        )
    elif headline == "INCONCLUSIVE":
        reasons.append("the differences above were not statistically significant; rerun with more --iterations")
    out.append(f"**Verdict: {headline}**" + (f" — {'; '.join(reasons)}" if reasons else ""))
    if not report["outputs_consistent"]:
        out.append("")
        out.append(
            "**⚠ Outputs differ between base and candidate** (hashes, dependency edges or impacted set). "
            "This is a behaviour change, not a performance change; see Correctness."
            + (" `--allow-output-diff` was given, so this does not fail the proof." if cfg.get("allow_output_diff") else "")
        )
    if report.get("failures"):
        out.append("")
        out.append(f"**{len(report['failures'])} run(s) failed**; see Failures.")
    out.append("")
    out.append("| | base | candidate |")
    out.append("|---|---|---|")
    out.append(f"| source | {cs['base']['describe']} | {cs['candidate']['describe']} |")
    out.append(f"| commit | `{cs['base'].get('commit') or 'n/a'}` | `{cs['candidate'].get('commit') or 'n/a'}` |")
    out.append(f"| tree | `{cs['base'].get('tree') or 'n/a'}` | `{cs['candidate'].get('tree') or 'n/a'}` |")
    out.append(f"| bazel-diff --version | {cs['base'].get('version') or 'n/a'} | {cs['candidate'].get('version') or 'n/a'} |")
    out.append(f"| build | {cs['base'].get('build') or 'prebuilt'} | {cs['candidate'].get('build') or 'prebuilt'} |")
    out.append("")
    out.append(
        f"Environment: {env.get('platform')}, {env.get('cpu_count')} CPUs, "
        f"{env.get('mem_total_gib') or '?'} GiB RAM, Bazel {env.get('bazel_version')}, Python {env.get('python')}. "
        f"{cfg['iterations']} interleaved iterations per workload (order alternates), "
        f"threshold ±{cfg['threshold_pct']:g}%, α={cfg['alpha']:g}, verdict metrics: "
        + ", ".join(METRICS[m][0] for m in cfg["verdict_metrics"])
        + "."
    )
    out.append("")
    out.append("## Correctness")
    out.append("")
    out.append("| workload | hashes (base rev) | hashes (head rev) | dependency edges | impacted set | impacted count |")
    out.append("|---|---|---|---|---|---:|")
    for wl in report["workloads"]:
        c = wl["correctness"]

        def mark(key: str) -> str:
            v = c.get(key)
            return "n/a" if v is None else ("identical" if v else "**DIFFERS**")

        out.append(
            f"| {wl['name']} | {mark('base_hashes')} | {mark('head_hashes')} | {mark('dep_edges')} | "
            f"{mark('impacted')} | {c.get('impacted_count', 'n/a')} |"
        )
    out.append("")
    for wl in report["workloads"]:
        meta = wl.get("meta", {})
        desc = ""
        if wl.get("kind") == "synthetic":
            desc = f" ({meta.get('packages')} packages, {meta.get('rule_targets')} rule targets)"
        elif wl.get("kind") == "repo":
            desc = f" ({meta.get('url')} {meta.get('from')}..{meta.get('to')})"
        out.append(f"## {wl['name']}{desc}")
        out.append("")
        out.append("| stage | metric | base (median) | candidate (median) | Δ | p | verdict |")
        out.append("|---|---|---:|---:|---:|---:|---|")
        for stage in STAGE_NAMES:
            first = True
            for metric in METRICS:
                cmp = wl["comparisons"].get(stage, {}).get(metric)
                if cmp is None:
                    continue
                label = STAGE_LABELS[stage] if first else ""
                first = False
                drives = metric in cfg["verdict_metrics"]
                badge = _verdict_badge(cmp["verdict"]) if drives else f"_{cmp['verdict']}_"
                out.append(
                    f"| {label} | {METRICS[metric][0]} | {fmt_metric(cmp['base_median'], metric)} | "
                    f"{fmt_metric(cmp['candidate_median'], metric)} | {fmt_delta(cmp['delta_pct'])} | "
                    f"{fmt_p(cmp['p_value'])} | {badge} |"
                )
        out.append("")
    if report.get("failures"):
        out.append("## Failures")
        out.append("")
        for f in report["failures"]:
            out.append(f"- `{f['workload']}` {f['variant']} iteration {f['iteration']} {f['stage']}: {f['error']}")
        out.append("")
    out.append("## How to read this")
    out.append("")
    out.append(
        "- Lower is better for every metric. Δ is the candidate's median relative to the base's; "
        "p is a two-sided Mann-Whitney U test over the per-iteration samples."
    )
    out.append(
        f"- A verdict of improved/regressed needs |Δ| ≥ {cfg['threshold_pct']:g}% **and** p < {cfg['alpha']:g}. "
        "A large Δ with a high p is *inconclusive*: rerun with more `--iterations`."
    )
    out.append(
        "- bazel-diff CPU and peak RSS are the kernel's accounting (`wait4`) for the bazel-diff process and the "
        "Bazel client it spawns. Bazel server CPU is the daemon's CPU delta over the stage, read from its pid "
        "(approximate). Wall time on the generate-hashes stages is mostly `bazel query`."
    )
    out.append(
        "- Verdict metrics decide the headline; the others are shown in italics for context."
    )
    return "\n".join(out) + "\n"


# ----------------------------------------------------------------------------------------------
# Process measurement
# ----------------------------------------------------------------------------------------------


def read_process_cpu_seconds(pid: Optional[int]) -> Optional[float]:
    """utime + stime of a live process, or None when it is gone or unreadable."""
    if pid is None or pid <= 0:
        return None
    stat = Path(f"/proc/{pid}/stat")
    if stat.exists():
        try:
            return parse_proc_stat_cpu(stat.read_text(), os.sysconf("SC_CLK_TCK"))
        except OSError:
            return None
    proc = subprocess.run(["ps", "-o", "utime=,stime=", "-p", str(pid)], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    if proc.returncode != 0 or not proc.stdout.strip():
        return None
    parts = proc.stdout.split()
    if len(parts) != 2:
        return None
    u, s = parse_ps_time(parts[0]), parse_ps_time(parts[1])
    return None if u is None or s is None else u + s


def _read_rss_bytes(pid: int) -> Optional[int]:
    status = Path(f"/proc/{pid}/status")
    if status.exists():
        try:
            rss, _ = parse_proc_status_rss(status.read_text())
            return rss
        except OSError:
            return None
    proc = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    if proc.returncode != 0 or not proc.stdout.strip():
        return None
    try:
        return int(proc.stdout.strip()) * 1024
    except ValueError:
        return None


@dataclass
class Measurement:
    ok: bool
    returncode: Optional[int]
    wall_s: float
    user_s: float
    sys_s: float
    cpu_s: float
    peak_rss_bytes: int
    sampled_peak_rss_bytes: Optional[int]
    bazel_cpu_s: Optional[float]
    timed_out: bool = False
    error: Optional[str] = None

    def as_dict(self) -> dict[str, Any]:
        return {
            "ok": self.ok,
            "returncode": self.returncode,
            "wall_s": round(self.wall_s, 4),
            "user_s": round(self.user_s, 4),
            "sys_s": round(self.sys_s, 4),
            "cpu_s": round(self.cpu_s, 4),
            "peak_rss_bytes": self.peak_rss_bytes,
            "sampled_peak_rss_bytes": self.sampled_peak_rss_bytes,
            "bazel_cpu_s": None if self.bazel_cpu_s is None else round(self.bazel_cpu_s, 4),
            "timed_out": self.timed_out,
            "error": self.error,
        }


def measure_process(
    cmd: list[str],
    cwd: Optional[Path],
    log_path: Path,
    timeout_s: float,
    bazel_cpu_before: Optional[float] = None,
    bazel_cpu_after: Optional[Callable[[], Optional[float]]] = None,
    env: Optional[dict[str, str]] = None,
) -> Measurement:
    """Run `cmd` to completion and return the kernel's resource accounting for it.

    The child is reaped with ``os.wait4`` so user/sys CPU and max RSS cover the whole process tree
    bazel-diff waited for (itself and the Bazel client). A sampler thread also polls the main
    process's RSS, as a cross-check and to enforce ``timeout_s``.
    """
    vlog(f"$ {' '.join(cmd)}" + (f"  (cwd={cwd})" if cwd else ""))
    log_path.parent.mkdir(parents=True, exist_ok=True)
    samples: list[int] = []
    stop = threading.Event()
    killed = {"timed_out": False}

    with open(log_path, "wb") as logf:
        t0 = time.monotonic()
        proc = subprocess.Popen(cmd, cwd=str(cwd) if cwd else None, stdout=logf, stderr=subprocess.STDOUT, env=env)

        def sampler() -> None:
            deadline = t0 + timeout_s
            while not stop.is_set():
                rss = _read_rss_bytes(proc.pid)
                if rss is not None:
                    samples.append(rss)
                if time.monotonic() > deadline:
                    killed["timed_out"] = True
                    try:
                        proc.kill()
                    except OSError:
                        pass
                    return
                stop.wait(RSS_SAMPLE_INTERVAL_S)

        th = threading.Thread(target=sampler, daemon=True)
        th.start()
        _, status, ru = os.wait4(proc.pid, 0)
        wall = time.monotonic() - t0
        stop.set()
        th.join()
        # Tell Popen the child is gone so its finalizer does not try to reap it again.
        proc.returncode = os.waitstatus_to_exitcode(status) if hasattr(os, "waitstatus_to_exitcode") else status

    after = bazel_cpu_after() if bazel_cpu_after else None
    bazel_cpu = None
    if after is not None:
        bazel_cpu = after - (bazel_cpu_before or 0.0)
    rc = proc.returncode
    ok = rc == 0 and not killed["timed_out"]
    error = None
    if killed["timed_out"]:
        error = f"timed out after {timeout_s:.0f}s"
    elif rc != 0:
        tail = ""
        try:
            tail = "\n".join(log_path.read_text(errors="replace").splitlines()[-15:])
        except OSError:
            pass
        error = f"exit {rc}: {' '.join(cmd)}\n{tail}"
    return Measurement(
        ok=ok,
        returncode=rc,
        wall_s=wall,
        user_s=ru.ru_utime,
        sys_s=ru.ru_stime,
        cpu_s=ru.ru_utime + ru.ru_stime,
        peak_rss_bytes=maxrss_to_bytes(ru.ru_maxrss),
        sampled_peak_rss_bytes=max(samples) if samples else None,
        bazel_cpu_s=bazel_cpu,
        timed_out=killed["timed_out"],
        error=error,
    )


# ----------------------------------------------------------------------------------------------
# Changeset resolution and builds
# ----------------------------------------------------------------------------------------------


@dataclass
class Variant:
    name: str  # "base" | "candidate"
    describe: str
    commit: Optional[str] = None
    tree: Optional[str] = None
    patches: list[str] = field(default_factory=list)
    binary: Optional[Path] = None
    build: Optional[str] = None
    version: Optional[str] = None

    def as_dict(self) -> dict[str, Any]:
        return {
            "name": self.name,
            "describe": self.describe,
            "commit": self.commit,
            "tree": self.tree,
            "patches": self.patches,
            "binary": str(self.binary) if self.binary else None,
            "build": self.build,
            "version": self.version,
        }


def repo_git(*args: str) -> str:
    return tc.git(REPO_ROOT, *args)


def short_describe(sha: str) -> str:
    subject = repo_git("log", "-1", "--format=%s", sha)
    if len(subject) > 60:
        subject = subject[:57] + "..."
    return f"{sha[:10]} {subject}"


def resolve_base(spec: Optional[str], main_branch: str) -> str:
    if spec:
        return repo_git("rev-parse", "--verify", spec + "^{commit}")
    for ref in (main_branch, f"origin/{main_branch}"):
        proc = tc.run(["git", "-C", REPO_ROOT, "merge-base", "HEAD", ref], check=False)
        if proc.returncode == 0 and proc.stdout.strip():
            return proc.stdout.strip()
    raise SystemExit(
        f"cannot find the merge-base of HEAD with {main_branch} or origin/{main_branch}; pass --base <rev>"
    )


def snapshot_working_tree(workdir: Path) -> tuple[str, str]:
    """Commit the current working tree (tracked changes + untracked, non-ignored files) without
    touching the real index or HEAD. Returns (commit, tree)."""
    index = workdir / "worktree-snapshot.index"
    env = dict(os.environ, GIT_INDEX_FILE=str(index))
    tc.run(["git", "-C", REPO_ROOT, "read-tree", "HEAD"], env=env)
    tc.run(["git", "-C", REPO_ROOT, "add", "-A"], env=env)
    tree = tc.run(["git", "-C", REPO_ROOT, "write-tree"], env=env).stdout.strip()
    head = repo_git("rev-parse", "HEAD")
    commit = tc.run(
        ["git", "-C", REPO_ROOT, "commit-tree", tree, "-p", head, "-m", "perf_proof: working tree snapshot"],
        env=dict(env, GIT_AUTHOR_NAME="perf_proof", GIT_AUTHOR_EMAIL="perf@bazel-diff.local",
                 GIT_COMMITTER_NAME="perf_proof", GIT_COMMITTER_EMAIL="perf@bazel-diff.local"),
    ).stdout.strip()
    return commit, tree


def tree_of_worktree(worktree: Path, workdir: Path) -> str:
    """Tree hash of a worktree's current content (for provenance after applying patches)."""
    index = workdir / "provenance.index"
    env = dict(os.environ, GIT_INDEX_FILE=str(index))
    tc.run(["git", "-C", worktree, "read-tree", "HEAD"], env=env)
    tc.run(["git", "-C", worktree, "add", "-A"], env=env)
    return tc.run(["git", "-C", worktree, "write-tree"], env=env).stdout.strip()


def rmtree_force(path: Path) -> None:
    """shutil.rmtree that copes with Bazel's write-protected output directories."""

    def on_error(func, failed_path, _exc_info):
        parent = Path(failed_path).parent
        try:
            os.chmod(parent, 0o700)
            os.chmod(failed_path, 0o700)
            func(failed_path)
        except OSError:
            pass

    shutil.rmtree(path, onerror=on_error)


def git_common_dir() -> str:
    return repo_git("rev-parse", "--path-format=absolute", "--git-common-dir")


def default_build_dir() -> Path:
    """A per-repository directory under the user's cache that survives between runs, so every
    run after the first pays an incremental build (seconds) rather than a cold one (minutes:
    a fresh output base re-fetches and rebuilds every crate and the toolchain)."""
    root = Path(os.environ.get("XDG_CACHE_HOME") or (Path.home() / ".cache")) / "bazel-diff-perf-proof"
    import hashlib
    key = hashlib.sha1(git_common_dir().encode()).hexdigest()[:12]
    return root / key


class SourceWorktree:
    """One detached git worktree the two variants are checked out into in turn, so the second
    build is an incremental one on top of the first (the same thing a developer's build does).

    The worktree and its Bazel output base live in a build directory that persists between runs
    (see default_build_dir); the harness re-attaches to it when it is a worktree of this
    repository and recreates it otherwise."""

    def __init__(self, build_dir: Path, base_sha: str, bazel: str, startup_options: list[str], fresh: bool = False):
        self.path = build_dir / "src"
        self.output_base = build_dir / "bazel-output-base"
        self.cargo_target = build_dir / "cargo-target"
        self.bazel_startup = [*startup_options, f"--output_base={self.output_base}"]
        if fresh and build_dir.exists():
            log(f"removing build directory {build_dir}")
            if self.output_base.exists() and self.path.is_dir():
                # Bazel knows how to take its own output base apart (write-protected trees included).
                tc.run([bazel, *self.bazel_startup, "clean", "--expunge"], cwd=self.path, check=False)
            tc.run(["git", "-C", REPO_ROOT, "worktree", "remove", "--force", self.path], check=False)
            rmtree_force(build_dir)
        build_dir.mkdir(parents=True, exist_ok=True)
        if self.path.exists() and self._is_our_worktree():
            log(f"reusing build worktree {self.path}")
            self.checkout(base_sha)
        else:
            if self.path.exists():
                log(f"{self.path} is not a worktree of this repository; recreating it")
                shutil.rmtree(self.path, ignore_errors=True)
            tc.run(["git", "-C", REPO_ROOT, "worktree", "prune"], check=False)
            log(f"checking out {base_sha[:10]} into {self.path}")
            tc.run(["git", "-C", REPO_ROOT, "worktree", "add", "--detach", "--quiet", self.path, base_sha])

    def _is_our_worktree(self) -> bool:
        proc = tc.run(["git", "-C", self.path, "rev-parse", "--path-format=absolute", "--git-common-dir"], check=False)
        return proc.returncode == 0 and proc.stdout.strip() == git_common_dir()

    def checkout(self, sha: str) -> None:
        tc.run(["git", "-C", self.path, "checkout", "--quiet", "--force", "--detach", sha])
        tc.run(["git", "-C", self.path, "clean", "-fdq"])

    def apply_patches(self, patches: list[Path]) -> None:
        for p in patches:
            log(f"applying {p}")
            tc.run(["git", "-C", self.path, "apply", "--whitespace=nowarn", p.resolve()])

    def build(self, builder: str, bazel: str, variant: Variant, dest: Path) -> None:
        t0 = time.monotonic()
        if builder == "bazel":
            cmd = [
                bazel, *self.bazel_startup, "build", BUILD_TARGET, "--config=release",
                "--output_groups=-clippy_checks,-rustfmt_checks",
            ]
            built = self.path / "bazel-bin" / "src" / "bazel-diff"
        elif builder == "cargo":
            cmd = ["cargo", "build", "--release", "--locked"]
            built = self.cargo_target / "release" / "bazel-diff"
        else:
            raise SystemExit(f"unknown builder {builder}")
        log(f"[{variant.name}] building: {' '.join(str(c) for c in cmd)}")
        env = dict(os.environ)
        if builder == "cargo":
            env["CARGO_TARGET_DIR"] = str(self.cargo_target)
        proc = tc.run(cmd, cwd=self.path, env=env, check=False)
        if proc.returncode != 0:
            tail = "\n".join(proc.stdout.splitlines()[-40:])
            raise SystemExit(f"[{variant.name}] build failed ({proc.returncode}):\n{tail}")
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(built.resolve(), dest)
        dest.chmod(0o755)
        variant.binary = dest
        variant.build = f"{builder} ({'--config=release' if builder == 'bazel' else '--release'}), {time.monotonic() - t0:.0f}s"

    def close(self, bazel: str) -> None:
        """Stop the build server; the worktree and output base stay for the next run."""
        if self.output_base.exists():
            tc.run([bazel, *self.bazel_startup, "shutdown"], cwd=self.path, check=False)


def bazel_diff_version(binary: Path) -> str:
    proc = tc.run([binary, "--version"], check=False)
    return proc.stdout.strip().splitlines()[-1] if proc.stdout.strip() else "unknown"


def resolve_changeset(args: argparse.Namespace, workdir: Path) -> tuple[Variant, Variant]:
    base_sha = resolve_base(args.base, args.main_branch)
    base = Variant(name="base", describe=short_describe(base_sha), commit=base_sha,
                   tree=repo_git("rev-parse", base_sha + "^{tree}"))

    if args.aa:
        cand = Variant(name="candidate", describe=f"{base.describe} (A/A: same binary)", commit=base_sha, tree=base.tree)
        return base, cand

    patches = [Path(p) for p in (args.patch or [])]
    kind = classify_candidate(args.candidate)
    if kind == "patch":
        patches.insert(0, Path(args.candidate))
        kind = "rev"
        cand_sha = base_sha
    elif kind == "worktree":
        dirty = tc.run(["git", "-C", REPO_ROOT, "status", "--porcelain"], check=False).stdout.strip()
        if not dirty:
            log("note: the working tree is clean; WORKTREE is the same as HEAD")
        cand_sha, _ = snapshot_working_tree(workdir)
    else:
        cand_sha = repo_git("rev-parse", "--verify", (args.candidate or "HEAD") + "^{commit}")
        if not patches and not args.candidate:
            dirty = tc.run(["git", "-C", REPO_ROOT, "status", "--porcelain", "--untracked-files=no"], check=False).stdout.strip()
            if dirty:
                log("note: the working tree has uncommitted changes that HEAD does not include; "
                    "pass --candidate WORKTREE to measure them")
    for p in patches:
        if not p.is_file():
            raise SystemExit(f"patch not found: {p}")
    if patches:
        describe = f"{base_sha[:10]} + {len(patches)} patch(es): " + ", ".join(p.name for p in patches)
    elif kind == "worktree":
        describe = f"working tree ({repo_git('rev-parse', '--short=10', 'HEAD')} + uncommitted changes)"
    else:
        describe = short_describe(cand_sha)
    if not patches and cand_sha == base_sha:
        raise SystemExit(
            f"base and candidate are the same commit ({base_sha[:10]}); nothing to compare. "
            "Pass --candidate/--patch, or --aa to measure the noise floor."
        )
    cand = Variant(name="candidate", describe=describe, commit=cand_sha,
                   tree=None if patches else repo_git("rev-parse", cand_sha + "^{tree}"),
                   patches=[str(p) for p in patches])
    return base, cand


def build_variants(args: argparse.Namespace, workdir: Path, base: Variant, cand: Variant) -> None:
    bin_dir = workdir / "bin"

    def adopt(variant: Variant, given: str) -> None:
        src = Path(given)
        if not src.is_file():
            raise SystemExit(f"--{variant.name}-binary: no file at {src}")
        dest = bin_dir / variant.name / "bazel-diff"
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src.resolve(), dest)
        dest.chmod(0o755)
        variant.binary = dest
        variant.build = f"prebuilt ({src})"

    if args.base_binary:
        adopt(base, args.base_binary)
    if args.candidate_binary:
        adopt(cand, args.candidate_binary)
    if args.aa and base.binary is None and cand.binary is not None:
        base.binary = cand.binary
    need_base = base.binary is None
    need_cand = cand.binary is None and not args.aa
    if need_base or need_cand:
        build_dir = Path(args.build_dir).resolve() if args.build_dir else default_build_dir()
        build_startup = startup_options_for(args.build_bazel_startup_options)
        wt = SourceWorktree(build_dir, base.commit or "HEAD", args.build_bazel, build_startup, fresh=args.fresh_build)
        try:
            if need_base:
                wt.build(args.builder, args.build_bazel, base, bin_dir / "base" / "bazel-diff")
            if need_cand:
                if cand.patches:
                    wt.checkout(base.commit)
                    wt.apply_patches([Path(p) for p in cand.patches])
                    cand.tree = tree_of_worktree(wt.path, workdir)
                else:
                    wt.checkout(cand.commit)
                wt.build(args.builder, args.build_bazel, cand, bin_dir / "candidate" / "bazel-diff")
        finally:
            wt.close(args.build_bazel)
        log(f"build directory kept for the next run: {build_dir} (--fresh-build recreates it)")
    if args.aa:
        cand.binary = base.binary
        cand.build = base.build
    for v in (base, cand):
        if v.binary is None or not v.binary.is_file():
            raise SystemExit(f"{v.name}: no bazel-diff binary at {v.binary}")
        v.version = bazel_diff_version(v.binary)


# ----------------------------------------------------------------------------------------------
# Workloads and the measured flow
# ----------------------------------------------------------------------------------------------


def bazel_server_pid(bazel: str, startup_options: list[str], workspace: Path) -> Optional[int]:
    proc = tc.run([bazel, *startup_options, "info", "server_pid"], cwd=workspace, check=False)
    # stderr is merged into stdout, so take the last line that is just a number.
    pids = [line.strip() for line in proc.stdout.splitlines() if line.strip().isdigit()]
    return int(pids[-1]) if pids else None


@dataclass
class FlowContext:
    bazel: str
    startup_options: list[str]
    generate_args: list[str]
    timeout_s: float


def run_flow(ctx: FlowContext, binary: Path, wl: tc.Workload, out_dir: Path) -> dict[str, Any]:
    """One pass of the four stages with one binary. Returns per-stage measurements and output paths."""
    out_dir.mkdir(parents=True, exist_ok=True)
    ws = wl.path
    startup = " ".join(ctx.startup_options)
    common = [str(binary)]
    gen_common = ["generate-hashes", "-w", str(ws), "-b", ctx.bazel]
    if startup:
        gen_common.append(f"--bazelStartupOptions={startup}")
    gen_common += ctx.generate_args
    paths = {
        "base_hashes": out_dir / "base.json",
        "base_hashes_warm": out_dir / "base_warm.json",
        "head_hashes": out_dir / "head.json",
        "dep_edges": out_dir / "deps.json",
        "impacted": out_dir / "impacted.json",
    }
    stages: dict[str, dict[str, Any]] = {}

    def record(stage: str, m: Measurement) -> bool:
        stages[stage] = m.as_dict()
        return m.ok

    tc.git(ws, "checkout", "--quiet", "--force", wl.base_sha)
    tc.run([ctx.bazel, *ctx.startup_options, "shutdown"], cwd=ws, check=False)

    # Stage 1: cold. The server does not exist yet, so its CPU is read after the stage only.
    server: dict[str, Optional[int]] = {"pid": None}

    def server_cpu_after_cold() -> Optional[float]:
        server["pid"] = bazel_server_pid(ctx.bazel, ctx.startup_options, ws)
        return read_process_cpu_seconds(server["pid"])

    m = measure_process(common + gen_common + [str(paths["base_hashes"])], ws, out_dir / "gen_base_cold.log",
                        ctx.timeout_s, bazel_cpu_before=0.0, bazel_cpu_after=server_cpu_after_cold)
    if not record("gen_base_cold", m):
        return {"stages": stages, "paths": paths}

    def server_cpu() -> Optional[float]:
        return read_process_cpu_seconds(server["pid"])

    m = measure_process(common + gen_common + [str(paths["base_hashes_warm"])], ws, out_dir / "gen_base_warm.log",
                        ctx.timeout_s, bazel_cpu_before=server_cpu(), bazel_cpu_after=server_cpu)
    if not record("gen_base_warm", m):
        return {"stages": stages, "paths": paths}

    tc.git(ws, "checkout", "--quiet", "--force", wl.head_sha)
    m = measure_process(common + gen_common + ["-d", str(paths["dep_edges"]), str(paths["head_hashes"])], ws,
                        out_dir / "gen_head_deps.log", ctx.timeout_s, bazel_cpu_before=server_cpu(), bazel_cpu_after=server_cpu)
    if not record("gen_head_deps", m):
        return {"stages": stages, "paths": paths}

    impacted_cmd = common + [
        "get-impacted-targets", "-w", str(ws), "-b", ctx.bazel,
        "--startingHashes", str(paths["base_hashes"]), "--finalHashes", str(paths["head_hashes"]),
        "-d", str(paths["dep_edges"]), "-o", str(paths["impacted"]),
    ]
    if startup:
        impacted_cmd.append(f"--bazelStartupOptions={startup}")
    # No Bazel is involved here; the server's idle churn would only add noise to the table.
    m = measure_process(impacted_cmd, ws, out_dir / "impacted.log", ctx.timeout_s)
    record("impacted", m)
    return {"stages": stages, "paths": paths}


def check_outputs(reference: dict[str, Path], run: dict[str, Path]) -> dict[str, Any]:
    """Compare a run's outputs with the reference run's. Hash files must be byte-identical."""
    result: dict[str, Any] = {}
    for key, label in (("base_hashes", "base_hashes"), ("head_hashes", "head_hashes"), ("dep_edges", "dep_edges")):
        ref_p, run_p = reference.get(key), run.get(key)
        if ref_p is None or run_p is None or not ref_p.is_file() or not run_p.is_file():
            result[label] = None
            continue
        ref_t, run_t = ref_p.read_bytes(), run_p.read_bytes()
        if ref_t == run_t:
            result[label] = True
        else:
            result[label] = False
            result[label + "_diff"] = json_dict_diff(ref_t.decode(errors="replace"), run_t.decode(errors="replace"))
    ref_p, run_p = reference.get("impacted"), run.get("impacted")
    if ref_p is None or run_p is None or not ref_p.is_file() or not run_p.is_file():
        result["impacted"] = None
    else:
        ref_l = tc.parse_impacted_targets(ref_p.read_text())
        run_l = tc.parse_impacted_targets(run_p.read_text())
        result["impacted_count"] = len(run_l)
        if set(ref_l) == set(run_l):
            result["impacted"] = True
        else:
            result["impacted"] = False
            result["impacted_diff"] = tc.compare_label_sets(ref_l, run_l)
    return result


def bench_workload(
    ctx: FlowContext,
    wl: tc.Workload,
    base: Variant,
    cand: Variant,
    iterations: int,
    warmup_runs: int,
    out_root: Path,
    threshold_pct: float,
    alpha: float,
) -> dict[str, Any]:
    variants = {"base": base, "candidate": cand}
    samples: dict[str, dict[str, dict[str, list[float]]]] = {
        v: {s: {m: [] for m in METRICS} for s in STAGE_NAMES} for v in variants
    }
    raw: list[dict[str, Any]] = []
    failures: list[dict[str, Any]] = []
    reference: Optional[dict[str, Path]] = None
    correctness: dict[str, Any] = {"base_hashes": None, "head_hashes": None, "dep_edges": None, "impacted": None,
                                   "impacted_count": None, "mismatches": []}

    def agg(key: str, value: Optional[bool]) -> None:
        if value is None:
            return
        correctness[key] = value if correctness[key] is None else (correctness[key] and value)

    for w in range(warmup_runs):
        log(f"[{wl.name}] warm-up {w + 1}/{warmup_runs} (untimed)")
        res = run_flow(ctx, base.binary, wl, out_root / wl.name / f"warmup{w}")
        bad = [s for s, m in res["stages"].items() if not m["ok"]]
        if bad:
            err = res["stages"][bad[0]].get("error")
            raise SystemExit(f"[{wl.name}] warm-up failed at {bad[0]}: {err}")

    def compare_against_reference(entry: dict[str, Any], paths: dict[str, Path]) -> None:
        assert reference is not None
        chk = check_outputs(reference, paths)
        entry["outputs"] = chk
        for key in ("base_hashes", "head_hashes", "dep_edges", "impacted"):
            agg(key, chk.get(key))
            if chk.get(key) is False:
                correctness["mismatches"].append({"iteration": entry["iteration"], "variant": entry["variant"],
                                                  "output": key, "diff": chk.get(key + "_diff")})
        if chk.get("impacted_count") is not None:
            correctness["impacted_count"] = chk["impacted_count"]

    # Complete runs whose outputs still await a reference (a candidate that ran before the first
    # successful base run), with their output paths.
    pending: list[tuple[dict[str, Any], dict[str, Path]]] = []

    for i in range(iterations):
        order = ["base", "candidate"] if i % 2 == 0 else ["candidate", "base"]
        for vname in order:
            v = variants[vname]
            log(f"[{wl.name}] iteration {i + 1}/{iterations}: {vname}")
            res = run_flow(ctx, v.binary, wl, out_root / wl.name / f"iter{i}" / vname)
            entry: dict[str, Any] = {"iteration": i, "variant": vname, "stages": res["stages"]}
            raw.append(entry)
            for stage, m in res["stages"].items():
                if not m["ok"]:
                    failures.append({"workload": wl.name, "variant": vname, "iteration": i, "stage": stage, "error": m.get("error")})
                    continue
                for metric in METRICS:
                    val = m.get(metric)
                    if val is not None:
                        samples[vname][stage][metric].append(float(val))
            complete = len(res["stages"]) == len(STAGE_NAMES) and all(m["ok"] for m in res["stages"].values())
            if not complete:
                continue
            if reference is None and vname == "base":
                reference = res["paths"]
                entry["outputs"] = {"reference": True}
            else:
                pending.append((entry, res["paths"]))
            if reference is not None:
                for pend_entry, pend_paths in pending:
                    compare_against_reference(pend_entry, pend_paths)
                pending.clear()

    comparisons: dict[str, dict[str, dict[str, Any]]] = {}
    for stage in STAGE_NAMES:
        comparisons[stage] = {}
        for metric in METRICS:
            b = samples["base"][stage][metric]
            c = samples["candidate"][stage][metric]
            if not b and not c:
                continue
            comparisons[stage][metric] = compare_samples(b, c, threshold_pct, alpha)
    # tc.Workload.meta holds the generator's metadata (packages, rule_targets, ...) or the repo spec.
    return {
        "name": wl.name,
        "kind": wl.kind,
        "meta": wl.meta,
        "base_sha": wl.base_sha,
        "head_sha": wl.head_sha,
        "comparisons": comparisons,
        "samples": samples,
        "correctness": correctness,
        "failures": failures,
        "raw": raw,
    }


# ----------------------------------------------------------------------------------------------
# Environment, CLI, main
# ----------------------------------------------------------------------------------------------


def collect_environment(bazel: str, startup_options: list[str], workspace: Optional[Path]) -> dict[str, Any]:
    env: dict[str, Any] = {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "cpu_count": os.cpu_count(),
        "bazel_path": bazel,
        "repo_head": "unknown",
        "mem_total_gib": None,
        "bazel_version": "unknown",
    }
    try:
        env["repo_head"] = repo_git("rev-parse", "HEAD")
    except RuntimeError:
        pass
    try:
        meminfo = Path("/proc/meminfo").read_text()
        kib = int(re.search(r"MemTotal:\s+(\d+)", meminfo).group(1))
        env["mem_total_gib"] = round(kib / 1024 / 1024, 1)
    except (OSError, AttributeError, ValueError):
        proc = subprocess.run(["sysctl", "-n", "hw.memsize"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        try:
            env["mem_total_gib"] = round(int(proc.stdout.strip()) / 1024 ** 3, 1)
        except ValueError:
            pass
    if workspace is not None:
        proc = tc.run([bazel, *startup_options, "version"], cwd=workspace, check=False)
        m = re.search(r"^Build label:\s*(\S+)", proc.stdout, re.MULTILINE)
        if m:
            env["bazel_version"] = m.group(1)
    return env


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="perf_proof.py",
        description="Prove a bazel-diff changeset's effect on CPU and memory across workspaces of several sizes.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__.split("Usage (from a checkout", 1)[-1] if __doc__ else None,
    )
    cs = p.add_argument_group("changeset")
    cs.add_argument("--base", help="base revision (default: merge-base of HEAD with --main-branch)")
    cs.add_argument("--candidate", help="candidate revision, a .patch/.diff file applied on top of the base, or WORKTREE for "
                                        "the uncommitted working tree (default: HEAD)")
    cs.add_argument("--patch", action="append", help="patch applied on top of the base (repeatable, in order); "
                                                     "makes the candidate 'base + patches'")
    cs.add_argument("--aa", action="store_true", help="A/A run: measure the base binary against itself (the noise floor)")
    cs.add_argument("--main-branch", default="master", help="branch the default base is the merge-base with (default: master)")
    cs.add_argument("--base-binary", help="use this prebuilt bazel-diff as the base instead of building")
    cs.add_argument("--candidate-binary", help="use this prebuilt bazel-diff as the candidate instead of building")
    cs.add_argument("--builder", choices=("bazel", "cargo"), default="bazel",
                    help="how to build each side: bazel (--config=release, what releases ship; default) or cargo (--release, faster rebuilds)")
    cs.add_argument("--build-bazel", default="bazel", help="bazel binary used to BUILD bazel-diff (default: bazel)")
    cs.add_argument("--build-bazel-startup-options", default="",
                    help="extra startup options for the builds, added after the mandatory --nohome_rc (a ~/.bazelrc that "
                         "streams to a build event service or changes compilation flags must not shape the compared binaries)")
    cs.add_argument("--build-dir", help="persistent build directory (worktree + Bazel output base) reused across runs "
                                        "(default: $XDG_CACHE_HOME/bazel-diff-perf-proof/<repo-key>, ~/.cache otherwise)")
    cs.add_argument("--fresh-build", action="store_true", help="delete the build directory first (a cold build, minutes)")

    wl = p.add_argument_group("workloads")
    wl.add_argument("--workload", action="append", choices=sorted(tc.SYNTHETIC_PRESETS),
                    help="synthetic preset (repeatable; default: small, medium, large)")
    wl.add_argument("--synthetic", action="append", default=[],
                    help="custom synthetic workload name:packages:targets_per_package[:layers] (repeatable)")
    wl.add_argument("--repo-url", help="also measure a real repository (git URL or local path)")
    wl.add_argument("--repo-from", default="HEAD~1", help="base revision in --repo-url (default: HEAD~1)")
    wl.add_argument("--repo-to", default="HEAD", help="head revision in --repo-url (default: HEAD)")
    wl.add_argument("--repo-clone-args", default="", help="extra `git clone` arguments for --repo-url, e.g. --depth=50")
    wl.add_argument("--bazel-version", default=None, help="Bazel version the synthetic workspaces pin (default: the generator's)")

    m = p.add_argument_group("measurement")
    m.add_argument("--iterations", type=int, default=None,
                   help="timed iterations per workload, each running both sides (default: 5; 2 with --quick)")
    m.add_argument("--warmup-runs", type=int, default=1, help="untimed flows with the base binary before timing (default: 1)")
    m.add_argument("--quick", action="store_true", help="smoke profile: tiny workload, 2 iterations")
    m.add_argument("--bazel", default="bazel", help="bazel binary the measured workspaces use (default: bazel)")
    m.add_argument("--bazel-startup-options", default="",
                   help="extra startup options for every measured Bazel invocation, added after the mandatory --nohome_rc "
                        "(your ~/.bazelrc never joins the measurement)")
    m.add_argument("--generate-arg", action="append", default=[],
                   help="extra flag for every generate-hashes invocation, e.g. --generate-arg=--useCquery (repeatable)")
    m.add_argument("--stage-timeout", type=float, default=1800.0, help="seconds before a stage is killed (default: 1800)")

    v = p.add_argument_group("verdict")
    v.add_argument("--threshold", type=float, default=5.0,
                   help="percent change below which a metric is 'unchanged' (default: 5; an A/A run of the same binary "
                        "drifts about 4%% in CPU on the small workload)")
    v.add_argument("--alpha", type=float, default=0.05, help="significance level for the Mann-Whitney test (default: 0.05)")
    v.add_argument("--verdict-metrics", default=",".join(DEFAULT_VERDICT_METRICS),
                   help=f"comma-separated metrics that decide the verdict; choices: {', '.join(METRICS)} (default: %(default)s)")
    v.add_argument("--allow-output-diff", action="store_true",
                   help="do not fail when the candidate's hashes or impacted set differ from the base's (an intentional hash change)")
    v.add_argument("--fail-on-regression", action="store_true", help=f"exit {EXIT_REGRESSED} when the verdict is REGRESSED")
    v.add_argument("--require-improvement", action="store_true",
                   help=f"exit {EXIT_NO_IMPROVEMENT} unless the verdict is IMPROVED (for 'this PR is a perf change')")

    o = p.add_argument_group("output")
    o.add_argument("--workdir", help="scratch directory (default: a fresh temp dir)")
    o.add_argument("--keep-workdir", action="store_true", help="keep the scratch directory (workspaces, binaries, logs, every output file)")
    o.add_argument("--summary-out", default="perf-proof-summary.md", help="markdown report path (default: %(default)s)")
    o.add_argument("--metrics-out", default="perf-proof-metrics.json", help="JSON metrics path (default: %(default)s)")
    o.add_argument("-v", "--verbose", action="store_true", help="echo every command")
    return p


def select_workloads(args: argparse.Namespace) -> list[tuple[str, str, Any]]:
    """[(name, kind, spec)] in run order; kind is 'synthetic' or 'repo'."""
    out: list[tuple[str, str, Any]] = []
    if args.quick:
        out.append(("tiny", "synthetic", tc.SYNTHETIC_PRESETS["tiny"]))
    else:
        for name in args.workload or ["small", "medium", "large"]:
            out.append((name, "synthetic", tc.SYNTHETIC_PRESETS[name]))
    for spec in args.synthetic:
        name, params = tc.parse_synthetic_spec(spec)
        out.append((name, "synthetic", params))
    if args.repo_url:
        out.append(("repo", "repo", {"url": args.repo_url, "from": args.repo_from, "to": args.repo_to,
                                     "clone_args": args.repo_clone_args.split()}))
    return out


def main(argv: list[str]) -> int:
    args = build_parser().parse_args(argv)
    tc.VERBOSE = args.verbose
    if args.iterations is None:
        args.iterations = 2 if args.quick else 5
    verdict_metrics = [m.strip() for m in args.verdict_metrics.split(",") if m.strip()]
    for m in verdict_metrics:
        if m not in METRICS:
            raise SystemExit(f"unknown verdict metric {m!r}; choices: {', '.join(METRICS)}")
    if args.iterations < 1:
        raise SystemExit("--iterations must be at least 1")

    workdir = Path(args.workdir).resolve() if args.workdir else Path(tempfile.mkdtemp(prefix="perf-proof-"))
    workdir.mkdir(parents=True, exist_ok=True)
    keep = args.keep_workdir or bool(args.workdir)
    log(f"workdir: {workdir}" + ("" if keep else " (removed at the end; --keep-workdir keeps it)"))
    startup_options = startup_options_for(args.bazel_startup_options)
    ctx = FlowContext(bazel=args.bazel, startup_options=startup_options, generate_args=args.generate_arg,
                      timeout_s=args.stage_timeout)
    t_start = time.monotonic()
    exit_code = EXIT_OK
    workloads: list[tc.Workload] = []
    try:
        base, cand = resolve_changeset(args, workdir)
        log(f"base:      {base.describe}")
        log(f"candidate: {cand.describe}")
        build_variants(args, workdir, base, cand)
        log(f"base binary:      {base.binary} ({base.version})")
        log(f"candidate binary: {cand.binary} ({cand.version})")

        gen = tc.load_gen_project()
        bazel_version = args.bazel_version or gen.DEFAULT_BAZEL_VERSION
        for name, kind, spec in select_workloads(args):
            if kind == "synthetic":
                workloads.append(tc.make_synthetic_workload(name, spec, workdir, bazel_version))
            else:
                workloads.append(tc.make_repo_workload(name, spec["url"], spec["from"], spec["to"], spec["clone_args"], workdir))
        env = collect_environment(args.bazel, startup_options, workloads[0].path if workloads else None)

        results: list[dict[str, Any]] = []
        for wl in workloads:
            results.append(bench_workload(ctx, wl, base, cand, args.iterations, args.warmup_runs,
                                          workdir / "runs", args.threshold, args.alpha))
            tc.run([args.bazel, *startup_options, "shutdown"], cwd=wl.path, check=False)

        decisive: list[dict[str, Any]] = []
        reasons: list[str] = []
        unconfirmed: list[str] = []
        for r in results:
            for stage in STAGE_NAMES:
                for metric in verdict_metrics:
                    cmp = r["comparisons"].get(stage, {}).get(metric)
                    if cmp is None:
                        continue
                    decisive.append(cmp)
                    if cmp["verdict"] in (VERDICT_IMPROVED, VERDICT_REGRESSED):
                        reasons.append(f"{r['name']}/{STAGE_LABELS[stage].split(',')[0]} {METRICS[metric][0]} "
                                       f"{fmt_delta(cmp['delta_pct'])} (p={fmt_p(cmp['p_value'])})")
                    elif cmp["verdict"] == VERDICT_INCONCLUSIVE:
                        unconfirmed.append(f"{r['name']}/{STAGE_LABELS[stage].split(',')[0]} {METRICS[metric][0]} "
                                           f"{fmt_delta(cmp['delta_pct'])} (p={fmt_p(cmp['p_value'])})")
        verdict = overall_verdict(decisive)
        if verdict == "INCONCLUSIVE":
            reasons.extend(unconfirmed[:3])
        failures = [f for r in results for f in r["failures"]]
        outputs_consistent = all(
            v is not False for r in results for k, v in r["correctness"].items()
            if k in ("base_hashes", "head_hashes", "dep_edges", "impacted")
        )
        report = {
            "tool": "perf_proof.py",
            "generated_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
            "verdict": verdict,
            "verdict_reasons": reasons[:8],
            "outputs_consistent": outputs_consistent,
            "changeset": {"base": base.as_dict(), "candidate": cand.as_dict()},
            "environment": env,
            "config": {
                "iterations": args.iterations,
                "warmup_runs": args.warmup_runs,
                "threshold_pct": args.threshold,
                "alpha": args.alpha,
                "verdict_metrics": verdict_metrics,
                "allow_output_diff": args.allow_output_diff,
                "builder": args.builder,
                "build_bazel_startup_options": startup_options_for(args.build_bazel_startup_options),
                "bazel_startup_options": startup_options,
                "generate_args": args.generate_arg,
                "stages": STAGES,
            },
            "workloads": results,
            "failures": failures,
            "total_s": round(time.monotonic() - t_start, 1),
        }
        summary = render_summary(report)
        Path(args.summary_out).write_text(summary)
        Path(args.metrics_out).write_text(json.dumps(report, indent=2, default=str) + "\n")
        print()
        print(summary)
        log(f"summary: {Path(args.summary_out).resolve()}")
        log(f"metrics: {Path(args.metrics_out).resolve()}")
        log(f"total time: {report['total_s']:.0f}s")

        if failures:
            exit_code = EXIT_FAILURE
        elif not outputs_consistent and not args.allow_output_diff:
            exit_code = EXIT_OUTPUT_MISMATCH
        elif verdict == "REGRESSED" and args.fail_on_regression:
            exit_code = EXIT_REGRESSED
        elif verdict != "IMPROVED" and args.require_improvement:
            exit_code = EXIT_NO_IMPROVEMENT
    finally:
        for wl in workloads:
            # The workspaces' Bazel output bases live outside the workdir; take them down with the
            # workspaces rather than leaving a few hundred MB per workload behind.
            tc.run([args.bazel, *startup_options, "shutdown" if keep else "clean", *([] if keep else ["--expunge"])],
                   cwd=wl.path, check=False)
        if keep:
            log(f"kept workdir {workdir}")
        else:
            rmtree_force(workdir)
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
