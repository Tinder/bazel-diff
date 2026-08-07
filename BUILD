load("@rules_kotlin//kotlin:core.bzl", "define_kt_toolchain")

exports_files(
    [
        "build.rs",
        "rust/proto/analysis_v2.proto",
        "rust/proto/build.proto",
    ],
    visibility = ["//src:__pkg__"],
)

alias(
    name = "bazel-diff",
    actual = "//cli:bazel-diff",
)

alias(
    name = "bazel-diff-rust",
    actual = "//src:bazel-diff",
    visibility = ["//visibility:public"],
)

test_suite(
    name = "rust_tests",
    tests = ["//src:rust_tests"],
)

alias(
    name = "format",
    actual = "//cli/format:format",
)

define_kt_toolchain(
    name = "kotlin_toolchain",
    jvm_target = "11",
)
