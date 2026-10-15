#!/usr/bin/env python3
"""Performance harness comparing bazel-diff against Uber's Tango on identical workloads.

Both tools answer the same question -- "which Bazel targets are impacted between two git
revisions?" -- with the same recipe: check out each revision, run `bazel query` over the whole
graph, hash every target, and diff the two hash maps. This harness runs both tools on the same
workspace and commit pair, times each end-to-end, and cross-checks that the impacted sets agree.

What is measured, per workload and iteration (the Bazel server state is what distinguishes the
phases; both tools run against the same bazelisk and the workload's own .bazelversion):

  cold    `bazel shutdown` is issued to both tools' Bazel servers first, so each pays a fresh
          server start plus a full graph load. This is what a CI job pays on every PR today.
  warm    Run again immediately, servers alive: incremental re-analysis after the checkouts.
          Tango is asked to `-bypass-cache` so it recomputes both graphs instead of serving the
          result it cached during the cold run.
  cached  Tango with its result cache enabled (a treehash lookup, no bazel) next to bazel-diff's
          `get-impacted-targets` over the hash files already on disk (a diff, no bazel). Neither
          number involves Bazel; they are reported as informational "diff only" rows.

bazel-diff's flow is `git checkout BASE; generate-hashes; git checkout HEAD; generate-hashes
--depEdgesFile; get-impacted-targets` (the README's recipe, run in-process by this harness, each
stage timed). Tango's flow is one `get-changed-targets` RPC against its example server, which
leases a worker clone, checks out and queries each revision, hashes, and compares -- timed as the
client sees it, with the server's own `total_duration` pulled from its log when available. Each
flow includes its own checkouts, so the totals are comparable. Tool order alternates every
iteration so neither tool systematically benefits from the other having just warmed the page cache.

Tango is built from source (`go build`) at a pinned commit so a Tango change never silently moves
the baseline; pass --tango-ref to compare against another commit or branch, or --tango-dir to reuse
a checkout. Tango's example server hard-codes 127.0.0.1:8081 and reads example/tango-config.yaml
from its working directory, so the harness runs it from a scratch directory it writes that file
into; the port must be free.

Workloads are the synthetic genrule workspaces from tools/firecracker/bench/gen_project.py
(hermetic, size-controlled, no external toolchains), selected by preset name (--workload tiny|small|
medium|large) or spelled out (--synthetic name:packages:targets_per_package[:layers]). A real
repository can be used instead (--repo-url with --repo-from/--repo-to), in which case the impacted
set is whatever the two tools say about those two real commits.

Usage:
    tools/perf/tango_compare.py --bazel ~/go/bin/bazelisk                  # build bazel-diff, then run
    tools/perf/tango_compare.py --bazel-diff bazel-bin/src/bazel-diff --skip-build --workload small
    tools/perf/tango_compare.py --quick                                     # tiny workload, 1 iteration
    tools/perf/tango_compare.py --workload medium --iterations 5 --metrics-out m.json --summary-out s.md
    tools/perf/tango_compare.py --synthetic huge:20000:5:40 --iterations 2
    tools/perf/tango_compare.py --repo-url https://github.com/bazelbuild/bazel-skylib.git \
        --repo-from HEAD~1 --repo-to HEAD --workload skylib

With no workload flags the four nightly presets run (small, medium, large, xlarge), 3 iterations each.

Exit status is non-zero when either tool fails on any run, and additionally (with --require-match)
when the two tools disagree on an impacted set. Requires git, go (1.25+, or GOTOOLCHAIN=auto so the
go command fetches it), and a bazel/bazelisk binary. Python stdlib only.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import platform
import re
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
GEN_PROJECT = REPO_ROOT / "tools" / "firecracker" / "bench" / "gen_project.py"
# Overridable (BAZEL_DIFF_LAUNCHER) like the serve harnesses, so a downloaded release asset can be
# measured without building.
LAUNCHER = Path(os.environ.get("BAZEL_DIFF_LAUNCHER", str(REPO_ROOT / "bazel-bin" / "src" / "bazel-diff")))
BUILD_TARGET = "//src:bazel-diff"

TANGO_REPO_URL = "https://github.com/uber/tango.git"
# uber/tango main as of 2026-10-09 ("fix(tgb): drop per-column zstd expansion cap (#332)"). Bump
# deliberately: a Tango change moves the baseline, and the nightly trend should say which side moved.
TANGO_PINNED_REF = "41480a27e50d3a86dc25cd4d773bbe1b60474cdb"
# Hard-coded in tango's example/main.go; the example server takes no address flag.
TANGO_PORT = 8081
TANGO_SERVER_STARTUP_TIMEOUT_S = 60.0

# packages x targets_per_package genrules (+1 filegroup per package, +sources_per_package source
# files). `tiny` is for smoke tests and the unit-test fixtures; the other four are the nightly legs.
SYNTHETIC_PRESETS: dict[str, dict[str, int]] = {
    "tiny": {"packages": 60, "targets_per_package": 3, "layers": 6},
    "small": {"packages": 400, "targets_per_package": 4, "layers": 20},
    "medium": {"packages": 1500, "targets_per_package": 4, "layers": 40},
    "large": {"packages": 4000, "targets_per_package": 4, "layers": 40},
    "xlarge": {"packages": 10000, "targets_per_package": 4, "layers": 40},
}

PHASES = ("cold", "warm")

VERBOSE = False


def log(msg: str) -> None:
    print(msg, flush=True)


def vlog(msg: str) -> None:
    if VERBOSE:
        print(f"  | {msg}", flush=True)


# ----------------------------------------------------------------------------------------------
# Pure helpers (unit-tested in tango_compare_test.py; nothing here shells out)
# ----------------------------------------------------------------------------------------------

_GO_DURATION_UNITS = {
    "h": 3600.0,
    "m": 60.0,
    "s": 1.0,
    "ms": 1e-3,
    "us": 1e-6,
    "µs": 1e-6,  # U+00B5 MICRO SIGN, what Go's time.Duration.String emits
    "μs": 1e-6,  # U+03BC GREEK SMALL LETTER MU, in case a log was transcoded
    "ns": 1e-9,
}
_GO_DURATION_RE = re.compile(r"(\d+(?:\.\d+)?)(h|ms|m|s|us|µs|μs|ns)")


def parse_go_duration(text: str) -> float | None:
    """Seconds for a Go `time.Duration` string such as `11.265275041s`, `1m2.5s`, `110.849µs`.

    Returns None for anything that is not entirely made of Go duration components (so a stray
    log field can never be mistaken for a timing)."""
    s = text.strip()
    if not s:
        return None
    sign = 1.0
    if s[0] in "+-":
        sign = -1.0 if s[0] == "-" else 1.0
        s = s[1:]
    if s == "0":
        return 0.0
    pos = 0
    total = 0.0
    while pos < len(s):
        m = _GO_DURATION_RE.match(s, pos)
        if m is None:
            return None
        total += float(m.group(1)) * _GO_DURATION_UNITS[m.group(2)]
        pos = m.end()
    return sign * total


def parse_tango_client_output(stdout: str) -> dict[str, Any]:
    """Resolve the changed-target labels from the tango example client's stdout.

    The client prints one JSON object per streamed message: `{"changed_targets": [...]}` packets
    (targets referenced by int32 id; gogo's `omitempty` drops an id of 0 and a distance of 0) and
    `{"target_id_mapping": {"<id>": "<label>"}}` metadata chunks that may arrive in several pieces.
    Returns the merged labels plus per-change-type counts; an id the metadata never names is kept as
    `<id:N>` so a truncated stream shows up as a bogus label rather than a silently smaller set."""
    changed: list[dict[str, Any]] = []
    id_to_label: dict[int, str] = {}
    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        changed.extend(obj.get("changed_targets") or [])
        for k, v in (obj.get("target_id_mapping") or {}).items():
            id_to_label[int(k)] = v

    labels: list[str] = []
    distances: dict[str, int] = {}
    by_type = {"new": 0, "deleted": 0, "changed": 0, "unknown": 0}
    type_names = {1: "new", 2: "deleted", 3: "changed"}
    for ct in changed:
        target = ct.get("new_target") or ct.get("old_target") or {}
        tid = int(target.get("id", 0))
        label = id_to_label.get(tid, f"<id:{tid}>")
        labels.append(label)
        distances[label] = int(ct.get("distance", 0))
        by_type[type_names.get(int(ct.get("change_type", 0)), "unknown")] += 1
    return {"labels": sorted(set(labels)), "count": len(changed), "by_type": by_type, "distances": distances}


def parse_impacted_targets(text: str) -> list[str]:
    """Labels from bazel-diff's `get-impacted-targets -o` output.

    Without --depEdgesFile the file is a JSON list of labels; with it, a list of
    `{"label", "targetDistance", "packageDistance"}` objects."""
    data = json.loads(text) if text.strip() else []
    labels = []
    for item in data:
        labels.append(item["label"] if isinstance(item, dict) else str(item))
    return sorted(set(labels))


def compare_label_sets(bazel_diff: list[str], tango: list[str]) -> dict[str, Any]:
    a, b = set(bazel_diff), set(tango)
    only_a = sorted(a - b)
    only_b = sorted(b - a)
    return {
        "match": not only_a and not only_b,
        "bazel_diff_count": len(a),
        "tango_count": len(b),
        "common_count": len(a & b),
        "only_bazel_diff": only_a,
        "only_tango": only_b,
    }


def parse_synthetic_spec(spec: str) -> tuple[str, dict[str, int]]:
    """`name:packages:targets_per_package[:layers]` -> (name, gen_project kwargs)."""
    parts = spec.split(":")
    if len(parts) not in (3, 4):
        raise argparse.ArgumentTypeError(
            f"--synthetic expects name:packages:targets_per_package[:layers], got {spec!r}"
        )
    name = parts[0].strip()
    if not name or not re.fullmatch(r"[A-Za-z0-9_.-]+", name):
        raise argparse.ArgumentTypeError(f"--synthetic name must be a plain identifier, got {name!r}")
    try:
        nums = [int(p) for p in parts[1:]]
    except ValueError as e:
        raise argparse.ArgumentTypeError(f"--synthetic sizes must be integers: {spec!r}") from e
    if any(n <= 0 for n in nums):
        raise argparse.ArgumentTypeError(f"--synthetic sizes must be positive: {spec!r}")
    params = {"packages": nums[0], "targets_per_package": nums[1]}
    if len(nums) == 3:
        params["layers"] = nums[2]
    else:
        params["layers"] = max(1, min(40, nums[0] // 10))
    return name, params


def median(xs: list[float]) -> float | None:
    return statistics.median(xs) if xs else None


def fmt_s(x: float | None) -> str:
    return "n/a" if x is None else f"{x:.2f}s"


def fmt_ratio(num: float | None, den: float | None) -> str:
    if num is None or den is None or den <= 0:
        return "n/a"
    return f"{num / den:.2f}x"


def summarize_samples(samples: list[dict[str, Any]], key: str = "total_s") -> dict[str, Any]:
    ok = [s for s in samples if s.get("ok")]
    vals = [float(s[key]) for s in ok if s.get(key) is not None]
    return {
        "runs": len(samples),
        "ok": len(ok),
        "median_s": round(median(vals), 3) if vals else None,
        "min_s": round(min(vals), 3) if vals else None,
        "max_s": round(max(vals), 3) if vals else None,
        "samples_s": [round(v, 3) for v in vals],
    }


def render_summary(report: dict[str, Any]) -> str:
    """GitHub-flavored markdown for a CI step summary."""
    env = report.get("environment", {})
    out: list[str] = []
    out.append("## bazel-diff vs Tango performance comparison")
    out.append("")
    out.append(
        f"bazel-diff `{env.get('bazel_diff_version', '?')}` (repo `{env.get('repo_sha', '?')[:12]}`) vs "
        f"Tango `{env.get('tango_sha', '?')[:12]}` ({env.get('tango_ref', '?')}) -- "
        f"Bazel {env.get('bazel_version', '?')}, {env.get('cpu_count', '?')} CPUs, "
        f"{env.get('mem_total_gib', '?')} GiB, {env.get('platform', '?')}. "
        f"Tango worker pool: {env.get('tango_pool_size', '?')}. Iterations: {report.get('iterations', '?')}."
    )
    out.append("")
    out.append(
        "`cold` = fresh Bazel server for both tools (CI-per-PR cost); `warm` = server alive, Tango with "
        "`-bypass-cache`; `diff only` = no Bazel involved (Tango result-cache hit vs bazel-diff "
        "`get-impacted-targets` on existing hash files). Medians over the iterations; ratio is "
        "Tango / bazel-diff, so above 1.00x means bazel-diff was faster."
    )
    out.append("")
    out.append("| Workload | Targets | Phase | bazel-diff | Tango | Tango / bazel-diff |")
    out.append("|---|---:|---|---:|---:|---:|")
    for wl in report.get("workloads", []):
        name = wl["name"]
        targets = wl.get("meta", {}).get("rule_targets")
        tcell = str(targets) if targets is not None else "-"
        for phase in PHASES:
            bd = wl["results"]["bazel_diff"].get(phase, {}).get("median_s")
            tg = wl["results"]["tango"].get(phase, {}).get("median_s")
            out.append(f"| {name} | {tcell} | {phase} | {fmt_s(bd)} | {fmt_s(tg)} | {fmt_ratio(tg, bd)} |")
        bd = wl["results"]["bazel_diff"].get("diff_only", {}).get("median_s")
        tg = wl["results"]["tango"].get("cached", {}).get("median_s")
        out.append(f"| {name} | {tcell} | diff only | {fmt_s(bd)} | {fmt_s(tg)} | {fmt_ratio(tg, bd)} |")
    out.append("")

    out.append("### bazel-diff stage breakdown (cold, medians)")
    out.append("")
    out.append("| Workload | checkout base | generate-hashes base | checkout head | generate-hashes head | get-impacted-targets |")
    out.append("|---|---:|---:|---:|---:|---:|")
    for wl in report.get("workloads", []):
        st = wl["results"]["bazel_diff"].get("cold_stages", {})
        out.append(
            f"| {wl['name']} | {fmt_s(st.get('checkout_base'))} | {fmt_s(st.get('generate_base'))} | "
            f"{fmt_s(st.get('checkout_head'))} | {fmt_s(st.get('generate_head'))} | {fmt_s(st.get('diff'))} |"
        )
    out.append("")

    out.append("### Impacted-set agreement")
    out.append("")
    out.append("| Workload | Commits | bazel-diff | Tango | Common | Agree |")
    out.append("|---|---|---:|---:|---:|---|")
    for wl in report.get("workloads", []):
        c = wl.get("correctness") or {}
        agree = "yes" if c.get("match") else ("**NO**" if c else "n/a")
        commits = f"`{wl.get('base_sha', '?')[:10]}..{wl.get('head_sha', '?')[:10]}`"
        out.append(
            f"| {wl['name']} | {commits} | {c.get('bazel_diff_count', '-')} | {c.get('tango_count', '-')} | "
            f"{c.get('common_count', '-')} | {agree} |"
        )
    mismatches = [wl for wl in report.get("workloads", []) if wl.get("correctness") and not wl["correctness"]["match"]]
    for wl in mismatches:
        c = wl["correctness"]
        out.append("")
        out.append(f"<details><summary>{wl['name']}: labels the tools disagree on</summary>")
        out.append("")
        if c["only_bazel_diff"]:
            out.append(f"Only bazel-diff ({len(c['only_bazel_diff'])}):")
            out.append("")
            out.extend(f"- `{lbl}`" for lbl in c["only_bazel_diff"][:50])
            if len(c["only_bazel_diff"]) > 50:
                out.append(f"- ... {len(c['only_bazel_diff']) - 50} more")
            out.append("")
        if c["only_tango"]:
            out.append(f"Only Tango ({len(c['only_tango'])}):")
            out.append("")
            out.extend(f"- `{lbl}`" for lbl in c["only_tango"][:50])
            if len(c["only_tango"]) > 50:
                out.append(f"- ... {len(c['only_tango']) - 50} more")
            out.append("")
        out.append("</details>")
    out.append("")

    failures = report.get("failures", [])
    if failures:
        out.append("### Failures")
        out.append("")
        for f in failures:
            out.append(f"- **{f['workload']} / {f['tool']} / {f['phase']} #{f['iteration']}**: {f['error']}")
        out.append("")
    return "\n".join(out)


# ----------------------------------------------------------------------------------------------
# Subprocess helpers
# ----------------------------------------------------------------------------------------------


def run(cmd: list, cwd: Path | None = None, env: dict | None = None, check: bool = True) -> subprocess.CompletedProcess:
    vlog(f"$ {' '.join(str(c) for c in cmd)}" + (f"  (cwd={cwd})" if cwd else ""))
    proc = subprocess.run(
        [str(c) for c in cmd],
        cwd=str(cwd) if cwd else None,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if check and proc.returncode != 0:
        tail = "\n".join(proc.stdout.splitlines()[-30:])
        raise RuntimeError(f"command failed ({proc.returncode}): {' '.join(str(c) for c in cmd)}\n{tail}")
    return proc


def timed(fn) -> tuple[float, Any]:
    t0 = time.monotonic()
    result = fn()
    return time.monotonic() - t0, result


def git(repo: Path, *args: str) -> str:
    return run(["git", "-C", repo, *args]).stdout.strip()


def bazel_shutdown(bazel: str, workspace: Path) -> None:
    """Stop the Bazel server owning `workspace` so the next command pays a cold start."""
    if not workspace.is_dir():
        return
    run([bazel, "shutdown"], cwd=workspace, check=False)


def port_in_use(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(0.2)
        return s.connect_ex(("127.0.0.1", port)) == 0


def wait_for_port(port: int, timeout_s: float, proc: subprocess.Popen | None = None) -> None:
    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        if proc is not None and proc.poll() is not None:
            raise RuntimeError(f"process exited early with code {proc.returncode}")
        if port_in_use(port):
            return
        time.sleep(0.1)
    raise RuntimeError(f"port {port} did not open within {timeout_s:.0f}s")


# ----------------------------------------------------------------------------------------------
# Workloads
# ----------------------------------------------------------------------------------------------


@dataclass
class Workload:
    name: str
    kind: str  # "synthetic" | "repo"
    path: Path
    base_sha: str
    head_sha: str
    meta: dict[str, Any] = field(default_factory=dict)


def load_gen_project():
    spec = importlib.util.spec_from_file_location("gen_project", GEN_PROJECT)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {GEN_PROJECT}")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def make_synthetic_workload(name: str, params: dict[str, int], workdir: Path, bazel_version: str) -> Workload:
    gen = load_gen_project()
    out = workdir / "workspaces" / name
    log(f"[{name}] generating synthetic workspace: {params}")
    meta = gen.gen_workspace(
        out,
        packages=params["packages"],
        targets_per_package=params["targets_per_package"],
        sources_per_package=params.get("sources_per_package", 3),
        fanin=params.get("fanin", 4),
        layers=params["layers"],
        bazel_version=bazel_version,
    )
    meta.update(gen.init_git(out, meta["deep_source"]))
    return Workload(name=name, kind="synthetic", path=out, base_sha=meta["base_sha"], head_sha=meta["target_sha"], meta=meta)


def make_repo_workload(name: str, url: str, from_rev: str, to_rev: str, clone_args: list[str], workdir: Path) -> Workload:
    out = workdir / "workspaces" / name
    log(f"[{name}] cloning {url} ({' '.join(clone_args) or 'full clone'})")
    run(["git", "clone", *clone_args, url, out])
    # Resolve both revisions against the fresh clone's HEAD, before either tool moves it.
    head_sha = git(out, "rev-parse", to_rev + "^{commit}")
    base_sha = git(out, "rev-parse", from_rev + "^{commit}")
    meta = {"url": url, "from": from_rev, "to": to_rev, "clone_args": clone_args}
    return Workload(name=name, kind="repo", path=out, base_sha=base_sha, head_sha=head_sha, meta=meta)


# ----------------------------------------------------------------------------------------------
# Tango: build, configure, serve
# ----------------------------------------------------------------------------------------------


def prepare_tango(workdir: Path, tango_dir: Path | None, tango_ref: str) -> tuple[Path, str]:
    """Return (checkout dir, resolved sha), cloning uber/tango at `tango_ref` unless a checkout is given."""
    if tango_dir is None:
        tango_dir = workdir / "tango-src"
        log(f"cloning {TANGO_REPO_URL} @ {tango_ref}")
        run(["git", "clone", "--quiet", "--filter=blob:none", TANGO_REPO_URL, tango_dir])
        run(["git", "-C", tango_dir, "fetch", "--quiet", "origin", tango_ref], check=False)
        run(["git", "-C", tango_dir, "checkout", "--quiet", "--detach", tango_ref])
    sha = git(tango_dir, "rev-parse", "HEAD")
    return tango_dir, sha


def build_tango(tango_dir: Path, bin_dir: Path, go: str) -> dict[str, Path]:
    bin_dir.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    # tango's go.mod declares go 1.25; let the go command fetch that toolchain if the host's is older.
    env.setdefault("GOTOOLCHAIN", "auto")
    env.setdefault("GOFLAGS", "-mod=mod")
    env["CGO_ENABLED"] = env.get("CGO_ENABLED", "0")
    bins = {"server": bin_dir / "tango-server", "client": bin_dir / "tango-client"}
    log("building tango example server + client (go build)")
    run([go, "build", "-o", bins["server"], "./example/"], cwd=tango_dir, env=env)
    run([go, "build", "-o", bins["client"], "./example/client/"], cwd=tango_dir, env=env)
    return bins


def render_tango_config(workloads: list[Workload], bazel: str, workspaces_root: Path, pool_size: int, query_timeout_s: int) -> str:
    """Tango's example server config, one repository entry per workload (remote = the workspace path).

    Mirrors tango's integration-test template: bzlmod on, bazel_tools excluded from hashing, main
    repo fully hashed. `remote` must match the client's -remote byte for byte."""
    lines = ["storage:", '  type: "memory"', "repository:"]
    for wl in workloads:
        lines += [
            f"  - remote: {json.dumps(str(wl.path))}",
            f"    repository_id: {json.dumps(wl.name)}",
            "    full_hash_repos: []",
            '    excluded_files: ["^@@?bazel_tools/"]',
            "    bzlmod_enabled: true",
            f"    bazel_command_path: {json.dumps(bazel)}",
            "    bazel_extra_args: []",
            "    bazel_startup_options: []",
            "    stream_bazel_logs: false",
            f"    query_timeout_seconds: {query_timeout_s}",
            "    seed_attributes: []",
            "    all_targets_files: []",
        ]
    lines += [
        "service:",
        f"  max_worker_pool_size: {pool_size}",
        f"  workspaces_root_path: {json.dumps(str(workspaces_root))}",
        "  max_message_bytes: 4250000",
        "graph:",
        "  default:",
        '    format: "gob"',
        "    shadow_compare: false",
        "",
    ]
    return "\n".join(lines)


class TangoServer:
    """The tango example server as a subprocess, with its log tailed for per-request timings."""

    def __init__(self, server_bin: Path, run_dir: Path, config: str):
        self.server_bin = server_bin
        self.run_dir = run_dir
        self.workspaces_root = run_dir / "workspaces"
        self.log_path = run_dir / "server.log"
        (run_dir / "example").mkdir(parents=True, exist_ok=True)
        self.workspaces_root.mkdir(parents=True, exist_ok=True)
        (run_dir / "example" / "tango-config.yaml").write_text(config)
        self.proc: subprocess.Popen | None = None
        self._log_fh = None
        self._log_offset = 0

    def start(self) -> None:
        if port_in_use(TANGO_PORT):
            raise RuntimeError(
                f"127.0.0.1:{TANGO_PORT} is already in use; tango's example server hard-codes that port"
            )
        self._log_fh = open(self.log_path, "wb")
        self.proc = subprocess.Popen(
            [str(self.server_bin)], cwd=str(self.run_dir), stdout=self._log_fh, stderr=subprocess.STDOUT
        )
        try:
            wait_for_port(TANGO_PORT, TANGO_SERVER_STARTUP_TIMEOUT_S, self.proc)
        except RuntimeError as e:
            self.stop()
            raise RuntimeError(f"tango server failed to start: {e}\n{self.log_tail()}") from e
        self._log_offset = self.log_path.stat().st_size

    def stop(self) -> None:
        if self.proc is not None and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=10)
        self.proc = None
        if self._log_fh is not None:
            self._log_fh.close()
            self._log_fh = None

    def log_tail(self, n: int = 40) -> str:
        if not self.log_path.exists():
            return ""
        return "\n".join(self.log_path.read_text(errors="replace").splitlines()[-n:])

    def new_log_lines(self) -> list[str]:
        """Lines appended to the server log since the last call."""
        if not self.log_path.exists():
            return []
        with open(self.log_path, "rb") as fh:
            fh.seek(self._log_offset)
            data = fh.read()
            self._log_offset = fh.tell()
        return data.decode("utf-8", errors="replace").splitlines()

    def worker_dirs(self, repository_id: str) -> list[Path]:
        root = self.workspaces_root / ".workers" / repository_id
        return sorted(p for p in root.glob("worker-*") if p.is_dir()) if root.is_dir() else []


