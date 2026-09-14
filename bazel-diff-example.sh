#!/bin/bash

set -e

# Path to your Bazel WORKSPACE directory
workspace_path=$1
# Path to your Bazel executable
bazel_path=$2
# Starting Revision SHA
previous_revision=$3
# Final Revision SHA
final_revision=$4

# Where the hash files, the impacted-target list and the built bazel-diff
# launcher are written. Defaults to /tmp; set BAZEL_DIFF_OUTPUT_DIR to keep a
# run's outputs apart from another's.
output_dir="${BAZEL_DIFF_OUTPUT_DIR:-/tmp}"
starting_hashes_json="$output_dir/starting_hashes.json"
final_hashes_json="$output_dir/final_hashes.json"
impacted_targets_path="$output_dir/impacted_targets.txt"
bazel_diff="$output_dir/bazel_diff"

# Set appropriate flags based on environment variable
bazel_diff_flags=""
if [ "${BAZEL_DIFF_DISABLE_WORKSPACE:-false}" = "true" ]; then
  echo "Disabling workspace for bazel-diff commands (BAZEL_DIFF_DISABLE_WORKSPACE=true)"
  bazel_diff_flags="-co --enable_workspace=false"
fi
if [ -n "${BAZEL_DIFF_EXTRA_FLAGS:-}" ]; then
  echo "Injecting extra bazel-diff flags: $BAZEL_DIFF_EXTRA_FLAGS"
  bazel_diff_flags="$bazel_diff_flags $BAZEL_DIFF_EXTRA_FLAGS"
fi
# Bazel command options applied to both top-level bazel invocations and generate-hashes (via -co)
if [ -n "${BAZEL_EXTRA_COMMAND_OPTIONS:-}" ]; then
  echo "Applying extra Bazel command options to top-level and generate-hashes: $BAZEL_EXTRA_COMMAND_OPTIONS"
  bazel_diff_flags="$bazel_diff_flags -co $BAZEL_EXTRA_COMMAND_OPTIONS"
fi

# Set git checkout flags based on environment variable
git_checkout_flags="--quiet"
if [ "${BAZEL_DIFF_FORCE_CHECKOUT:-false}" = "true" ]; then
  echo "Force checkout enabled (BAZEL_DIFF_FORCE_CHECKOUT=true) - will discard uncommitted changes"
  git_checkout_flags="--force --quiet"
fi

# Build bazel-diff from this checkout, unless BAZEL_DIFF_BINARY names a
# prebuilt one. The override is what lets the e2e suite (tests/e2e/example.rs)
# drive this script against the binary it is already testing: under `bazel test`
# there is no source tree to `bazel run` from. Everyday use leaves it unset.
if [ -n "${BAZEL_DIFF_BINARY:-}" ]; then
  echo "Using prebuilt bazel-diff binary: $BAZEL_DIFF_BINARY"
  bazel_diff="$BAZEL_DIFF_BINARY"
else
  # shellcheck disable=SC2086
  "$bazel_path" run ${BAZEL_EXTRA_COMMAND_OPTIONS:-} :bazel-diff --script_path="$bazel_diff"
fi

# shellcheck disable=SC2086
git -C "$workspace_path" checkout $git_checkout_flags "$previous_revision"

echo "Generating Hashes for Revision '$previous_revision'"
# shellcheck disable=SC2086
$bazel_diff generate-hashes -w "$workspace_path" -b "$bazel_path" $bazel_diff_flags "$starting_hashes_json"

# shellcheck disable=SC2086
git -C "$workspace_path" checkout $git_checkout_flags "$final_revision"

echo "Generating Hashes for Revision '$final_revision'"
# shellcheck disable=SC2086
$bazel_diff generate-hashes -w "$workspace_path" -b "$bazel_path" $bazel_diff_flags "$final_hashes_json"

echo "Determining Impacted Targets"
$bazel_diff get-impacted-targets -w "$workspace_path" -b "$bazel_path" -sh $starting_hashes_json -fh $final_hashes_json -o $impacted_targets_path

impacted_targets=()
IFS=$'\n' read -d '' -r -a impacted_targets < $impacted_targets_path || true
formatted_impacted_targets=$(IFS=$'\n'; echo "${impacted_targets[*]}")
echo "Impacted Targets between $previous_revision and $final_revision:"
echo "$formatted_impacted_targets"
echo ""
