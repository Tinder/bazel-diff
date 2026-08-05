#!/usr/bin/env python3
"""Cross-instance hash-consistency harness for `bazel-diff serve`.

Runs N `serve` instances side by side against one shared (mock) S3 cache tier and checks that they
produce **the same hashes for the same commit**. If they do not, target hashes are carrying
host-specific state, and every impacted-target answer the fleet gives is suspect.

Nothing else in the repo can catch that. `E2ETest#testGenerateHashesIsHermeticAcrossWorkspacePaths`
runs two hashes in one JVM on one machine; `serve_harness.py` drives a single instance. This
harness is the first thing that runs several instances and compares what they each *wrote*.

Why the S3 tier is the observation point
----------------------------------------
`HashService.generate` writes the full `{label: hash}` map for a revision to the cache under
`<sha>.<configFingerprint>` before returning. Intercepting those writes gives us, for one commit,
N independently-computed hash maps -- so a failure names the exact targets that diverged rather
than just reporting "the impacted set differed". `mock_s3.py` stands in for the bucket and records
every request; it needs no container and no third-party dependency.

Each instance always writes under its own `--s3Prefix` (`i0/`, `i1/`, ...) so writes stay
attributable. `--s3Prefix` is deliberately *not* part of `ServeCommand.computeConfigFingerprint()`,
so the instances still agree on the cache key. The mock's read mode then decides what they can see:

  * isolated -- everyone generates independently. This is the poisoning detector.
  * shared   -- the real fleet topology; followers must hit the entry the first instance published.

What is deliberately skewed between instances
---------------------------------------------
Anything that must *not* change a hash, and that is not part of the config fingerprint (skewing a
fingerprinted flag would give the instances different cache keys and there would be nothing to
compare):

  * clone paths of different lengths -- Bazel's output base is `_bazel_<user>/<md5 of the workspace
    path>`, so this genuinely gives each instance a different output base and its own Bazel server.
    That is the lever for the highest-risk leak: `RuleHasher.ruleBzlSeed` filters macro
    instantiation-stack frames only by an `external/` / `@` / `../` prefix, so an absolute `.bzl`
    frame would be hashed verbatim -- output base and all.
  * `HOME` / `TMPDIR` / `USER` (opt-in, `--env-skew`).
  * the Bazel binary (opt-in, `--bazel-alt`) -- the Bazel version is absent from the config
    fingerprint, so two versions share a cache key by construction.

Usage:
    tools/serve_consistency.py                          # build, then run the hermetic matrix
    tools/serve_consistency.py --skip-build -v
    tools/serve_consistency.py --only baseline
    tools/serve_consistency.py --instances 5
    tools/serve_consistency.py --env-skew               # also skew HOME/TMPDIR/USER
    tools/serve_consistency.py --bazel-alt ~/bin/bazel-8 --only version-skew
    tools/serve_consistency.py --real-repo-url <url> --real-from <sha> --real-to <sha>

Exit code is non-zero if any gating check fails. Requires git (with `git daemon`) and a bazel
binary. Pure Python stdlib, no third-party deps.

Run it directly (`python3 tools/serve_consistency.py`), never via `bazel run` -- it shells out to
`bazel build //cli:bazel-diff` and nested bazel deadlocks on the output-base lock. See tools/BUILD.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import contextlib
import hashlib
import json
import os
import shutil
import sys
import tempfile
import time
import traceback
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import mock_s3  # noqa: E402
import serve_harness as base  # noqa: E402

BUCKET = "bazel-diff-consistency"
S3_REGION = "us-east-1"

# Flag-shaped option values argparse would otherwise reject; see fold_flag_values.
FLAG_VALUED_OPTS = ("--real-clone-args", "--serve-arg")


# ----------------------------------------------------------------------------------------------
# Environment for a serve instance
# ----------------------------------------------------------------------------------------------


def aws_env() -> dict:
    """The AWS wiring every instance needs, plus the guards that make a misconfiguration loud.

    Credentials are mandatory, not cosmetic: the SDK's default provider chain throws
    `SdkClientException` when it cannot resolve any, and `S3HashCacheStorage` swallows every
    `SdkException` into a silent cache miss -- so without these the harness would run green having
    exercised no S3 at all. `AWS_REGION` is even more load-bearing: the region is resolved eagerly
    inside `createS3Storage()`, which has no try/catch, so an unresolvable region kills the process
    at startup and every instance reports `down`.

    The `/dev/null` config paths and the cleared `AWS_PROFILE` mean a stray `~/.aws/config` on a
    developer machine can never redirect this harness at a real bucket. `AWS_MAX_ATTEMPTS=1`
    prevents SDK retries from double-counting writes if the mock ever answers badly.
    """
    return {
        "AWS_ACCESS_KEY_ID": "mock-access-key",
        "AWS_SECRET_ACCESS_KEY": "mock-secret-key",
        "AWS_REGION": S3_REGION,
        "AWS_DEFAULT_REGION": S3_REGION,
        "AWS_EC2_METADATA_DISABLED": "true",
        "AWS_MAX_ATTEMPTS": "1",
        # Shrinks (but does not eliminate) the aws-chunked PutObject path; mock_s3 decodes it
        # either way.
        "AWS_REQUEST_CHECKSUM_CALCULATION": "when_required",
        "AWS_RESPONSE_CHECKSUM_VALIDATION": "when_required",
        "AWS_SHARED_CREDENTIALS_FILE": os.devnull,
        "AWS_CONFIG_FILE": os.devnull,
        "AWS_PROFILE": None,  # None removes the variable from the child's environment
        "AWS_SESSION_TOKEN": None,
    }


def skewed_clone_names(n: int, *, skew: bool = True) -> list:
    """Directory names of deliberately different lengths, one per instance.

    Bazel derives its output base from an md5 of the absolute workspace path, so different-length
    names put each instance on its own output base (and its own Bazel server) without touching any
    flag that feeds the config fingerprint. With [skew] off the names are uniform length, which is
    the control condition.
    """
    if not skew:
        return [f"ws-i{i}-pad" for i in range(n)]
    return [f"ws-i{i}-" + "p" * (3 + 11 * i) for i in range(n)]


def output_base_of(workspace: Path) -> str:
    """Bazel's output-base directory name for [workspace]: md5 of the absolute path.

    Reported as an INFO row so "were these actually different-looking hosts?" is answerable from
    the run output alone.
    """
    return hashlib.md5(str(workspace).encode("utf-8")).hexdigest()


# ----------------------------------------------------------------------------------------------
# One serve instance in the fleet
# ----------------------------------------------------------------------------------------------


@dataclass
class Instance:
    idx: int
    iid: str  # "i0", "i1", ... -- also the S3 prefix stem
    clone: Path
    cache: Path
    home: Path | None = None
    tmpdir: Path | None = None
    bazel: str | None = None  # None => the harness-wide --bazel
    seed_file: Path | None = None  # --seed-filepaths, used by the negative control
    serve: object | None = None
    health: str = "unstarted"

    @property
    def prefix(self) -> str:
        return f"{self.iid}/"

    def env(self, *, env_skew: bool) -> dict:
        out = aws_env()
        if env_skew:
            out["HOME"] = str(self.home)
            out["TMPDIR"] = str(self.tmpdir)
            out["USER"] = f"bd-consistency-{self.iid}"
            out["LOGNAME"] = f"bd-consistency-{self.iid}"
        return out

    def s3_flags(self, endpoint: str) -> list:
        return [
            "--s3Bucket", BUCKET,
            "--s3Prefix", self.prefix,
            "--s3Region", S3_REGION,
            "--s3Endpoint", endpoint,
            "--s3ForcePathStyle",
        ]

    def extra_args(self, endpoint: str, serve_args=()) -> list:
        args = self.s3_flags(endpoint)
        if self.seed_file is not None:
            args += ["--seed-filepaths", str(self.seed_file)]
        # Applied identically to every instance. Uniformity is what keeps this safe: several of
        # these flags feed computeConfigFingerprint(), so passing them to only some instances would
        # split the fleet across cache keys and leave nothing to compare.
        args += [str(a) for a in serve_args]
        return args


# ----------------------------------------------------------------------------------------------
# Payload comparison -- the load-bearing logic (pure; unit-tested)
# ----------------------------------------------------------------------------------------------


def decode_payload(body: bytes):
    """Parses a cached hash entry into (hashes, module_graph_json, dep_edges).

    `HashService.serialize` emits one of two shapes: a bare `{label: hash}` object when there is no
    metadata, or `{"hashes": {...}, "metadata": {"moduleGraphJson": ..., "depEdges": {...}}}` when
    there is. Labels always start with `//` or `@`, so the `"hashes"` key cannot be ambiguous.
    """
    obj = json.loads(body.decode("utf-8"))
    if isinstance(obj, dict) and isinstance(obj.get("hashes"), dict):
        meta = obj.get("metadata") or {}
        return obj["hashes"], meta.get("moduleGraphJson"), meta.get("depEdges") or {}
    return obj, None, {}


def _generated(label: str) -> bool:
    """Generated-file labels, whose presence can fluctuate with Bazel's incremental state."""
    return label.endswith(".out")