_SERVER_TOTAL_RE = re.compile(r'GetChangedTargets: Successfully processed request.*?"total_duration":\s*"([^"]+)"')
_SERVER_FETCH_RE = re.compile(r'"graph_fetch_duration":\s*"([^"]+)"')


def extract_server_timings(lines: list[str]) -> dict[str, float | None]:
    """Server-side durations for the last GetChangedTargets in a slice of tango server log."""
    total = None
    fetch = None
    for line in lines:
        m = _SERVER_TOTAL_RE.search(line)
        if m:
            total = parse_go_duration(m.group(1))
        m = _SERVER_FETCH_RE.search(line)
        if m:
            fetch = parse_go_duration(m.group(1))
    return {"server_total_s": total, "server_graph_fetch_s": fetch}


# ----------------------------------------------------------------------------------------------
# The two flows under test
# ----------------------------------------------------------------------------------------------


def run_bazel_diff_flow(bazel_diff: Path, bazel: str, wl: Workload, out_dir: Path) -> dict[str, Any]:
    """README recipe: checkout base, generate-hashes, checkout head, generate-hashes (+deps), diff."""
    out_dir.mkdir(parents=True, exist_ok=True)
    base_json = out_dir / "base.json"
    head_json = out_dir / "head.json"
    deps_json = out_dir / "deps.json"
    impacted = out_dir / "impacted.json"
    stages: dict[str, float] = {}
    t_start = time.monotonic()
    try:
        stages["checkout_base"], _ = timed(lambda: git(wl.path, "checkout", "--quiet", "--force", wl.base_sha))
        stages["generate_base"], _ = timed(
            lambda: run([bazel_diff, "generate-hashes", "-w", wl.path, "-b", bazel, base_json])
        )
        stages["checkout_head"], _ = timed(lambda: git(wl.path, "checkout", "--quiet", "--force", wl.head_sha))
        stages["generate_head"], _ = timed(
            lambda: run([bazel_diff, "generate-hashes", "-w", wl.path, "-b", bazel, "-d", deps_json, head_json])
        )
        stages["diff"], _ = timed(
            lambda: run(
                [
                    bazel_diff, "get-impacted-targets", "-w", wl.path, "-b", bazel,
                    "-sh", base_json, "-fh", head_json, "-d", deps_json, "-o", impacted,
                ]
            )
        )
    except RuntimeError as e:
        return {"ok": False, "error": str(e), "stages": stages, "total_s": time.monotonic() - t_start}
    total = time.monotonic() - t_start
    labels = parse_impacted_targets(impacted.read_text())
    return {"ok": True, "total_s": total, "stages": stages, "labels": labels, "count": len(labels)}


