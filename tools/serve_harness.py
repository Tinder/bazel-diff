#!/usr/bin/env python3
"""Integration harness that fully exercises the `bazel-diff serve` query service.

Unlike the in-process `E2ETest#testServeEndToEnd` unit test (which only ever uses a
single full, local, `--no-initial-fetch` repo), this harness drives the *real* built
`bazel-diff` binary as a subprocess against a *live git remote* served over `git://`,
across the combinations that actually break in production:

  * both git engines            -- `--gitEngine=jgit` (default) and `--gitEngine=subprocess`
  * three clone shapes          -- full, shallow (`--depth=1`), partial (`--filter=blob:none`)
  * the on-demand refetch path  -- commits/branches that land on the remote *after* startup
  * the targeted by-SHA path    -- a commit reachable only via refs/pull/* that a broad fetch misses
  * the startup fetch path       -- lame-duck vs. ready health semantics

It then asserts a *parity* invariant: for every scenario, the in-process JGit engine must
succeed wherever the native-git subprocess engine succeeds. A violation is exactly the class
of bug behind reports like:

    400 git error: JGit fetch origin failed: Missing delta base <sha>

which happens when JGit fetches a thin pack whose delta base is not present in a shallow or
partial clone -- a case native git handles but JGit 5.13 does not.

Usage:
    tools/serve_harness.py                 # build, then run the full matrix
    tools/serve_harness.py --skip-build    # reuse an existing bazel-bin/cli/bazel-diff
    tools/serve_harness.py --only shallow  # run only cases whose name contains "shallow"
    tools/serve_harness.py --keep-artifacts# leave the temp workdir on disk for inspection
    tools/serve_harness.py -v              # stream serve/git output as it happens
    tools/serve_harness.py --bazel ~/go/bin/bazelisk   # use a specific bazel binary (CI)

Exit code is non-zero if any gating scenario fails (parity violations included).
Requires: git (with `git daemon`) and a bazel binary (`--bazel`, default `bazel`); `git daemon` ships
with git. Pure Python stdlib, no third-party deps.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
LAUNCHER = REPO_ROOT / "bazel-bin" / "cli" / "bazel-diff"
DEFAULT_BRANCH = "master"

# ----------------------------------------------------------------------------------------------
# Small terminal + logging helpers
# ----------------------------------------------------------------------------------------------


class C:
    """ANSI colors, disabled when stdout is not a tty."""

    _on = sys.stdout.isatty()
    GREEN = "\033[32m" if _on else ""
    RED = "\033[31m" if _on else ""
    YELLOW = "\033[33m" if _on else ""
    BLUE = "\033[34m" if _on else ""
    DIM = "\033[2m" if _on else ""
    BOLD = "\033[1m" if _on else ""
    RESET = "\033[0m" if _on else ""


VERBOSE = False
BAZEL = "bazel"  # overridable via --bazel (e.g. a bazelisk path on CI)


def log(msg: str) -> None:
    print(msg, flush=True)


def vlog(msg: str) -> None:
    if VERBOSE:
        print(f"{C.DIM}{msg}{C.RESET}", flush=True)


# ----------------------------------------------------------------------------------------------
# Subprocess / git helpers
# ----------------------------------------------------------------------------------------------


def run(cmd, cwd=None, check=True, env=None) -> subprocess.CompletedProcess:
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
        raise RuntimeError(
            f"command failed ({proc.returncode}): {' '.join(str(c) for c in cmd)}\n{proc.stdout}"
        )
    return proc


def git(repo: Path, *args, check=True) -> str:
    # -c protocol.ext... not needed; keep identity deterministic for commits.
    base = [
        "git",
        "-c",
        "user.name=serve-harness",
        "-c",
        "user.email=serve-harness@example.com",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "init.defaultBranch=" + DEFAULT_BRANCH,
    ]
    return run(base + list(args), cwd=repo, check=check).stdout.strip()


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_port(port: int, timeout: float) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(0.5)
            try:
                s.connect(("127.0.0.1", port))
                return True
            except OSError:
                time.sleep(0.1)
    return False


# ----------------------------------------------------------------------------------------------
# HTTP client for the serve endpoints
# ----------------------------------------------------------------------------------------------


def http(port: int, path: str, method: str = "GET", timeout: float = 300.0):
    """Returns (status_code_or_None, body_text). status is None on a transport error."""
    url = f"http://127.0.0.1:{port}{path}"
    req = urllib.request.Request(url, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            return resp.status, resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")
    except (urllib.error.URLError, ConnectionError, OSError) as e:
        return None, str(e)


# ----------------------------------------------------------------------------------------------
# Generated hermetic Bazel workspace (genrule-only: no external deps, no network)
# ----------------------------------------------------------------------------------------------

MODULE_BAZEL = 'module(name = "serve_harness_ws", version = "0.0.0")\n'

# //:mid depends on //:core's output, //:other is independent. Editing core.txt therefore
# impacts //:core AND //:mid (distance 0 and 1); editing other.txt impacts only //:other.
BUILD_BAZEL = """\
genrule(name = "core", srcs = ["core.txt"], outs = ["core.out"], cmd = "cp $< $@")

