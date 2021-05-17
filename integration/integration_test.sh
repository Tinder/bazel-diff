#!/bin/bash

workspace_path="$PWD/integration"
bazel_path=$(which bazelisk)

previous_revision="HEAD^"
final_revision="HEAD"
output_dir="/tmp"
modified_filepaths_output="$workspace_path/modified_filepaths.txt"
starting_hashes_json="$output_dir/starting_hashes.json"
final_hashes_json="$output_dir/final_hashes.json"
impacted_targets_path="$output_dir/impacted_targets.txt"
shared_flags="--config=verbose"
command_options="--incompatible_restrict_string_escapes=false"

export USE_BAZEL_VERSION=last_downstream_green

containsElement () {
  local e match="$1"
  shift
  for e; do [[ "$e" == "$match" ]] && return 0; done
  return 1
}

$bazel_path run :bazel-diff $shared_flags -- generate-hashes -w $workspace_path -b $bazel_path $starting_hashes_json -co $command_options

$bazel_path run :bazel-diff $shared_flags -- generate-hashes -w $workspace_path -b $bazel_path -m $modified_filepaths_output $final_hashes_json -co $command_options

awk '{gsub(/:StringGenerator.java": \"\w+\"/,"modifiedhash");print}' $final_hashes_json > /dev/null

$bazel_path run :bazel-diff $shared_flags -- -sh $starting_hashes_json -fh $final_hashes_json -w $workspace_path -b $bazel_path -o $impacted_targets_path -aq "attr('tags', 'manual', //...)" -co $command_options

IFS=$'\n' read -d '' -r -a impacted_targets < $impacted_targets_path
target1="//test/java/com/integration:bazel-diff-integration-test-lib"
target2="//src/main/java/com/integration:bazel-diff-integration-lib"
target3="//test/java/com/integration:bazel-diff-integration-tests"
target4="//src/main/java/com/integration/submodule:Submodule"
if containsElement $target1 "${impacted_targets[@]}" && \
    containsElement $target2 "${impacted_targets[@]}" && \
    containsElement $target3 "${impacted_targets[@]}"
then
    if containsElement $target4 "${impacted_targets[@]}"
    then
        echo "FAILURE Incorrect impacted targets: ${impacted_targets[@]}"
        exit 1
    else
        echo "SUCCESS: Correct impacted targets: ${impacted_targets[@]}"
    fi
else
    echo "FAILURE: Incorrect impacted targets: ${impacted_targets[@]}"
    exit 1
fi