@dataclass
class Divergence:
    """What differs between the payloads several instances wrote for one cache key."""

    logical_key: str
    instances: list = field(default_factory=list)
    byte_identical: bool = True
    key_order_only: bool = False
    # instance -> labels it is missing that at least one other instance has
    label_only_in: dict = field(default_factory=dict)
    # (label, {instance: hash}) for labels present everywhere with differing values
    hash_mismatches: list = field(default_factory=list)
    total_hash_mismatches: int = 0
    module_graph_differs: bool = False
    dep_edges_differ: bool = False
    parse_errors: dict = field(default_factory=dict)

    @property
    def stable_label_sets_differ(self) -> bool:
        return any(
            any(not _generated(label) for label in labels)
            for labels in self.label_only_in.values()
        )

    @property
    def stable_hash_mismatches(self) -> list:
        return [(label, vals) for label, vals in self.hash_mismatches if not _generated(label)]

    def as_json(self) -> dict:
        return {
            "logical_key": self.logical_key,
            "instances": self.instances,
            "byte_identical": self.byte_identical,
            "key_order_only": self.key_order_only,
            "label_only_in": {k: sorted(v) for k, v in self.label_only_in.items()},
            "hash_mismatches": [
                {"label": label, "hashes": vals} for label, vals in self.hash_mismatches
            ],
            "total_hash_mismatches": self.total_hash_mismatches,
            "module_graph_differs": self.module_graph_differs,
            "dep_edges_differ": self.dep_edges_differ,
            "parse_errors": self.parse_errors,
        }


def compare_payloads(bodies: dict, *, max_labels: int = 20) -> Divergence:
    """Compares the cache payloads [bodies] (instance id -> bytes) written for one key.

    Compares *semantically*, not byte-wise. `HashService.serialize` gsons a `HashMap` produced by
    `Collectors.toMap` over a parallel stream, so JSON key order depends on how the ForkJoinPool
    happened to split the work -- i.e. on the host's CPU count. Two hosts can legitimately emit the
    same hashes in a different order. Byte-identity is still recorded (`byte_identical` /
    `key_order_only`) because "identical bytes" is a stronger and useful signal, just not the gate.
    """
    div = Divergence(logical_key="", instances=sorted(bodies))
    if len(bodies) < 2:
        return div

    raw = list(bodies.values())
    div.byte_identical = all(b == raw[0] for b in raw)

    decoded: dict = {}
    for iid, body in bodies.items():
        try:
            decoded[iid] = decode_payload(body)
        except (ValueError, TypeError, UnicodeDecodeError) as exc:
            div.parse_errors[iid] = f"{type(exc).__name__}: {exc}"
    if len(decoded) < 2:
        return div

    hashes = {iid: d[0] for iid, d in decoded.items()}
    graphs = {iid: d[1] for iid, d in decoded.items()}
    edges = {iid: d[2] for iid, d in decoded.items()}

    all_labels = set()
    for h in hashes.values():
        all_labels |= set(h)
    for iid, h in hashes.items():
        missing = all_labels - set(h)
        if missing:
            div.label_only_in[iid] = sorted(missing)

    shared_labels = set(all_labels)
    for h in hashes.values():
        shared_labels &= set(h)
    mismatches = []
    for label in sorted(shared_labels):
        values = {iid: h[label] for iid, h in hashes.items()}
        if len(set(values.values())) > 1:
            mismatches.append((label, values))
    div.total_hash_mismatches = len(mismatches)
    # Report the stable (non-generated) labels first: those are the ones that gate.
    ordered = [m for m in mismatches if not _generated(m[0])]
    ordered += [m for m in mismatches if _generated(m[0])]
    div.hash_mismatches = ordered[:max_labels]

    div.module_graph_differs = _module_graphs_differ(list(graphs.values()))
    normalized_edges = [{k: sorted(v) for k, v in e.items()} for e in edges.values()]
    div.dep_edges_differ = any(e != normalized_edges[0] for e in normalized_edges[1:])

    div.key_order_only = (
        not div.byte_identical
        and not div.label_only_in
        and not mismatches
        and not div.module_graph_differs
        and not div.dep_edges_differ
    )
    return div


