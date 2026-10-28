#!/usr/bin/env python3
"""Cache-probe selective testing vs bazel-diff: the nightly experiment behind
.github/workflows/cache-probe-bench.yml.

THIS IS AN EXPERIMENT, NOT A FEATURE. bazel-diff has no cache-probe mode and the README does not
mention one. This harness exists to decide, from trend data, whether it should get one: the mode is
not advertised until the cron shows it is both sound and a win on precision or time.

The technique under test (Aspect's `aspect cache diff`, https://aspect.build/blog/selective-testing-with-cache-diff):
ask the remote cache which actions it already holds, and treat a test as unaffected when its own
TestRunner action is a cache hit. One `bazel test` under `--experimental_remote_require_cached`
executes nothing -- every action is a hit or an EXECUTION_DENIED miss -- and `--remote_grpc_log`
records a GetActionResult per probed action, labelled with its target and mnemonic. Bazel only
enforces require-cached on the remote-execution path, so the probe points `--remote_executor` at a
dummy REAPI server (tools/perf/cache_probe/dummy_executor) that never receives an Execute call.

Per synthetic workload the harness:

  1. generates a layered genrule workspace with `check_test` targets (tools/firecracker/bench/
     gen_project.py --tests-per-package) and commits three change scenarios on top of `base`:
       leaf_edit         one source file ~75% deep in the graph (a PR-sized change)
       identical_output  an input whose genrule ignores its content: the action key moves, the
                         output and everything downstream stay byte-identical
       wide_edit         a layer-0 source with the most dependents (a shared-library change)
  2. seeds a fresh bazel-remote from `base` (`bazel clean` first: on a warm output base Bazel
     executes nothing and uploads nothing) and verifies the seed with a clean-tree probe that must
     report zero misses, then snapshots the cache directory;
  3. per scenario establishes GROUND TRUTH: the tests a real `bazel test //...` against the seeded
     cache actually executes (not "(cached)"), which is what CI would have paid without any
     selector;
  4. per iteration times, cold (Bazel server stopped) and warm:
       bazel-diff        the README recipe: checkout base, generate-hashes, checkout head,
                         generate-hashes --depEdgesFile, get-impacted-targets
       probe overreport  pass 1 (a salt flip that drops Bazel's memoized action nodes without
                         re-analysis) + pass 2 (the require-cached test probe) + log parse + one
                         `tests(rdeps(//..., set(<frontier misses>)))` query
       probe precise     `bazel build //...` with upload (executes the missed non-test actions),
                         then the probe again; affected = tests whose own action missed
     and restores the cache snapshot so one iteration's uploads never leak into the next;
  5. scores every selector against ground truth: recall (must be 1.0; a miss is a soundness bug
     and fails the run under --require-sound) and precision (truth / flagged).

Outputs: --summary-out (GitHub-flavored markdown for the step summary) and --metrics-out (JSON:
environment, per-workload seed data, per-scenario truth, raw timing samples, label-level accuracy).

Requirements: git, go 1.25+, a bazel/bazelisk binary, Python 3.9+, stdlib only. bazel-remote is
`go install`ed at the pinned BAZEL_REMOTE_MODULE and the dummy executor is built from this repo;
--install-fixtures-only builds both into --fixture-dir and exits (the workflow does that in its own
step). Not a py_binary: it shells out to `bazel build //src:bazel-diff`, which would deadlock under
`bazel run`. Run it directly:

    python3 tools/perf/cache_probe_bench.py --bazel ~/go/bin/bazelisk --quick
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterator

sys.path.insert(0, str(Path(__file__).resolve().parent))
import tango_compare as tc  # noqa: E402  (shared helpers: run, timed, git, presets, workloads)

REPO_ROOT = tc.REPO_ROOT
DUMMY_EXECUTOR_SRC = REPO_ROOT / "tools" / "perf" / "cache_probe" / "dummy_executor"
# Pinned so a cache-server change never silently moves the baseline. Bump deliberately.
BAZEL_REMOTE_MODULE = "github.com/buchgr/bazel-remote/v2@v2.6.4"

SCENARIOS = ("leaf_edit", "identical_output", "wide_edit")
TESTS_PER_PACKAGE = 2
PHASES = ("cold", "warm")

GET_ACTION_RESULT = "build.bazel.remote.execution.v2.ActionCache/GetActionResult"
TEST_MNEMONIC = "TestRunner"
# Pass 1 of the probe. RemoteModule.computeActionExecutionSalt folds --remote_default_exec_properties
# in, so flipping it drops every ACTION_EXECUTION Skyframe node while the analysis graph survives;
# pass 2 runs without it, so the keys it looks up are the baseline's.
SALT_FLAG = "--remote_default_exec_properties=bazel_diff_cache_probe=1"
REQUIRE_CACHED_DENIAL = "must be cached due to --experimental_remote_require_cached"


# ----------------------------------------------------------------------------------------------
# Pure helpers (unit-tested in cache_probe_bench_test.py; nothing here shells out)
# ----------------------------------------------------------------------------------------------


def _varint(b: bytes, i: int) -> tuple[int, int]:
    result = shift = 0
    while True:
        c = b[i]
        i += 1
        result |= (c & 0x7F) << shift
        shift += 7
        if not c & 0x80:
            return result, i


def _fields(b: bytes) -> Iterator[tuple[int, int, Any]]:
    """(field_number, wire_type, value) for each field of one protobuf message."""
    i = 0
    n = len(b)
    while i < n:
        key, i = _varint(b, i)
        fno, wt = key >> 3, key & 7
        if wt == 0:
            v, i = _varint(b, i)
        elif wt == 1:
            v, i = b[i : i + 8], i + 8
        elif wt == 2:
            ln, i = _varint(b, i)
            v, i = b[i : i + ln], i + ln
        elif wt == 5:
            v, i = b[i : i + 4], i + 4
        else:
            raise ValueError(f"unsupported wire type {wt} at offset {i}")
        yield fno, wt, v


def _sub(b: bytes, want: set[int]) -> dict[int, Any]:
    return {f: v for f, _, v in _fields(b) if f in want}


def iter_delimited(data: bytes) -> Iterator[bytes]:
    """Messages of a varint-length-delimited stream (Bazel's --remote_grpc_log framing)."""
    i = 0
    while i < len(data):
        n, i = _varint(data, i)
        yield data[i : i + n]
        i += n


def parse_grpc_log(data: bytes) -> dict[str, Any]:
    """Decode a --remote_grpc_log into per-method counts and the action-cache lookups.

    Each entry is a remote_logging.LogEntry: metadata (1, a RequestMetadata whose action_mnemonic is
    field 5, target_id field 6, configuration_id field 7), status (2, google.rpc.Status: code field
    1), method_name (3). Only ActionCache/GetActionResult entries carry a verdict: code 0 is a hit,
    anything else (NOT_FOUND = 5) is a miss. No proto runtime needed, so it runs anywhere."""
    methods: dict[str, int] = {}
    lookups: list[dict[str, Any]] = []
    for raw in iter_delimited(data):
        top = _sub(raw, {1, 2, 3})
        method = top.get(3, b"").decode()
        methods[method] = methods.get(method, 0) + 1
        if method != GET_ACTION_RESULT:
            continue
        meta = _sub(top.get(1, b""), {5, 6, 7})
        status = _sub(top.get(2, b""), {1})
        lookups.append(
            {
                "label": meta.get(6, b"").decode(),
                "mnemonic": meta.get(5, b"").decode(),
                "config": meta.get(7, b"").decode(),
                "code": int(status.get(1, 0)),
            }
        )
    return {"methods": methods, "lookups": lookups}


def verdict(lookups: list[dict[str, Any]]) -> dict[str, Any]:
    """Per-label hit/miss from the lookups. Bazel may look an action up more than once (a test
    action is checked twice per run); a label is a miss if any of its lookups missed. `misses` maps
    label -> the mnemonic of the first missed action; `hits` are labels that only ever hit."""
    misses: dict[str, str] = {}
    seen: set[str] = set()
    for lk in lookups:
        label = lk["label"]
        if not label:
            continue
        seen.add(label)
        if lk["code"] != 0 and label not in misses:
            misses[label] = lk["mnemonic"]
    return {"misses": misses, "hits": sorted(seen - set(misses)), "looked_up": len(seen)}


def probe_unexpected_errors(stderr: str) -> list[str]:
    """ERROR lines of a require-cached probe that are not the expected denials. The probe is
    expected to exit non-zero; a loading/analysis error must not pass as a partial answer."""
    out = []
    for line in stderr.splitlines():
        if "ERROR:" not in line:
            continue
        if REQUIRE_CACHED_DENIAL in line or "Build did NOT complete successfully" in line:
            continue
        out.append(line.strip())
    return out


def parse_test_lines(text: str) -> dict[str, list[str]]:
    """Classify the per-test lines of `bazel test` output: `//a:t  (cached) PASSED in 0.1s`,
    `//a:t  PASSED in 0.3s`, `//a:t  FAILED TO BUILD`, `//a:t  NO STATUS`, ...
    `executed` is what Bazel actually ran (passed or failed), `cached` what the cache served."""
    executed, cached, not_built, other = [], [], [], []
    for line in text.splitlines():
        s = line.strip()
        if not s.startswith("//"):
            continue
        parts = s.split(None, 1)
        if len(parts) != 2:
            continue
        label, rest = parts[0], parts[1].strip()
        if rest.startswith("(cached)"):
            cached.append(label)
        elif rest.startswith("FAILED TO BUILD") or rest.startswith("NO STATUS"):
            not_built.append(label)
        elif rest.startswith(("PASSED", "FAILED", "TIMEOUT", "FLAKY")):
            executed.append(label)
        else:
            other.append(label)
    return {
        "executed": sorted(set(executed)),
        "cached": sorted(set(cached)),
        "not_built": sorted(set(not_built)),
        "other": sorted(set(other)),
    }


def rdeps_query(frontier: list[str]) -> str:
    """The one attribution query: tests that transitively depend on any missed non-test target."""
    return "tests(rdeps(//..., set(" + " ".join(sorted(set(frontier))) + ")))"


def accuracy(truth: set[str], flagged: set[str]) -> dict[str, Any]:
    """Recall (1.0 = sound) and precision (truth / flagged) of a selector against ground truth."""
    missed = sorted(truth - flagged)
    extra = sorted(flagged - truth)
    return {
        "truth": len(truth),
        "flagged": len(flagged),
        "missed": missed,
        "extra_count": len(extra),
        "recall": round(1.0 - len(missed) / len(truth), 4) if truth else 1.0,
        "precision": round(len(truth & flagged) / len(flagged), 4) if flagged else (1.0 if not truth else 0.0),
    }


def summarize(samples: list[float]) -> dict[str, Any]:
    vals = [float(v) for v in samples]
    return {
        "runs": len(vals),
        "median_s": round(tc.median(vals), 3) if vals else None,
        "min_s": round(min(vals), 3) if vals else None,
        "max_s": round(max(vals), 3) if vals else None,
        "samples_s": [round(v, 3) for v in vals],
    }


def _pct(x: float | None) -> str:
    return "n/a" if x is None else f"{100 * x:.0f}%"


def _cell(acc: dict[str, Any] | None) -> str:
    if not acc:
        return "n/a"
    sound = "" if acc["recall"] >= 1.0 else f" **MISSED {len(acc['missed'])}**"
    return f"{acc['flagged']} ({_pct(acc['precision'])} precise){sound}"


def render_summary(report: dict[str, Any]) -> str:
    env = report.get("environment", {})
    lines = [
        "## Cache probe vs bazel-diff (experiment)",
        "",
        "Flagged counts are test targets; precision is ground truth / flagged, and recall below 100% "
        "is a soundness failure. Ground truth is what `bazel test //...` against the seeded cache "
        "executed. Times are medians over the iterations.",
        "",
        f"Bazel {env.get('bazel_version', '?')}, bazel-diff {env.get('bazel_diff_version', '?')}, "
        f"{env.get('bazel_remote', '?')}, {env.get('cpu_count', '?')} CPUs, {env.get('platform', '?')}.",
        "",
    ]
    for wl in report.get("workloads", []):
        p = wl.get("params", {})
        seed = wl.get("seed", {})
        lines.append(
            f"### {wl['name']}: {p.get('packages', '?')} packages, {wl.get('rule_targets', '?')} rule targets, "
            f"{wl.get('test_targets', '?')} tests"
        )
        lines.append("")
        verified = seed.get("verified")
        lines.append(
            f"Seed: {tc.fmt_s(seed.get('seed_s'))} to build and upload base; clean-tree probe "
            + ("verified (0 misses)." if verified else f"**NOT verified** ({seed.get('misses', '?')} misses).")
        )
        lines.append("")
        lines.append("| Scenario | Truth (tests run) | bazel-diff | probe overreport | probe precise |")
        lines.append("|---|---:|---:|---:|---:|")
        for sc in wl.get("scenarios", []):
            acc = sc.get("accuracy", {})
            lines.append(
                f"| {sc['name']} | {sc.get('truth', {}).get('count', 'n/a')} | {_cell(acc.get('bazel_diff'))} "
                f"| {_cell(acc.get('overreport'))} | {_cell(acc.get('precise'))} |"
            )
        lines.append("")
        lines.append(
            "| Scenario | truth run | bazel-diff cold | bazel-diff warm | probe cold (2 passes) | probe cold (pass 2 only) "
            "| probe warm | precise (build + probe) |"
        )
        lines.append("|---|---:|---:|---:|---:|---:|---:|---:|")
        for sc in wl.get("scenarios", []):
            t = sc.get("timing", {})

            def med(key: str) -> str:
                return tc.fmt_s(t.get(key, {}).get("median_s"))

            lines.append(
                f"| {sc['name']} | {tc.fmt_s(sc.get('truth', {}).get('time_s'))} | {med('bazel_diff_cold')} "
                f"| {med('bazel_diff_warm')} | {med('probe_cold')} | {med('probe_cold_single')} | {med('probe_warm')} | {med('precise')} |"
            )
        lines.append("")
        for sc in wl.get("scenarios", []):
            st = sc.get("probe_breakdown", {})
            if st:
                lines.append(
                    f"- {sc['name']} probe (cold medians): pass 1 {tc.fmt_s(st.get('pass1_s'))}, pass 2 "
                    f"{tc.fmt_s(st.get('pass2_s'))}, log parse {tc.fmt_s(st.get('parse_s'))}, rdeps "
                    f"{tc.fmt_s(st.get('attribute_s'))}; frontier {st.get('frontier', '?')} misses, "
                    f"{st.get('lookups', '?')} lookups."
                )
        lines.append("")
    failures = report.get("failures", [])
    if failures:
        lines.append("### Failures")
        lines.append("")
        for f in failures:
            lines.append(f"- **{f.get('where', '?')}**: {f.get('error', '')}")
        lines.append("")
    unsound = report.get("unsound", [])
    if unsound:
        lines.append("### Soundness failures")
        lines.append("")
        for u in unsound:
            lines.append(f"- {u}")
        lines.append("")
    return "\n".join(lines)


# ----------------------------------------------------------------------------------------------
# Fixtures: bazel-remote and the dummy executor
# ----------------------------------------------------------------------------------------------


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def install_fixtures(fixture_dir: Path, go: str) -> dict[str, Path]:
    """`go install` bazel-remote (pinned) and `go build` the dummy executor into fixture_dir."""
    fixture_dir.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, GOBIN=str(fixture_dir), GOTOOLCHAIN=os.environ.get("GOTOOLCHAIN", "auto"))
    bazel_remote = fixture_dir / "bazel-remote"
    if not bazel_remote.exists():
        tc.log(f"installing {BAZEL_REMOTE_MODULE} into {fixture_dir}")
        tc.run([go, "install", BAZEL_REMOTE_MODULE], env=env)
    dummy = fixture_dir / "dummy-executor"
    if not dummy.exists():
        tc.log(f"building dummy executor from {DUMMY_EXECUTOR_SRC}")
        tc.run([go, "build", "-o", dummy, "."], cwd=DUMMY_EXECUTOR_SRC, env=env)
    return {"bazel_remote": bazel_remote, "dummy_executor": dummy}