def run_bazel_diff_diff_only(bazel_diff: Path, bazel: str, wl: Workload, out_dir: Path) -> dict[str, Any]:
    """Just `get-impacted-targets` over the hash files the last full flow left in out_dir."""
    impacted = out_dir / "impacted_only.json"
    try:
        total, _ = timed(
            lambda: run(
                [
                    bazel_diff, "get-impacted-targets", "-w", wl.path, "-b", bazel,
                    "-sh", out_dir / "base.json", "-fh", out_dir / "head.json", "-d", out_dir / "deps.json",
                    "-o", impacted,
                ]
            )
        )
    except RuntimeError as e:
        return {"ok": False, "error": str(e)}
    labels = parse_impacted_targets(impacted.read_text())
    return {"ok": True, "total_s": total, "labels": labels, "count": len(labels)}


def run_tango_flow(client: Path, server: TangoServer, wl: Workload, bypass_cache: bool, timeout_s: int) -> dict[str, Any]:
    """One get-changed-targets RPC, timed as the client sees it, plus the server's own numbers."""
    cmd = [
        str(client), "-addr", f"127.0.0.1:{TANGO_PORT}", "-method", "get-changed-targets",
        "-remote", str(wl.path), "-base-sha", wl.base_sha, "-new-base-sha", wl.head_sha,
        "-max-distance", "-1", "-timeout", f"{timeout_s}s",
    ]
    if bypass_cache:
        cmd.append("-bypass-cache")
    server.new_log_lines()  # discard anything logged before this request
    vlog(f"$ {' '.join(cmd)}")
    t0 = time.monotonic()
    proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    total = time.monotonic() - t0
    server_timings = extract_server_timings(server.new_log_lines())
    if proc.returncode != 0:
        tail = "\n".join(proc.stderr.splitlines()[-20:])
        return {
            "ok": False,
            "error": f"tango client exited {proc.returncode}:\n{tail}\n--- server log tail ---\n{server.log_tail(20)}",
            "total_s": total,
            **server_timings,
        }
    parsed = parse_tango_client_output(proc.stdout)
    return {
        "ok": True,
        "total_s": total,
        "labels": parsed["labels"],
        "count": parsed["count"],
        "by_type": parsed["by_type"],
        **server_timings,
    }


