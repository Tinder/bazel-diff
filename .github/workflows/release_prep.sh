#!/usr/bin/env bash

set -o errexit -o nounset -o pipefail

# Argument provided by reusable workflow caller, see
# https://github.com/bazel-contrib/.github/blob/master/.github/workflows/release_ruleset.yaml
TAG=$1

"$(dirname "$0")/pack_release_archive.sh" archives/release.tar.gz

SHA=$(shasum -a 256 archives/release.tar.gz | awk '{print $1}')

cat << EOF
## Using Bzlmod (MODULE.bazel)

Add to your \`MODULE.bazel\` file:

\`\`\`starlark
bazel_dep(name = "bazel-diff", version = "${TAG#v}")
\`\`\`

Then run the tool with \`bazel run @bazel-diff//:bazel-diff\`.

Source archive SHA-256: \`${SHA}\`

## Prebuilt binaries

Statically linked (Linux) and host-native (macOS, Windows) CLIs, attached by the
release workflow:

- Linux amd64: https://github.com/Tinder/bazel-diff/releases/download/${TAG}/bazel-diff-rust-linux-amd64
- Linux arm64: https://github.com/Tinder/bazel-diff/releases/download/${TAG}/bazel-diff-rust-linux-arm64
- macOS arm64: https://github.com/Tinder/bazel-diff/releases/download/${TAG}/bazel-diff-rust-macos-arm64
- Windows amd64: https://github.com/Tinder/bazel-diff/releases/download/${TAG}/bazel-diff-rust-windows-amd64.exe
EOF
