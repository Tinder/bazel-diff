load("@rules_shell//shell:sh_library.bzl", "sh_library")
load("@rules_shell//shell:sh_binary.bzl", "sh_binary")

sh_library(
    name = "lib",
    srcs = ["lib.sh"],
)

sh_binary(
    name = "bin",
    srcs = ["bin.sh"],
    deps = [":lib"],
)