class CacheServer:
    """A bazel-remote on fixed loopback ports, with directory snapshot/restore between iterations."""

    def __init__(self, binary: Path, root: Path, grpc_port: int, http_port: int):
        self.binary = binary
        self.dir = root / "cache"
        self.snapshot_dir = root / "cache-snapshot"
        self.log_path = root / "bazel-remote.log"
        self.grpc_port = grpc_port
        self.http_port = http_port
        self.proc: subprocess.Popen | None = None
        self.uri = f"grpc://127.0.0.1:{grpc_port}"

    def start(self) -> None:
        self.dir.mkdir(parents=True, exist_ok=True)
        log = open(self.log_path, "ab")
        self.proc = subprocess.Popen(
            [
                str(self.binary), "--dir", str(self.dir), "--max_size", "4",
                "--grpc_address", f"127.0.0.1:{self.grpc_port}", "--http_address", f"127.0.0.1:{self.http_port}",
                "--access_log_level", "none",
            ],
            stdout=log, stderr=subprocess.STDOUT,
        )
        tc.wait_for_port(self.grpc_port, 30.0, self.proc)

    def stop(self) -> None:
        if self.proc is None:
            return
        self.proc.terminate()
        try:
            self.proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.proc = None
        # Let the kernel release the listening socket before a restart binds the same port.
        deadline = time.monotonic() + 10
        while tc.port_in_use(self.grpc_port) and time.monotonic() < deadline:
            time.sleep(0.1)

    def snapshot(self) -> None:
        self.stop()
        if self.snapshot_dir.exists():
            shutil.rmtree(self.snapshot_dir)
        shutil.copytree(self.dir, self.snapshot_dir)
        self.start()

    def restore(self) -> None:
        self.stop()
        shutil.rmtree(self.dir)
        shutil.copytree(self.snapshot_dir, self.dir)
        self.start()


