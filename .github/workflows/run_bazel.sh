#!/usr/bin/env bash
# Run a Bazel command with a JSON build event file and, when it fails, print
# the stderr of every failed action and the test.log of every failed test.
# GitHub Actions shows those as annotations and a job summary instead of
# leaving them in the Bazel scrollback.
#
# Usage:
#   run_bazel.sh [--no-report] -- <bazel> [startup flags] <command> [args...]
#
# Every argument after `--` is the command. `--build_event_json_file` is
# inserted right after the Bazel command verb (build, test, coverage, ...),
# so startup flags, command flags, and a trailing `-- <program args>` are all
# forwarded unchanged. A caller that appends flags to this script's command
# line (the release workflow does this) forwards them too.
#
# --no-report skips the summary. Retry loops pass it on every attempt but the
# last, so a flake that later succeeds does not annotate the green job.
set -u

report=1
if [[ "${1:-}" == "--no-report" ]]; then
  report=0
  shift
fi
if [[ "${1:-}" == "--" ]]; then
  shift
fi
if [[ $# -eq 0 ]]; then
  echo "usage: run_bazel.sh [--no-report] -- <bazel> [startup flags] <command> [args...]" >&2
  exit 2
fi

# RUNNER_TEMP is a native path on every GitHub runner, including Windows,
# where Bazel would not understand an MSYS /tmp path.
bep="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/bazel-bep-$$.json"
rm -f "$bep"

cmd=("$@")
# Startup flags come before the verb and all start with `-`. The verb is the
# first argument after the executable that does not.
for ((i = 1; i < ${#cmd[@]}; i++)); do
  if [[ "${cmd[i]}" != -* ]]; then
    cmd=(
      "${cmd[@]:0:i+1}"
      "--build_event_json_file=$bep"
      "--build_event_json_file_path_conversion=false"
      "${cmd[@]:i+1}"
    )
    break
  fi
done

"${cmd[@]}"
status=$?

if [[ "$status" -ne 0 && "$report" -eq 1 ]]; then
  root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
  py=
  if command -v python3 >/dev/null 2>&1; then
    py=python3
  elif command -v python >/dev/null 2>&1; then
    py=python
  fi
  if [[ -z "$py" ]]; then
    echo "warning: python is not available; Bazel failure summary skipped" >&2
  elif [[ ! -s "$bep" ]]; then
    echo "warning: Bazel wrote no build event file; failure summary skipped" >&2
  else
    "$py" "$root/tools/report_bazel_failures.py" "$bep" || true
  fi
fi
rm -f "$bep"
exit "$status"
