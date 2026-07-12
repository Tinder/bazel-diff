#!/usr/bin/env python3
"""Cron stress harness for the `bazel-diff serve` query service.

Where tools/serve_harness.py validates *functional* coverage (clone shapes, fetch paths),
this harness validates *operational* behavior under sustained load and injected faults, and
captures throughput / response-time metrics from every request it sends. It drives the real
built `bazel-diff` binary against a live `git://` remote (git daemon), just like the
functional harness, and layers on:

  load phases
    * hot        -- concurrent GET/POST bursts against cached SHA pairs (cache-hit throughput)
    * cold       -- concurrent identical requests for uncached pairs (checkout + `bazel query`
                    serialization: one generation, everyone gets the same answer)
    * error_mix  -- malformed / unknown-revision / wrong-method requests interleaved with good
                    ones (error paths must not disturb concurrent successes)

  fault-injection phases (the errors we force and the response we require)
    * lock_heal    -- an orphaned `<git-dir>/index.lock` is planted before a cold request; the
                      service must self-heal (delete + retry) and answer 200 (issue #425)
    * lock_storm   -- a background thread keeps re-planting `index.lock` while cold requests
                      run; every response must be a clean 200 (healed) or 400 (git error) --
                      never a hang, 5xx, or transport error -- and the service must recover to
                      200 once the storm stops
    * remote_outage-- the git daemon is killed; a request needing a fetch must fail fast with
                      400 (not hang, not lame-duck the instance), and succeed after the daemon
                      returns
    * lameduck     -- a fresh instance started while the remote is down must come up lame-duck
                      (health 503, queries 503) yet still serve /metrics

Throughout the run a background prober hits /health twice a second: readiness must never
regress on the main instance, even mid-storm/outage (per-request git errors are 400s, not
health failures).

Every request is timed. The run emits per-phase throughput (req/s) and latency percentiles
(p50/p90/p95/p99/max), plus /metrics snapshots at phase boundaries, as JSON (--metrics-out)
and a GitHub-flavored markdown summary (--summary-out) for a CI step summary.

Usage:
    tools/serve_stress.py                       # build, then run every phase
    tools/serve_stress.py --skip-build          # reuse an existing bazel-bin/cli/bazel-diff
    tools/serve_stress.py --quick               # reduced request counts (sanity profile)
    tools/serve_stress.py --only lock           # run only phases whose name contains "lock"
    tools/serve_stress.py --metrics-out m.json --summary-out s.md
    tools/serve_stress.py --bazel ~/go/bin/bazelisk    # specific bazel binary (CI)

Exit code is non-zero if any gating check fails. Pure Python stdlib; requires git (with
`git daemon`) and a bazel binary, same as the functional harness.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import math
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import traceback
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import serve_harness as base  # noqa: E402  (shared plumbing: remote, serve, report, http)

PASS, FAIL = base.PASS, base.FAIL


# ----------------------------------------------------------------------------------------------
# Metrics capture
# ----------------------------------------------------------------------------------------------


@dataclass
class Sample:
    """One timed request. [ok] means the response matched the phase's expectation."""

    phase: str
    op: str
    status: int | None  # None = transport error (connection refused/reset/timeout)
    latency_ms: float
    ok: bool
    detail: str = ""


