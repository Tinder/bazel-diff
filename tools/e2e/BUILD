load("@rules_python//python:defs.bzl", "py_binary", "py_library", "py_test")

# Regenerates the per-case lists in kotlin_e2e_cases.bzl and rust_e2e_cases.bzl
# from the e2e test sources. `make regen-e2e` is the entry point; this target is
# what it runs. Writes through $BUILD_WORKSPACE_DIRECTORY, so it only works under
# `bazel run`, never `bazel build`.
py_binary(
    name = "regen",
    srcs = ["split_e2e_tests.py"],
    main = "split_e2e_tests.py",
    python_version = "PY3",
    visibility = ["//visibility:public"],
)

py_library(
    name = "split_e2e_tests_lib",
    srcs = ["split_e2e_tests.py"],
    imports = ["."],
    visibility = ["//visibility:private"],
)

# The CI gate: re-derives the case lists from the sources in the runfiles tree
# and fails if they differ from the checked-in ones. This is what makes the
# split self-maintaining -- an e2e test added, renamed or deleted without a
# regen cannot merge.
#
# The `data` are the inputs the generator reads: the two e2e source trees and
# the two files it produces. They reach the test as runfiles at their
# repo-relative paths, which is where find_repo_root() looks under
# $TEST_SRCDIR/$TEST_WORKSPACE.
py_test(
    name = "regen_check",
    srcs = ["regen_check.py"],
    data = [
        "kotlin_e2e_cases.bzl",
        "rust_e2e_cases.bzl",
        "//cli:e2e_test_sources",
        "//tests:e2e_test_sources",
    ],
    python_version = "PY3",
    deps = [":split_e2e_tests_lib"],
)

# Unit tests for the parsers and renderers themselves. They drive the generator
# with inline source snippets rather than the repo's own e2e files, so they can
# assert the cases it *should* reject (a duplicate target name, an undeclared
# Rust module, an unknown timeout) -- which a run against the real sources can
# never reach.
py_test(
    name = "split_e2e_tests_test",
    srcs = ["split_e2e_tests_test.py"],
    # RepoConsistencyTest runs the collectors over the repo's own e2e sources,
    # so it needs the same runfiles :regen_check does.
    data = [
        "//cli:e2e_test_sources",
        "//tests:e2e_test_sources",
    ],
    python_version = "PY3",
    deps = [":split_e2e_tests_lib"],
)