# ----------------------------------------------------------------------------------------------
# Driver
# ----------------------------------------------------------------------------------------------


def shutdown_all(bazel: str, wl: Workload, server: TangoServer) -> None:
    bazel_shutdown(bazel, wl.path)
    for d in server.worker_dirs(wl.name):
        bazel_shutdown(bazel, d)


def bench_workload(
    wl: Workload,
    *,
    bazel_diff: Path,
    bazel: str,
    client: Path,
    server: TangoServer,
    iterations: int,
    tango_timeout_s: int,
    workdir: Path,
    failures: list[dict[str, Any]],
) -> dict[str, Any]:
    out_dir = workdir / "bazel-diff-out" / wl.name
    samples: dict[str, dict[str, list[dict[str, Any]]]] = {
        "bazel_diff": {"cold": [], "warm": [], "diff_only": []},
        "tango": {"cold": [], "warm": [], "cached": []},
    }
    last_labels: dict[str, list[str]] = {}

    def record(tool: str, phase: str, i: int, result: dict[str, Any]) -> None:
        samples[tool][phase].append(result)
        if result.get("ok"):
            if "labels" in result:
                last_labels[tool] = result["labels"]
            extra = ""
            if tool == "tango" and result.get("server_total_s") is not None:
                extra = f" (server {result['server_total_s']:.2f}s)"
            log(f"[{wl.name}] #{i} {tool:<10} {phase:<9} {result['total_s']:7.2f}s{extra}  impacted={result.get('count')}")
        else:
            failures.append({"workload": wl.name, "tool": tool, "phase": phase, "iteration": i, "error": result.get("error", "?")})
            log(f"[{wl.name}] #{i} {tool:<10} {phase:<9} FAILED: {str(result.get('error', '?')).splitlines()[0]}")

    # Warm-up: lets Tango clone its origin + worker (a one-off cost bazel-diff never pays because it
    # works in the workspace in place) and lets both Bazel servers fetch bazel_tools & co. into their
    # output bases. Not timed. A warm-up failure is fatal for the workload: nothing after it would mean
    # anything.
    log(f"[{wl.name}] warm-up (untimed): tango clone + first query, bazel-diff first query")
    w = run_tango_flow(client, server, wl, bypass_cache=True, timeout_s=tango_timeout_s)
    if not w["ok"]:
        failures.append({"workload": wl.name, "tool": "tango", "phase": "warm-up", "iteration": 0, "error": w["error"]})
        log(f"[{wl.name}] tango warm-up FAILED:\n{w['error']}")
    w2 = run_bazel_diff_flow(bazel_diff, bazel, wl, out_dir)
    if not w2["ok"]:
        failures.append({"workload": wl.name, "tool": "bazel_diff", "phase": "warm-up", "iteration": 0, "error": w2["error"]})
        log(f"[{wl.name}] bazel-diff warm-up FAILED:\n{w2['error']}")
    if not (w["ok"] and w2["ok"]):
        return _workload_report(wl, samples, last_labels, aborted=True)

    for i in range(1, iterations + 1):
        bazel_diff_first = i % 2 == 1
        order = ["bazel_diff", "tango"] if bazel_diff_first else ["tango", "bazel_diff"]

        # cold: both servers down, each tool pays a fresh server start + full load.
        shutdown_all(bazel, wl, server)
        for tool in order:
            if tool == "bazel_diff":
                record(tool, "cold", i, run_bazel_diff_flow(bazel_diff, bazel, wl, out_dir))
            else:
                record(tool, "cold", i, run_tango_flow(client, server, wl, bypass_cache=True, timeout_s=tango_timeout_s))

        # warm: servers alive from the cold run; incremental re-analysis after the checkouts.
        for tool in order:
            if tool == "bazel_diff":
                record(tool, "warm", i, run_bazel_diff_flow(bazel_diff, bazel, wl, out_dir))
            else:
                record(tool, "warm", i, run_tango_flow(client, server, wl, bypass_cache=True, timeout_s=tango_timeout_s))

        # diff only: no Bazel on either side.
        record("bazel_diff", "diff_only", i, run_bazel_diff_diff_only(bazel_diff, bazel, wl, out_dir))
        record("tango", "cached", i, run_tango_flow(client, server, wl, bypass_cache=False, timeout_s=tango_timeout_s))

    return _workload_report(wl, samples, last_labels, aborted=False)