genrule(
    name = "mid",
    srcs = ["mid.txt", ":core"],
    outs = ["mid.out"],
    cmd = "cat $(SRCS) > $@",
)

genrule(name = "other", srcs = ["other.txt"], outs = ["other.out"], cmd = "cp $< $@")
"""


def _block(tag: str, changed: set[int] = frozenset(), n: int = 400) -> str:
    """A large deterministic text block so git forms real deltas between revisions."""
    lines = []
    for i in range(n):
        if i in changed:
            lines.append(f"{tag} line {i:03d} CHANGED")
        else:
            lines.append(f"{tag} line {i:03d}")
    return "\n".join(lines) + "\n"


# core.txt content across revisions. The A-family blocks are mutually similar (small deltas);
# block B is entirely dissimilar. This is what lets a later A'' commit delta against a
# below-shallow-boundary A blob after an aggressive repack.
CORE_A = _block("A")
CORE_A1 = _block("A", {10, 20, 30})  # A' -- a few lines changed
CORE_A2 = _block("A", {10, 20, 30, 40, 50, 60})  # A'' -- still A-family
CORE_B = _block("B")  # entirely different content


def write_workspace(ws: Path, core: str, mid: str, other: str) -> None:
    ws.mkdir(parents=True, exist_ok=True)
    (ws / "MODULE.bazel").write_text(MODULE_BAZEL)
    (ws / "BUILD.bazel").write_text(BUILD_BAZEL)
    (ws / "core.txt").write_text(core)
    (ws / "mid.txt").write_text(mid)
    (ws / "other.txt").write_text(other)
    bv = REPO_ROOT / ".bazelversion"
    if bv.exists():
        shutil.copy(bv, ws / ".bazelversion")


# ----------------------------------------------------------------------------------------------
# The remote: a git repo with controlled history, served over git:// by `git daemon`
# ----------------------------------------------------------------------------------------------


@dataclass
class Remote:
    root: Path  # temp root
    srv_base: Path  # git-daemon base-path
    origin: Path  # the served repo (srv_base/origin)
    port: int
    url: str
    daemon: subprocess.Popen
    shas: dict = field(default_factory=dict)

    def commit(self, msg: str, core=None, mid=None, other=None) -> str:
        if core is not None:
            (self.origin / "core.txt").write_text(core)
        if mid is not None:
            (self.origin / "mid.txt").write_text(mid)
        if other is not None:
            (self.origin / "other.txt").write_text(other)
        git(self.origin, "add", "-A")
        git(self.origin, "commit", "-q", "-m", msg)
        return git(self.origin, "rev-parse", "HEAD")

    def pr_head_commit(self, msg: str, pr: int, base: str, core=None, mid=None, other=None) -> str:
        """Create a commit advertised ONLY under refs/pull/<pr>/head (like a GitHub PR head),
        reachable from no branch. A clone's default refspec covers only refs/heads/*, so a broad
        `git fetch --all` cannot bring it in -- resolving it requires the targeted by-SHA fetch."""
        tmp = f"_pr{pr}"
        git(self.origin, "checkout", "-q", "-b", tmp, base)
        sha = self.commit(msg, core=core, mid=mid, other=other)
        git(self.origin, "update-ref", f"refs/pull/{pr}/head", sha)
        # Drop the branch so only the pull ref reaches the commit; leave the tree on DEFAULT_BRANCH.
        git(self.origin, "checkout", "-q", DEFAULT_BRANCH)
        git(self.origin, "branch", "-q", "-D", tmp)
        return sha

    def repack(self) -> None:
        # Aggressive repack so newer blobs are delta-compressed against the most similar base
        # anywhere in history -- including objects below a shallow clone's boundary.
        git(self.origin, "repack", "-a", "-d", "-f", "--window=250", "--depth=250")

    def clone(self, dest: Path, *extra) -> Path:
        run(["git", "clone", "-q", *extra, self.url, str(dest)])
        # Give the clone a deterministic identity for any commits (none expected) + disable gc.
        git(dest, "config", "user.name", "serve-harness")
        git(dest, "config", "user.email", "serve-harness@example.com")
        git(dest, "config", "gc.auto", "0")
        return dest

    def stop(self) -> None:
        with contextlib.suppress(Exception):
            self.daemon.terminate()
            self.daemon.wait(timeout=5)


def build_remote(root: Path) -> Remote:
    srv_base = root / "srv"
    origin = srv_base / "origin"
    origin.mkdir(parents=True)
    git(origin, "init", "-q")
    git(origin, "config", "user.name", "serve-harness")
    git(origin, "config", "user.email", "serve-harness@example.com")
    # Server-side capabilities needed for fetch-by-sha and partial (blob:none) clones.
    git(origin, "config", "uploadpack.allowFilter", "true")
    git(origin, "config", "uploadpack.allowAnySHA1InWant", "true")
    git(origin, "config", "uploadpack.allowReachableSHA1InWant", "true")

    shas: dict = {}
    write_workspace(origin, core=CORE_A, mid="mid v0\n", other="other v0\n")
    git(origin, "add", "-A")
    git(origin, "commit", "-q", "-m", "C0 base")
    shas["C0"] = git(origin, "rev-parse", "HEAD")

    port = free_port()
    daemon = subprocess.Popen(
        [
            "git",
            "daemon",
            "--reuseaddr",
            "--listen=127.0.0.1",
            f"--port={port}",
            f"--base-path={srv_base}",
            "--export-all",
            "--enable=upload-pack",
            "--informative-errors",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if not wait_port(port, timeout=10):
        daemon.terminate()
        raise RuntimeError(f"git daemon did not come up on port {port}")

    remote = Remote(
        root=root,
        srv_base=srv_base,
        origin=origin,
        port=port,
        url=f"git://127.0.0.1:{port}/origin",
        daemon=daemon,
        shas=shas,
    )

    # Build the rest of the pre-clone history (C1: edit core A->A', C2: replace core A'->B).
    remote.shas["C1"] = remote.commit("C1 tweak core", core=CORE_A1)
    remote.shas["C2"] = remote.commit("C2 replace core", core=CORE_B)
    remote.repack()
    return remote


# ----------------------------------------------------------------------------------------------
# A running `bazel-diff serve` process
# ----------------------------------------------------------------------------------------------


@dataclass
class Serve:
    proc: subprocess.Popen
    port: int
    workspace: Path
    stderr_path: Path
    engine: str

    def stderr_tail(self, n: int = 25) -> str:
        try:
            lines = self.stderr_path.read_text("utf-8", "replace").splitlines()
        except OSError:
            return ""
        return "\n".join(lines[-n:])

    def stop(self) -> None:
        with contextlib.suppress(Exception):
            self.proc.terminate()
            try:
                self.proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                self.proc.kill()


@contextlib.contextmanager
def serve(
    workspace: Path,
    cache: Path,
    engine: str,
    *,
    initial_fetch: bool,
    track_deps: bool,
    ready_timeout: float,
):
    """Launches serve, waits for a health verdict, yields (Serve, health) then tears down.

    health is one of: "ready" (200), "lameduck" (up but stuck 503), "down" (never bound).
    """
    port = free_port()
    stderr_path = workspace.parent / f"serve.{engine}.stderr.log"
    args = [
        str(LAUNCHER),
        "serve",
        "-w",
        str(workspace),
        "-b",
        BAZEL,
        "--cacheDir",
        str(cache),
        "--port",
        str(port),
        "--gitEngine",
        engine,
    ]
    if not initial_fetch:
        args.append("--no-initial-fetch")
    if track_deps:
        args.append("--trackDeps")

    vlog(f"launch serve[{engine}] on :{port}  ws={workspace.name}  initial_fetch={initial_fetch}")
    with open(stderr_path, "w") as errf:
        proc = subprocess.Popen(
            args,
            cwd=str(workspace),
            stdout=(None if VERBOSE else subprocess.DEVNULL),
            stderr=(errf if not VERBOSE else None),
        )
        s = Serve(proc=proc, port=port, workspace=workspace, stderr_path=stderr_path, engine=engine)
        try:
            health = _await_health(s, ready_timeout)
            yield s, health
        finally:
            s.stop()


def _await_health(s: Serve, timeout: float) -> str:
    """Polls /health. 200 => ready. Trailing 503 after the process is up => lameduck."""
    deadline = time.time() + timeout
    last = None
    bound = False
    while time.time() < deadline:
        if s.proc.poll() is not None:
            return "down"  # process exited
        code, _ = http(s.port, "/health", timeout=3)
        if code == 200:
            return "ready"
        if code == 503:
            bound = True
            last = 503
        time.sleep(0.4)
    # Timed out. If we ever saw a 503, the server is up but not becoming ready => lame-duck.
    return "lameduck" if (bound or last == 503) else "down"


# ----------------------------------------------------------------------------------------------
# Reporting
# ----------------------------------------------------------------------------------------------

PASS, FAIL, XFAIL, INFO, SKIP = "PASS", "FAIL", "XFAIL", "INFO", "SKIP"


@dataclass
class Report:
    rows: list = field(default_factory=list)  # (case, name, status, detail)

    def add(self, case: str, name: str, status: str, detail: str = "") -> None:
        self.rows.append((case, name, status, detail))
        color = {
            PASS: C.GREEN,
            FAIL: C.RED,
            XFAIL: C.YELLOW,
            INFO: C.BLUE,
            SKIP: C.DIM,
        }[status]
        tail = f"  {C.DIM}{detail}{C.RESET}" if detail else ""
        log(f"  {color}{status:<5}{C.RESET} {name}{tail}")

    def check(self, case, name, cond, ok_detail="", fail_detail="") -> bool:
        self.add(case, name, PASS if cond else FAIL, ok_detail if cond else fail_detail)
        return bool(cond)

    def summary(self) -> int:
        n = {PASS: 0, FAIL: 0, XFAIL: 0, INFO: 0, SKIP: 0}
        for _, _, status, _ in self.rows:
            n[status] += 1
        log("")
        log(f"{C.BOLD}==== summary ===={C.RESET}")
        log(
            f"  {C.GREEN}{n[PASS]} pass{C.RESET}  "
            f"{C.RED}{n[FAIL]} fail{C.RESET}  "
            f"{C.YELLOW}{n[XFAIL]} xfail{C.RESET}  "
            f"{C.BLUE}{n[INFO]} info{C.RESET}  "
            f"{C.DIM}{n[SKIP]} skip{C.RESET}"
        )
        if n[FAIL]:
            log("")
            log(f"{C.RED}Failing scenarios:{C.RESET}")
            for case, name, status, detail in self.rows:
                if status == FAIL:
                    log(f"  - [{case}] {name}: {detail}")
        return n[FAIL]


# ----------------------------------------------------------------------------------------------
# Scenario primitives
# ----------------------------------------------------------------------------------------------


def _impacted(s: Serve, frm: str, to: str, target_type: str | None = None, timeout: float = 300.0):
    q = f"/impacted_targets?from={frm}&to={to}"
    if target_type:
        q += f"&targetType={target_type}"
    code, body = http(s.port, q, timeout=timeout)
    try:
        data = json.loads(body)
    except (ValueError, TypeError):
        data = None
    return code, body, data


def _iset(data) -> set:
    if not data or not isinstance(data.get("impactedTargets"), list):
        return set()
    return set(data["impactedTargets"])


def _stable(data) -> set:
    """Impacted labels with generated-file (`*.out`) targets dropped.

    A generated file's hash equals its generating rule's hash, yet whether it surfaces as a
    distinct impacted target fluctuates with Bazel's incremental analysis state across the
    service's sequential checkouts (observed identically under both git engines). Rules and
    source files are stable, so scenario assertions compare only that deterministic subset.
    """
    return {label for label in _iset(data) if not label.endswith(".out")}


# ----------------------------------------------------------------------------------------------
# Cases
# ----------------------------------------------------------------------------------------------


def case_full(remote: Remote, clone: Path, root: Path, rep: Report, engine: str, track_deps: bool) -> None:
    """The broad functional suite on a full clone, run identically for both git engines so any
    divergence is attributable to the engine alone."""
    case = f"full/{engine}" + ("+deps" if track_deps else "")
    log(f"\n{C.BOLD}== case {case} =={C.RESET}")
    sh = remote.shas
    cache = root / f"cache-full-{engine}-{int(track_deps)}"
    track = track_deps
    with serve(clone, cache, engine, initial_fetch=False, track_deps=track, ready_timeout=30) as (s, health):
        if not rep.check(case, "health becomes ready", health == "ready",
                         fail_detail=f"health={health}\n{s.stderr_tail()}"):
            return

        # Editing core.txt impacts //:core (directly) and //:mid (depends on //:core), plus the
        # source //:core.txt. Nothing in the //:other subtree.
        core_edit = {"//:core", "//:core.txt", "//:mid"}
        code, body, data = _impacted(s, sh["C1"], sh["C2"])
        got = _stable(data)
        rep.check(case, "impacted_targets C1->C2 (edit core.txt)",
                  code == 200 and got == core_edit,
                  ok_detail=str(sorted(got)), fail_detail=f"code={code} got={sorted(got)} raw={sorted(_iset(data))}")

        # Caching: an identical follow-up request returns the identical stable set (served from cache).
        c2, _, d2 = _impacted(s, sh["C1"], sh["C2"])
        rep.check(case, "impacted_targets cached repeat is stable",
                  c2 == 200 and _stable(d2) == got, fail_detail=f"code={c2} got={sorted(_stable(d2))}")

        # On-demand refetch: C3 landed on the remote after this clone was made (#411 path).
        code, body, data = _impacted(s, sh["C2"], sh["C3"])
        got = _stable(data)
        rep.check(case, "refetch new commit C2->C3 (edit core.txt)",
                  code == 200 and got == core_edit,
                  ok_detail=str(sorted(got)),
                  fail_detail=f"code={code} got={sorted(got)} raw={sorted(_iset(data))}\n{s.stderr_tail()}")

        # On-demand refetch of a commit reachable only via a brand-new branch ref (feature@C4,
        # which edits only other.txt).
        code, body, data = _impacted(s, sh["C2"], sh["C4"])
        got = _stable(data)
        rep.check(case, "refetch new branch commit C2->C4 (edit other.txt)",
                  code == 200 and got == {"//:other", "//:other.txt"},
                  ok_detail=str(sorted(got)),
                  fail_detail=f"code={code} got={sorted(got)} raw={sorted(_iset(data))}\n{s.stderr_tail()}")

        # On-demand targeted fetch of a commit reachable only via refs/pull/7/head (a GitHub PR
        # head): a broad `git fetch --all` cannot bring it in, so this is the exact case that used to
        # 400 with "revision ... is missing from the local clone". It must now resolve via the
        # targeted by-SHA fetch and return the same set as the equivalent branch edit (C4).
        code, body, data = _impacted(s, sh["C2"], sh["C5"])
        got = _stable(data)
        rep.check(case, "targeted fetch pr-head commit C2->C5 (refs/pull/*, edit other.txt)",
                  code == 200 and got == {"//:other", "//:other.txt"},
                  ok_detail=str(sorted(got)),
                  fail_detail=f"code={code} got={sorted(got)} raw={sorted(_iset(data))}\n{s.stderr_tail()}")

        _full_extras(rep, case, s, remote, track_deps)

    _bazel_shutdown(clone)


def _full_extras(rep: Report, case: str, s: Serve, remote: Remote, track_deps: bool) -> None:
    sh = remote.shas

    # targetType filter -> only the changed source file comes back (no rules, no generated files).
    code, body, data = _impacted(s, sh["C1"], sh["C2"], target_type="SourceFile")
    iset = _iset(data)
    rep.check(case, "targetType=SourceFile filters to sources",
              code == 200 and iset == {"//:core.txt"},
              ok_detail=str(sorted(iset)), fail_detail=f"code={code} got={sorted(iset)}")

    # Genuinely-unknown revision -> 400 after one on-demand refetch that still can't find it.
    dead = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
    code, body, _ = _impacted(s, sh["C2"], dead)
    rep.check(case, "unknown revision -> 400",
              code == 400 and "missing from the local clone" in body,
              fail_detail=f"code={code} body={body[:200]}")

    # Missing required params.
    code, body = http(s.port, "/impacted_targets")
    rep.check(case, "missing from/to -> 400",
              code == 400 and "missing required query parameters" in body,
              fail_detail=f"code={code} body={body[:160]}")

    # Wrong method.
    code, body = http(s.port, f"/impacted_targets?from={sh['C1']}&to={sh['C2']}", method="POST")
    rep.check(case, "POST -> 405", code == 405, fail_detail=f"code={code}")

    # Distances endpoint: with --trackDeps, core is directly changed (d0), mid depends on core
    # (d>=1); without it, the endpoint 400s.
    code, body = http(s.port, f"/impacted_targets_with_distances?from={sh['C1']}&to={sh['C2']}")
    if track_deps:
        try:
            dd = json.loads(body)
            bylabel = {e["label"]: e for e in dd.get("impactedTargets", [])}
        except (ValueError, TypeError, KeyError):
            bylabel = {}
        rep.check(case, "distances: core d0, mid d>=1",
                  code == 200
                  and bylabel.get("//:core", {}).get("targetDistance") == 0
                  and bylabel.get("//:mid", {}).get("targetDistance", 0) >= 1,
                  ok_detail=f"core={bylabel.get('//:core',{}).get('targetDistance')} "
                            f"mid={bylabel.get('//:mid',{}).get('targetDistance')}",
                  fail_detail=f"code={code} body={body[:200]}")
    else:
        rep.check(case, "distances 400 without --trackDeps",
                  code == 400 and "trackDeps" in body, fail_detail=f"code={code} body={body[:160]}")

    # Concurrency: a burst of identical requests all succeed with the identical set (one shared
    # workspace lock + per-SHA cache serialize the cold generation).
    results: list = []
    lock = threading.Lock()

    def worker():
        c, _, d = _impacted(s, sh["C1"], sh["C2"])
        with lock:
            results.append((c, frozenset(_stable(d))))

    threads = [threading.Thread(target=worker) for _ in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    codes = {c for c, _ in results}
    sets = {fs for _, fs in results}
    rep.check(case, "8 concurrent requests consistent",
              codes == {200} and len(sets) == 1,
              ok_detail=f"codes={codes}", fail_detail=f"codes={codes} distinct_sets={len(sets)}")


def case_shallow(remote: Remote, clone: Path, root: Path, rep: Report, engine: str) -> None:
    """Fetch-parity gate on a shallow (--depth=1) clone: a just-landed commit must be servable via
    on-demand refetch under BOTH engines. This is the clone shape behind production
    'JGit fetch ... Missing delta base' failures (the remote's thin pack is delta-compressed against
    an object below the shallow boundary). A cooperative local git-daemon negotiates it cleanly, so
    in-process JGit usually succeeds here; the native-git fetch fallback is what keeps the default
    engine at parity on the nastier real-world remotes where JGit's in-process fetch cannot."""
    case = f"shallow/{engine}"
    log(f"\n{C.BOLD}== case {case} =={C.RESET}")
    sh = remote.shas
    cache = root / f"cache-shallow-{engine}"
    with serve(clone, cache, engine, initial_fetch=False, track_deps=False, ready_timeout=30) as (s, health):
        if not rep.check(case, "health becomes ready", health == "ready",
                         fail_detail=f"health={health}\n{s.stderr_tail()}"):
            return
        # from = shallow tip (present); to = C3 (landed after clone, must be fetched on demand).
        code, body, data = _impacted(s, sh["C2"], sh["C3"])
        got = _stable(data)
        rep.check(case, "refetch new commit into shallow clone C2->C3",
                  code == 200 and got == {"//:core", "//:core.txt", "//:mid"},
                  ok_detail=str(sorted(got)),
                  fail_detail=f"code={code} got={sorted(got)} raw={sorted(_iset(data))} body={body[:200]}\n"
                              f"--- serve stderr ---\n{s.stderr_tail()}")
    _bazel_shutdown(clone)