class DummyExecutor:
    def __init__(self, binary: Path, root: Path):
        self.binary = binary
        self.log_path = root / "dummy-executor.log"
        self.proc: subprocess.Popen | None = None
        self.uri = ""

    def start(self) -> None:
        self.proc = subprocess.Popen(
            [str(self.binary), "--addr", "127.0.0.1:0"],
            stdout=subprocess.PIPE, stderr=open(self.log_path, "ab"), text=True,
        )
        assert self.proc.stdout is not None
        line = self.proc.stdout.readline().strip()
        if not line.startswith("grpc://"):
            raise RuntimeError(f"dummy executor did not report its address: {line!r}")
        self.uri = line

    def stop(self) -> dict[str, Any]:
        if self.proc is None:
            return {}
        self.proc.terminate()
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.proc = None
        text = self.log_path.read_text() if self.log_path.exists() else ""
        execute_calls = sum(1 for ln in text.splitlines() if "EXECUTE #" in ln)
        return {"execute_calls": execute_calls}


# ----------------------------------------------------------------------------------------------
# Bazel driving
# ----------------------------------------------------------------------------------------------


@dataclass
class Bazel:
    binary: str
    workspace: Path
    cache_uri: str
    # --nohome_rc keeps a developer's ~/.bazelrc (BES, caches) out of the measurement; bazel-diff is
    # given the same startup option so both share one server instead of restarting it.
    startup: tuple[str, ...] = ("--nohome_rc",)

    def run(self, args: list[str], check: bool = True, capture_to: Path | None = None) -> subprocess.CompletedProcess:
        proc = tc.run([self.binary, *self.startup, *args], cwd=self.workspace, check=False)
        if capture_to is not None:
            capture_to.write_text(proc.stdout)
        if check and proc.returncode != 0:
            tail = "\n".join(proc.stdout.splitlines()[-30:])
            raise RuntimeError(f"bazel {' '.join(args[:2])} failed ({proc.returncode}):\n{tail}")
        return proc

    def shutdown(self) -> None:
        self.run(["shutdown"], check=False)

    def clean(self) -> None:
        self.run(["clean"])

    @property
    def cache_flag(self) -> str:
        return f"--remote_cache={self.cache_uri}"


