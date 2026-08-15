load("//release:defs.bzl", "RELEASE_ASSET_NAME", "release_binary")

package(default_visibility = ["//visibility:public"])

# Built by the `deploy` job in ci.yaml and the `rust-binaries` job in
# release.yaml, which upload the file it writes to bazel-bin/release/ as-is.
release_binary(
    name = "bazel-diff-rust",
    asset_name = RELEASE_ASSET_NAME,
    binary = "//:bazel-diff-rust",
)