def _workload_report(wl: Workload, samples, last_labels, *, aborted: bool) -> dict[str, Any]:
    results: dict[str, dict[str, Any]] = {"bazel_diff": {}, "tango": {}}
    for tool, phases in samples.items():
        for phase, runs in phases.items():
            results[tool][phase] = summarize_samples(runs)
        if tool == "tango":
            for phase, runs in phases.items():
                srv = [r["server_total_s"] for r in runs if r.get("ok") and r.get("server_total_s") is not None]
                results[tool][phase]["server_median_s"] = round(median(srv), 3) if srv else None
    cold_ok = [r for r in samples["bazel_diff"]["cold"] if r.get("ok")]
    results["bazel_diff"]["cold_stages"] = {
        stage: round(median([r["stages"][stage] for r in cold_ok if stage in r["stages"]]), 3)
        for stage in ("checkout_base", "generate_base", "checkout_head", "generate_head", "diff")
        if any(stage in r["stages"] for r in cold_ok)
    }
    correctness = None
    if "bazel_diff" in last_labels and "tango" in last_labels:
        correctness = compare_label_sets(last_labels["bazel_diff"], last_labels["tango"])
    return {
        "name": wl.name,
        "kind": wl.kind,
        "path": str(wl.path),
        "base_sha": wl.base_sha,
        "head_sha": wl.head_sha,
        "meta": wl.meta,
        "aborted": aborted,
        "results": results,
        "correctness": correctness,
        "raw": samples,
    }


