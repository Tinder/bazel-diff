load("@bazel_diff_crates//:defs.bzl", "all_crate_deps")
load("@rules_rust//cargo:defs.bzl", "cargo_build_script")
load("@rules_rust//rust:defs.bzl", "rust_binary", "rust_library", "rust_test")
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
    deps = all_crate_deps(
        package_name = "",
        build = True,
    ),
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
    deps = all_crate_deps(
        package_name = "",
        normal = True,
    ) + [":build_script"],
)

rust_binary(
    name = "bazel-diff",
    srcs = ["main.rs"],
    crate_root = "main.rs",
    rustc_env = {
        "CARGO_PKG_VERSION": module_version() if module_version() else "unknown",
    },
    visibility = ["//visibility:public"],
    deps = all_crate_deps(
        package_name = "",
        normal = True,
    ) + [":bazel_diff_lib"],
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