class Metrics:
    """Thread-safe request-sample sink plus phase wall-clock windows."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.samples: list[Sample] = []
        self.phase_wall: dict[str, float] = {}
        self.server_snapshots: list[dict] = []
        self.time_to_ready_seconds: float | None = None

    def record(self, phase: str, op: str, status: int | None, latency_ms: float, ok: bool,
               detail: str = "") -> None:
        with self._lock:
            self.samples.append(Sample(phase, op, status, latency_ms, ok, detail))

    @contextlib.contextmanager
    def phase(self, name: str):
        t0 = time.perf_counter()
        try:
            yield
        finally:
            with self._lock:
                self.phase_wall[name] = self.phase_wall.get(name, 0.0) + (time.perf_counter() - t0)

    def snapshot_server(self, phase: str, port: int) -> dict | None:
        """Scrapes /metrics and stashes the parsed snapshot tagged with the phase name."""
        code, body = base.http(port, "/metrics", timeout=10)
        try:
            snap = json.loads(body) if code == 200 else None
        except (ValueError, TypeError):
            snap = None
        with self._lock:
            self.server_snapshots.append({"phase": phase, "status": code, "snapshot": snap})
        return snap

    # ------------------------------- aggregation -------------------------------

    def _phase_samples(self, phase: str) -> list[Sample]:
        return [s for s in self.samples if s.phase == phase]

    def phase_stats(self, phase: str) -> dict:
        samples = self._phase_samples(phase)
        lat = sorted(s.latency_ms for s in samples)
        wall = self.phase_wall.get(phase, 0.0)
        by_status: dict[str, int] = {}
        for s in samples:
            key = str(s.status) if s.status is not None else "transport"
            by_status[key] = by_status.get(key, 0) + 1
        return {
            "name": phase,
            "requests": len(samples),
            "ok": sum(1 for s in samples if s.ok),
            "mismatches": sum(1 for s in samples if not s.ok),
            "by_status": dict(sorted(by_status.items())),
            "wall_seconds": round(wall, 3),
            "rps": round(len(samples) / wall, 2) if wall > 0 and samples else 0.0,
            "latency_ms": _latency_summary(lat),
        }

    def health_stats(self) -> dict:
        probes = [s for s in self.samples if s.op == "health"]
        lat = sorted(s.latency_ms for s in probes)
        return {
            "probes": len(probes),
            "non_200": sum(1 for s in probes if s.status != 200),
            "latency_ms": _latency_summary(lat),
        }


def _latency_summary(sorted_ms: list[float]) -> dict:
    if not sorted_ms:
        return {}
    return {
        "min": round(sorted_ms[0], 1),
        "mean": round(statistics.fmean(sorted_ms), 1),
        "p50": round(_percentile(sorted_ms, 0.50), 1),
        "p90": round(_percentile(sorted_ms, 0.90), 1),
        "p95": round(_percentile(sorted_ms, 0.95), 1),
        "p99": round(_percentile(sorted_ms, 0.99), 1),
        "max": round(sorted_ms[-1], 1),
    }


def _percentile(sorted_vals: list[float], p: float) -> float:
    """Linear-interpolated percentile over an already-sorted list."""
    k = (len(sorted_vals) - 1) * p
    lo, hi = math.floor(k), math.ceil(k)
    if lo == hi:
        return sorted_vals[int(k)]
    return sorted_vals[lo] + (sorted_vals[hi] - sorted_vals[lo]) * (k - lo)


# ----------------------------------------------------------------------------------------------
# Timed HTTP helpers
# ----------------------------------------------------------------------------------------------


def timed_http(port: int, path: str, method: str = "GET", body: str | None = None,
               timeout: float = 600.0):
    """base.http plus wall-clock timing. Returns (status, body, latency_ms)."""
    t0 = time.perf_counter()
    code, text = base.http(port, path, method=method, timeout=timeout, body=body)
    return code, text, (time.perf_counter() - t0) * 1000.0


def impacted_get(s: base.Serve, frm: str, to: str, timeout: float = 600.0):
    code, text, ms = timed_http(s.port, f"/impacted_targets?from={frm}&to={to}", timeout=timeout)
    return code, text, ms


def impacted_post(s: base.Serve, frm: str, to: str, timeout: float = 600.0):
    body = json.dumps({"from": frm, "to": to})
    code, text, ms = timed_http(s.port, "/impacted_targets", method="POST", body=body,
                                timeout=timeout)
    return code, text, ms


def stable_set(text: str) -> frozenset:
    """The deterministic subset of impacted labels (see serve_harness._stable)."""
    try:
        data = json.loads(text)
    except (ValueError, TypeError):
        return frozenset()
    return frozenset(base._stable(data))


# ----------------------------------------------------------------------------------------------
# Background health prober
# ----------------------------------------------------------------------------------------------


class HealthProber:
    """Polls /health on an interval for the whole run; readiness must never regress."""

    def __init__(self, port: int, metrics: Metrics, interval: float = 0.5) -> None:
        self.port = port
        self.metrics = metrics
        self.interval = interval
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, name="health-prober", daemon=True)

    def start(self) -> "HealthProber":
        self._thread.start()
        return self

    def stop(self) -> None:
        self._stop.set()
        self._thread.join(timeout=5)

    def _run(self) -> None:
        while not self._stop.is_set():
            code, _, ms = timed_http(self.port, "/health", timeout=5)
            self.metrics.record("background", "health", code, ms, ok=(code == 200))
            self._stop.wait(self.interval)


# ----------------------------------------------------------------------------------------------
# Fault injectors
# ----------------------------------------------------------------------------------------------


def git_dir(clone: Path) -> Path:
    out = base.git(clone, "rev-parse", "--git-dir")
    p = Path(out)
    return p if p.is_absolute() else clone / p


def plant_index_lock(clone: Path) -> Path:
    """Creates an orphaned `<git-dir>/index.lock`, mimicking a git process killed mid-checkout.
    O_EXCL so a lock held by a genuinely live git process is never stomped."""
    lock = git_dir(clone) / "index.lock"
    try:
        fd = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        os.close(fd)
    except FileExistsError:
        pass
    return lock


class LockStorm:
    """Continuously re-plants index.lock on an interval until stopped."""

    def __init__(self, clone: Path, interval: float = 0.03) -> None:
        self.clone = clone
        self.interval = interval
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, name="lock-storm", daemon=True)
        self.plants = 0

    def __enter__(self) -> "LockStorm":
        self._thread.start()
        return self

    def __exit__(self, *exc) -> None:
        self._stop.set()
        self._thread.join(timeout=5)

    def _run(self) -> None:
        while not self._stop.is_set():
            with contextlib.suppress(OSError):
                plant_index_lock(self.clone)
                self.plants += 1
            self._stop.wait(self.interval)


def kill_daemon(remote: base.Remote) -> None:
    remote.daemon.terminate()
    with contextlib.suppress(Exception):
        remote.daemon.wait(timeout=10)
    # The port must actually be closed before outage assertions run.
    deadline = time.time() + 10
    while base.wait_port(remote.port, timeout=0.2) and time.time() < deadline:
        time.sleep(0.2)


def restart_daemon(remote: base.Remote) -> None:
    remote.daemon = subprocess.Popen(
        [
            "git",
            "daemon",
            "--reuseaddr",
            "--listen=127.0.0.1",
            f"--port={remote.port}",
            f"--base-path={remote.srv_base}",
            "--export-all",
            "--enable=upload-pack",
            "--informative-errors",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if not base.wait_port(remote.port, timeout=10):
        raise RuntimeError(f"git daemon did not come back up on port {remote.port}")


# ----------------------------------------------------------------------------------------------
# Load profile
# ----------------------------------------------------------------------------------------------


@dataclass
class Profile:
    hot_requests: int = 64
    hot_workers: int = 8
    cold_pairs: int = 4  # uncached SHA pairs hit during cold_concurrent
    cold_fanout: int = 4  # identical concurrent requests per cold pair
    error_batch: int = 40
    lock_heal_rounds: int = 3
    storm_commits: int = 4  # cold generations attempted while the lock storm runs
    outage_probes: int = 3

    @staticmethod
    def quick() -> "Profile":
        return Profile(hot_requests=16, hot_workers=4, cold_pairs=2, cold_fanout=3,
                       error_batch=12, lock_heal_rounds=2, storm_commits=2, outage_probes=2)


# ----------------------------------------------------------------------------------------------
# The stress run
# ----------------------------------------------------------------------------------------------

CORE_EDIT = {"//:core", "//:core.txt", "//:mid"}
OTHER_EDIT = {"//:other", "//:other.txt"}


@dataclass
class Ctx:
    remote: base.Remote
    clone: Path
    lameduck_clone: Path
    root: Path
    rep: base.Report
    metrics: Metrics
    profile: Profile
    serve: base.Serve = None  # set once the instance is up


def build_history(remote: base.Remote, profile: Profile) -> None:
    """Extends the remote's C0..C2 history with dedicated commit chains per phase, so every
    phase has its own guaranteed-uncached SHA pairs and cross-phase caching never hides a
    checkout. All chains land before the clone is made: the checkout-error phases must
    exercise the checkout path alone, with no fetch in the way."""
    sh = remote.shas
    for i in range(profile.cold_pairs):
        # Alternate the file edited so consecutive diffs stay small but distinct.
        kwargs = {"other": f"other cold {i}\n"} if i % 2 else {"core": base._block("A", {i})}
        sh[f"S{i}"] = remote.commit(f"S{i} cold chain", **kwargs)
    for i in range(profile.lock_heal_rounds):
        sh[f"L{i}"] = remote.commit(f"L{i} lock-heal chain", other=f"other lock {i}\n")
    for i in range(profile.storm_commits):
        sh[f"T{i}"] = remote.commit(f"T{i} storm chain", mid=f"mid storm {i}\n")
    remote.repack()


# ------------------------------------ phases ------------------------------------


def phase_startup(ctx: Ctx, health: str, elapsed: float) -> bool:
    case = "startup"
    ctx.metrics.time_to_ready_seconds = round(elapsed, 2)
    ok = ctx.rep.check(case, "instance becomes ready (initial fetch + bind)", health == "ready",
                       ok_detail=f"ready in {elapsed:.1f}s",
                       fail_detail=f"health={health}\n{ctx.serve.stderr_tail()}")
    ctx.metrics.snapshot_server(case, ctx.serve.port)
    return ok


def phase_warm(ctx: Ctx) -> None:
    """Primes the Bazel server and caches the two hot pairs; also the cold-start latency sample."""
    case = "warm"
    sh = ctx.remote.shas
    with ctx.metrics.phase(case):
        for frm, to, expect in ((sh["C0"], sh["C1"], CORE_EDIT), (sh["C1"], sh["C2"], CORE_EDIT)):
            code, text, ms = impacted_get(ctx.serve, frm, to)
            good = code == 200 and stable_set(text) == frozenset(expect)
            ctx.metrics.record(case, "impacted_cold", code, ms, good)
            ctx.rep.check(case, f"cold generation {frm[:8]}->{to[:8]}", good,
                          ok_detail=f"{ms/1000:.1f}s",
                          fail_detail=f"code={code} got={sorted(stable_set(text))} "
                                      f"body={text[:160]}\n{ctx.serve.stderr_tail()}")
    ctx.metrics.snapshot_server(case, ctx.serve.port)


def phase_hot(ctx: Ctx) -> None:
    """Concurrent GET/POST bursts against the two already-cached pairs."""
    case = "hot"
    sh = ctx.remote.shas
    pairs = [(sh["C0"], sh["C1"]), (sh["C1"], sh["C2"])]
    results: list[tuple[int | None, frozenset]] = []
    lock = threading.Lock()

    def one(i: int) -> None:
        frm, to = pairs[i % len(pairs)]
        fn = impacted_post if i % 3 == 0 else impacted_get  # mix in the POST body form
        code, text, ms = fn(ctx.serve, frm, to)
        good = code == 200
        ctx.metrics.record(case, "impacted_hot", code, ms, good)
        with lock:
            results.append((code, stable_set(text)))

    with ctx.metrics.phase(case):
        with ThreadPoolExecutor(max_workers=ctx.profile.hot_workers) as pool:
            list(pool.map(one, range(ctx.profile.hot_requests)))

    codes = {c for c, _ in results}
    sets = {fs for c, fs in results if c == 200}
    ctx.rep.check(case, f"{ctx.profile.hot_requests} concurrent cache-hit requests all 200",
                  codes == {200}, ok_detail=f"codes={codes}",
                  fail_detail=f"codes={codes}\n{ctx.serve.stderr_tail()}")
    ctx.rep.check(case, "hot responses consistent", len(sets) == 1 and next(iter(sets)) == frozenset(CORE_EDIT),
                  fail_detail=f"distinct_sets={[sorted(s) for s in sets]}")
    ctx.metrics.snapshot_server(case, ctx.serve.port)


def phase_cold_concurrent(ctx: Ctx) -> None:
    """For each uncached pair, [cold_fanout] identical concurrent requests: the workspace lock
    plus the per-key double-check must yield one generation and identical 200s for all."""
    case = "cold"
    sh = ctx.remote.shas
    with ctx.metrics.phase(case):
        prev = sh["C2"]
        for i in range(ctx.profile.cold_pairs):
            to = sh[f"S{i}"]
            outcomes: list[tuple[int | None, frozenset]] = []
            lock = threading.Lock()

            def one(_: int, frm: str = prev, dst: str = to) -> None:
                code, text, ms = impacted_get(ctx.serve, frm, dst)
                ctx.metrics.record(case, "impacted_cold", code, ms, code == 200)
                with lock:
                    outcomes.append((code, stable_set(text)))

            with ThreadPoolExecutor(max_workers=ctx.profile.cold_fanout) as pool:
                list(pool.map(one, range(ctx.profile.cold_fanout)))
            codes = {c for c, _ in outcomes}
            sets = {fs for _, fs in outcomes}
            ctx.rep.check(case, f"S{i}: {ctx.profile.cold_fanout} concurrent cold requests agree",
                          codes == {200} and len(sets) == 1,
                          ok_detail=f"{sorted(next(iter(sets)))}",
                          fail_detail=f"codes={codes} sets={len(sets)}\n{ctx.serve.stderr_tail()}")
            prev = to
    ctx.metrics.snapshot_server(case, ctx.serve.port)


def phase_error_mix(ctx: Ctx) -> None:
    """Bad requests under concurrency: each must map to its documented status, and the good
    requests interleaved with them must be untouched."""
    case = "error_mix"
    sh = ctx.remote.shas
    dead = "deadbeef" * 5
    good_pair = (sh["C0"], sh["C1"])

    # (op, expected status, thunk)
    def bad_sha():
        return timed_http(ctx.serve.port, f"/impacted_targets?from={sh['C1']}&to={dead}")

    def missing_params():
        return timed_http(ctx.serve.port, "/impacted_targets")

    def malformed_post():
        return timed_http(ctx.serve.port, "/impacted_targets", method="POST", body="{not json")

    def wrong_method():
        return timed_http(ctx.serve.port,
                          f"/impacted_targets?from={good_pair[0]}&to={good_pair[1]}", method="PUT")

    def distances_untracked():
        return timed_http(ctx.serve.port,
                          f"/impacted_targets_with_distances?from={good_pair[0]}&to={good_pair[1]}")

    def good():
        return timed_http(ctx.serve.port,
                          f"/impacted_targets?from={good_pair[0]}&to={good_pair[1]}")

    menu = [
        ("bad_sha", 400, bad_sha),
        ("missing_params", 400, missing_params),
        ("malformed_post", 400, malformed_post),
        ("wrong_method", 405, wrong_method),
        ("distances_untracked", 400, distances_untracked),
        ("good_interleaved", 200, good),
    ]

    mismatches: list[str] = []
    lock = threading.Lock()

    def one(i: int) -> None:
        op, want, thunk = menu[i % len(menu)]
        code, text, ms = thunk()
        good_resp = code == want
        ctx.metrics.record(case, op, code, ms, good_resp)
        if not good_resp:
            with lock:
                mismatches.append(f"{op}: want {want} got {code} body={text[:120]}")

    with ctx.metrics.phase(case):
        with ThreadPoolExecutor(max_workers=ctx.profile.hot_workers) as pool:
            list(pool.map(one, range(ctx.profile.error_batch)))

    ctx.rep.check(case, f"{ctx.profile.error_batch} mixed bad/good requests -> documented statuses",
                  not mismatches, fail_detail="; ".join(mismatches[:5]))
    ctx.metrics.snapshot_server(case, ctx.serve.port)


def phase_lock_heal(ctx: Ctx) -> None:
    """Plant an orphaned index.lock, then force a checkout (cold request). The service must
    clear the stale lock, retry, and answer 200 -- the #425 self-heal path."""
    case = "lock_heal"
    sh = ctx.remote.shas
    prev = sh[f"S{ctx.profile.cold_pairs - 1}"]
    with ctx.metrics.phase(case):
        for i in range(ctx.profile.lock_heal_rounds):
            to = sh[f"L{i}"]
            lock_path = plant_index_lock(ctx.clone)
            code, text, ms = impacted_get(ctx.serve, prev, to)
            healed = code == 200 and not lock_path.exists()
            ctx.metrics.record(case, "impacted_cold_locked", code, ms, healed)
            ctx.rep.check(case, f"L{i}: orphaned index.lock self-heals -> 200",
                          healed,
                          ok_detail=f"{ms/1000:.1f}s",
                          fail_detail=f"code={code} lock_still_there={lock_path.exists()} "
                                      f"body={text[:160]}\n{ctx.serve.stderr_tail()}")
            prev = to
    healed_logs = ctx.serve.stderr_path.read_text("utf-8", "replace").count(
        "cleared stale git index.lock")
    ctx.rep.check(case, "self-heal path logged for each planted lock",
                  healed_logs >= ctx.profile.lock_heal_rounds,
                  ok_detail=f"{healed_logs} heals logged",
                  fail_detail=f"expected >= {ctx.profile.lock_heal_rounds}, got {healed_logs}")
    ctx.metrics.snapshot_server(case, ctx.serve.port)


