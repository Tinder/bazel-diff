#!/usr/bin/env python3
"""Nightly harness for `bazel-diff serve --partialGraph`: is it faster, and is it exactly as accurate?

Partial-graph mode derives a revision's hashes from the cached `from` entry -- no checkout, no
`bazel query` -- whenever the diff between the two only edits the content of regular source files,
and falls back to a full generation otherwise. The mode is opt-in until this harness has shown,
night after night, that

  * accuracy: for every revision pair the impacted set (and the distance-annotated set) is the
    same as the default mode's, and a derived cache entry is byte-identical to the entry a full
    generation in the same mode writes for that revision;
  * correctness of the classifier: the scenarios that must derive do, and the ones that must not
    (a BUILD or MODULE edit, an added or deleted path) fall back to a full generation;
  * speed: how long the `to` retrieval and the whole request take, derived versus the default
    mode's full generation, cold (fresh Bazel server) and warm.

Three `serve` instances run one after another on the same workspace, so they share one Bazel
server and the comparison is server-state for server-state:

  baseline   the default mode, `--trackDeps`                    -> ground truth and today's cost
  partial    `--partialGraph --trackDeps`                      -> the derivation under test
  verify     `--partialGraph --trackDeps`, a fresh cache, every revision generated from scratch
                                                                -> the entry a derivation must equal

Synthetic workloads come from tools/firecracker/bench/gen_project.py (the Tango-comparison presets).
Per workload the scenarios below are committed on top of the base revision, once per iteration with
iteration-specific content so no two commits share a tree (the serve cache is keyed by tree), plus
one extra commit per scenario that the baseline answers against a freshly restarted Bazel server:

  leaf_edit       edit one source file deep in the graph            derive
  wide_edit       edit the layer-0 source with the most dependents  derive
  multi_edit      edit ten sources spread across the packages       derive
  exec_bit        flip a source's executable bit (and edit it)      derive
  unmatched_edit  edit a tracked file no target reads               derive, empty impacted set
  chain           a second leaf edit on top of leaf_edit            derive from a derived entry
  build_edit      touch a BUILD.bazel (plus a leaf edit)            full generation
  module_edit     touch MODULE.bazel                                full generation
  add_file        add an unreferenced file                          full generation
  delete_file     delete the unreferenced tracked file (+ leaf)  full generation

A real repository (--repo-url) is replayed instead along its first-parent history (--repo-commits
consecutive pairs ending at --repo-to): there is no expected mode, so the report shows how often
real commits derive and that every pair still matches.

Usage:
    tools/perf/partial_graph_bench.py --bazel ~/go/bin/bazelisk                  # build, then run
    tools/perf/partial_graph_bench.py --quick --skip-build --bazel-diff bazel-bin/src/bazel-diff
    tools/perf/partial_graph_bench.py --workload medium --iterations 3 --metrics-out m.json --summary-out s.md
    tools/perf/partial_graph_bench.py --repo-url https://github.com/bazelbuild/bazel-skylib.git \\
        --repo-to HEAD --repo-commits 10 --workload skylib

Exit status is non-zero when any run fails and, unless --no-require-match, when any accuracy or
classifier check fails. Not a py_binary (it builds bazel-diff with `bazel build`, which would
deadlock under `bazel run`). Requires git and a bazel/bazelisk binary. Python stdlib only.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import platform
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterator

sys.path.insert(0, str(Path(__file__).resolve().parent))
import tango_compare as tc  # noqa: E402  (shared helpers: run, timed, git, presets, workloads)

REPO_ROOT = tc.REPO_ROOT
LAUNCHER = tc.LAUNCHER

# Scenario name -> the mode the partial server must report for it ("derived" | "full").
SCENARIOS: dict[str, str] = {
    "leaf_edit": "derived",
    "wide_edit": "derived",
    "multi_edit": "derived",
    "exec_bit": "derived",
    "unmatched_edit": "derived",
    "chain": "derived",
    "build_edit": "full",
    "module_edit": "full",
    "add_file": "full",
    "delete_file": "full",
}
# A tracked file no target reads; committed into the base revision by this harness.
UNMATCHED_FILE = "docs/NOTES.md"
MULTI_EDIT_FILES = 10
SERVE_READY_TIMEOUT_S = 120.0
REQUEST_TIMEOUT_S = 3600.0

VERBOSE = False


def log(msg: str) -> None:
    print(msg, flush=True)


def vlog(msg: str) -> None:
    if VERBOSE:
        print(f"  {msg}", flush=True)


# ----------------------------------------------------------------------------------------------
# Pure helpers (unit-tested in partial_graph_bench_test.py)
# ----------------------------------------------------------------------------------------------


@dataclass
class Edit:
    """One file operation of a scenario: `append` text, `chmod` executable, `add` a file, `delete`."""

    op: str
    path: str
    text: str = ""


def plan_edits(scenario: str, meta: dict[str, Any], iteration: int, tag: str) -> list[Edit]:
    """The file operations a scenario commit makes, from the workload's gen_project metadata.

    `tag` distinguishes the commits of one scenario (iteration and cold/warm leg), so each has a
    distinct tree. `chain` edits a *different* deep file than `leaf_edit` so that it changes
    something relative to the leaf_edit commit it builds on.
    """
    deep = meta["deep_source"]
    wide = meta["wide_source"]
    packages = int(meta["packages"])
    stamp = f"{scenario} {tag} iteration {iteration}\n"
    if scenario == "leaf_edit":
        return [Edit("append", deep, stamp)]
    if scenario == "wide_edit":
        return [Edit("append", wide, stamp)]
    if scenario == "multi_edit":
        step = max(1, packages // MULTI_EDIT_FILES)
        return [Edit("append", f"pkg/p{i:05d}/src_1.txt", stamp) for i in range(0, packages, step)][:MULTI_EDIT_FILES]
    if scenario == "exec_bit":
        return [Edit("chmod", deep), Edit("append", deep, stamp)]
    if scenario == "unmatched_edit":
        return [Edit("append", UNMATCHED_FILE, stamp)]
    if scenario == "chain":
        sibling = str(Path(deep).with_name("src_2.txt"))
        return [Edit("append", sibling, stamp)]
    if scenario == "build_edit":
        return [Edit("append", str(Path(deep).with_name("BUILD.bazel")), f"# {stamp}"), Edit("append", deep, stamp)]
    if scenario == "module_edit":
        return [Edit("append", "MODULE.bazel", f"# {stamp}")]
    if scenario == "add_file":
        return [Edit("add", str(Path(deep).with_name(f"extra_{tag}_{iteration}.txt")), stamp)]
    if scenario == "delete_file":
        # Plus a leaf edit, so the cold and warm legs do not delete their way to the same tree.
        return [Edit("delete", UNMATCHED_FILE), Edit("append", deep, stamp)]
    raise ValueError(f"unknown scenario {scenario}")


def retrieval(profile: dict[str, Any], index: int) -> dict[str, Any]:
    """`profile.hashRetrievals[index]` of a serve response, or `{}`."""
    try:
        return profile["hashRetrievals"][index]
    except (KeyError, IndexError, TypeError):
        return {}


def observed_mode(to_retrieval: dict[str, Any]) -> str:
    """How a server produced the `to` entry: `derived`, `full`, `cached`, or `default` (no mode field)."""
    if to_retrieval.get("cacheHit"):
        return "cached"
    generation = to_retrieval.get("generation") or {}
    return generation.get("mode") or "default"


def label_set(body: dict[str, Any]) -> set[str]:
    """Impacted labels of an `/impacted_targets` body; a distance-annotated body yields `label@t/p`."""
    out: set[str] = set()
    for item in body.get("impactedTargets", []):
        if isinstance(item, str):
            out.add(item)
        else:
            out.add(f"{item['label']}@{item.get('targetDistance')}/{item.get('packageDistance')}")
    return out


def compare_sets(expected: set[str], actual: set[str]) -> dict[str, Any]:
    missing = sorted(expected - actual)
    extra = sorted(actual - expected)
    return {"match": not missing and not extra, "missing": missing[:50], "extra": extra[:50],
            "missing_count": len(missing), "extra_count": len(extra)}


def check_pair(pair: dict[str, Any]) -> list[str]:
    """The accuracy and classifier failures of one measured pair, as human-readable strings."""
    failures: list[str] = []
    name = f"{pair['scenario']}#{pair['iteration']}{'/' + pair['leg'] if pair.get('leg') else ''}"
    baseline, partial, verify = pair.get("baseline", {}), pair.get("partial", {}), pair.get("verify", {})
    for side, result in (("baseline", baseline), ("partial", partial), ("verify", verify)):
        if result.get("error"):
            failures.append(f"{name}: {side} failed: {result['error']}")
    if failures:
        return failures
    expected = pair.get("expected_mode")
    if expected and partial.get("mode") != expected:
        failures.append(
            f"{name}: expected a {expected} generation, partial server reported {partial.get('mode')}"
            + (f" ({partial['fallback_reason']})" if partial.get("fallback_reason") else "")
        )
    if pair.get("impacted") and not pair["impacted"]["match"]:
        failures.append(
            f"{name}: impacted sets differ (missing {pair['impacted']['missing_count']}, "
            f"extra {pair['impacted']['extra_count']}): missing {pair['impacted']['missing'][:5]} "
            f"extra {pair['impacted']['extra'][:5]}"
        )
    if pair.get("distances") and not pair["distances"]["match"]:
        failures.append(
            f"{name}: distance-annotated sets differ (missing {pair['distances']['missing_count']}, "
            f"extra {pair['distances']['extra_count']})"
        )
    if verify and verify.get("entry_identical") is False:
        failures.append(f"{name}: derived cache entry differs from a full generation's ({verify.get('entry_detail', '')})")
    if pair["scenario"] == "unmatched_edit" and baseline.get("impacted_count", 0) != 0:
        failures.append(f"{name}: an edit outside the graph impacted {baseline['impacted_count']} targets")
    return failures


def summarize_workload(wl: dict[str, Any]) -> dict[str, Any]:
    """Per-scenario medians over the pairs of one workload."""
    by_scenario: dict[str, list[dict[str, Any]]] = {}
    for pair in wl["pairs"]:
        by_scenario.setdefault(pair["scenario"], []).append(pair)
    rows = []
    for scenario, pairs in by_scenario.items():
        def med(side: str, key: str, leg: str | None = None) -> float | None:
            vals = [p[side][key] for p in pairs if p.get(side, {}).get(key) is not None
                    and (leg is None or p.get("leg") == leg)]
            return round(tc.median(vals), 3) if vals else None

        modes = sorted({p.get("partial", {}).get("mode", "?") for p in pairs})
        rows.append({
            "scenario": scenario,
            "expected_mode": pairs[0].get("expected_mode"),
            "observed_modes": modes,
            "pairs": len(pairs),
            "baseline_cold_s": med("baseline", "to_s", "cold"),
            "baseline_warm_s": med("baseline", "to_s", "warm"),
            "baseline_total_warm_s": med("baseline", "total_s", "warm"),
            "partial_to_s": med("partial", "to_s"),
            "partial_total_s": med("partial", "total_s", "warm"),
            "verify_full_s": med("verify", "to_s"),
            "impacted_match": all(p.get("impacted", {}).get("match", True) for p in pairs),
            "entry_identical": all(p.get("verify", {}).get("entry_identical", True) for p in pairs),
            "failures": sum(len(check_pair(p)) for p in pairs),
        })
    derived = sum(1 for p in wl["pairs"] if p.get("partial", {}).get("mode") == "derived")
    return {"rows": rows, "derived": derived, "pairs": len(wl["pairs"])}


def render_summary(report: dict[str, Any]) -> str:
    """GitHub-flavored markdown for a CI step summary."""
    env = report.get("environment", {})
    out: list[str] = []
    out.append("## serve --partialGraph: speed and accuracy against the default mode")
    out.append("")
    out.append(
        f"bazel-diff `{env.get('bazel_diff_version', '?')}` (repo `{str(env.get('repo_sha', '?'))[:12]}`), "
        f"Bazel {env.get('bazel_version', '?')}, {env.get('cpu_count', '?')} CPUs, "
        f"{env.get('mem_total_gib', '?')} GiB, {env.get('platform', '?')}. "
        f"Iterations: {report.get('iterations', '?')}."
    )
    out.append("")
    out.append(
        "`baseline` = default mode (`--trackDeps`), the `to` revision's generation, cold (Bazel server "
        "restarted) and warm; `partial` = `--partialGraph`, the `to` retrieval (derived, or a full "
        "generation when the classifier fell back); `verify` = a full generation of the same revision "
        "under `--partialGraph` in a fresh cache, whose cache entry the partial server's must equal "
        "byte for byte. Medians; the partial request column is warm legs only; speedup is baseline warm / "
        "partial `to`."
    )
    out.append("")
    for wl in report.get("workloads", []):
        summary = wl["summary"]
        targets = wl.get("meta", {}).get("rule_targets")
        out.append(f"### {wl['name']}" + (f" ({targets} rule targets)" if targets else ""))
        out.append("")
        out.append(f"Derived {summary['derived']} of {summary['pairs']} revision pairs.")
        out.append("")
        out.append("| Scenario | Expected | Observed | Baseline cold | Baseline warm | Partial `to` | Partial request | Verify full | Speedup | Impacted match | Entry identical |")
        out.append("|---|---|---|---:|---:|---:|---:|---:|---:|---|---|")
        for row in summary["rows"]:
            out.append(
                f"| {row['scenario']} | {row['expected_mode'] or '-'} | {', '.join(row['observed_modes'])} | "
                f"{tc.fmt_s(row['baseline_cold_s'])} | {tc.fmt_s(row['baseline_warm_s'])} | "
                f"{tc.fmt_s(row['partial_to_s'])} | {tc.fmt_s(row['partial_total_s'])} | "
                f"{tc.fmt_s(row['verify_full_s'])} | {tc.fmt_ratio(row['baseline_warm_s'], row['partial_to_s'])} | "
                f"{'yes' if row['impacted_match'] else 'NO'} | {'yes' if row['entry_identical'] else 'NO'} |"
            )
        out.append("")
        if wl.get("aborted"):
            out.append(f"**Aborted:** {wl['aborted']}")
            out.append("")
    failures = report.get("failures", [])
    if failures:
        out.append(f"### Failures ({len(failures)})")
        out.append("")
        for failure in failures[:100]:
            out.append(f"- {failure}")
        out.append("")
    else:
        out.append("No accuracy or classifier failures.")
        out.append("")
    return "\n".join(out)


# ----------------------------------------------------------------------------------------------
# Serve process
# ----------------------------------------------------------------------------------------------


def decode_body(raw: bytes) -> dict[str, Any]:
    """A JSON body as a dict; anything else (`/health` answers plain text) under `text`."""
    text = raw.decode("utf-8", "replace")
    try:
        body = json.loads(text or "{}")
    except json.JSONDecodeError:
        return {"text": text}
    return body if isinstance(body, dict) else {"value": body}


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@dataclass
class Serve:
    proc: subprocess.Popen
    port: int
    cache: Path
    stderr_path: Path

    def get(self, path: str, timeout: float = REQUEST_TIMEOUT_S) -> tuple[int, dict[str, Any]]:
        request = urllib.request.Request(f"http://127.0.0.1:{self.port}{path}")
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                return response.status, decode_body(response.read())
        except urllib.error.HTTPError as error:
            return error.code, decode_body(error.read())

    def stderr_tail(self, n: int = 30) -> str:
        try:
            return "\n".join(self.stderr_path.read_text("utf-8", "replace").splitlines()[-n:])
        except OSError:
            return ""


@contextlib.contextmanager
def serve(bazel_diff: Path, bazel: str, workspace: Path, cache: Path, name: str, extra: list[str]) -> Iterator[Serve]:
    cache.mkdir(parents=True, exist_ok=True)
    port = free_port()
    stderr_path = cache.parent / f"serve.{name}.stderr.log"
    args = [str(bazel_diff), "serve", "-w", str(workspace), "-b", bazel, "--cacheDir", str(cache),
            "--port", str(port), "--no-initial-fetch", "--trackDeps", *extra]
    vlog(f"$ {' '.join(args)}")
    with open(stderr_path, "w") as errf:
        proc = subprocess.Popen(args, cwd=str(workspace), stdout=subprocess.DEVNULL, stderr=errf)
    server = Serve(proc=proc, port=port, cache=cache, stderr_path=stderr_path)
    try:
        deadline = time.monotonic() + SERVE_READY_TIMEOUT_S
        while True:
            if proc.poll() is not None:
                raise RuntimeError(f"serve ({name}) exited with {proc.returncode}:\n{server.stderr_tail()}")
            try:
                status, _ = server.get("/health", timeout=3)
            except (urllib.error.URLError, OSError, socket.timeout):
                status = 0
            if status == 200:
                break
            if time.monotonic() > deadline:
                raise RuntimeError(f"serve ({name}) not healthy after {SERVE_READY_TIMEOUT_S:.0f}s:\n{server.stderr_tail()}")
            time.sleep(0.3)
        yield server
    finally:
        with contextlib.suppress(Exception):
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()


def query(server: Serve, from_sha: str, to_sha: str, endpoint: str = "impacted_targets") -> dict[str, Any]:
    """One profiled query; returns the measured `to` retrieval plus the impacted set."""
    path = f"/{endpoint}?from={from_sha}&to={to_sha}&profile=true"
    elapsed, (status, body) = tc.timed(lambda: server.get(path))
    if status != 200:
        raise RuntimeError(f"{path} -> {status}: {json.dumps(body)[:500]}\n{server.stderr_tail()}")
    profile = body.get("profile", {})
    to_retrieval = retrieval(profile, 1)
    generation = to_retrieval.get("generation") or {}
    return {
        "status": status,
        "client_s": round(elapsed, 3),
        "total_s": round(profile.get("totalDurationMillis", 0) / 1000, 3),
        "to_s": round(to_retrieval.get("durationMillis", 0) / 1000, 3),
        "from_cache_hit": retrieval(profile, 0).get("cacheHit"),
        "mode": observed_mode(to_retrieval),
        "fallback_reason": generation.get("partialFallbackReason"),
        "bazel_query_s": round(generation.get("bazelQueryMillis", 0) / 1000, 3),
        "classify_ms": generation.get("classifyMillis"),
        "derive_ms": generation.get("deriveMillis"),
        "changed_sources": generation.get("changedSources"),
        "unmatched_paths": generation.get("unmatchedPaths"),
        "target_count": generation.get("targetCount"),
        "impacted_count": len(body.get("impactedTargets", [])),
        "labels": label_set(body),
    }


def cache_entry(cache: Path, tree: str) -> Path | None:
    matches = sorted(p for p in cache.glob(f"{tree}.*.json"))
    return matches[0] if matches else None


def compare_entries(partial_cache: Path, verify_cache: Path, tree: str) -> tuple[bool | None, str]:
    """Byte-compares the two servers' entries for one tree. `None` when either is missing."""
    left, right = cache_entry(partial_cache, tree), cache_entry(verify_cache, tree)
    if left is None or right is None:
        return None, f"entry missing: partial={left} verify={right}"
    left_bytes, right_bytes = left.read_bytes(), right.read_bytes()
    if left_bytes == right_bytes:
        return True, ""
    try:
        left_json, right_json = json.loads(left_bytes), json.loads(right_bytes)
        left_hashes, right_hashes = left_json.get("hashes", left_json), right_json.get("hashes", right_json)
        differing = sorted(k for k in set(left_hashes) | set(right_hashes) if left_hashes.get(k) != right_hashes.get(k))
        meta_differs = left_json.get("metadata") != right_json.get("metadata")
        return False, f"{len(differing)} hashes differ (e.g. {differing[:5]}), metadata differs: {meta_differs}"
    except (json.JSONDecodeError, AttributeError):
        return False, "entries are not comparable JSON"


