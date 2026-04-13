#!/usr/bin/env bash

set -o errexit -o nounset -o pipefail

# Argument provided by reusable workflow caller, see
# https://github.com/bazel-contrib/.github/blob/master/.github/workflows/release_ruleset.yaml
TAG=$1

mkdir -p archives
tar --exclude-vcs \
  --exclude=bazel-* \
  --exclude=.github \
  --exclude=archives \
  -zcf "archives/release.tar.gz" .

make release_deploy_jar &> /dev/null

cp bazel-bin/cli/bazel-diff_deploy.jar archives/bazel-diff_deploy.jar

SHA=$(shasum -a 256 archives/release.tar.gz | awk '{print $1}')

cat << EOF
## Using Bzlmod (MODULE.bazel)

Add to your \`MODULE.bazel\` file:

\`\`\`starlark
bazel_dep(name = "bazel-diff", version = "${TAG#v}")
\`\`\`

## Using WORKSPACE

\`\`\`starlark
load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive")
http_archive(
  name = "bazel-diff",
  sha256 = "${SHA}",
  strip_prefix = "",
  url = "https://github.com/Tinder/bazel-diff/releases/download/${TAG}/release.tar.gz",
)
\`\`\`
EOF
