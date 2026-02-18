load("@rules_kotlin//kotlin:core.bzl", "define_kt_toolchain")

alias(
    name = "bazel-diff",
    actual = "//cli:bazel-diff",
)

alias(
    name = "format",
    actual = "//cli/format:format",
)

define_kt_toolchain(
    name = "kotlin_toolchain",
    jvm_target = "11",
)
