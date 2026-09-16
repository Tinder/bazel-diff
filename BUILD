load("@rules_license//rules:license.bzl", "license")
load("@rules_rust//rust:defs.bzl", "rust_clippy_test", "rustfmt_test")

exports_files(
    [
        "build.rs",
        "rust/proto/analysis_v2.proto",
        "rust/proto/build.proto",
    ],
    visibility = ["//src:__pkg__"],
)

# The README's getting-started script. //tests:e2e_test runs it end to end
# (tests/e2e/example.rs), so it has to reach that test as a runfile.
exports_files(
    ["bazel-diff-example.sh"],
    visibility = ["//tests:__pkg__"],
)

alias(
    name = "bazel-diff",
    actual = "//src:bazel-diff",
    visibility = ["//visibility:public"],
)

# Kept for consumers that adopted the Rust binary while it was published next
# to the JVM one under this name (`bazel run @bazel-diff//:bazel-diff-rust`).
# It is the same binary as //:bazel-diff; prefer that label.
alias(
    name = "bazel-diff-rust",
    actual = "//src:bazel-diff",
    visibility = ["//visibility:public"],
)

test_suite(
    name = "rust_tests",
    tests = [
        "//src:cli_tests",
        "//src:rust_tests",
    ],
)

# What CI runs in place of `cargo clippy --all-targets -- -D warnings` and
# `cargo fmt --all -- --check`.
#
# The .bazelrc aspects lint whatever a build names, which is the right tradeoff
# for local feedback but makes coverage a property of the command line: nothing
# in a build of //src:bazel-diff checks //tools/coverage. These two targets pin
# the roots instead, so the gate cannot quietly shrink when a CI command changes.
# `transitive` walks deps/crate from each root; external crates are skipped by
# the aspects themselves.
#
# Adding a first-party Rust crate? Add its root here.
_RUST_LINT_ROOTS = [
    "//src:bazel-diff",
    "//src:bazel_diff_lib",
    # The un-split, whole-crate e2e target (//tests:e2e_test is a test_suite
    # over one target per case, and the per-case targets carry
    # no-clippy/no-rustfmt so the crate is linted once rather than 38 times).
    "//tests:e2e_test_all",
    "//tools/coverage:lcov_merger",
    "//tools/coverage:lcov_merger_test",
]

rust_clippy_test(
    name = "rust_clippy_check",
    targets = _RUST_LINT_ROOTS,
    transitive = True,
)

rustfmt_test(
    name = "rust_format_check",
    targets = _RUST_LINT_ROOTS,
    transitive = True,
)

alias(
    name = "format",
    actual = "//tools/format:rustfmt",
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
    package_version = "49.0.2",
)
