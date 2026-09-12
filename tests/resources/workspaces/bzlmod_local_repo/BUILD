genrule(
    name = "consume",
    srcs = ["@dep_repo//:hello"],
    outs = ["consume.out"],
    cmd = "cat $(SRCS) > $@",
)