def enumerate_tests(bz: Bazel) -> set[str]:
    proc = bz.run(["query", "tests(//...)", "--output=label"])
    return {ln.strip() for ln in proc.stdout.splitlines() if ln.strip().startswith("//")}


def seed_cache(bz: Bazel) -> float:
    """Build + test base and upload everything. `bazel clean` first: on a warm output base Bazel
    executes nothing and so uploads nothing (the one trap Aspect's docs call out, and the one this
    harness fell into first)."""
    bz.clean()
    secs, _ = tc.timed(lambda: bz.run(["test", "//...", bz.cache_flag, "--remote_upload_local_results"]))
    return secs


def run_probe(bz: Bazel, exec_uri: str, out_dir: Path, name: str, pass1: bool, anchor: str) -> dict[str, Any]:
    """The two-pass require-cached probe. Returns timings and the decoded verdict, or an error."""
    out_dir.mkdir(parents=True, exist_ok=True)
    log_path = out_dir / f"{name}.grpc.binpb"
    if log_path.exists():
        log_path.unlink()
    result: dict[str, Any] = {"ok": True}
    common = [exec_uri and f"--remote_executor={exec_uri}", bz.cache_flag, "--experimental_remote_require_cached",
              "--keep_going", "--remote_download_outputs=minimal"]
    common = [c for c in common if c]
    t_start = time.monotonic()
    if pass1:
        result["pass1_s"], _ = tc.timed(
            lambda: bz.run(["build", anchor, *common, SALT_FLAG], check=False, capture_to=out_dir / f"{name}.pass1.log")
        )
    else:
        result["pass1_s"] = 0.0
    result["pass2_s"], proc = tc.timed(
        lambda: bz.run(
            ["test", "//...", *common, "--nouse_action_cache", f"--remote_grpc_log={log_path}"],
            check=False, capture_to=out_dir / f"{name}.pass2.log",
        )
    )
    if not log_path.exists():
        result.update(ok=False, error="probe produced no gRPC log (Bazel failed before any cache lookup)",
                      stderr_tail="\n".join(proc.stdout.splitlines()[-20:]))
        return result
    unexpected = probe_unexpected_errors(proc.stdout)
    if unexpected:
        result.update(ok=False, error="probe hit errors other than require-cached denials", stderr_tail="\n".join(unexpected[:10]))
        return result
    result["parse_s"], parsed = tc.timed(lambda: parse_grpc_log(log_path.read_bytes()))
    result["log_bytes"] = log_path.stat().st_size
    result["methods"] = parsed["methods"]
    result["verdict"] = verdict(parsed["lookups"])
    result["total_s"] = time.monotonic() - t_start
    log_path.unlink()
    return result