def _module_graphs_differ(graphs: list) -> bool:
    """Compares `moduleGraphJson` blobs as parsed JSON, falling back to string equality.

    The blob is `bazel mod graph --output=json` output captured verbatim, so comparing it as a
    string would flag key-ordering noise as a difference.
    """
    if not graphs:
        return False
    parsed = []
    for g in graphs:
        if g is None:
            parsed.append(None)
            continue
        try:
            parsed.append(json.loads(g))
        except (ValueError, TypeError):
            parsed.append(g)
    return any(p != parsed[0] for p in parsed[1:])


def divergence_verdict(div: Divergence) -> tuple:
    """Maps a Divergence onto a (status, detail) row.

    Rule and source-file differences gate. Generated-file (`*.out`) label-set differences are an
    XFAIL: their membership fluctuates with Bazel's incremental analysis state across the service's
    sequential checkouts, which is why `serve_harness._stable` drops them from impacted-target
    comparisons too. A payload that differs only in JSON key order passes with a note.
    """
    if div.parse_errors:
        return base.FAIL, "unparseable payload: " + json.dumps(div.parse_errors)
    if div.stable_hash_mismatches or div.stable_label_sets_differ or div.module_graph_differs:
        return base.FAIL, format_divergence(div)
    if div.dep_edges_differ:
        return base.FAIL, "depEdges differ between instances" + format_divergence(div)
    if div.label_only_in or div.hash_mismatches:
        return base.XFAIL, "generated-file (*.out) differences only: " + format_divergence(div)
    if div.byte_identical:
        return base.PASS, "byte-identical payloads"
    if div.key_order_only:
        return base.PASS, "identical hashes (JSON key order differs -- expected)"
    return base.PASS, "identical hashes"


def summarize_divergence(div: Divergence) -> str:
    """One-line shape of a divergence, without the per-label hash dump."""
    parts = []
    if div.module_graph_differs:
        parts.append("moduleGraphJson differs")
    if div.dep_edges_differ:
        parts.append("depEdges differ")
    if div.label_only_in:
        parts.append(
            "label sets differ: "
            + ", ".join(
                f"{iid} missing {len(labels)}" for iid, labels in sorted(div.label_only_in.items())
            )
        )
    if div.total_hash_mismatches:
        example = div.hash_mismatches[0][0] if div.hash_mismatches else ""
        parts.append(f"{div.total_hash_mismatches} label(s) hash differently (e.g. {example})")
    return "; ".join(parts) if parts else "no differences"


def format_divergence(div: Divergence, limit: int = 8) -> str:
    """The full report: the one-line shape plus the first [limit] diverging labels, one per line.

    This is the payload of a failing run -- the labels named here are where a host-specific input
    entered the hash, so they are the starting point for tracing the leak.
    """
    out = summarize_divergence(div)
    for label, values in div.hash_mismatches[:limit]:
        rendered = "  ".join(f"{iid}={h}" for iid, h in sorted(values.items()))
        out += f"\n      {label}: {rendered}"
    if div.total_hash_mismatches > limit:
        out += f"\n      ... {div.total_hash_mismatches - limit} more"
    return out


# ----------------------------------------------------------------------------------------------
# Fleet lifecycle
# ----------------------------------------------------------------------------------------------


@dataclass
class Ctx:
    args: argparse.Namespace
    root: Path
    rep: base.Report
    store: mock_s3.MockS3Store
    server: mock_s3.MockS3Server
    remote: object = None
    divergences: dict = field(default_factory=dict)  # case -> [Divergence]
    output_bases: dict = field(default_factory=dict)
    clones: list = field(default_factory=list)


def make_instances(ctx: Ctx, case: str, n: int, *, clone_args=()) -> list:
    """Clones the remote n times into differently-named directories and registers the prefixes."""
    names = skewed_clone_names(n, skew=not ctx.args.no_path_skew)
    instances = []
    for idx, name in enumerate(names):
        iid = f"i{idx}"
        clone = ctx.root / case / name
        clone.parent.mkdir(parents=True, exist_ok=True)
        ctx.remote.clone(clone, *clone_args)
        inst = Instance(
            idx=idx,
            iid=iid,
            clone=clone,
            cache=ctx.root / case / f"cache-{iid}",
            home=ctx.root / case / f"home-{iid}",
            tmpdir=ctx.root / case / f"tmp-{iid}",
        )
        inst.cache.mkdir(parents=True, exist_ok=True)
        inst.home.mkdir(parents=True, exist_ok=True)
        inst.tmpdir.mkdir(parents=True, exist_ok=True)
        ctx.store.register(iid, inst.prefix)
        ctx.output_bases[f"{case}/{iid}"] = output_base_of(clone)
        ctx.clones.append(clone)
        instances.append(inst)
    return instances


@contextlib.contextmanager
def fleet(ctx: Ctx, instances: list, *, track_deps: bool):
    """Starts every instance, yields the list, and always tears them down.

    Instances come up one at a time (`base.serve` blocks on the health handshake before yielding),
    which keeps N cold Bazel servers from starting simultaneously. With `--no-initial-fetch` and no
    warmup revision, readiness is immediate, so this costs almost nothing.
    """
    ports = base.reserve_ports(len(instances))
    with contextlib.ExitStack() as stack:
        for inst, port in zip(instances, ports):
            cm = base.serve(
                inst.clone,
                inst.cache,
                initial_fetch=False,
                track_deps=track_deps,
                ready_timeout=ctx.args.ready_timeout,
                verbose_server=True,
                extra_args=inst.extra_args(ctx.server.url, ctx.args.serve_arg),
                env=inst.env(env_skew=ctx.args.env_skew),
                port=port,
                bazel=inst.bazel,
            )
            served, health = stack.enter_context(cm)
            inst.serve = served
            inst.health = health
        yield instances


def all_ready(ctx: Ctx, case: str, instances: list) -> bool:
    ok = True
    for inst in instances:
        if not ctx.rep.check(
            case,
            f"{inst.iid} became ready",
            inst.health == "ready",
            ok_detail=f"port {inst.serve.port}, output_base {output_base_of(inst.clone)[:12]}",
            fail_detail=f"health={inst.health}; stderr:\n{inst.serve.stderr_tail(15)}",
        ):
            ok = False
    return ok


