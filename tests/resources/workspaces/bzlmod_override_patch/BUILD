exports_files(["dep.patch"])

genrule(
    name = "consumer",
    srcs = ["@patched_dep//:data.txt"],
    outs = ["copied.txt"],
    cmd = "cat $(SRCS) > $@",
    cmd_bat = "type $(SRCS) > $@",
)

filegroup(
    name = "unrelated",
    srcs = ["unrelated.txt"],
)