# ----------------------------------------------------------------------------------------------
# Workloads and scenario commits
# ----------------------------------------------------------------------------------------------


@dataclass
class Pair:
    scenario: str
    iteration: int
    leg: str  # "cold" | "warm"
    from_sha: str
    to_sha: str
    expected_mode: str | None
    edits: list[str] = field(default_factory=list)


def apply_edit(workspace: Path, edit: Edit) -> None:
    path = workspace / edit.path
    if edit.op == "append":
        with path.open("a") as f:
            f.write(edit.text)
    elif edit.op == "chmod":
        path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    elif edit.op == "add":
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(edit.text)
    elif edit.op == "delete":
        path.unlink()
    else:
        raise ValueError(edit.op)


def commit_scenario(workspace: Path, base: str, scenario: str, meta: dict[str, Any], iteration: int, tag: str) -> tuple[str, list[str]]:
    tc.git(workspace, "checkout", "-q", "--force", base)
    edits = plan_edits(scenario, meta, iteration, tag)
    for edit in edits:
        apply_edit(workspace, edit)
    tc.git(workspace, "add", "-A")
    tc.git(workspace, "-c", "user.name=partial-graph-bench", "-c", "user.email=bench@bazel-diff.local",
           "commit", "-q", "-m", f"{scenario} {tag} {iteration}")
    return tc.git(workspace, "rev-parse", "HEAD"), [e.path for e in edits]


