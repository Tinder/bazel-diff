load("@rules_shell//shell:sh_binary.bzl", "sh_binary")
load("@rules_shell//shell:sh_library.bzl", "sh_library")

sh_binary(
    name = "one",
    srcs = ["one.sh"],
)

# Put a generated file in the path to ensure that we properly handle deps that cross
# one.
genrule(
    name = "gen_two",
    outs = ["two.sh"],
    cmd = "$(location :one) && echo two > $@",
    tools = [":one"]
)
sh_library(
    name = "two",
    srcs = ["two.sh"],
)

sh_library(
    name = "three",
    srcs = ["three.sh"],
    deps =["two"],
    visibility = ["//visibility:public"],
)


