genrule(
    name = "consume",
    srcs = ["@outer//:lib"],
    outs = ["consume.out"],
    cmd = "cat $(SRCS) > $@",
)