def synthetic_pairs(workspace: Path, base: str, meta: dict[str, Any], iterations: int, scenarios: list[str]) -> list[Pair]:
    """Commits every scenario for a cold leg plus `iterations` warm legs; `chain` builds on leaf_edit."""
    pairs: list[Pair] = []
    legs = [("cold", 0)] + [("warm", i) for i in range(1, iterations + 1)]
    leaf_commits: dict[tuple[str, int], str] = {}
    for scenario in scenarios:
        for leg, iteration in legs:
            if scenario == "chain":
                start = leaf_commits.get((leg, iteration))
                if start is None:
                    start, _ = commit_scenario(workspace, base, "leaf_edit", meta, iteration, f"{leg}-for-chain")
            else:
                start = base
            sha, edited = commit_scenario(workspace, start, scenario, meta, iteration, leg)
            if scenario == "leaf_edit":
                leaf_commits[(leg, iteration)] = sha
            pairs.append(Pair(scenario, iteration, leg, start, sha, SCENARIOS[scenario], edited))
    tc.git(workspace, "checkout", "-q", "--force", base)
    return pairs


def repo_pairs(workspace: Path, to_rev: str, count: int) -> list[Pair]:
    """`count` consecutive first-parent pairs ending at `to_rev`, oldest first."""
    shas = tc.git(workspace, "rev-list", "--first-parent", f"--max-count={count + 1}", to_rev + "^{commit}").split()
    shas.reverse()
    if len(shas) < 2:
        raise RuntimeError(f"need at least two commits before {to_rev}, found {len(shas)}")
    return [Pair("history", i, "warm", shas[i - 1], shas[i], None) for i in range(1, len(shas))]


