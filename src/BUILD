load("@rules_rs//rs:cargo_build_script.bzl", "cargo_build_script")
load("@rules_rs//rs:rust_binary.bzl", "rust_binary")
load("@rules_rs//rs:rust_library.bzl", "rust_library")
load("@rules_rs//rs:rust_test.bzl", "rust_test")
load("//:crate_deps.bzl", "crate_features", "normal_crate_deps")
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
    # Bazel supplies protoc, so omit Cargo's optional protoc-bin-vendored dependency.
    deps = ["@bd_crates//:buffa-build"],
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
    deps = normal_crate_deps() + [":build_script"],
)

# Public: BCR presubmit builds this module as a dependency with
# --incompatible_config_setting_private_default_visibility, under which a
# config_setting without a visibility is private even to the select() below.
config_setting(
    name = "without_mimalloc",
    values = {"features": "-mimalloc"},
    visibility = ["//visibility:public"],
)

rust_binary(
    name = "bazel-diff",
    srcs = ["main.rs"],
    crate_features = select({
        ":without_mimalloc": crate_features(exclude = ["mimalloc"]),
        "//conditions:default": crate_features(),
    }),
    crate_root = "main.rs",
    rustc_env = {
        "CARGO_PKG_VERSION": module_version() if module_version() else "unknown",
    },
    visibility = ["//visibility:public"],
    deps = normal_crate_deps() + [":bazel_diff_lib"],
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
