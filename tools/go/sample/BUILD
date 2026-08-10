load("@rules_go//go:def.bzl", "go_library", "go_test")
load("//tools/coverage:defs.bzl", "coverage_enforced_test")

go_library(
    name = "sample",
    srcs = ["sample.go"],
    importpath = "github.com/Tinder/bazel-diff/tools/go/sample",
    visibility = ["//visibility:public"],
)

# `bazel coverage` fails this target if line coverage of tools/go/ drops
# below 90% — the same floor the repo-wide CI gate applies to Go — enforced
# per-target by //tools/coverage:lcov_merger.
coverage_enforced_test(
    name = "sample_test",
    srcs = ["sample_test.go"],
    coverage_include = ["tools/go/"],
    embed = [":sample"],
    min_line_coverage = 90,
    rule = go_test,
)
