#!/usr/bin/env python3
"""Synthetic Bazel workspace generator for bazel-diff benchmarking.

Produces a large, self-contained Bazel workspace that uses **only native
genrule + source-file targets** — no external toolchains, no network, no
rules_* downloads. The point is to stress the exact thing the Firecracker
snapshot RFC targets: the cost of `bazel query deps(//...)` (Skyframe graph
load + package analysis) on a large graph.

The graph is a layered DAG: package ``i`` depends on a few lower-indexed
packages, giving both depth (long dependency chains) and width (many
packages). Every package contains real source files so that bazel-diff's
SourceFileHasher has content to hash, plus a chain of genrules that consume
those sources and the outputs of upstream packages.

Optionally (``--git``) the generator initialises a git repo with two commits:
a ``base`` commit and a ``change`` commit that mutates a single source file
deep in the graph. The two SHAs are what the benchmark / orchestrator diffs.

Example:

    python3 gen_project.py --out /tmp/bigproj --packages 2000 \
        --targets-per-package 4 --git

    # prints JSON: {"workspace": "...", "base_sha": "...", "target_sha": "..."}
"""

from __future__ import annotations

import argparse
import json
import os
import random
import subprocess
import sys
from pathlib import Path

# Bazel version the generated workspace pins. Kept in sync with the repo root
# .bazelversion so a single bazelisk install serves both.
DEFAULT_BAZEL_VERSION = "8.5.1"


def _layer_of(i: int, packages: int, layers: int) -> int:
    return (i * layers) // packages


def _upstream_deps(i: int, packages: int, layers: int, fanin: int) -> list[int]:
    """Pick a deterministic set of upstream packages for package ``i``.

    Packages are partitioned into ``layers`` contiguous bands; package ``i`` (in
    layer ``k``) depends on ``fanin`` packages drawn from layer ``k-1``. This
    keeps the dependency-graph **depth bounded by ``layers``** (a realistic,
    modular monorepo) instead of forming one long chain — important because
    bazel-diff hashes transitively with a per-level dependency-path set, so an
    N-deep chain costs O(N^2) memory and an N-frame recursion.

    Always acyclic (deps live in a strictly-lower layer) and deterministic.
    """
    k = _layer_of(i, packages, layers)
    if k == 0:
        return []
    lo = ((k - 1) * packages) // layers  # first index of previous layer
    hi = (k * packages) // layers        # first index of this layer (exclusive bound)
    span = hi - lo
    if span <= 0:
        return []
    deps = set()
    for j in range(fanin):
        deps.add(lo + (i * 2654435761 + j * 40503) % span)
    return sorted(deps)


def _write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)