def attribute_overreport(bz: Bazel, probe: dict[str, Any], tests: set[str], out_dir: Path) -> dict[str, Any]:
    """overreport affected set: tests whose own action missed, plus tests(rdeps(frontier))."""
    misses: dict[str, str] = probe["verdict"]["misses"]
    own = {lb for lb, mn in misses.items() if mn == TEST_MNEMONIC and lb in tests}
    frontier = sorted(lb for lb, mn in misses.items() if lb not in own and lb.startswith("//"))
    attributed: set[str] = set()
    secs = 0.0
    if frontier:
        query_file = out_dir / "rdeps.query"
        query_file.write_text(rdeps_query(frontier))
        secs, proc = tc.timed(lambda: bz.run(["query", f"--query_file={query_file}", "--output=label"]))
        attributed = {ln.strip() for ln in proc.stdout.splitlines() if ln.strip().startswith("//")} & tests
    return {"affected": sorted(own | attributed), "own_misses": len(own), "frontier": len(frontier), "attribute_s": secs}


def precise_affected(probe: dict[str, Any], tests: set[str]) -> list[str]:
    misses: dict[str, str] = probe["verdict"]["misses"]
    return sorted(lb for lb, mn in misses.items() if mn == TEST_MNEMONIC and lb in tests)


def run_bazel_diff(bazel_diff: Path, bz: Bazel, base_sha: str, head_sha: str, out_dir: Path) -> dict[str, Any]:
    """The README recipe, with bazel-diff sharing the harness's Bazel startup options."""
    out_dir.mkdir(parents=True, exist_ok=True)
    base_json, head_json, deps_json, impacted = (out_dir / n for n in ("base.json", "head.json", "deps.json", "impacted.json"))
    startup = ["--bazelStartupOptions", " ".join(bz.startup)] if bz.startup else []
    common = ["-w", bz.workspace, "-b", bz.binary, *startup]
    stages: dict[str, float] = {}
    t0 = time.monotonic()
    try:
        stages["checkout_base"], _ = tc.timed(lambda: tc.git(bz.workspace, "checkout", "--quiet", "--force", base_sha))
        stages["generate_base"], _ = tc.timed(lambda: tc.run([bazel_diff, "generate-hashes", *common, base_json]))
        stages["checkout_head"], _ = tc.timed(lambda: tc.git(bz.workspace, "checkout", "--quiet", "--force", head_sha))
        stages["generate_head"], _ = tc.timed(lambda: tc.run([bazel_diff, "generate-hashes", *common, "-d", deps_json, head_json]))
        stages["diff"], _ = tc.timed(
            lambda: tc.run([bazel_diff, "get-impacted-targets", *common, "-sh", base_json, "-fh", head_json, "-d", deps_json, "-o", impacted])
        )
    except RuntimeError as e:
        return {"ok": False, "error": str(e), "stages": stages, "total_s": time.monotonic() - t0}
    labels = tc.parse_impacted_targets(impacted.read_text())
    return {"ok": True, "total_s": time.monotonic() - t0, "stages": stages, "labels": labels}


def ground_truth(bz: Bazel, out_dir: Path) -> dict[str, Any]:
    """What CI pays without a selector: a cold `bazel test //...` against the seeded cache, from a
    clean output base so local state cannot stand in for the cache. No upload, so the cache stays
    the seed."""
    bz.clean()
    bz.shutdown()
    # --remote_upload_local_results defaults to true: say no explicitly so head's results never
    # enter the cache (the snapshot restore would drop them anyway; this keeps the run honest).
    secs, proc = tc.timed(
        lambda: bz.run(["test", "//...", bz.cache_flag, "--noremote_upload_local_results"], check=False, capture_to=out_dir / "truth.log")
    )
    classes = parse_test_lines(proc.stdout)
    ok = proc.returncode == 0 and not classes["not_built"] and not classes["other"]
    return {"ok": ok, "time_s": secs, "executed": classes["executed"], "count": len(classes["executed"]),
            "cached": len(classes["cached"]), "not_built": classes["not_built"][:20], "exit": proc.returncode}


# ----------------------------------------------------------------------------------------------
# Workloads and scenarios
# ----------------------------------------------------------------------------------------------


@dataclass
class Scenario:
    name: str
    sha: str
    file: str


@dataclass
class Workload:
    name: str
    params: dict[str, int]
    path: Path
    base_sha: str
    meta: dict[str, Any]
    scenarios: list[Scenario] = field(default_factory=list)