def phase_lock_storm(ctx: Ctx) -> None:
    """Re-plant index.lock every ~30ms while cold requests force checkouts. Every response must
    be a clean 200 or 400 (no hang, no 5xx, no dropped connection); once the storm stops,
    anything that failed must succeed on retry -- even with a final orphaned lock left behind."""
    case = "lock_storm"
    sh = ctx.remote.shas
    prev = sh[f"L{ctx.profile.lock_heal_rounds - 1}"]
    attempted: list[tuple[str, str, int | None]] = []
    with ctx.metrics.phase(case):
        with LockStorm(ctx.clone) as storm:
            for i in range(ctx.profile.storm_commits):
                to = sh[f"T{i}"]
                code, text, ms = impacted_get(ctx.serve, prev, to)
                acceptable = code in (200, 400)
                ctx.metrics.record(case, "impacted_cold_storm", code, ms, acceptable,
                                   detail="" if acceptable else text[:120])
                attempted.append((prev, to, code))
                prev = to
        plants = storm.plants
        # Storm over (a final orphaned lock may remain -- the service must heal it itself).
        # Every pair that failed mid-storm must now succeed.
        retry_bad: list[str] = []
        for frm, to, code in attempted:
            if code == 200:
                continue
            rcode, rtext, rms = impacted_get(ctx.serve, frm, to)
            ctx.metrics.record(case, "impacted_retry_after_storm", rcode, rms, rcode == 200)
            if rcode != 200:
                retry_bad.append(f"{frm[:8]}->{to[:8]}: {rcode} {rtext[:100]}")

    codes = {c for _, _, c in attempted}
    ctx.rep.check(case, f"storm responses all clean 200/400 ({plants} locks planted)",
                  codes <= {200, 400},
                  ok_detail=f"codes={codes}",
                  fail_detail=f"codes={codes}\n{ctx.serve.stderr_tail()}")
    ctx.rep.check(case, "every storm failure succeeds on post-storm retry", not retry_bad,
                  fail_detail="; ".join(retry_bad) + "\n" + ctx.serve.stderr_tail())
    ctx.metrics.snapshot_server(case, ctx.serve.port)


