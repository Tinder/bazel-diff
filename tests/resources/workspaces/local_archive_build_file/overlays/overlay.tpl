load("@@//:tools.bzl", "tool")

genrule(
    name = "payload",
    outs = ["value.txt"],
    cmd = "echo VALUE > $@",
    visibility = ["//visibility:public"],
)

tool(
    name = "implementation",
    value = ":payload",
    visibility = ["//visibility:public"],
)
