#!/usr/bin/env bash
# Run a command, usually bazelisk build or bazelisk test, and when it fails
# parse the log for the build and test errors. GitHub Actions then shows those
# as annotations and a job summary instead of leaving them in the Bazel scrollback.
#
# Usage:
#   run_bazel.sh [--no-report] -- <command> [args...]
#
# Every argument after `--` is the command. A caller that appends flags to this
# script's command line (the release workflow does this) forwards them too.
#
# --no-report skips the summary. Retry loops pass it on every attempt but the
# last, so a flake that later succeeds does not annotate the green job.
set -uo pipefail

report=1
if [[ "${1:-}" == "--no-report" ]]; then
  report=0
  shift
fi
if [[ "${1:-}" == "--" ]]; then
  shift
fi
if [[ $# -eq 0 ]]; then
  echo "usage: run_bazel.sh [--no-report] -- <command> [args...]" >&2
  exit 2
fi

log=$(mktemp)
set +e
"$@" 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
set -e

if [[ "$status" -ne 0 && "$report" -eq 1 ]]; then
  root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
  py=
  if command -v python3 >/dev/null 2>&1; then
    py=python3
  elif command -v python >/dev/null 2>&1; then
    py=python
  fi
  if [[ -n "$py" ]]; then
    "$py" "$root/tools/report_bazel_failures.py" "$log" || true
  else
    echo "warning: python is not available; Bazel failure summary skipped" >&2
  fi
fi
rm -f "$log"
exit "$status"