def case_partial(remote: Remote, clone: Path, root: Path, rep: Report, engine: str) -> None:
    """Fetch-parity gate on a partial (--filter=blob:none) clone, exercised through the *startup*
    fetch: with an initial fetch enabled the instance must reach ready under both engines. A partial
    clone's thin pack can be delta-compressed against an absent promised blob, which JGit cannot
    fetch in-process; the native-git fallback keeps it ready. Health-only (no requests) so JGit's
    separate lack of lazy-blob checkout support is not conflated with the fetch path under test --
    a partial clone still needs --gitEngine=subprocess to actually *serve* queries."""
    case = f"partial/{engine}"
    log(f"\n{C.BOLD}== case {case} =={C.RESET}")
    cache = root / f"cache-partial-{engine}"
    with serve(clone, cache, engine, initial_fetch=True, track_deps=False, ready_timeout=25) as (s, health):
        rep.check(case, "startup fetch on partial clone -> ready",
                  health == "ready",
                  ok_detail="ready",
                  fail_detail=f"health={health}\n--- serve stderr ---\n{s.stderr_tail()}")
    _bazel_shutdown(clone)


def _bazel_shutdown(clone: Path) -> None:
    with contextlib.suppress(Exception):
        run([BAZEL, "shutdown"], cwd=clone, check=False)