def gen_workspace(
    out: Path,
    packages: int,
    targets_per_package: int,
    sources_per_package: int,
    fanin: int,
    layers: int,
    bazel_version: str,
) -> dict:
    out.mkdir(parents=True, exist_ok=True)

    # --- workspace-level files (bzlmod, no external deps) ---
    _write(out / ".bazelversion", bazel_version + "\n")
    _write(
        out / "MODULE.bazel",
        'module(name = "bench", version = "0.0.0")\n',
    )
    _write(
        out / ".bazelrc",
        # Keep analysis honest and reproducible; no remote cache so each cold
        # run actually re-analyses.
        "common --lockfile_mode=off\n"
        "common --color=no\n",
    )
    _write(out / "BUILD.bazel", '# root package (intentionally empty)\n')
    _write(out / ".gitignore", "/bazel-*\n")

    total_targets = 0
    # Remember a deep source file to mutate for the "change" commit.
    deep_source: Path | None = None

    for i in range(packages):
        pkg_dir = out / "pkg" / f"p{i:05d}"
        rel_pkg = f"//pkg/p{i:05d}"

        # source files
        src_labels = []
        for s in range(sources_per_package):
            fname = f"src_{s}.txt"
            _write(pkg_dir / fname, f"package {i} source {s}\n")
            src_labels.append(f'"{fname}"')

        deps = _upstream_deps(i, packages, layers, fanin)

        lines = [
            'package(default_visibility = ["//visibility:public"])',
            "",
        ]

        # genrule chain within the package
        prev_out_label = None
        for t in range(targets_per_package):
            name = f"g{t}"
            srcs = list(src_labels)
            if prev_out_label is not None:
                srcs.append(f'"{prev_out_label}"')
            elif deps:
                # first genrule pulls in upstream packages' final outputs
                for d in deps:
                    srcs.append(f'"//pkg/p{d:05d}:g{targets_per_package - 1}"')
            out_file = f"out_{t}.txt"
            lines += [
                "genrule(",
                f'    name = "{name}",',
                f"    srcs = [{', '.join(srcs)}],",
                f'    outs = ["{out_file}"],',
                '    cmd = "cat $(SRCS) > $@",',
                ")",
                "",
            ]
            prev_out_label = name
            total_targets += 1

        # a filegroup aggregating everything (extra graph edges)
        lines += [
            "filegroup(",
            '    name = "all_srcs",',
            f"    srcs = [{', '.join(src_labels)}],",
            ")",
            "",
        ]
        total_targets += 1

        _write(pkg_dir / "BUILD.bazel", "\n".join(lines))

        # remember a file roughly 75% deep to mutate later
        if i == int(packages * 0.75):
            deep_source = pkg_dir / "src_0.txt"

    if deep_source is None:
        deep_source = out / "pkg" / "p00000" / "src_0.txt"

    return {
        "workspace": str(out),
        "packages": packages,
        "rule_targets": total_targets,
        "deep_source": str(deep_source.relative_to(out)),
        "bazel_version": bazel_version,
    }


def _git(out: Path, *args: str) -> str:
    return subprocess.check_output(
        ["git", "-C", str(out), *args], text=True
    ).strip()


def init_git(out: Path, deep_source_rel: str) -> dict:
    if (out / ".git").exists():
        raise SystemExit(f"{out} already has a .git; refusing to re-init")
    _git(out, "init", "-q")
    _git(out, "config", "user.email", "bench@bazel-diff.local")
    _git(out, "config", "user.name", "bazel-diff bench")
    _git(out, "add", "-A")
    _git(out, "commit", "-q", "-m", "base")
    base_sha = _git(out, "rev-parse", "HEAD")

    # mutate one deep source file -> minimal, realistic PR-sized change
    deep = out / deep_source_rel
    with deep.open("a") as f:
        f.write("mutated for change commit\n")
    _git(out, "add", "-A")
    _git(out, "commit", "-q", "-m", "change")
    target_sha = _git(out, "rev-parse", "HEAD")

    return {"base_sha": base_sha, "target_sha": target_sha}


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--out", required=True, help="output workspace directory")
    p.add_argument("--packages", type=int, default=2000)
    p.add_argument("--targets-per-package", type=int, default=4)
    p.add_argument("--sources-per-package", type=int, default=3)
    p.add_argument(
        "--fanin",
        type=int,
        default=4,
        help="upstream packages each package depends on (from the previous layer)",
    )
    p.add_argument(
        "--layers",
        type=int,
        default=40,
        help="number of dependency layers; bounds graph depth (and recursion depth)",
    )
    p.add_argument("--bazel-version", default=DEFAULT_BAZEL_VERSION)
    p.add_argument(
        "--git",
        action="store_true",
        help="git init + create base/change commits",
    )
    p.add_argument("--seed", type=int, default=0)
    args = p.parse_args(argv)

    random.seed(args.seed)
    out = Path(args.out).resolve()

    meta = gen_workspace(
        out,
        packages=args.packages,
        targets_per_package=args.targets_per_package,
        sources_per_package=args.sources_per_package,
        fanin=args.fanin,
        layers=args.layers,
        bazel_version=args.bazel_version,
    )

    if args.git:
        meta.update(init_git(out, meta["deep_source"]))

    json.dump(meta, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
