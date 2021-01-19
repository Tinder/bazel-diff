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

export USE_BAZEL_VERSION=last_downstream_green

containsElement () {
  local e match="$1"
  shift
  for e; do [[ "$e" == "$match" ]] && return 0; done
  return 1
}

createDockerBuildfileContent () {
    base_path="$(git rev-parse --show-toplevel)/integration"
    packages="$1"
    cat <<EOF > "${base_path}/BUILD.bazel"
load("@io_bazel_rules_docker//container:container.bzl", "container_image")
load("@io_bazel_rules_docker//docker/util:run.bzl", "container_run_and_commit_layer")
load("@io_bazel_rules_docker//docker/package_managers:download_pkgs.bzl", "download_pkgs")
load("@io_bazel_rules_docker//docker/package_managers:install_pkgs.bzl", "install_pkgs")

# Base docker image for our Scala services
download_pkgs(
    name = "base_packages",
    image_tar = "@openjdk_11_slim//image",
    packages = [
        "${packages}",
    ],
)

install_pkgs(
    name = "install_base_packages",
    image_tar = "@openjdk_11_slim//image",
    installables_tar = ":base_packages.tar",
    installation_cleanup_commands = "rm -rf /var/lib/apt/lists/*",
    output_image_name = "install_base_packages",
)

container_run_and_commit_layer(
    name = "base_run_commands",
    commands = [
        "wget -O /bin/grpc_health_probe https://github.com/grpc-ecosystem/grpc-health-probe/releases/download/v0.3.6/grpc_health_probe-linux-amd64",
        "chmod +x /bin/grpc_health_probe",
    ],
    image = "install_base_packages.tar",
)

container_image(
    name = "scala_base_image",
    base = "@openjdk_11_slim//image",
    layers = ["base_run_commands"],
    symlinks = {
        "/usr/bin/java": "/usr/local/openjdk-11/bin/java",
    },
    visibility = ["//visibility:public"],
)
EOF
}

$bazel_path run :bazel-diff $shared_flags -- generate-hashes -w $workspace_path -b $bazel_path $starting_hashes_json

$bazel_path run :bazel-diff $shared_flags -- generate-hashes -w $workspace_path -b $bazel_path -m $modified_filepaths_output $final_hashes_json

awk '{gsub(/:StringGenerator.java": \"\w+\"/,"modifiedhash");print}' $final_hashes_json > /dev/null

$bazel_path run :bazel-diff $shared_flags -- -sh $starting_hashes_json -fh $final_hashes_json -w $workspace_path -b $bazel_path -o $impacted_targets_path -aq "attr('tags', 'manual', //...)"

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

echo "==================================="
echo "Testing rules_docker config change"

docker_modified_filepaths_output=/tmp/docker_modified_filepaths_output.txt

createDockerBuildfileContent wget

$bazel_path run :bazel-diff $shared_flags -- generate-hashes -w $workspace_path -b $bazel_path $starting_hashes_json

createDockerBuildfileContent curl

echo "BUILD.bazel" >> $docker_modified_filepaths_output

$bazel_path run :bazel-diff $shared_flags -- generate-hashes -w $workspace_path -b $bazel_path -m $docker_modified_filepaths_output $final_hashes_json

$bazel_path run :bazel-diff $shared_flags -- -sh $starting_hashes_json -fh $final_hashes_json -w $workspace_path -b $bazel_path -o $impacted_targets_path -aq "attr('tags', 'manual', //...)"

IFS=$'\n' read -d '' -r -a impacted_targets < $impacted_targets_path
target1="//:base_packages"
if containsElement $target1 "${impacted_targets[@]}"
then
    echo "SUCCESS: Correct impacted targets: ${impacted_targets[@]}"
else
    echo "FAILURE: Incorrect impacted targets: ${impacted_targets[@]}"
    exit 1
fi