def prepare_synthetic(name: str, params: dict[str, int], workdir: Path, bazel_version: str) -> tuple[Path, str, dict[str, Any]]:
    wl = tc.make_synthetic_workload(name, params, workdir, bazel_version)
    # The base revision gets a tracked file no target reads, for the unmatched_edit / delete_file
    # scenarios; its commit becomes the base every scenario builds on.
    tc.git(wl.path, "checkout", "-q", "--force", wl.base_sha)
    notes = wl.path / UNMATCHED_FILE
    notes.parent.mkdir(parents=True, exist_ok=True)
    notes.write_text("Notes nobody builds.\n")
    tc.git(wl.path, "add", "-A")
    tc.git(wl.path, "-c", "user.name=partial-graph-bench", "-c", "user.email=bench@bazel-diff.local",
           "commit", "-q", "-m", "base with notes")
    base = tc.git(wl.path, "rev-parse", "HEAD")
    return wl.path, base, wl.meta


# ----------------------------------------------------------------------------------------------
# Benchmark driver
# ----------------------------------------------------------------------------------------------


def bench_workload(name: str, workspace: Path, pairs: list[Pair], meta: dict[str, Any], *, bazel_diff: Path,
                   bazel: str, workdir: Path, serve_args: list[str], verify: bool) -> dict[str, Any]:
    results: dict[str, dict[str, Any]] = {f"{p.scenario}/{p.leg}/{p.iteration}": {
        "scenario": p.scenario, "iteration": p.iteration, "leg": p.leg, "from": p.from_sha, "to": p.to_sha,
        "expected_mode": p.expected_mode, "edits": p.edits[:20],
    } for p in pairs}
    aborted = None
    caches = workdir / "caches" / name
    trees = {p.to_sha: tc.git(workspace, "rev-parse", f"{p.to_sha}^{{tree}}") for p in pairs}
    # Base revisions no pair produces are warmed untimed; a base that is itself a pair's `to`
    # (the leaf_edit commit `chain` builds on) is left to that pair, so on the partial server the
    # chain derives from a derived entry.
    warm_shas = sorted({p.from_sha for p in pairs} - {p.to_sha for p in pairs})

    def run_side(side: str, extra: list[str], fresh_cache: bool, each) -> None:
        nonlocal aborted
        cache = caches / side
        if fresh_cache and cache.exists():
            shutil.rmtree(cache)
        try:
            with serve(bazel_diff, bazel, workspace, cache, f"{name}-{side}", [*extra, *serve_args]) as server:
                each(server)
        except Exception as error:  # noqa: BLE001 - a failed side is reported, not raised
            aborted = f"{side}: {error}"
            log(f"[{name}] {side} aborted: {error}")

    def baseline(server: Serve) -> None:
        # The measurement is the `to` generation, so the bases are warmed untimed.
        for sha in warm_shas:
            query(server, sha, sha)
        for pair in pairs:
            key = f"{pair.scenario}/{pair.leg}/{pair.iteration}"
            if pair.leg == "cold":
                tc.bazel_shutdown(bazel, workspace)
            try:
                result = query(server, pair.from_sha, pair.to_sha)
                distances = query(server, pair.from_sha, pair.to_sha, "impacted_targets_with_distances")
                result["distance_labels"] = distances["labels"]
                results[key]["baseline"] = result
                log(f"[{name}] baseline {key}: to={result['to_s']}s total={result['total_s']}s impacted={result['impacted_count']}")
            except Exception as error:  # noqa: BLE001
                results[key]["baseline"] = {"error": str(error)}
                log(f"[{name}] baseline {key} failed: {error}")

    def partial(server: Serve) -> None:
        for sha in warm_shas:
            query(server, sha, sha)
        for pair in pairs:
            key = f"{pair.scenario}/{pair.leg}/{pair.iteration}"
            if pair.leg == "cold":
                tc.bazel_shutdown(bazel, workspace)
            try:
                result = query(server, pair.from_sha, pair.to_sha)
                distances = query(server, pair.from_sha, pair.to_sha, "impacted_targets_with_distances")
                result["distance_labels"] = distances["labels"]
                results[key]["partial"] = result
                reason = f" ({result['fallback_reason']})" if result.get("fallback_reason") else ""
                log(f"[{name}] partial {key}: mode={result['mode']}{reason} to={result['to_s']}s total={result['total_s']}s impacted={result['impacted_count']}")
            except Exception as error:  # noqa: BLE001
                results[key]["partial"] = {"error": str(error)}
                log(f"[{name}] partial {key} failed: {error}")

    def verifier(server: Serve) -> None:
        done: dict[str, dict[str, Any]] = {}
        for pair in pairs:
            key = f"{pair.scenario}/{pair.leg}/{pair.iteration}"
            if pair.to_sha in done:
                results[key]["verify"] = dict(done[pair.to_sha])
                continue
            try:
                # from == to: the first retrieval is the full generation of this revision.
                path = f"/impacted_targets?from={pair.to_sha}&to={pair.to_sha}&profile=true"
                elapsed, (status, body) = tc.timed(lambda: server.get(path))
                if status != 200:
                    raise RuntimeError(f"{path} -> {status}: {json.dumps(body)[:300]}")
                first = retrieval(body.get("profile", {}), 0)
                identical, detail = compare_entries(caches / "partial", server.cache, trees[pair.to_sha])
                result = {
                    "to_s": round(first.get("durationMillis", 0) / 1000, 3),
                    "client_s": round(elapsed, 3),
                    "mode": observed_mode(first),
                    "entry_identical": identical,
                    "entry_detail": detail,
                }
                log(f"[{name}] verify {key}: full={result['to_s']}s identical={identical}{' ' + detail if detail else ''}")
            except Exception as error:  # noqa: BLE001
                result = {"error": str(error)}
                log(f"[{name}] verify {key} failed: {error}")
            done[pair.to_sha] = result
            results[key]["verify"] = dict(result)

    run_side("baseline", [], True, baseline)
    if aborted is None:
        run_side("partial", ["--partialGraph"], True, partial)
    if aborted is None and verify:
        run_side("verify", ["--partialGraph"], True, verifier)

    for key, pair in results.items():
        baseline_result, partial_result = pair.get("baseline", {}), pair.get("partial", {})
        if "labels" in baseline_result and "labels" in partial_result:
            pair["impacted"] = compare_sets(baseline_result["labels"], partial_result["labels"])
            pair["distances"] = compare_sets(baseline_result["distance_labels"], partial_result["distance_labels"])
        for side in ("baseline", "partial"):
            pair.get(side, {}).pop("labels", None)
            pair.get(side, {}).pop("distance_labels", None)
    tc.bazel_shutdown(bazel, workspace)
    report = {"name": name, "path": str(workspace), "meta": meta, "aborted": aborted, "pairs": list(results.values())}
    report["summary"] = summarize_workload(report)
    return report


