load("@rules_go//go:def.bzl", "go_library", "go_test")

go_library(
    name = "lib",
    srcs = ["lib.go"],
    importpath = "example.com/go_mod_change_test/lib",
    deps = ["@com_github_pkg_errors//:errors"],
)

go_test(
    name = "lib_test",
    srcs = ["lib_test.go"],
    embed = [":lib"],
)