def report_query_failures(ctx: Ctx, case: str, instances: list, results: dict) -> bool:
    """Surfaces the server-side error when a query failed. Returns True if every instance answered.

    Called before `assert_wired`, because a failed query writes nothing to the cache and would
    otherwise trip the "no PutObject reached the mock" assertion -- reporting a broken S3 tier when
    the real problem was an exception inside the hash generation. The instance's stderr holds the
    actual stack trace, so it goes in the failure detail.
    """
    failed = {iid: code for iid, (code, _b, _d) in results.items() if code != 200}
    if not failed:
        return True
    by_iid = {inst.iid: inst for inst in instances}
    for iid, code in sorted(failed.items()):
        inst = by_iid.get(iid)
        error_lines = [
            line
            for line in (inst.serve.stderr_tail(200).splitlines() if inst else [])
            if "Error" in line or "Exception" in line or "ERROR:" in line
        ]
        ctx.rep.add(
            case,
            f"{iid} query failed",
            base.FAIL,
            f"HTTP {code}; server said: " + (" | ".join(error_lines[:4]) or "(no error in stderr)"),
        )
    return False


def assert_wired(ctx: Ctx, case: str, instances: list) -> bool:
    """Proves the S3 tier is actually in the loop before any consistency claim is believed.

    `S3HashCacheStorage` contains every failure -- a read error becomes a miss, a write error a
    warning -- so an instance whose S3 client is broken behaves exactly like one whose cache is
    simply cold. Without these checks a wholly disconnected fleet would report "no divergence".
    """
    ok = True
    for inst in instances:
        code, body = base.http(inst.serve.port, "/metrics", timeout=30)
        remote = None
        with contextlib.suppress(ValueError, TypeError, AttributeError):
            remote = (json.loads(body).get("cache") or {}).get("remote")
        expected = f"s3://{BUCKET}/{inst.prefix}"
        ok &= ctx.rep.check(
            case,
            f"{inst.iid} reports the remote cache tier",
            code == 200 and remote == expected,
            ok_detail=remote or "",
            fail_detail=f"code={code} cache.remote={remote!r} expected={expected!r}",
        )
        ok &= ctx.rep.check(
            case,
            f"{inst.iid} wrote to the mock S3",
            bool(ctx.store.writes(instance=inst.iid)),
            ok_detail=f"{len(ctx.store.writes(instance=inst.iid))} object(s)",
            fail_detail="no PutObject reached the mock -- the S3 tier is not in the loop",
        )
        ok &= ctx.rep.check(
            case,
            f"{inst.iid} read from the mock S3",
            bool(ctx.store.reads(instance=inst.iid)),
            ok_detail=f"{len(ctx.store.reads(instance=inst.iid))} request(s)",
            fail_detail="no GetObject/HeadObject reached the mock",
        )
        swallowed = [
            line
            for line in inst.serve.stderr_tail(400).splitlines()
            if "S3 cache" in line and "failed" in line
        ]
        ok &= ctx.rep.check(
            case,
            f"{inst.iid} logged no swallowed S3 errors",
            not swallowed,
            fail_detail="; ".join(swallowed[:3]),
        )
    ok &= ctx.rep.check(
        case,
        "mock S3 saw no unexpected requests",
        not ctx.store.bad_requests(),
        fail_detail=str(ctx.store.bad_requests()[:3]),
    )
    return ok


def chunked_transport_check(ctx: Ctx, case: str) -> None:
    """Records whether PutObject arrived aws-chunked.

    Informational, but worth surfacing: if a future SDK bump changes the framing and the decoder
    silently starts recording raw bytes, every payload would differ and this row is the first place
    to look.
    """
    writes = ctx.store.writes()
    if not writes:
        return
    chunked = sum(1 for w in writes if w.chunked)
    ctx.rep.add(
        case,
        "PutObject transfer framing",
        base.INFO,
        f"{chunked}/{len(writes)} writes arrived aws-chunked",
    )


def compare_all_keys(ctx: Ctx, case: str, instances: list, *, expect_divergence: bool = False):
    """Diffs the payloads every instance wrote, per cache key. The core assertion."""
    keys = ctx.store.logical_keys()
    ctx.rep.check(
        case,
        "instances agree on the cache key(s)",
        bool(keys),
        ok_detail=f"{len(keys)} key(s): {', '.join(k[:20] for k in keys)}",
        fail_detail="no cache keys were written at all",
    )

    diverged_any = False
    for key in keys:
        bodies = ctx.store.bodies_for(key)
        if len(bodies) < 2:
            ctx.rep.add(
                case,
                f"key {key[:16]}... written by only {len(bodies)} instance(s)",
                base.INFO,
                # In isolated mode this means an instance failed to publish; in shared mode it is
                # the expected outcome of a cache hit.
                f"wrote: {', '.join(sorted(bodies)) or 'nobody'}",
            )
            continue
        div = compare_payloads(bodies, max_labels=ctx.args.max_diff_labels)
        div.logical_key = key
        ctx.divergences.setdefault(case, []).append(div)
        status, detail = divergence_verdict(div)
        if status == base.FAIL:
            diverged_any = True
        if expect_divergence:
            # Polarity is inverted: the negative control passes when divergence IS detected. The
            # detail stays short here -- a detected-as-expected difference is not a finding.
            ctx.rep.check(
                case,
                f"divergence detected for {key[:16]}...",
                status == base.FAIL,
                ok_detail=summarize_divergence(div),
                fail_detail="no divergence detected -- the comparator is not actually comparing",
            )
        else:
            ctx.rep.add(case, f"payloads agree for {key[:16]}...", status, detail)

    if not expect_divergence:
        sizes = {
            iid: len(decode_payload(body)[0])
            for key in keys
            for iid, body in ctx.store.bodies_for(key).items()
        }
        if sizes:
            ctx.rep.add(
                case,
                "target counts per instance",
                base.INFO,
                ", ".join(f"{iid}={n}" for iid, n in sorted(sizes.items())),
            )
    return diverged_any