def phase_remote_outage(ctx: Ctx) -> None:
    """Kill the git daemon: a request that needs a fetch must fail fast with a 400 git error
    (and never lame-duck the ready instance); after the daemon returns, the same request must
    succeed via the on-demand refetch."""
    case = "remote_outage"
    sh = ctx.remote.shas
    # Landed on the remote after the clone was made, so serving it requires a fetch.
    sh["Z0"] = ctx.remote.commit("Z0 post-outage commit", other="other outage\n")
    frm = sh[f"T{ctx.profile.storm_commits - 1}"]

    with ctx.metrics.phase(case):
        kill_daemon(ctx.remote)
        outage_codes: list[int | None] = []
        for _ in range(ctx.profile.outage_probes):
            code, text, ms = impacted_get(ctx.serve, frm, sh["Z0"], timeout=120)
            ctx.metrics.record(case, "impacted_fetch_outage", code, ms, code == 400)
            outage_codes.append(code)
        ctx.rep.check(case, "fetch-requiring request during outage -> 400 (fail fast)",
                      set(outage_codes) == {400},
                      fail_detail=f"codes={outage_codes}\n{ctx.serve.stderr_tail()}")

        hcode, _, hms = timed_http(ctx.serve.port, "/health", timeout=5)
        ctx.metrics.record(case, "health", hcode, hms, hcode == 200)
        ctx.rep.check(case, "instance stays ready through the outage", hcode == 200,
                      fail_detail=f"health={hcode}")

        phase_lameduck(ctx)  # while the daemon is still down

        restart_daemon(ctx.remote)
        code, text, ms = impacted_get(ctx.serve, frm, sh["Z0"])
        good = code == 200 and stable_set(text) == frozenset(OTHER_EDIT)
        ctx.metrics.record(case, "impacted_fetch_recovered", code, ms, good)
        ctx.rep.check(case, "same request succeeds after daemon returns", good,
                      ok_detail=f"{ms/1000:.1f}s",
                      fail_detail=f"code={code} got={sorted(stable_set(text))} "
                                  f"body={text[:160]}\n{ctx.serve.stderr_tail()}")
    ctx.metrics.snapshot_server(case, ctx.serve.port)


