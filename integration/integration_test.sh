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
shared_flags=""

export USE_BAZEL_VERSION=4.1.0

containsElement () {
  local e match="$1"
  shift
  for e; do [[ "$e" == "$match" ]] && return 0; done
  return 1
}

$bazel_path run :bazel-diff $shared_flags -- generate-hashes -w $workspace_path -b $bazel_path $starting_hashes_json

$bazel_path run :bazel-diff $shared_flags -- generate-hashes -w $workspace_path -b $bazel_path $final_hashes_json

final_tmp=$(mktemp)
awk '{gsub(/32c874ad651f0dd234d712b7f1dce6b54aa36cf16380da763a8eb7bb7c1f9514/,"modifiedhash");print}' $final_hashes_json > $final_tmp
cp $final_tmp $final_hashes_json

$bazel_path run :bazel-diff $shared_flags -- -sh $starting_hashes_json -fh $final_hashes_json -w $workspace_path -b $bazel_path -o $impacted_targets_path

IFS=$'\n' read -d '' -r -a impacted_targets < $impacted_targets_path
target1="//src/main/java/com/integration:StringGenerator.java"
if containsElement $target1 "${impacted_targets[@]}"
then
    echo "SUCCESS: Correct impacted targets: ${impacted_targets[@]}"
else
    echo "FAILURE: Incorrect impacted targets: ${impacted_targets[@]}"
    exit 1
fi