def compare_impacted(ctx: Ctx, case: str, results: dict) -> None:
    """The user-visible symptom: do the instances return the same impacted targets?"""
    sets = {iid: base._stable(data) for iid, (code, _body, data) in results.items() if data}
    codes = {iid: code for iid, (code, _b, _d) in results.items()}
    ctx.rep.check(
        case,
        "every instance answered 200",
        all(c == 200 for c in codes.values()),
        ok_detail=f"{len(codes)} responses",
        fail_detail=str(codes),
    )
    if len(sets) < 2:
        return
    first_iid = sorted(sets)[0]
    reference = sets[first_iid]
    mismatched = {iid: s for iid, s in sets.items() if s != reference}
    detail = f"{len(reference)} stable targets"
    if mismatched:
        lines = []
        for iid, s in sorted(mismatched.items()):
            lines.append(
                f"{iid}: +{sorted(s - reference)[:5]} -{sorted(reference - s)[:5]}"
            )
        detail = f"vs {first_iid}: " + "; ".join(lines)
    ctx.rep.check(
        case,
        "impacted targets identical across instances",
        not mismatched,
        ok_detail=detail,
        fail_detail=detail,
    )


def impacted_all(
    instances: list,
    frm: str,
    to: str,
    *,
    concurrent_requests: bool = False,
    timeout: float = 300.0,
) -> dict:
    """POSTs the same query to every instance (sequentially by default) with profiling on.

    Sequential is the default so N cold Bazel servers do not analyze at once; only the
    `concurrent` case, which is specifically about the race, fires them together. [timeout] needs
    raising for a real repository -- a cold output base fetching external repos can take far longer
    than the synthetic workspace's few seconds.
    """
    if not concurrent_requests:
        return {
            inst.iid: base._impacted_post(inst.serve, frm, to, profile=True, timeout=timeout)
            for inst in instances
        }
    results: dict = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=len(instances)) as pool:
        futures = {
            pool.submit(
                base._impacted_post, inst.serve, frm, to, profile=True, timeout=timeout
            ): inst.iid
            for inst in instances
        }
        for fut in concurrent.futures.as_completed(futures):
            results[futures[fut]] = fut.result()
    return results


def retrievals(data) -> list:
    """The `profile.hashRetrievals` list from an /impacted_targets response, or []."""
    if not isinstance(data, dict):
        return []
    profile = data.get("profile") or {}
    got = profile.get("hashRetrievals")
    return got if isinstance(got, list) else []


# ----------------------------------------------------------------------------------------------
# Cases
# ----------------------------------------------------------------------------------------------


def case_baseline(ctx: Ctx) -> None:
    """N instances, isolated reads: everyone hashes the same commits independently."""
    case = "baseline"
    base.log(f"\n{base.C.BOLD}[{case}] {ctx.args.instances} instances, isolated cache{base.C.RESET}")
    ctx.store.reset()
    ctx.store.set_mode(mock_s3.ISOLATED)
    instances = make_instances(ctx, case, ctx.args.instances)
    frm, to = ctx.remote.shas["C1"], ctx.remote.shas["C2"]

    with fleet(ctx, instances, track_deps=ctx.args.track_deps):
        if not all_ready(ctx, case, instances):
            return
        results = impacted_all(instances, frm, to, timeout=ctx.args.request_timeout)
        if not report_query_failures(ctx, case, instances, results):
            return
        assert_wired(ctx, case, instances)
        chunked_transport_check(ctx, case)
        compare_all_keys(ctx, case, instances)
        compare_impacted(ctx, case, results)
        ctx.rep.add(
            case,
            "output bases",
            base.INFO,
            ", ".join(f"{i.iid}={output_base_of(i.clone)[:10]}" for i in instances),
        )
    _shutdown(ctx, instances)


def case_negative_control(ctx: Ctx) -> None:
    """Proves the detector is live by making one instance's hashes genuinely differ.

    `--seed-filepaths` is hashed into every target
    (`BuildGraphHasher.createSeedForFilepaths`) but is absent from
    `ServeCommand.computeConfigFingerprint()`, so giving one instance a different seed file changes
    all of its hashes while leaving the cache key untouched -- exactly the shape of a real
    poisoning bug, and a genuine product observation in its own right (see the INFO row).
    """
    case = "negative-control"
    base.log(f"\n{base.C.BOLD}[{case}] one instance seeded differently{base.C.RESET}")
    ctx.store.reset()
    ctx.store.set_mode(mock_s3.ISOLATED)
    instances = make_instances(ctx, case, max(2, ctx.args.instances))

    # The seed content lives outside every clone, so `git checkout --force` cannot disturb it.
    seeds = ctx.root / case / "seeds"
    seeds.mkdir(parents=True, exist_ok=True)
    for inst in instances:
        content = seeds / f"seed-{inst.iid}.txt"
        content.write_text("shared seed\n" if inst.idx == 0 else f"divergent seed {inst.iid}\n")
        listing = seeds / f"seedlist-{inst.iid}.txt"
        listing.write_text(f"{content}\n")
        inst.seed_file = listing
    # i0 and i1 differ; that is enough for the comparator to have something to find.
    (seeds / "seed-i1.txt").write_text("a genuinely different seed\n")

    frm, to = ctx.remote.shas["C1"], ctx.remote.shas["C2"]
    with fleet(ctx, instances, track_deps=ctx.args.track_deps):
        if not all_ready(ctx, case, instances):
            return
        impacted_all(instances, frm, to, timeout=ctx.args.request_timeout)
        compare_all_keys(ctx, case, instances, expect_divergence=True)
        ctx.rep.add(
            case,
            "seedFilepaths is absent from the config fingerprint",
            base.INFO,
            "instances with different --seed-filepaths share a cache key while producing "
            "different hashes (ServeCommand.computeConfigFingerprint)",
        )
    _shutdown(ctx, instances)


