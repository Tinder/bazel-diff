load("@rules_rust//rust:defs.bzl", "rust_binary", "rust_library", "rust_test")
load("//tools/coverage:defs.bzl", "coverage_enforced_test")

# The LCOV merger Bazel invokes inside every test action during
# `bazel coverage` runs (wired up via --coverage_output_generator in
# .bazelrc). Besides merging, it enforces the per-target line-coverage
# minimums declared through //tools/coverage:defs.bzl. See README.md.
rust_library(
    name = "lcov_merger_lib",
    srcs = [
        "src/args.rs",
        "src/enforce.rs",
        "src/lcov.rs",
        "src/lib.rs",
        "src/pattern.rs",
    ],
    crate_name = "lcov_merger",
    crate_root = "src/lib.rs",
    edition = "2021",
)

rust_binary(
    name = "lcov_merger",
    srcs = ["src/main.rs"],
    edition = "2021",
    visibility = ["//visibility:public"],
    deps = [":lcov_merger_lib"],
)

# The merger's own tests carry a coverage minimum, enforced — pleasingly —
# by the merger itself when this package runs under `bazel coverage`.
coverage_enforced_test(
    name = "lcov_merger_test",
    coverage_include = ["tools/coverage/src/"],
    crate = ":lcov_merger_lib",
    rule = rust_test,
    # //:rust_clippy_check and //:rust_format_check name this target directly.
    visibility = ["//:__pkg__"],
)
