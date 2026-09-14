# Consumer behind the hub alias:
#   //:consumer -> @pip//numpy:pkg (hub alias) -> @pip_numpy//:lib (spoke)
genrule(
    name = "consumer",
    srcs = ["@pip//numpy:pkg"],
    outs = ["consumer.out"],
    cmd = "cat $(SRCS) > $@",
)