def case_shared(ctx: Ctx) -> None:
    """The real fleet topology: i0 publishes, the followers must be served by the remote tier."""
    case = "shared"
    base.log(f"\n{base.C.BOLD}[{case}] shared cache, follower cache-hit propagation{base.C.RESET}")
    ctx.store.reset()
    ctx.store.set_mode(mock_s3.SHARED)
    instances = make_instances(ctx, case, max(2, ctx.args.instances))
    frm, to = ctx.remote.shas["C0"], ctx.remote.shas["C1"]

    leader, followers = instances[0], instances[1:]

    with fleet(ctx, [leader], track_deps=ctx.args.track_deps):
        if not all_ready(ctx, case, [leader]):
            return
        code, _body, data = base._impacted_post(leader.serve, frm, to, profile=True)
        ctx.rep.check(
            case,
            "leader generated the entries (cache miss)",
            code == 200 and all(not r.get("cacheHit") for r in retrievals(data)),
            ok_detail=f"{len(retrievals(data))} retrieval(s), all misses",
            fail_detail=f"code={code} retrievals={retrievals(data)}",
        )
        leader_targets = base._stable(data)
        published = ctx.store.writes(instance=leader.iid)
        if not ctx.rep.check(
            case,
            "leader published to the shared tier",
            bool(published),
            ok_detail=f"{len(published)} object(s)",
            fail_detail="nothing reached the mock; followers would have nothing to hit",
        ):
            _shutdown(ctx, [leader])
            return
    _shutdown(ctx, [leader])

    with fleet(ctx, followers, track_deps=ctx.args.track_deps):
        if not all_ready(ctx, case, followers):
            return
        for inst in followers:
            code, _body, data = base._impacted_post(inst.serve, frm, to, profile=True)
            rets = retrievals(data)
            ctx.rep.check(
                case,
                f"{inst.iid} served entirely from cache",
                code == 200 and bool(rets) and all(r.get("cacheHit") for r in rets),
                ok_detail=f"{len(rets)} retrieval(s), all hits",
                fail_detail=f"code={code} retrievals={rets}",
            )
            # A local-disk hit also reports cacheHit=true, so the profile alone proves nothing.
            # The mock's read log is what shows the *remote* tier served it, and whose write did.
            remote_hits = [
                r
                for r in ctx.store.reads(instance=inst.iid, op="GET")
                if r.hit and r.served_from_prefix == leader.prefix
            ]
            ctx.rep.check(
                case,
                f"{inst.iid} was served by {leader.iid}'s objects over S3",
                bool(remote_hits),
                ok_detail=f"{len(remote_hits)} remote hit(s)",
                fail_detail="no GET resolved to the leader's prefix -- a local-tier hit, not remote",
            )
            ctx.rep.check(
                case,
                f"{inst.iid} did not re-publish on a hit",
                not ctx.store.writes(instance=inst.iid),
                fail_detail=f"{len(ctx.store.writes(instance=inst.iid))} redundant write(s)",
            )
            ctx.rep.check(
                case,
                f"{inst.iid} impacted targets match the leader",
                base._stable(data) == leader_targets,
                ok_detail=f"{len(leader_targets)} stable targets",
                fail_detail=(
                    f"+{sorted(base._stable(data) - leader_targets)[:5]} "
                    f"-{sorted(leader_targets - base._stable(data))[:5]}"
                ),
            )
    _shutdown(ctx, followers)


def case_concurrent(ctx: Ctx) -> None:
    """All instances race the same cold revision pair against one shared bucket.

    Cross-process generation is unlocked by design (`HashService.generationLock` is per-JVM), so
    several instances generating the same key is expected and benign. Divergent *content* under one
    key is the bug.
    """
    case = "concurrent"
    base.log(f"\n{base.C.BOLD}[{case}] all instances race one cold revision{base.C.RESET}")
    ctx.store.reset()
    ctx.store.set_mode(mock_s3.SHARED)
    instances = make_instances(ctx, case, ctx.args.instances)
    frm, to = ctx.remote.shas["C0"], ctx.remote.shas["C2"]

    with fleet(ctx, instances, track_deps=ctx.args.track_deps):
        if not all_ready(ctx, case, instances):
            return
        results = impacted_all(
            instances, frm, to, concurrent_requests=True, timeout=ctx.args.request_timeout
        )
        compare_impacted(ctx, case, results)
        for key in ctx.store.logical_keys():
            writes = ctx.store.writes(logical_key=key)
            digests = {w.sha256 for w in writes}
            ctx.rep.check(
                case,
                f"racing writers agree on {key[:16]}...",
                len(digests) <= 1,
                ok_detail=f"{len(writes)} write(s), 1 distinct payload",
                fail_detail=(
                    f"{len(writes)} write(s) produced {len(digests)} distinct payloads:\n"
                    + format_divergence(
                        compare_payloads(
                            ctx.store.bodies_for(key), max_labels=ctx.args.max_diff_labels
                        )
                    )
                ),
            )
        generated = sum(
            1
            for _iid, (_c, _b, data) in results.items()
            if any(not r.get("cacheHit") for r in retrievals(data))
        )
        ctx.rep.add(
            case,
            "race outcome",
            base.INFO,
            f"{generated}/{len(results)} instances generated the hashes themselves; "
            f"{len(results) - generated} were served by the shared tier",
        )
    _shutdown(ctx, instances)


def case_version_skew(ctx: Ctx) -> None:
    """Two instances, identical config, different Bazel binaries.

    The Bazel version is not part of `computeConfigFingerprint()`, yet it changes the query
    pipeline materially (`BazelQueryService` gates `streamed_proto`, `--output_file` and
    `mod show_repo` on version thresholds). If the payloads diverge, two Bazel versions are sharing
    a cache key -- a real product bug, reported as XFAIL rather than a harness failure.
    """
    case = "version-skew"
    if not ctx.args.bazel_alt:
        ctx.rep.add(case, "skipped", base.SKIP, "pass --bazel-alt <path> to run this case")
        return
    base.log(f"\n{base.C.BOLD}[{case}] two Bazel versions, one cache key{base.C.RESET}")
    ctx.store.reset()
    ctx.store.set_mode(mock_s3.ISOLATED)
    instances = make_instances(ctx, case, 2)
    instances[1].bazel = ctx.args.bazel_alt
    frm, to = ctx.remote.shas["C1"], ctx.remote.shas["C2"]

    with fleet(ctx, instances, track_deps=ctx.args.track_deps):
        if not all_ready(ctx, case, instances):
            return
        impacted_all(instances, frm, to, timeout=ctx.args.request_timeout)
        keys = ctx.store.logical_keys()
        ctx.rep.add(
            case,
            "both Bazel versions computed the same cache key",
            base.INFO if len(keys) else base.FAIL,
            f"{len(keys)} distinct key(s) -- the version is absent from the config fingerprint",
        )
        for key in keys:
            bodies = ctx.store.bodies_for(key)
            if len(bodies) < 2:
                continue
            div = compare_payloads(bodies, max_labels=ctx.args.max_diff_labels)
            div.logical_key = key
            ctx.divergences.setdefault(case, []).append(div)
            status, detail = divergence_verdict(div)
            if status == base.FAIL:
                ctx.rep.add(
                    case,
                    f"Bazel versions disagree on {key[:16]}...",
                    base.XFAIL,
                    "two Bazel versions share a cache key but hash differently: " + detail,
                )
            else:
                ctx.rep.add(case, f"Bazel versions agree on {key[:16]}...", base.PASS, detail)
    _shutdown(ctx, instances)