# ----------------------------------------------------------------------------------------------
# Orchestration
# ----------------------------------------------------------------------------------------------

VARIANTS = {
    "full": [],
    "shallow": ["--depth=1"],
    "partial": ["--filter=blob:none"],
}

# (name, variant, engine, track_deps). The full clone runs the 2x2 of engine x trackDeps so any
# divergence is attributable to a single variable; shallow/partial exercise the fetch paths.
# Native (subprocess) rows run before their jgit twins so the correct baseline prints first.
CASES = [
    ("full/subprocess", "full", "subprocess", False),
    ("full/jgit", "full", "jgit", False),
    ("full/subprocess+deps", "full", "subprocess", True),
    ("full/jgit+deps", "full", "jgit", True),
    ("shallow/subprocess", "shallow", "subprocess", None),
    ("shallow/jgit", "shallow", "jgit", None),
    ("partial/subprocess", "partial", "subprocess", None),
    ("partial/jgit", "partial", "jgit", None),
]


def _run_case(name, variant, engine, td, remote, clone, root, rep) -> None:
    if variant == "full":
        case_full(remote, clone, root, rep, engine, td)
    elif variant == "shallow":
        case_shallow(remote, clone, root, rep, engine)
    else:
        case_partial(remote, clone, root, rep, engine)