def collect_environment(bazel: str, bazel_diff: Path, wl_for_bazel: Workload | None, tango_sha: str, tango_ref: str, pool_size: int) -> dict[str, Any]:
    env: dict[str, Any] = {
        "platform": platform.platform(),
        "python": platform.python_version(),
        "cpu_count": os.cpu_count(),
        "tango_sha": tango_sha,
        "tango_ref": tango_ref,
        "tango_pool_size": pool_size,
        "bazel_path": bazel,
        "bazel_diff_path": str(bazel_diff),
    }
    try:
        meminfo = Path("/proc/meminfo").read_text()
        kib = int(re.search(r"MemTotal:\s+(\d+)", meminfo).group(1))
        env["mem_total_gib"] = round(kib / 1024 / 1024, 1)
    except (OSError, AttributeError, ValueError):
        env["mem_total_gib"] = None
    try:
        env["repo_sha"] = git(REPO_ROOT, "rev-parse", "HEAD")
    except RuntimeError:
        env["repo_sha"] = "unknown"
    proc = run([bazel_diff, "--version"], check=False)
    env["bazel_diff_version"] = proc.stdout.strip().splitlines()[-1] if proc.stdout.strip() else "unknown"
    if wl_for_bazel is not None:
        proc = run([bazel, "--version"], cwd=wl_for_bazel.path, check=False)
        env["bazel_version"] = proc.stdout.strip().splitlines()[-1].replace("bazel ", "") if proc.stdout.strip() else "unknown"
    return env