def case_real(ctx: Ctx) -> None:
    """The `baseline` comparison against a real repository at real revisions."""
    case = "real"
    args = ctx.args
    source = args.real_repo_url or args.real_repo_path
    if not source or not args.real_from or not args.real_to:
        ctx.rep.add(
            case,
            "skipped",
            base.SKIP,
            "pass --real-repo-url/--real-repo-path plus --real-from and --real-to",
        )
        return
    base.log(f"\n{base.C.BOLD}[{case}] {source} {args.real_from[:12]}..{args.real_to[:12]}{base.C.RESET}")
    ctx.store.reset()
    ctx.store.set_mode(mock_s3.ISOLATED)

    clone_args = args.real_clone_args.split() if args.real_clone_args else []
    names = skewed_clone_names(args.instances, skew=not args.no_path_skew)
    instances = []
    for idx, name in enumerate(names):
        iid = f"i{idx}"
        clone = ctx.root / case / name
        clone.parent.mkdir(parents=True, exist_ok=True)
        base.run(["git", "clone", "-q", *clone_args, str(source), str(clone)])
        base.git(clone, "config", "gc.auto", "0")
        inst = Instance(
            idx=idx,
            iid=iid,
            clone=clone,
            cache=ctx.root / case / f"cache-{iid}",
            home=ctx.root / case / f"home-{iid}",
            tmpdir=ctx.root / case / f"tmp-{iid}",
        )
        for d in (inst.cache, inst.home, inst.tmpdir):
            d.mkdir(parents=True, exist_ok=True)
        ctx.store.register(iid, inst.prefix)
        ctx.output_bases[f"{case}/{iid}"] = output_base_of(clone)
        ctx.clones.append(clone)
        instances.append(inst)

    with fleet(ctx, instances, track_deps=ctx.args.track_deps):
        if not all_ready(ctx, case, instances):
            return
        results = impacted_all(
            instances, args.real_from, args.real_to, timeout=args.request_timeout
        )
        if not report_query_failures(ctx, case, instances, results):
            return
        assert_wired(ctx, case, instances)
        chunked_transport_check(ctx, case)
        compare_all_keys(ctx, case, instances)
        compare_impacted(ctx, case, results)
    _shutdown(ctx, instances)


def _shutdown(ctx: Ctx, instances: list) -> None:
    """Stops each instance's Bazel server. Must happen at the case boundary, not just at exit:
    every instance holds its own output base, so leaving N servers resident across cases is what
    turns a 3-instance run into a memory problem on a CI runner."""
    for inst in instances:
        base._bazel_shutdown(inst.clone)


CASES = [
    ("baseline", case_baseline),
    ("negative-control", case_negative_control),
    ("shared", case_shared),
    ("concurrent", case_concurrent),
    ("version-skew", case_version_skew),
    ("real", case_real),
]


# ----------------------------------------------------------------------------------------------
# Output: metrics JSON + markdown summary
# ----------------------------------------------------------------------------------------------


def build_metrics_json(ctx: Ctx, wall_seconds: float, failures: int) -> dict:
    return {
        "meta": {
            "harness": "serve_consistency",
            "instances": ctx.args.instances,
            "bazel": base.BAZEL,
            "bazel_alt": ctx.args.bazel_alt,
            "path_skew": not ctx.args.no_path_skew,
            "env_skew": ctx.args.env_skew,
            "track_deps": ctx.args.track_deps,
            "serve_args": list(ctx.args.serve_arg),
            "wall_seconds": round(wall_seconds, 1),
            "python": sys.version.split()[0],
        },
        "s3": {
            # The store is reset at each case boundary, so `final_case` describes only the last
            # case that ran; `run_total` spans every case.
            "run_total": ctx.store.total_counts(),
            "final_case": ctx.store.counts(),
            "final_case_per_instance": ctx.store.counts_by_instance(),
            "bad_requests": ctx.store.bad_requests(),
        },
        "output_bases": ctx.output_bases,
        "divergences": {
            case: [d.as_json() for d in divs] for case, divs in ctx.divergences.items()
        },
        "checks": [
            {"case": c, "name": n, "status": st, "detail": d} for c, n, st, d in ctx.rep.rows
        ],
        "failures": failures,
    }


def build_summary_md(data: dict) -> str:
    meta = data["meta"]
    verdict = (
        "✅ hashes are consistent across instances"
        if data["failures"] == 0
        else f"❌ {data['failures']} check(s) failed"
    )
    lines = [
        "# `bazel-diff serve` cross-instance hash consistency",
        "",
        f"**{verdict}** — {meta['instances']} instances, wall time {meta['wall_seconds']}s, "
        f"path skew {'on' if meta['path_skew'] else 'off'}, "
        f"env skew {'on' if meta['env_skew'] else 'off'}",
        "",
        "## Checks",
        "",
        "| case | check | status | detail |",
        "|---|---|---|---|",
    ]
    for row in data["checks"]:
        detail = row["detail"].replace("\n", " ").replace("|", "\\|")
        lines.append(
            f"| {row['case']} | {row['name']} | {row['status']} | {detail[:300]} |"
        )

    diverged = {
        case: [d for d in divs if d["total_hash_mismatches"] or d["label_only_in"]]
        for case, divs in data.get("divergences", {}).items()
    }
    diverged = {k: v for k, v in diverged.items() if v}
    if diverged:
        lines += ["", "## Diverging targets", ""]
        for case, divs in diverged.items():
            for div in divs:
                lines += [
                    f"### `{case}` — key `{div['logical_key']}`",
                    "",
                    f"{div['total_hash_mismatches']} label(s) hash differently; "
                    f"moduleGraph differs: {div['module_graph_differs']}; "
                    f"depEdges differ: {div['dep_edges_differ']}",
                    "",
                    "```",
                ]
                for entry in div["hash_mismatches"]:
                    rendered = "  ".join(
                        f"{iid}={h}" for iid, h in sorted(entry["hashes"].items())
                    )
                    lines.append(f"{entry['label']}: {rendered}")
                lines += ["```", ""]

    s3 = data["s3"]["run_total"]
    lines += [
        "",
        "## Mock S3 traffic (whole run)",
        "",
        f"{s3['puts']} PutObject, {s3['gets']} GetObject, {s3['heads']} HeadObject, "
        f"{s3['hits']} hit(s), {s3['bad_requests']} unexpected request(s).",
        "",
        "## Bazel output bases",
        "",
        "| instance | output base |",
        "|---|---|",
    ]
    for name, digest in sorted(data["output_bases"].items()):
        lines.append(f"| {name} | `{digest}` |")
    return "\n".join(lines) + "\n"


