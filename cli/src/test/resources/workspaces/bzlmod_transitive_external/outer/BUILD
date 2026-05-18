genrule(
    name = "lib",
    srcs = ["@inner//:data"],
    outs = ["lib.out"],
    cmd = "cat $(SRCS) > $@",
    visibility = ["//visibility:public"],
)