def phase_lameduck(ctx: Ctx) -> None:
    """A fresh instance started while the remote is unreachable must lame-duck: health 503,
    queries 503, /metrics still served with ready=false."""
    case = "lameduck"
    sh = ctx.remote.shas
    cache = ctx.root / "cache-lameduck"
    with ctx.metrics.phase(case):
        with base.serve(ctx.lameduck_clone, cache, initial_fetch=True, track_deps=False,
                        ready_timeout=15) as (s2, health):
            ctx.rep.check(case, "startup with remote down -> lame-duck (503, never ready)",
                          health == "lameduck",
                          fail_detail=f"health={health}\n{s2.stderr_tail()}")
            qcode, qtext, qms = impacted_get(s2, sh["C0"], sh["C1"], timeout=30)
            ctx.metrics.record(case, "impacted_lameduck", qcode, qms, qcode == 503)
            ctx.rep.check(case, "query against lame-duck instance -> 503", qcode == 503,
                          fail_detail=f"code={qcode} body={qtext[:120]}")
            mcode, mtext, mms = timed_http(s2.port, "/metrics", timeout=10)
            ctx.metrics.record(case, "metrics_lameduck", mcode, mms, mcode == 200)
            try:
                ready_flag = json.loads(mtext).get("ready")
            except (ValueError, TypeError):
                ready_flag = None
            ctx.rep.check(case, "/metrics still served while lame-ducked (ready=false)",
                          mcode == 200 and ready_flag is False,
                          fail_detail=f"code={mcode} ready={ready_flag} body={mtext[:160]}")


