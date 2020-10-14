#!/bin/bash

workspace_path="$PWD/integration"
bazel_path=$(which bazelisk)

previous_revision="HEAD^"
final_revision="HEAD"
modified_filepaths_output="$PWD/integration/modified_filepaths.txt"
starting_hashes_json="/tmp/starting_hashes.json"
final_hashes_json="/tmp/final_hashes_json.json"
impacted_targets_path="/tmp/impacted_targets.txt"
impacted_test_targets_path="/tmp/impacted_test_targets.txt"

export USE_BAZEL_VERSION=last_downstream_green

containsElement () {
  local e match="$1"
  shift
  for e; do [[ "$e" == "$match" ]] && return 0; done
  return 1
}

$bazel_path run :bazel-diff -- generate-hashes -w $workspace_path -b $bazel_path $starting_hashes_json

$bazel_path run :bazel-diff -- generate-hashes -w $workspace_path -b $bazel_path -m $modified_filepaths_output $final_hashes_json

ruby ./integration/update_final_hashes.rb

$bazel_path run :bazel-diff -- -sh $starting_hashes_json -fh $final_hashes_json -w $workspace_path -b $bazel_path -o $impacted_targets_path

$bazel_path run :bazel-diff -- -sh $starting_hashes_json -fh $final_hashes_json -w $workspace_path -b $bazel_path -o $impacted_test_targets_path -t

IFS=$'\n' read -d '' -r -a impacted_targets < $impacted_targets_path
target1="//test/java/com/integration:bazel-diff-integration-test-lib"
target2="//src/main/java/com/integration:bazel-diff-integration-lib"
target3="//test/java/com/integration:bazel-diff-integration-tests"
if containsElement $target1 "${impacted_targets[@]}" && \
    containsElement $target2 "${impacted_targets[@]}" && \
    containsElement $target3 "${impacted_targets[@]}"
then
    echo "Correct impacted targets"
else
    echo "Incorrect impacted targets: ${impacted_targets[@]}"
    exit 1
fi

IFS=$'\n' read -d '' -r -a impacted_test_targets < $impacted_test_targets_path
target="//test/java/com/integration:bazel-diff-integration-tests"
if containsElement $target "${impacted_test_targets[@]}";
then
    echo "Correct first impacted test target"
else
    echo "Impacted test targets: ${impacted_test_targets[@]}"
    echo "Incorrect first impacted test target: ${target}"
    exit 1
fi