def main() -> int:
    global VERBOSE, BAZEL
    ap = argparse.ArgumentParser(description="Exercise `bazel-diff serve` end to end.")
    ap.add_argument("--skip-build", action="store_true", help="reuse existing bazel-bin launcher")
    ap.add_argument("--only", default="", help="run only cases whose name contains this substring")
    ap.add_argument("--keep-artifacts", action="store_true", help="do not delete the temp workdir")
    ap.add_argument(
        "--bazel", default="bazel", help="bazel binary to use (e.g. a bazelisk path on CI)")
    ap.add_argument("-v", "--verbose", action="store_true", help="stream serve/git output")
    args = ap.parse_args()
    VERBOSE = args.verbose
    BAZEL = args.bazel

    if not args.skip_build:
        log(f"{C.BOLD}Building //cli:bazel-diff ...{C.RESET}")
        run([BAZEL, "build", "//cli:bazel-diff"], cwd=REPO_ROOT)
    if not LAUNCHER.exists():
        log(f"{C.RED}launcher not found at {LAUNCHER}; run without --skip-build{C.RESET}")
        return 2

    root = Path(tempfile.mkdtemp(prefix="serve-harness-"))
    log(f"{C.DIM}workdir: {root}{C.RESET}")
    rep = Report()
    remote = None
    clones: dict = {}
    try:
        log(f"{C.BOLD}Setting up git remote (git daemon) + clones ...{C.RESET}")
        remote = build_remote(root)

        # Pre-make each case's own clone while the remote is at C2, BEFORE the post-startup commits
        # land, so those commits genuinely require an on-demand fetch. Each case gets its own clone
        # + cache so sequential checkouts never cross-contaminate.
        selected = [c for c in CASES if not args.only or args.only in c[0]]
        for name, variant, engine, td in selected:
            slug = name.replace("/", "-").replace("+", "-")
            clones[name] = remote.clone(root / f"clone-{slug}", *VARIANTS[variant])

        # Land the post-clone history on the remote.
        remote.shas["C3"] = remote.commit("C3 restore core (A'')", core=CORE_A2)  # master advances
        git(remote.origin, "checkout", "-q", "-b", "feature", remote.shas["C2"])
        remote.shas["C4"] = remote.commit("C4 edit other (feature)", other="other v1\n")
        git(remote.origin, "checkout", "-q", DEFAULT_BRANCH)
        # C5: reachable only via refs/pull/7/head (a GitHub PR head), so a broad fetch cannot bring
        # it in -- resolving it exercises the targeted by-SHA fetch fallback (the reported bug).
        remote.shas["C5"] = remote.pr_head_commit(
            "C5 edit other (pr head)", pr=7, base=remote.shas["C2"], other="other v2\n")
        remote.repack()  # force new blobs to delta against the most similar (possibly out-of-clone) base
        vlog(f"shas: {remote.shas}")

        for name, variant, engine, td in selected:
            try:
                _run_case(name, variant, engine, td, remote, clones[name], root, rep)
            except Exception:
                rep.add(name, "case crashed", FAIL, traceback.format_exc().splitlines()[-1])
                vlog(traceback.format_exc())
    finally:
        for clone in clones.values():
            _bazel_shutdown(clone)
        if remote:
            remote.stop()
        if args.keep_artifacts:
            log(f"{C.DIM}artifacts kept at {root}{C.RESET}")
        else:
            shutil.rmtree(root, ignore_errors=True)

    fails = rep.summary()
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