def build_bazel_diff(bazel: str) -> None:
    log(f"building {BUILD_TARGET} with {bazel} (-c opt)")
    run([bazel, "build", BUILD_TARGET, "-c", "opt"], cwd=REPO_ROOT)


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bazel", default="bazel", help="bazel/bazelisk binary both tools run (default: bazel)")
    ap.add_argument("--bazel-diff", type=Path, default=None, help=f"bazel-diff binary (default: {LAUNCHER})")
    ap.add_argument("--skip-build", action="store_true", help="do not `bazel build` bazel-diff; use --bazel-diff / the existing launcher")
    ap.add_argument("--go", default="go", help="go binary used to build tango (default: go)")
    ap.add_argument("--tango-ref", default=TANGO_PINNED_REF, help=f"uber/tango commit/branch/tag to build (default: pinned {TANGO_PINNED_REF[:12]})")
    ap.add_argument("--tango-dir", type=Path, default=None, help="reuse an existing uber/tango checkout instead of cloning")
    ap.add_argument("--tango-pool-size", type=int, default=1,
                    help="tango max_worker_pool_size; 1 computes the two revisions sequentially like bazel-diff, "
                         "2+ lets tango analyse both revisions in parallel on separate workers (default: 1)")
    ap.add_argument("--tango-timeout", type=int, default=3600, help="per-request tango client/server query timeout, seconds (default: 3600)")
    ap.add_argument("--workload", action="append", default=[], metavar="NAME",
                    help=f"synthetic preset ({', '.join(SYNTHETIC_PRESETS)}), repeatable; or the name for a --repo-url workload")
    ap.add_argument("--synthetic", action="append", default=[], type=parse_synthetic_spec, metavar="NAME:PKGS:TPP[:LAYERS]",
                    help="custom synthetic workload, repeatable")
    ap.add_argument("--repo-url", default=None, help="benchmark a real repository instead of / in addition to synthetic ones")
    ap.add_argument("--repo-from", default="HEAD~1", help="base revision for --repo-url (default: HEAD~1)")
    ap.add_argument("--repo-to", default="HEAD", help="head revision for --repo-url (default: HEAD)")
    ap.add_argument("--repo-clone-args", default="", help="extra `git clone` args for --repo-url, e.g. --depth=50")
    ap.add_argument("--bazel-version", default=None, help="Bazel version pinned in synthetic workspaces (default: this repo's .bazelversion)")
    ap.add_argument("--iterations", type=int, default=3, help="timed iterations per workload (default: 3)")
    ap.add_argument("--quick", action="store_true", help="tiny workload, 1 iteration (smoke profile)")
    ap.add_argument("--workdir", type=Path, default=None, help="scratch directory (default: a fresh temp dir)")
    ap.add_argument("--keep-workdir", action="store_true", help="leave the scratch directory on disk")
    ap.add_argument("--metrics-out", type=Path, default=None, help="write the full report JSON here")
    ap.add_argument("--summary-out", type=Path, default=None, help="write the markdown summary here")
    ap.add_argument("--require-match", action="store_true", help="exit non-zero if the two tools' impacted sets differ")
    ap.add_argument("-v", "--verbose", action="store_true", help="echo every command")
    return ap


