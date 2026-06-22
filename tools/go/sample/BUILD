load("@rules_go//go:def.bzl", "go_library", "go_test")

go_library(
    name = "sample",
    srcs = ["sample.go"],
    importpath = "github.com/Tinder/bazel-diff/tools/go/sample",
    visibility = ["//visibility:public"],
)

go_test(
    name = "sample_test",
    srcs = ["sample_test.go"],
    embed = [":sample"],
)
