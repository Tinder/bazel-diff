#!/bin/bash

# Path to your Bazel WORKSPACE directory
workspace_path=$1
# Path to your Bazel executable
bazel_path=$2
# Starting Revision SHA
previous_revision=$3
# Final Revision SHA
final_revision=$4

starting_hashes_json="/tmp/starting_hashes.json"
final_hashes_json="/tmp/final_hashes.json"
impacted_targets_path="/tmp/impacted_targets.txt"
bazel_diff="/tmp/bazel_diff"

shared_flags=""

# Uncomment the line below to see debug information
# shared_flags="--config=verbose"

"$bazel_path" run :bazel-diff "$shared_flags" --script_path="$bazel_diff"

git -C "$workspace_path" checkout "$previous_revision" --quiet

echo "Generating Hashes for Revision '$previous_revision'"
$bazel_diff generate-hashes -w "$workspace_path" -b "$bazel_path" $starting_hashes_json

git -C "$workspace_path" checkout $final_revision --quiet

echo "Generating Hashes for Revision '$final_revision'"
$bazel_diff generate-hashes -w "$workspace_path" -b "$bazel_path" $final_hashes_json

echo "Determining Impacted Targets"
$bazel_diff get-impacted-targets -sh $starting_hashes_json -fh $final_hashes_json -o $impacted_targets_path

IFS=$'\n' read -d '' -r -a impacted_targets < $impacted_targets_path
formatted_impacted_targets=$(IFS=$'\n'; echo "${impacted_targets[*]}")
echo "Impacted Targets between $previous_revision and $final_revision:"
echo "$formatted_impacted_targets"
echo ""