def phase_final(ctx: Ctx) -> None:
    """End-of-run integrity: /metrics sane, health never regressed, no unexplained 5xx."""
    case = "final"
    snap = ctx.metrics.snapshot_server(case, ctx.serve.port)
    entries = (snap or {}).get("cache", {}).get("entries")
    ctx.rep.check(case, "cache populated over the run", bool(entries),
                  ok_detail=f"{entries} entries",
                  fail_detail=f"snapshot={json.dumps(snap)[:200]}")

    health = ctx.metrics.health_stats()
    ctx.rep.check(case, "health prober saw zero non-200s across all phases",
                  health["probes"] > 0 and health["non_200"] == 0,
                  ok_detail=f"{health['probes']} probes",
                  fail_detail=f"{health['non_200']}/{health['probes']} probes failed")

    # 503s asserted by the lame-duck phase are expected (ok=True); anything else >= 500 is not.
    fivehundreds = [s for s in ctx.metrics.samples
                    if s.status is not None and s.status >= 500 and not s.ok and s.op != "health"]
    ctx.rep.check(case, "no unexpected 5xx from any request in any phase", not fivehundreds,
                  fail_detail="; ".join(f"{s.phase}/{s.op}={s.status}" for s in fivehundreds[:5]))

    stderr = ctx.serve.stderr_path.read_text("utf-8", "replace")
    checkouts = stderr.count("git checkout ")
    ctx.rep.add(case, f"server performed {checkouts} git checkouts this run", base.INFO)


