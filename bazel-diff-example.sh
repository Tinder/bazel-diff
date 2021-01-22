#!/bin/bash

# Path to your Bazel WORKSPACE directory
workspace_path=$1
# Path to your Bazel executable
bazel_path=$2
# Starting Revision SHA
previous_revision=$3
# Final Revision SHA
final_revision=$4

modified_filepaths_output="/tmp/modified_filepaths.txt"
starting_hashes_json="/tmp/starting_hashes.json"
final_hashes_json="/tmp/final_hashes.json"
impacted_targets_path="/tmp/impacted_targets.txt"
impacted_test_targets_path="/tmp/impacted_test_targets.txt"
bazel_diff="/tmp/bazel_diff"

shared_flags=""

# Uncomment the line below to see debug information
# shared_flags="--config=verbose"

$bazel_path run :bazel-diff $shared_flags --script_path="$bazel_diff"

$bazel_diff modified-filepaths $previous_revision $final_revision -w $workspace_path -b $bazel_path $modified_filepaths_output

IFS=$'\n' read -d '' -r -a modified_filepaths < $modified_filepaths_output
formatted_filepaths=$(IFS=$'\n'; echo "${modified_filepaths[*]}")
echo "Modified Filepaths:"
echo $formatted_filepaths
echo ""

git -C $workspace_path checkout $previous_revision --quiet

echo "Generating Hashes for Revision '$previous_revision'"
$bazel_diff generate-hashes -w $workspace_path -b $bazel_path $starting_hashes_json

git -C $workspace_path checkout $final_revision --quiet

echo "Generating Hashes for Revision '$final_revision'"
$bazel_diff generate-hashes -w $workspace_path -b $bazel_path -m $modified_filepaths_output $final_hashes_json

echo "Determining Impacted Targets"
$bazel_diff -sh $starting_hashes_json -fh $final_hashes_json -w $workspace_path -b $bazel_path -o $impacted_targets_path

echo "Determining Impacted Test Targets"
$bazel_diff -sh $starting_hashes_json -fh $final_hashes_json -w $workspace_path -b $bazel_path -o $impacted_test_targets_path --avoid-query "//... except tests(//...)"

IFS=$'\n' read -d '' -r -a impacted_targets < $impacted_targets_path
formatted_impacted_targets=$(IFS=$'\n'; echo "${impacted_targets[*]}")
echo "Impacted Targets between $previous_revision and $final_revision:"
echo $formatted_impacted_targets
echo ""

IFS=$'\n' read -d '' -r -a impacted_test_targets < $impacted_test_targets_path
formatted_impacted_test_targets=$(IFS=$'\n'; echo "${impacted_test_targets[*]}")
echo "Impacted Test Targets between $previous_revision and $final_revision:"
echo $formatted_impacted_test_targets
echo ""