def main(argv: list[str]) -> int:
    global VERBOSE
    args = build_parser().parse_args(argv)
    VERBOSE = args.verbose

    if args.quick:
        if not args.workload and not args.synthetic and not args.repo_url:
            args.workload = ["tiny"]
        args.iterations = 1
    if not args.workload and not args.synthetic and not args.repo_url:
        args.workload = ["small", "medium", "large", "xlarge"]
    if args.iterations < 1:
        log("--iterations must be >= 1")
        return 2

    bazel_diff = args.bazel_diff or LAUNCHER
    if not args.skip_build and args.bazel_diff is None:
        build_bazel_diff(args.bazel)
    if not Path(bazel_diff).exists():
        log(f"bazel-diff binary not found at {bazel_diff}; pass --bazel-diff or drop --skip-build")
        return 2
    bazel_diff = Path(bazel_diff).resolve()
    bazel = shutil.which(args.bazel) or str(Path(args.bazel).expanduser().resolve())
    if not Path(bazel).exists():
        log(f"bazel binary not found: {args.bazel}")
        return 2
    bazel_version = args.bazel_version or (REPO_ROOT / ".bazelversion").read_text().strip()

    workdir = args.workdir.resolve() if args.workdir else Path(tempfile.mkdtemp(prefix="tango-compare-"))
    workdir.mkdir(parents=True, exist_ok=True)
    log(f"workdir: {workdir}")

    failures: list[dict[str, Any]] = []
    server: TangoServer | None = None
    workloads: list[Workload] = []
    rc = 0
    try:
        # --- workloads ---
        synthetic: list[tuple[str, dict[str, int]]] = []
        repo_name = None
        for name in args.workload:
            if name in SYNTHETIC_PRESETS:
                synthetic.append((name, SYNTHETIC_PRESETS[name]))
            elif args.repo_url and repo_name is None:
                repo_name = name
            else:
                log(f"unknown workload {name!r}; presets are {', '.join(SYNTHETIC_PRESETS)}")
                return 2
        synthetic.extend(args.synthetic)
        for name, params in synthetic:
            workloads.append(make_synthetic_workload(name, params, workdir, bazel_version))
        if args.repo_url:
            name = repo_name or re.sub(r"\.git$", "", args.repo_url.rstrip("/").rsplit("/", 1)[-1]) or "repo"
            workloads.append(make_repo_workload(name, args.repo_url, args.repo_from, args.repo_to, args.repo_clone_args.split(), workdir))
        if len({wl.name for wl in workloads}) != len(workloads):
            log("workload names must be unique")
            return 2

        # --- tango ---
        tango_dir, tango_sha = prepare_tango(workdir, args.tango_dir, args.tango_ref)
        bins = build_tango(tango_dir, workdir / "tango-bin", args.go)
        run_dir = workdir / "tango-run"
        config = render_tango_config(workloads, bazel, run_dir / "workspaces", args.tango_pool_size, args.tango_timeout)
        server = TangoServer(bins["server"], run_dir, config)
        log(f"starting tango server (127.0.0.1:{TANGO_PORT}); log: {server.log_path}")
        server.start()

        environment = collect_environment(bazel, bazel_diff, workloads[0] if workloads else None, tango_sha, args.tango_ref, args.tango_pool_size)
        log(f"environment: {json.dumps(environment)}")

        # --- benchmark ---
        reports = []
        for wl in workloads:
            log(f"[{wl.name}] {wl.kind} workload at {wl.path}  {wl.base_sha[:10]}..{wl.head_sha[:10]}")
            reports.append(
                bench_workload(
                    wl,
                    bazel_diff=bazel_diff,
                    bazel=bazel,
                    client=bins["client"],
                    server=server,
                    iterations=args.iterations,
                    tango_timeout_s=args.tango_timeout,
                    workdir=workdir,
                    failures=failures,
                )
            )
            shutdown_all(bazel, wl, server)

        report = {
            "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "iterations": args.iterations,
            "environment": environment,
            "workloads": reports,
            "failures": failures,
        }
        summary = render_summary(report)
        log("\n" + summary)
        if args.metrics_out:
            args.metrics_out.parent.mkdir(parents=True, exist_ok=True)
            args.metrics_out.write_text(json.dumps(report, indent=2) + "\n")
            log(f"wrote {args.metrics_out}")
        if args.summary_out:
            args.summary_out.parent.mkdir(parents=True, exist_ok=True)
            args.summary_out.write_text(summary + "\n")
            log(f"wrote {args.summary_out}")

        if failures:
            log(f"{len(failures)} run(s) failed")
            rc = 1
        mismatched = [r["name"] for r in reports if r.get("correctness") and not r["correctness"]["match"]]
        if mismatched:
            log(f"impacted sets differ on: {', '.join(mismatched)}" + ("" if args.require_match else " (informational; pass --require-match to fail on this)"))
            if args.require_match:
                rc = 1
    finally:
        # Stop every Bazel server the run started (both tools' sides) before the tango server exits
        # and deletes its workspaces, so no JVM lingers on a directory that is about to disappear.
        for wl in workloads:
            bazel_shutdown(bazel, wl.path)
            if server is not None:
                for d in server.worker_dirs(wl.name):
                    bazel_shutdown(bazel, d)
        if server is not None:
            server.stop()
        if args.keep_workdir:
            log(f"kept workdir {workdir}")
        elif args.workdir is None:
            shutil.rmtree(workdir, ignore_errors=True)
    return rc


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