def make_workload(name: str, params: dict[str, int], workdir: Path, bazel_version: str) -> Workload:
    gen = tc.load_gen_project()
    out = workdir / "workspaces" / name
    tc.log(f"[{name}] generating synthetic workspace with tests: {params}")
    meta = gen.gen_workspace(
        out, packages=params["packages"], targets_per_package=params["targets_per_package"],
        sources_per_package=params.get("sources_per_package", 3), fanin=params.get("fanin", 4),
        layers=params["layers"], bazel_version=bazel_version, tests_per_package=TESTS_PER_PACKAGE,
    )
    git_meta = gen.init_git(out, meta["deep_source"])  # base + change(leaf_edit) commits
    wl = Workload(name=name, params=params, path=out, base_sha=git_meta["base_sha"], meta=meta)
    wl.scenarios.append(Scenario("leaf_edit", git_meta["target_sha"], meta["deep_source"]))
    for scenario, rel in (("identical_output", meta["deep_notes"]), ("wide_edit", meta["wide_source"])):
        tc.git(out, "checkout", "--quiet", "--force", wl.base_sha)
        with (out / rel).open("a") as f:
            f.write(f"mutated for {scenario}\n")
        tc.git(out, "add", "-A")
        tc.git(out, "commit", "--quiet", "-m", scenario)
        wl.scenarios.append(Scenario(scenario, tc.git(out, "rev-parse", "HEAD"), rel))
    tc.git(out, "checkout", "--quiet", "--force", wl.base_sha)
    return wl