# ----------------------------------------------------------------------------------------------
# Output: metrics JSON + markdown summary
# ----------------------------------------------------------------------------------------------

PHASES = ["warm", "hot", "cold", "error_mix", "lock_heal", "lock_storm", "remote_outage",
          "lameduck"]


def build_metrics_json(ctx: Ctx, wall_seconds: float, quick: bool, failures: int) -> dict:
    return {
        "meta": {
            "harness": "serve_stress",
            "started_epoch": int(time.time() - wall_seconds),
            "wall_seconds": round(wall_seconds, 1),
            "quick": quick,
            "bazel": base.BAZEL,
            "python": sys.version.split()[0],
        },
        "time_to_ready_seconds": ctx.metrics.time_to_ready_seconds,
        "phases": [ctx.metrics.phase_stats(p) for p in PHASES
                   if any(s.phase == p for s in ctx.metrics.samples)],
        "health": ctx.metrics.health_stats(),
        "server_metrics": ctx.metrics.server_snapshots,
        "checks": [
            {"case": c, "name": n, "status": st, "detail": d} for c, n, st, d in ctx.rep.rows
        ],
        "failures": failures,
    }


def build_summary_md(data: dict) -> str:
    lines = ["# `bazel-diff serve` stress run", ""]
    meta = data["meta"]
    verdict = "✅ all checks passed" if data["failures"] == 0 else f"❌ {data['failures']} check(s) failed"
    lines += [
        f"**{verdict}** — wall time {meta['wall_seconds']}s, "
        f"time-to-ready {data['time_to_ready_seconds']}s, "
        f"profile {'quick' if meta['quick'] else 'full'}",
        "",
        "## Throughput & response times",
        "",
        "| phase | requests | ok | statuses | wall (s) | req/s | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |",
        "|---|---|---|---|---|---|---|---|---|---|",
    ]
    for ph in data["phases"]:
        lat = ph["latency_ms"] or {}
        statuses = ", ".join(f"{k}×{v}" for k, v in ph["by_status"].items())
        lines.append(
            f"| {ph['name']} | {ph['requests']} | {ph['ok']} | {statuses} | {ph['wall_seconds']} "
            f"| {ph['rps']} | {lat.get('p50', '—')} | {lat.get('p95', '—')} "
            f"| {lat.get('p99', '—')} | {lat.get('max', '—')} |")
    health = data["health"]
    hlat = health.get("latency_ms") or {}
    lines += [
        "",
        f"Health prober: {health['probes']} probes, {health['non_200']} non-200, "
        f"p99 {hlat.get('p99', '—')} ms.",
        "",
        "## Checks",
        "",
        "| case | check | status |",
        "|---|---|---|",
    ]
    for c in data["checks"]:
        icon = {"PASS": "✅", "FAIL": "❌", "INFO": "ℹ️", "XFAIL": "⚠️", "SKIP": "⏭️"}.get(
            c["status"], c["status"])
        lines.append(f"| {c['case']} | {c['name']} | {icon} {c['status']} |")
    failing = [c for c in data["checks"] if c["status"] == "FAIL"]
    if failing:
        lines += ["", "## Failure details", ""]
        for c in failing:
            detail = (c["detail"] or "").strip()
            lines += [f"### [{c['case']}] {c['name']}", "", "```", detail[:2000], "```", ""]
    return "\n".join(lines) + "\n"


# ----------------------------------------------------------------------------------------------
# Orchestration
# ----------------------------------------------------------------------------------------------


def run_phases(ctx: Ctx, only: str) -> None:
    ordered = [
        ("warm", phase_warm),
        ("hot", phase_hot),
        ("cold", phase_cold_concurrent),
        ("error_mix", phase_error_mix),
        ("lock_heal", phase_lock_heal),
        ("lock_storm", phase_lock_storm),
        ("remote_outage", phase_remote_outage),  # includes lameduck + recovery
    ]
    for name, fn in ordered:
        if only and only not in name and not (only in "lameduck" and name == "remote_outage"):
            continue
        base.log(f"\n{base.C.BOLD}== phase {name} =={base.C.RESET}")
        try:
            fn(ctx)
        except Exception:
            ctx.rep.add(name, "phase crashed", FAIL, traceback.format_exc().splitlines()[-1])
            base.vlog(traceback.format_exc())
    phase_final(ctx)