def collect_environment(bazel: str, bazel_diff: Path, workspace: Path | None) -> dict[str, Any]:
    env: dict[str, Any] = {
        "platform": platform.platform(),
        "python": platform.python_version(),
        "cpu_count": os.cpu_count(),
        "bazel_path": bazel,
        "bazel_diff_path": str(bazel_diff),
    }
    try:
        meminfo = Path("/proc/meminfo").read_text()
        kib = int(next(line for line in meminfo.splitlines() if line.startswith("MemTotal:")).split()[1])
        env["mem_total_gib"] = round(kib / 1024 / 1024, 1)
    except (OSError, StopIteration, ValueError, IndexError):
        env["mem_total_gib"] = None
    try:
        env["repo_sha"] = tc.git(REPO_ROOT, "rev-parse", "HEAD")
    except RuntimeError:
        env["repo_sha"] = "unknown"
    proc = tc.run([bazel_diff, "--version"], check=False)
    env["bazel_diff_version"] = proc.stdout.strip().splitlines()[-1] if proc.stdout.strip() else "unknown"
    if workspace is not None:
        proc = tc.run([bazel, "--version"], cwd=workspace, check=False)
        env["bazel_version"] = proc.stdout.strip().splitlines()[-1].replace("bazel ", "") if proc.stdout.strip() else "unknown"
    return env


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bazel", default="bazel", help="bazel/bazelisk binary the servers run (default: bazel)")
    ap.add_argument("--bazel-diff", type=Path, default=None, help=f"bazel-diff binary (default: {LAUNCHER})")
    ap.add_argument("--skip-build", action="store_true", help="do not `bazel build` bazel-diff; use --bazel-diff / the existing launcher")
    ap.add_argument("--workload", action="append", default=[], metavar="NAME",
                    help=f"synthetic preset ({', '.join(tc.SYNTHETIC_PRESETS)}), repeatable; or the name for a --repo-url workload")
    ap.add_argument("--synthetic", action="append", default=[], type=tc.parse_synthetic_spec, metavar="NAME:PKGS:TPP[:LAYERS]",
                    help="custom synthetic workload, repeatable")
    ap.add_argument("--scenario", action="append", default=[], choices=sorted(SCENARIOS), help="run only these scenarios (repeatable)")
    ap.add_argument("--repo-url", default=None, help="replay a real repository's history instead of synthetic scenarios")
    ap.add_argument("--repo-to", default="HEAD", help="newest revision of the replayed history (default: HEAD)")
    ap.add_argument("--repo-commits", type=int, default=5, help="consecutive first-parent pairs to replay (default: 5)")
    ap.add_argument("--repo-clone-args", default="", help="extra `git clone` args for --repo-url, e.g. --depth=50")
    ap.add_argument("--bazel-version", default=None, help="Bazel version pinned in synthetic workspaces (default: this repo's .bazelversion)")
    ap.add_argument("--iterations", type=int, default=2, help="warm legs per scenario, besides the one cold leg (default: 2)")
    ap.add_argument("--serve-arg", action="append", default=[], metavar="ARG", help="extra flag for every serve instance (repeatable)")
    ap.add_argument("--no-verify", action="store_true", help="skip the verify server (no entry-identity check)")
    ap.add_argument("--require-match", dest="require_match", action="store_true", default=True,
                    help="fail on any accuracy or classifier failure (default)")
    ap.add_argument("--no-require-match", dest="require_match", action="store_false", help="report failures without failing the run")
    ap.add_argument("--quick", action="store_true", help="smoke profile: tiny workload, one warm iteration")
    ap.add_argument("--workdir", type=Path, default=None, help="scratch directory (default: a temp dir)")
    ap.add_argument("--keep-workdir", action="store_true", help="keep the scratch directory")
    ap.add_argument("--metrics-out", type=Path, default=None, help="write the JSON report here")
    ap.add_argument("--summary-out", type=Path, default=None, help="write the markdown summary here")
    ap.add_argument("-v", "--verbose", action="store_true")
    return ap