def bench_workload(wl: Workload, args: argparse.Namespace, bazel_diff: Path, fixtures: dict[str, Path], workdir: Path,
                   failures: list[dict[str, Any]], unsound: list[str]) -> dict[str, Any]:
    root = workdir / "runs" / wl.name
    root.mkdir(parents=True, exist_ok=True)
    cache = CacheServer(fixtures["bazel_remote"], root, free_port(), free_port())
    dummy = DummyExecutor(fixtures["dummy_executor"], root)
    report: dict[str, Any] = {
        "name": wl.name, "params": wl.params, "rule_targets": wl.meta.get("rule_targets"),
        "test_targets": wl.meta.get("test_targets"), "seed": {}, "scenarios": [],
    }
    bz = Bazel(args.bazel, wl.path, "")
    try:
        cache.start()
        dummy.start()
        bz = Bazel(args.bazel, wl.path, cache.uri)
        anchor = f"//pkg/p00000:t0"

        tc.git(wl.path, "checkout", "--quiet", "--force", wl.base_sha)
        tests = enumerate_tests(bz)
        tc.log(f"[{wl.name}] {len(tests)} tests; seeding the cache from base")
        seed_s = seed_cache(bz)
        bz.shutdown()
        check = run_probe(bz, dummy.uri, root / "seed", "verify", pass1=False, anchor=anchor)
        misses = len(check.get("verdict", {}).get("misses", {})) if check["ok"] else None
        report["seed"] = {"seed_s": round(seed_s, 3), "verified": check["ok"] and misses == 0, "misses": misses,
                          "lookups": check.get("verdict", {}).get("looked_up"), "error": check.get("error")}
        tc.log(f"[{wl.name}] seed {tc.fmt_s(seed_s)}; verification probe: {misses} misses")
        if not report["seed"]["verified"]:
            failures.append({"where": f"{wl.name}/seed", "error": f"seed verification failed: {check.get('error') or f'{misses} misses on a clean tree'}"})
            return report
        cache.snapshot()

        for sc in wl.scenarios:
            sc_dir = root / sc.name
            sc_dir.mkdir(parents=True, exist_ok=True)
            tc.log(f"[{wl.name}/{sc.name}] ground truth")
            cache.restore()
            tc.git(wl.path, "checkout", "--quiet", "--force", sc.sha)
            truth = ground_truth(bz, sc_dir)
            sc_report: dict[str, Any] = {"name": sc.name, "file": sc.file, "truth": {k: v for k, v in truth.items() if k != "executed"}}
            if not truth["ok"]:
                failures.append({"where": f"{wl.name}/{sc.name}/truth", "error": f"ground-truth run not clean (exit {truth['exit']}, not built: {truth['not_built'][:5]})"})
                report["scenarios"].append(sc_report)
                continue
            truth_set = set(truth["executed"])
            tc.log(f"[{wl.name}/{sc.name}] truth: {len(truth_set)} of {len(tests)} tests ran in {tc.fmt_s(truth['time_s'])}")

            samples: dict[str, list[float]] = {
                k: [] for k in ("bazel_diff_cold", "bazel_diff_warm", "probe_cold", "probe_cold_single", "probe_warm", "precise")
            }
            breakdown: dict[str, list[float]] = {k: [] for k in ("pass1_s", "pass2_s", "parse_s", "attribute_s")}
            last: dict[str, Any] = {}
            for it in range(args.iterations):
                cache.restore()
                for phase in PHASES:
                    if phase == "cold":
                        bz.shutdown()
                    r = run_bazel_diff(bazel_diff, bz, wl.base_sha, sc.sha, sc_dir / "bazel-diff")
                    if not r["ok"]:
                        failures.append({"where": f"{wl.name}/{sc.name}/bazel-diff/{phase}", "error": r["error"]})
                        continue
                    samples[f"bazel_diff_{phase}"].append(r["total_s"])
                    last["bazel_diff"] = r
                for phase in PHASES:
                    if phase == "cold":
                        bz.shutdown()
                    pr = run_probe(bz, dummy.uri, sc_dir / "probe", f"{phase}{it}", pass1=True, anchor=anchor)
                    if not pr["ok"]:
                        failures.append({"where": f"{wl.name}/{sc.name}/probe/{phase}", "error": f"{pr['error']}\n{pr.get('stderr_tail', '')}"})
                        continue
                    at = attribute_overreport(bz, pr, tests, sc_dir / "probe")
                    total = pr["total_s"] + at["attribute_s"]
                    samples[f"probe_{phase}"].append(total)
                    if phase == "cold":
                        for k in ("pass1_s", "pass2_s", "parse_s"):
                            breakdown[k].append(pr[k])
                        breakdown["attribute_s"].append(at["attribute_s"])
                        last["probe"] = {**{k: v for k, v in pr.items() if k != "verdict"}, "frontier": at["frontier"],
                                         "own_misses": at["own_misses"], "lookups": pr["verdict"]["looked_up"]}
                        last["overreport"] = at["affected"]
                # A fresh server has nothing memoized, so pass 1 is redundant there: this is what a
                # cold CI runner would actually pay for the probe.
                bz.shutdown()
                pr = run_probe(bz, dummy.uri, sc_dir / "probe", f"single{it}", pass1=False, anchor=anchor)
                if pr["ok"]:
                    at = attribute_overreport(bz, pr, tests, sc_dir / "probe")
                    samples["probe_cold_single"].append(pr["total_s"] + at["attribute_s"])
                else:
                    failures.append({"where": f"{wl.name}/{sc.name}/probe/cold-single", "error": f"{pr['error']}\n{pr.get('stderr_tail', '')}"})
                # precise: build the frontier with upload (executes real actions), then probe. From a
                # clean output base, like a CI runner: on a warm one Bazel would find the frontier
                # already built by the ground-truth run, execute nothing and upload nothing, and the
                # probe would still see every frontier action denied.
                bz.clean()
                (sc_dir / "precise").mkdir(parents=True, exist_ok=True)
                build_s, bp = tc.timed(
                    lambda: bz.run(["build", "//...", bz.cache_flag, "--remote_upload_local_results", "--keep_going"],
                                   check=False, capture_to=sc_dir / "precise" / f"build{it}.log")
                )
                if bp.returncode != 0:
                    failures.append({"where": f"{wl.name}/{sc.name}/precise/build", "error": "\n".join(bp.stdout.splitlines()[-10:])})
                    continue
                pr = run_probe(bz, dummy.uri, sc_dir / "precise", f"probe{it}", pass1=True, anchor=anchor)
                if not pr["ok"]:
                    failures.append({"where": f"{wl.name}/{sc.name}/precise/probe", "error": f"{pr['error']}\n{pr.get('stderr_tail', '')}"})
                    continue
                samples["precise"].append(build_s + pr["total_s"])
                last["precise"] = precise_affected(pr, tests)
                last["precise_build_s"] = build_s

            acc: dict[str, Any] = {}
            if "bazel_diff" in last:
                acc["bazel_diff"] = accuracy(truth_set, set(last["bazel_diff"]["labels"]) & tests)
            if "overreport" in last:
                acc["overreport"] = accuracy(truth_set, set(last["overreport"]))
            if "precise" in last:
                acc["precise"] = accuracy(truth_set, set(last["precise"]))
            for selector, a in acc.items():
                if a["recall"] < 1.0:
                    unsound.append(f"{wl.name}/{sc.name}: {selector} missed {len(a['missed'])} test(s) that ground truth ran, e.g. {a['missed'][:3]}")
            sc_report.update(
                accuracy=acc,
                timing={k: summarize(v) for k, v in samples.items()},
                bazel_diff_stages=last.get("bazel_diff", {}).get("stages"),
                probe_breakdown={**{k: (tc.median(v) if v else None) for k, v in breakdown.items()},
                                 "frontier": last.get("probe", {}).get("frontier"), "lookups": last.get("probe", {}).get("lookups"),
                                 "log_bytes": last.get("probe", {}).get("log_bytes"), "precise_build_s": last.get("precise_build_s")},
            )
            tc.log(f"[{wl.name}/{sc.name}] " + " | ".join(f"{k}: {_cell(v)}" for k, v in acc.items()))
            report["scenarios"].append(sc_report)
    except (RuntimeError, OSError) as e:
        failures.append({"where": wl.name, "error": str(e)})
    finally:
        bz.shutdown()
        report["dummy_executor"] = dummy.stop()
        cache.stop()
        if report.get("dummy_executor", {}).get("execute_calls"):
            failures.append({"where": f"{wl.name}/executor", "error": f"the dummy executor received {report['dummy_executor']['execute_calls']} Execute call(s); the probe is supposed to execute nothing"})
    return report