def main() -> int:
    ap = argparse.ArgumentParser(description="Stress `bazel-diff serve` and capture metrics.")
    ap.add_argument("--skip-build", action="store_true", help="reuse existing bazel-bin launcher")
    ap.add_argument("--quick", action="store_true", help="reduced request counts")
    ap.add_argument("--only", default="", help="run only phases whose name contains this")
    ap.add_argument("--keep-artifacts", action="store_true", help="keep the temp workdir")
    ap.add_argument("--metrics-out", default="", help="write the metrics JSON to this path")
    ap.add_argument("--summary-out", default="", help="write a markdown summary to this path")
    ap.add_argument("--bazel", default="bazel", help="bazel binary (e.g. a bazelisk path on CI)")
    ap.add_argument("-v", "--verbose", action="store_true", help="stream serve/git output")
    args = ap.parse_args()
    base.VERBOSE = args.verbose
    base.BAZEL = args.bazel

    if not args.skip_build:
        base.log(f"{base.C.BOLD}Building //cli:bazel-diff ...{base.C.RESET}")
        base.run([base.BAZEL, "build", "//cli:bazel-diff"], cwd=base.REPO_ROOT)
    if not base.LAUNCHER.exists():
        base.log(f"{base.C.RED}launcher not found at {base.LAUNCHER}; run without --skip-build{base.C.RESET}")
        return 2

    profile = Profile.quick() if args.quick else Profile()
    root = Path(tempfile.mkdtemp(prefix="serve-stress-"))
    base.log(f"{base.C.DIM}workdir: {root}{base.C.RESET}")
    rep = base.Report()
    metrics = Metrics()
    t_run = time.perf_counter()
    remote = None
    prober = None
    ctx = None
    try:
        base.log(f"{base.C.BOLD}Setting up git remote (git daemon) + history ...{base.C.RESET}")
        remote = base.build_remote(root)
        build_history(remote, profile)
        clone = remote.clone(root / "clone-stress")
        lameduck_clone = remote.clone(root / "clone-lameduck")
        ctx = Ctx(remote=remote, clone=clone, lameduck_clone=lameduck_clone, root=root,
                  rep=rep, metrics=metrics, profile=profile)

        base.log(f"\n{base.C.BOLD}== phase startup =={base.C.RESET}")
        t0 = time.perf_counter()
        with base.serve(clone, root / "cache-stress", initial_fetch=True, track_deps=False,
                        ready_timeout=60) as (s, health):
            ctx.serve = s
            if phase_startup(ctx, health, time.perf_counter() - t0):
                prober = HealthProber(s.port, metrics).start()
                run_phases(ctx, args.only)
                # Stop probing while the instance is still up: teardown 503s/refusals must not
                # pollute the health series in the emitted metrics.
                prober.stop()
    finally:
        if prober:
            prober.stop()
        if ctx:
            base._bazel_shutdown(ctx.clone)
            base._bazel_shutdown(ctx.lameduck_clone)
        if remote:
            remote.stop()
        if args.keep_artifacts:
            base.log(f"{base.C.DIM}artifacts kept at {root}{base.C.RESET}")
        else:
            shutil.rmtree(root, ignore_errors=True)

    fails = rep.summary()
    wall = time.perf_counter() - t_run

    data = build_metrics_json(ctx, wall, args.quick, fails) if ctx else {"failures": fails}
    if args.metrics_out:
        Path(args.metrics_out).write_text(json.dumps(data, indent=2))
        base.log(f"{base.C.DIM}metrics written to {args.metrics_out}{base.C.RESET}")
    if args.summary_out and ctx:
        Path(args.summary_out).write_text(build_summary_md(data))
        base.log(f"{base.C.DIM}summary written to {args.summary_out}{base.C.RESET}")

    # Always echo the phase table so plain console runs still show the numbers.
    if ctx:
        base.log("")
        base.log(f"{base.C.BOLD}==== throughput / response times ===={base.C.RESET}")
        for ph in data.get("phases", []):
            lat = ph["latency_ms"] or {}
            base.log(
                f"  {ph['name']:<14} n={ph['requests']:<4} rps={ph['rps']:<8} "
                f"p50={lat.get('p50', '—')}ms p95={lat.get('p95', '—')}ms "
                f"p99={lat.get('p99', '—')}ms max={lat.get('max', '—')}ms")
        h = data.get("health", {})
        base.log(f"  {'health':<14} n={h.get('probes', 0):<4} non200={h.get('non_200', 0)}")

    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