# ----------------------------------------------------------------------------------------------
# CLI
# ----------------------------------------------------------------------------------------------


def fold_flag_values(argv, opts=FLAG_VALUED_OPTS) -> list:
    """Rewrites `--opt <flag-shaped-value>` into `--opt=<value>`.

    argparse rejects an option value that starts with `-` unless it is spelled with `=`, which is
    exactly how `--real-clone-args --depth=50` fails. `serve_stress.py` carries the same helper for
    the same reason; it broke the serve-stress cron twice before it was added.
    """
    out: list = []
    i = 0
    while i < len(argv):
        token = argv[i]
        if token in opts and i + 1 < len(argv) and argv[i + 1].startswith("-"):
            out.append(f"{token}={argv[i + 1]}")
            i += 2
            continue
        out.append(token)
        i += 1
    return out


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        description="Check that several `bazel-diff serve` instances hash a commit identically."
    )
    ap.add_argument("--instances", type=int, default=3, help="fleet size (default 3)")
    ap.add_argument("--only", default="", help="run only cases whose name contains this substring")
    ap.add_argument("--skip-build", action="store_true", help="reuse existing bazel-bin launcher")
    ap.add_argument("--keep-artifacts", action="store_true", help="do not delete the temp workdir")
    ap.add_argument("--bazel", default="bazel", help="bazel binary (e.g. a bazelisk path on CI)")
    ap.add_argument(
        "--bazel-alt", default="", help="second bazel binary; enables the version-skew case")
    ap.add_argument(
        "--track-deps", action="store_true", help="run instances with --trackDeps (compares depEdges)")
    ap.add_argument(
        "--no-path-skew",
        action="store_true",
        help="give every clone the same-length path (control condition)",
    )
    ap.add_argument(
        "--env-skew",
        action="store_true",
        help="also skew HOME/TMPDIR/USER between instances (note: a skewed USER makes Bazel "
        "create additional output user roots outside the temp workdir, which the harness "
        "shuts down but does not delete)",
    )
    ap.add_argument(
        "--max-diff-labels", type=int, default=20, help="labels reported per divergence (default 20)")
    ap.add_argument(
        "--ready-timeout", type=float, default=90.0, help="seconds to wait for /health (default 90)")
    ap.add_argument(
        "--request-timeout",
        type=float,
        default=300.0,
        help="seconds to wait for an /impacted_targets response (default 300; raise it for a real "
        "repository, where a cold output base must fetch external repos before it can query)",
    )
    ap.add_argument(
        "--serve-arg",
        action="append",
        default=[],
        metavar="ARG",
        help="extra `serve` flag applied identically to every instance; repeatable. Use the `=` "
        "spelling for flag-shaped values, e.g. "
        "--serve-arg=--fineGrainedHashExternalRepos=@rules_kotlin",
    )
    ap.add_argument("--metrics-out", default="", help="write the run's metrics JSON here")
    ap.add_argument("--summary-out", default="", help="write a markdown summary here")
    ap.add_argument("-v", "--verbose", action="store_true", help="stream serve/git output")

    real = ap.add_argument_group("real repository mode")
    real.add_argument("--real-repo-url", default="", help="clone URL for the `real` case")
    real.add_argument("--real-repo-path", default="", help="local repo path for the `real` case")
    real.add_argument("--real-from", default="", help="base revision")
    real.add_argument("--real-to", default="", help="head revision")
    real.add_argument("--real-clone-args", default="", help="extra `git clone` args, space separated")
    return ap


def parse_args(argv=None) -> argparse.Namespace:
    argv = list(sys.argv[1:] if argv is None else argv)
    return build_parser().parse_args(fold_flag_values(argv))


def main() -> int:
    args = parse_args()
    base.VERBOSE = args.verbose
    base.BAZEL = args.bazel
    if args.instances < 2:
        base.log(f"{base.C.RED}--instances must be at least 2{base.C.RESET}")
        return 2

    if not args.skip_build:
        base.log(f"{base.C.BOLD}Building //cli:bazel-diff ...{base.C.RESET}")
        base.run([base.BAZEL, "build", "//cli:bazel-diff"], cwd=base.REPO_ROOT)
    if not base.LAUNCHER.exists():
        base.log(f"{base.C.RED}launcher not found at {base.LAUNCHER}; drop --skip-build{base.C.RESET}")
        return 2

    root = Path(tempfile.mkdtemp(prefix="serve-consistency-"))
    base.log(f"{base.C.DIM}workdir: {root}{base.C.RESET}")
    started = time.time()
    rep = base.Report()
    store = mock_s3.MockS3Store(BUCKET)
    server = mock_s3.MockS3Server(store)
    server.start()
    ctx = Ctx(args=args, root=root, rep=rep, store=store, server=server)
    base.log(f"{base.C.DIM}mock S3: {server.url}/{BUCKET}{base.C.RESET}")

    selected = [(name, fn) for name, fn in CASES if not args.only or args.only in name]
    try:
        base.log(f"{base.C.BOLD}Setting up git remote (git daemon) ...{base.C.RESET}")
        ctx.remote = base.build_remote(root)
        # C3 gives the synthetic history a revision beyond the clone point; the cases here use
        # only pre-clone revisions, so no instance needs a fetch (`--no-initial-fetch`).
        ctx.remote.shas["C3"] = ctx.remote.commit("C3 restore core (A'')", core=base.CORE_A2)
        ctx.remote.repack()
        base.vlog(f"shas: {ctx.remote.shas}")

        for name, fn in selected:
            try:
                fn(ctx)
            except Exception:
                rep.add(name, "case crashed", base.FAIL, traceback.format_exc().splitlines()[-1])
                base.vlog(traceback.format_exc())
    finally:
        for clone in ctx.clones:
            base._bazel_shutdown(clone)
        if ctx.remote:
            ctx.remote.stop()
        server.stop()
        if args.keep_artifacts:
            base.log(f"{base.C.DIM}artifacts kept at {root}{base.C.RESET}")
        else:
            shutil.rmtree(root, ignore_errors=True)

    failures = rep.summary()
    data = build_metrics_json(ctx, time.time() - started, failures)
    if args.metrics_out:
        Path(args.metrics_out).write_text(json.dumps(data, indent=2) + "\n")
        base.log(f"{base.C.DIM}metrics written to {args.metrics_out}{base.C.RESET}")
    if args.summary_out:
        Path(args.summary_out).write_text(build_summary_md(data))
        base.log(f"{base.C.DIM}summary written to {args.summary_out}{base.C.RESET}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