def collect_environment(bazel: str, bazel_diff: Path, wl: Workload | None) -> dict[str, Any]:
    env: dict[str, Any] = {
        "platform": f"{platform.system()} {platform.machine()}",
        "cpu_count": os.cpu_count(),
        "python": platform.python_version(),
        "bazel_remote": BAZEL_REMOTE_MODULE.rsplit("/", 1)[-1],
        "tests_per_package": TESTS_PER_PACKAGE,
    }
    try:
        env["bazel_diff_version"] = tc.run([bazel_diff, "--version"]).stdout.strip().split()[-1]
    except (RuntimeError, IndexError):
        env["bazel_diff_version"] = "unknown"
    if wl is not None:
        try:
            out = tc.run([bazel, "--version"], cwd=wl.path).stdout.strip()
            env["bazel_version"] = out.split()[-1]
        except RuntimeError:
            env["bazel_version"] = "unknown"
    return env


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bazel", default="bazel", help="bazel/bazelisk binary (default: bazel)")
    ap.add_argument("--bazel-diff", type=Path, default=None, help=f"bazel-diff binary (default: {tc.LAUNCHER})")
    ap.add_argument("--skip-build", action="store_true", help="do not `bazel build` bazel-diff; use --bazel-diff / the existing launcher")
    ap.add_argument("--go", default="go", help="go binary for the fixtures (default: go)")
    ap.add_argument("--fixture-dir", type=Path, default=None, help="where bazel-remote and the dummy executor live / get built (default: <workdir>/bin)")
    ap.add_argument("--install-fixtures-only", action="store_true", help="build the fixtures into --fixture-dir and exit")
    ap.add_argument("--workload", action="append", default=[], metavar="NAME", help=f"synthetic preset: {', '.join(tc.SYNTHETIC_PRESETS)} (repeatable; default: small, medium, large)")
    ap.add_argument("--synthetic", action="append", default=[], type=tc.parse_synthetic_spec, metavar="NAME:PKGS:TPP[:LAYERS]", help="custom synthetic size")
    ap.add_argument("--bazel-version", default=None, help="Bazel version pinned in the synthetic workspaces (default: this repo's .bazelversion)")
    ap.add_argument("--iterations", type=int, default=3, help="timed iterations per scenario (default: 3)")
    ap.add_argument("--quick", action="store_true", help="tiny workload, 1 iteration (smoke profile)")
    ap.add_argument("--workdir", type=Path, default=None, help="scratch directory (default: a fresh temp dir)")
    ap.add_argument("--keep-workdir", action="store_true", help="leave the scratch directory on disk")
    ap.add_argument("--metrics-out", type=Path, default=None, help="write the full report JSON here")
    ap.add_argument("--summary-out", type=Path, default=None, help="write the markdown summary here")
    ap.add_argument("--require-sound", action="store_true", help="exit non-zero if any selector missed a ground-truth test")
    ap.add_argument("-v", "--verbose", action="store_true", help="echo every command")
    return ap


def main(argv: list[str]) -> int:
    args = build_parser().parse_args(argv)
    tc.VERBOSE = args.verbose

    workdir = args.workdir.resolve() if args.workdir else Path(tempfile.mkdtemp(prefix="cache-probe-"))
    workdir.mkdir(parents=True, exist_ok=True)
    fixture_dir = (args.fixture_dir or workdir / "bin").resolve()
    if args.install_fixtures_only:
        install_fixtures(fixture_dir, args.go)
        return 0

    if args.quick:
        if not args.workload and not args.synthetic:
            args.workload = ["tiny"]
        args.iterations = 1
    if not args.workload and not args.synthetic:
        args.workload = ["small", "medium", "large"]
    if args.iterations < 1:
        tc.log("--iterations must be >= 1")
        return 2

    bazel_diff = args.bazel_diff or tc.LAUNCHER
    if not args.skip_build and args.bazel_diff is None:
        tc.build_bazel_diff(args.bazel)
    if not Path(bazel_diff).exists():
        tc.log(f"bazel-diff binary not found at {bazel_diff}; pass --bazel-diff or drop --skip-build")
        return 2
    bazel_diff = Path(bazel_diff).resolve()
    bazel = shutil.which(args.bazel) or str(Path(args.bazel).expanduser().resolve())
    if not Path(bazel).exists():
        tc.log(f"bazel binary not found: {args.bazel}")
        return 2
    args.bazel = bazel
    bazel_version = args.bazel_version or (REPO_ROOT / ".bazelversion").read_text().strip()
    tc.log(f"workdir: {workdir}")

    failures: list[dict[str, Any]] = []
    unsound: list[str] = []
    report: dict[str, Any] = {"workloads": [], "failures": failures, "unsound": unsound}
    rc = 0
    try:
        fixtures = install_fixtures(fixture_dir, args.go)
        synthetic: list[tuple[str, dict[str, int]]] = []
        for name in args.workload:
            if name not in tc.SYNTHETIC_PRESETS:
                tc.log(f"unknown workload {name!r}; presets are {', '.join(tc.SYNTHETIC_PRESETS)}")
                return 2
            synthetic.append((name, tc.SYNTHETIC_PRESETS[name]))
        synthetic.extend(args.synthetic)
        workloads = [make_workload(name, params, workdir, bazel_version) for name, params in synthetic]
        report["environment"] = collect_environment(bazel, bazel_diff, workloads[0] if workloads else None)
        tc.log(f"environment: {json.dumps(report['environment'])}")
        for wl in workloads:
            report["workloads"].append(bench_workload(wl, args, bazel_diff, fixtures, workdir, failures, unsound))
    except RuntimeError as e:
        failures.append({"where": "harness", "error": str(e)})
    finally:
        summary = render_summary(report)
        print()
        print(summary)
        if args.metrics_out:
            args.metrics_out.write_text(json.dumps(report, indent=2, sort_keys=True))
            tc.log(f"metrics: {args.metrics_out}")
        if args.summary_out:
            args.summary_out.write_text(summary)
            tc.log(f"summary: {args.summary_out}")
        if not args.keep_workdir and args.workdir is None:
            shutil.rmtree(workdir, ignore_errors=True)
    if failures:
        rc = 1
    if args.require_sound and unsound:
        rc = 1
    return rc


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
