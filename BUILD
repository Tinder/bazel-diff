load("@rules_kotlin//kotlin:core.bzl", "define_kt_toolchain")
load("@rules_license//rules:license.bzl", "license")

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

package(
    default_applicable_licenses = [":license"],
    default_visibility = ["//visibility:public"],
)

license(
    name = "license",
    package_name = "bazel-diff",
    copyright_notice = "Copyright (c) 2020, Match Group, LLC",
    license_kind = "@rules_license//licenses/spdx:BSD-3-Clause",
    license_text = "LICENSE",
    package_url = "https://github.com/Tinder/bazel-diff",
    package_version = "39.0.1",
)

define_kt_toolchain(
    name = "kotlin_toolchain",
    jvm_target = "11",
)
