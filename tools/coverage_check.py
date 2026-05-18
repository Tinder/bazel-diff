"""Enforce a minimum line-coverage threshold on a Bazel LCOV report.

Usage:
    coverage_check.py [--threshold N] [--include PREFIX,...] [LCOV_FILE]

Defaults:
    LCOV_FILE   bazel-out/_coverage/_coverage_report.dat
    --threshold 90 (or $COVERAGE_THRESHOLD)
    --include   cli/src/main/,tools/coverage_check.py

Only production-source files (whose path matches one of the include prefixes)
contribute to the numerator and denominator. Bazel's Kotlin and Python
instrumentation report test sources too; including them would let thin
production-code coverage hide behind well-covered tests, so they are stripped.

Exit codes:
    0  overall coverage meets or exceeds the threshold
    1  overall coverage is below the threshold
    2  the LCOV report is missing, unreadable, or has no production-source lines
"""

import argparse
import os
import sys
from dataclasses import dataclass
from typing import Iterable, List


@dataclass(frozen=True)
class FileCoverage:
    """Per-file line coverage extracted from one LCOV `SF` record."""

    path: str
    lines_found: int
    lines_hit: int

    @property
    def pct(self) -> float:
        return (self.lines_hit / self.lines_found * 100.0) if self.lines_found else 0.0


def parse_lcov(text: str) -> List[FileCoverage]:
    """Parse an LCOV `.info`/`.dat` blob into per-file coverage records.

    Only `SF`, `LF`, `LH`, and `end_of_record` lines are read; everything else
    (`FN`, `FNDA`, `DA`, `BRF`, etc.) is ignored because the threshold is
    line-coverage-based.

    Malformed integer payloads on `LF`/`LH` fall back to 0 rather than raising,
    so a single bad record can't take down the whole check.
    """
    records: List[FileCoverage] = []
    sf = None
    lf = 0
    lh = 0
    for raw in text.splitlines():
        if raw.startswith("SF:"):
            sf = raw[3:]
            lf = 0
            lh = 0
        elif raw.startswith("LF:") and sf is not None:
            try:
                lf = int(raw[3:])
            except ValueError:
                lf = 0
        elif raw.startswith("LH:") and sf is not None:
            try:
                lh = int(raw[3:])
            except ValueError:
                lh = 0
        elif raw == "end_of_record" and sf is not None:
            records.append(FileCoverage(sf, lf, lh))
            sf = None
    return records


def filter_main_source(
    records: Iterable[FileCoverage], include_prefixes: List[str]
) -> List[FileCoverage]:
    """Keep records whose path begins with one of `include_prefixes` and that
    have at least one instrumented line.

    Files with `LF:0` are dropped because LCOV has no measurable content for
    them. This happens in two ways: (a) the file genuinely has no
    instrumentable lines (rare), and (b) the language's instrumentation tool
    didn't run for that file. Bazel's Python coverage in particular only
    gathers data when the toolchain is configured with `coverage_tool` set,
    so without that step a `py_test` produces an `LF:0`/`LH:0` placeholder
    record for its sources. Treating that as "0% covered" would be misleading
    and tank the threshold for files that are actually well-tested.
    """
    if not include_prefixes:
        return []
    return [
        r
        for r in records
        if r.lines_found > 0
        and any(r.path.startswith(p) for p in include_prefixes)
    ]


def format_report(
    records: List[FileCoverage], total_lh: int, total_lf: int, threshold: float
) -> str:
    """Render the per-file table + overall summary as a multi-line string."""
    lines = []
    lines.append(f"{'COV%':>8}  {'LINES (hit/total)':<17}  FILE")
    lines.append(f"{'-' * 8:>8}  {'-' * 17:<17}  ----")
    for r in sorted(records, key=lambda r: (r.pct, r.path)):
        lines.append(
            f"{r.pct:>7.2f}%  {r.lines_hit:>5} / {r.lines_found:<7}    {r.path}"
        )
    overall = (total_lh / total_lf * 100.0) if total_lf else 0.0
    lines.append("")
    lines.append(
        f"Overall main-source line coverage: {overall:.2f}% ({total_lh} / {total_lf})"
    )
    lines.append(f"Threshold:                         {threshold:.2f}%")
    return "\n".join(lines)


def _chdir_to_workspace_if_invoked_via_bazel_run() -> None:
    """`bazel run //tools:coverage-check -- bazel-out/_coverage/...` would otherwise
    leave us in the runfiles directory and fail to find the LCOV file. When Bazel
    sets `BUILD_WORKING_DIRECTORY` (the user's cwd at the time they ran the binary)
    chdir to it so relative paths resolve the way the user expects.
    """
    cwd = os.environ.get("BUILD_WORKING_DIRECTORY")
    if cwd and os.path.isdir(cwd):
        os.chdir(cwd)


def main(argv: List[str] | None = None) -> int:
    _chdir_to_workspace_if_invoked_via_bazel_run()

    parser = argparse.ArgumentParser(
        description="Enforce a minimum line-coverage threshold on a Bazel LCOV report.",
    )
    parser.add_argument(
        "lcov",
        nargs="?",
        default="bazel-out/_coverage/_coverage_report.dat",
        help="Path to the LCOV report (default: %(default)s).",
    )
    parser.add_argument(
        "--threshold",
        "-t",
        type=float,
        default=float(os.environ.get("COVERAGE_THRESHOLD", "90")),
        help=(
            "Minimum overall line coverage percentage. "
            "Default 90 (override with --threshold or $COVERAGE_THRESHOLD)."
        ),
    )
    parser.add_argument(
        "--include",
        default=os.environ.get(
            "COVERAGE_INCLUDE", "cli/src/main/,tools/coverage_check.py"
        ),
        help=(
            "Comma-separated path prefixes counted as production code "
            "(default: cli/src/main/,tools/coverage_check.py)."
        ),
    )
    args = parser.parse_args(argv)

    if not os.path.isfile(args.lcov):
        print(f"error: LCOV report not found at '{args.lcov}'.", file=sys.stderr)
        print(
            "Hint: run 'bazel coverage --combined_report=lcov //cli/... //tools/...' first,",
            file=sys.stderr,
        )
        print("      or pass an explicit path as the first argument.", file=sys.stderr)
        return 2

    with open(args.lcov, "r", encoding="utf-8", errors="replace") as f:
        all_records = parse_lcov(f.read())

    include_prefixes = [p.strip() for p in args.include.split(",") if p.strip()]
    records = filter_main_source(all_records, include_prefixes)

    total_lf = sum(r.lines_found for r in records)
    total_lh = sum(r.lines_hit for r in records)
    if total_lf == 0:
        print(
            f"error: no instrumented production-source lines found in {args.lcov}.",
            file=sys.stderr,
        )
        print(f"  include prefixes: {include_prefixes!r}", file=sys.stderr)
        return 2

    print(format_report(records, total_lh, total_lf, args.threshold))

    overall = total_lh / total_lf * 100.0
    # Allow a tiny epsilon so 89.99999... that displays as 90.00 still passes a 90
    # threshold. Use the raw value, not the rounded one, otherwise 89.99 would slip
    # through.
    if overall + 1e-9 < args.threshold:
        print(
            f"FAIL: coverage {overall:.2f}% is below threshold {args.threshold:.2f}%.",
            file=sys.stderr,
        )
        return 1
    print("PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