def main(argv: list[str]) -> int:
    global VERBOSE
    args = build_parser().parse_args(argv)
    VERBOSE = tc.VERBOSE = args.verbose
    if args.quick:
        if not args.workload and not args.synthetic and not args.repo_url:
            args.workload = ["tiny"]
        args.iterations = 1
    if not args.workload and not args.synthetic and not args.repo_url:
        args.workload = ["small", "medium"]
    if args.iterations < 1:
        log("--iterations must be >= 1")
        return 2
    scenarios = args.scenario or list(SCENARIOS)

    bazel_diff = args.bazel_diff or LAUNCHER
    if not args.skip_build and args.bazel_diff is None:
        tc.build_bazel_diff(args.bazel)
    if not Path(bazel_diff).exists():
        log(f"bazel-diff binary not found at {bazel_diff}; pass --bazel-diff or drop --skip-build")
        return 2
    bazel_diff = Path(bazel_diff).resolve()
    bazel = shutil.which(args.bazel) or str(Path(args.bazel).expanduser().resolve())
    if not Path(bazel).exists():
        log(f"bazel binary not found: {args.bazel}")
        return 2
    bazel_version = args.bazel_version or (REPO_ROOT / ".bazelversion").read_text().strip()

    workdir = args.workdir or Path(tempfile.mkdtemp(prefix="partial-graph-bench-"))
    workdir.mkdir(parents=True, exist_ok=True)
    log(f"workdir: {workdir}")
    report: dict[str, Any] = {"iterations": args.iterations, "scenarios": scenarios, "workloads": [], "failures": []}
    exit_code = 0
    try:
        plans: list[tuple[str, Path, str, dict[str, Any], list[Pair]]] = []
        if args.repo_url:
            name = args.workload[0] if args.workload else "repo"
            wl = tc.make_repo_workload(name, args.repo_url, args.repo_to, args.repo_to, args.repo_clone_args.split(), workdir)
            pairs = repo_pairs(wl.path, args.repo_to, args.repo_commits)
            plans.append((name, wl.path, pairs[0].from_sha, wl.meta, pairs))
        else:
            specs = [(w, tc.SYNTHETIC_PRESETS[w]) for w in args.workload] + list(args.synthetic)
            for name, params in specs:
                path, base, meta = prepare_synthetic(name, params, workdir, bazel_version)
                plans.append((name, path, base, meta, synthetic_pairs(path, base, meta, args.iterations, scenarios)))
        report["environment"] = collect_environment(bazel, bazel_diff, plans[0][1] if plans else None)
        for name, path, _base, meta, pairs in plans:
            log(f"[{name}] {len(pairs)} revision pairs")
            wl_report = bench_workload(name, path, pairs, meta, bazel_diff=bazel_diff, bazel=bazel, workdir=workdir,
                                       serve_args=args.serve_arg, verify=not args.no_verify)
            report["workloads"].append(wl_report)
            if wl_report["aborted"]:
                report["failures"].append(f"{name}: {wl_report['aborted']}")
                exit_code = 1
                continue
            for pair in wl_report["pairs"]:
                report["failures"].extend(f"{name}: {f}" for f in check_pair(pair))
    except Exception as error:  # noqa: BLE001
        log(f"harness failed: {error}")
        report["failures"].append(f"harness: {error}")
        exit_code = 1
    finally:
        summary = render_summary(report)
        log("")
        log(summary)
        if args.metrics_out:
            args.metrics_out.write_text(json.dumps(report, indent=2, default=str) + "\n")
        if args.summary_out:
            args.summary_out.write_text(summary + "\n")
        if not args.keep_workdir and args.workdir is None:
            shutil.rmtree(workdir, ignore_errors=True)
    if report["failures"] and args.require_match:
        exit_code = 1
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
