load("@rules_rs//rs:cargo_build_script.bzl", "cargo_build_script")
load("@rules_rs//rs:rust_binary.bzl", "rust_binary")
load("@rules_rs//rs:rust_library.bzl", "rust_library")
load("@rules_rs//rs:rust_test.bzl", "rust_test")
load("//tools/coverage:defs.bzl", "coverage_enforced_test")

cargo_build_script(
    name = "build_script",
    srcs = ["//:build.rs"],
    build_script_env = {
        "BAZEL_DIFF_ANALYSIS_PROTO": "$(execpath //:rust/proto/analysis_v2.proto)",
        "BAZEL_DIFF_BUILD_PROTO": "$(execpath //:rust/proto/build.proto)",
        "PROTOC": "$(execpath @protobuf//:protoc)",
    },
    crate_name = "build_script_build",
    data = [
        "//:rust/proto/analysis_v2.proto",
        "//:rust/proto/build.proto",
    ],
    tools = ["@protobuf//:protoc"],
    deps = ["@bazel_diff_crates//:buffa-build"],
)

rust_library(
    name = "bazel_diff_lib",
    srcs = glob(
        ["**/*.rs"],
        exclude = ["main.rs"],
    ),
    crate_name = "bazel_diff",
    crate_root = "lib.rs",
    rustc_env = {
        "CARGO_PKG_VERSION": module_version() if module_version() else "unknown",
    },
    # //:rust_clippy_check and //:rust_format_check name this crate directly.
    visibility = ["//:__pkg__"],
    deps = [
        ":build_script",
        "@bazel_diff_crates//:anyhow",
        "@bazel_diff_crates//:buffa",
        "@bazel_diff_crates//:hex",
        "@bazel_diff_crates//:rayon",
        "@bazel_diff_crates//:rust-s3",
        "@bazel_diff_crates//:serde",
        "@bazel_diff_crates//:serde_json",
        "@bazel_diff_crates//:sha2",
        "@bazel_diff_crates//:tempfile",
        "@bazel_diff_crates//:tiny_http",
        "@bazel_diff_crates//:url",
        "@bazel_diff_crates//:walkdir",
    ],
)

rust_binary(
    name = "bazel-diff",
    srcs = ["main.rs"],
    crate_root = "main.rs",
    rustc_env = {
        "CARGO_PKG_VERSION": module_version() if module_version() else "unknown",
    },
    visibility = ["//visibility:public"],
    deps = [
        ":bazel_diff_lib",
        "@bazel_diff_crates//:anyhow",
        "@bazel_diff_crates//:clap",
        "@bazel_diff_crates//:mimalloc",
        "@bazel_diff_crates//:serde",
        "@bazel_diff_crates//:serde_json",
        "@bazel_diff_crates//:tempfile",
    ],
)

coverage_enforced_test(
    name = "rust_tests",
    coverage_include = ["src/"],
    crate = ":bazel_diff_lib",
    rule = rust_test,
    visibility = ["//:__pkg__"],
)

coverage_enforced_test(
    name = "cli_tests",
    coverage_include = ["src/main.rs"],
    crate = ":bazel-diff",
    rule = rust_test,
    visibility = ["//:__pkg__"],
)
